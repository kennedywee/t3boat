//! One remote machine at runtime: its power state on Boat, its watchdog, waking it on demand and
//! capturing its final state when it goes to sleep.

mod capture;
mod connect;
mod link;
pub mod ports;
pub mod projects;
pub mod provision;
pub mod signin;
mod work;

use crate::boat::{Boat, Resume, Sandbox};
use crate::proxy::auth::Auth;
use crate::proxy::cache::{self, Cache};
use crate::proxy::classify;
use crate::proxy::threads;
use crate::proxy::upstream;
use crate::store::{GithubUploads, Machine, Settings};
use crate::t3desktop::T3Desktop;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{mpsc, watch};

const WATCHDOG_POLL: Duration = Duration::from_secs(5);
const BOAT_POLL: Duration = Duration::from_secs(60);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(20);
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(120);
/// A requested sleep: snapshot save, drain grace and the stop itself.
const SLEEP_TIMEOUT: Duration = Duration::from_secs(600);
/// After a failed wake, requests fail fast instead of each retrying `boat resume`.
const WAKE_COOLDOWN: Duration = Duration::from_secs(30);
/// The watchdog's limit too.
pub const MAX_KEEP_AWAKE_MINUTES: u32 = 24 * 60;
const KEEP_ALIVE_AFTER: chrono::TimeDelta = chrono::TimeDelta::days(12);
const KEEP_ALIVE_RETRY: Duration = Duration::from_secs(3600);
/// A machine woken for work due is up this much ahead of it: a wake from a snapshot is slow.
const DUE_LEAD: chrono::TimeDelta = chrono::TimeDelta::minutes(2);
const DUE_WAKE_RETRY: Duration = Duration::from_secs(5 * 60);
/// A running machine's link is checked this often: links can be turned on, fail or near expiry
/// while it stays up.
const LINK_CHECK: Duration = Duration::from_secs(10 * 60);
const GH_UPLOADS_RETRY: Duration = Duration::from_secs(60);
/// How long a wake waits for a changed GitHub login to reach the machine.
const GH_UPLOADS_ON_WAKE: Duration = Duration::from_secs(20);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Power {
    Unknown,
    Starting,
    Running,
    Stopping,
    Stopped,
    /// The sandbox is gone on Boat, or the machine was deleted here. Final.
    Missing,
}

impl Power {
    fn from_sandbox(sb: &Sandbox) -> Self {
        match sb.state.as_str() {
            "ready" | "idle" | "running" => Power::Running,
            "stopped" => Power::Stopped,
            "stopping" => Power::Stopping,
            "provisioned" | "cloning" | "resuming" | "starting" => Power::Starting,
            _ => Power::Unknown,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub sequence: u64,
    pub threads: HashMap<String, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WatchdogPhase {
    Active,
    /// Going to sleep: saving the snapshot it can be restored from. Any use calls it off.
    Saving,
    Draining,
    Stopping,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchdogStatus {
    pub phase: WatchdogPhase,
    pub busy: bool,
    /// Agent runs in flight.
    pub working_threads: u32,
    pub working_subagents: u32,
    /// Busy only because of CPU use (a build or job outside an agent turn).
    pub cpu_busy: bool,
    /// T3 is not answering, so the watchdog keeps it up (at most 30 min) in case an agent works.
    pub agents_unknown: bool,
    /// Seconds left of a "keep awake"; 0 when off or until stopped.
    pub keep_awake_secs: u64,
    /// Kept awake until someone stops it: no auto sleep at all.
    pub keep_awake_forever: bool,
    pub idle_secs: u64,
    pub sleep_after_secs: u64,
    /// When T3 next starts work on its own, ms since epoch. Older watchdogs don't say.
    #[serde(default)]
    pub next_due_at: Option<i64>,
    /// That is soon: the machine waits for it instead of sleeping.
    #[serde(default)]
    pub work_due: bool,
    /// The T3 version installed there. Older watchdogs don't say.
    #[serde(default)]
    pub t3_version: Option<String>,
    pub manifest: Option<Manifest>,
    /// When this app got the status: what its second counts are relative to.
    #[serde(skip, default = "SystemTime::now")]
    pub received: SystemTime,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// Something visible changed.
    Changed,
    /// A machine record changed and must be saved.
    Persist,
    Notify { title: String, body: String },
}

/// What every machine shares: the user's CLIs, settings and the app's event channel.
pub struct Shared {
    pub boat: Boat,
    pub t3: T3Desktop,
    pub settings: RwLock<Settings>,
    pub github_uploads: RwLock<Option<GithubUploads>>,
    pub events: mpsc::UnboundedSender<Event>,
    pub http: reqwest::Client,
    /// Names this running app to a machine's watchdog, which may hear from several computers.
    pub client: String,
    /// When a client last used any machine through this app: a paired device, an agent over
    /// MCP. Starts at the app's start.
    pub last_client_activity: Mutex<Instant>,
    /// True once the mothership commits to `boat stop self`, blocking new wakes and MCP work.
    pub stopping: std::sync::atomic::AtomicBool,
}

impl Shared {
    pub fn extras(&self) -> provision::Extras {
        let uploads = self.github_uploads.read().unwrap();
        provision::Extras {
            fleet: self.settings.read().unwrap().fleet_repo.clone(),
            gh_upload_token: uploads.as_ref().map(|g| g.token.clone()),
            gh_everything: uploads.as_ref().is_some_and(|g| g.everything),
        }
    }
    pub fn changed(&self) {
        let _ = self.events.send(Event::Changed);
    }
    /// Saved by the event loop, the state file's one writer.
    pub fn persist(&self) {
        let _ = self.events.send(Event::Persist);
    }
    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>) {
        let _ = self.events.send(Event::Notify { title: title.into(), body: body.into() });
    }
    /// A client used this app just now.
    pub fn touch_activity(&self) {
        *self.last_client_activity.lock().unwrap() = Instant::now();
    }
    /// Since the last client activity, or since the start if none.
    pub fn idle_for(&self) -> Duration {
        self.last_client_activity.lock().unwrap().elapsed()
    }
    pub fn set_stopping(&self, stopping: bool) {
        self.stopping.store(stopping, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub struct MachineRuntime {
    pub shared: Arc<Shared>,
    record: RwLock<Machine>,
    dir: PathBuf,
    pub cache: Mutex<Cache>,
    pub auth: Auth,
    power: watch::Sender<Power>,
    stopped_at: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
    watchdog: Mutex<Option<WatchdogStatus>>,
    /// One wake at a time; a delete takes it too, so no restore swaps the sandbox under it.
    pub(crate) waking: tokio::sync::Mutex<()>,
    /// One change to the machine at a time. Each reads the settings once it holds this, so a
    /// disconnect that overtakes a connect still wins.
    provisioning: tokio::sync::Mutex<()>,
    /// The loop's "GitHub login" try: for which settings, and when it failed (`None`: running).
    gh_uploads_try: Mutex<Option<(String, Option<Instant>)>>,
    /// One capture at a time: "sleep now" and the background poll can see the same drain.
    capturing: tokio::sync::Mutex<()>,
    last_heartbeat: Mutex<Option<Instant>>,
    /// When the watchdog was seen stopping the machine; cleared once Boat reports it stopped or
    /// the watchdog is active again.
    stop_seen: Mutex<Option<Instant>>,
    /// The last failed wake: its message and when.
    wake_failure: Mutex<Option<(String, Instant)>>,
    /// Its T3 projects as last read, and those t3boat is adding.
    projects: Mutex<Vec<projects::MachineProject>>,
    adding: Mutex<Vec<projects::MachineProject>>,
    ports: ports::Ports,
    /// The machine's T3 version the user was last told is ahead of this computer's.
    warned_ahead: Mutex<Option<String>>,
}

impl MachineRuntime {
    pub fn new(shared: Arc<Shared>, record: Machine, dir: PathBuf) -> Self {
        let cache = Cache::load(&cache::cache_path(&dir));
        let shell = cache.capture.as_ref().map(|c| c.shell.clone());
        let this = Self {
            shared,
            record: RwLock::new(record),
            dir,
            cache: Mutex::new(cache),
            auth: Auth::default(),
            power: watch::channel(Power::Unknown).0,
            stopped_at: Mutex::new(None),
            watchdog: Mutex::new(None),
            waking: tokio::sync::Mutex::new(()),
            provisioning: tokio::sync::Mutex::new(()),
            gh_uploads_try: Mutex::new(None),
            capturing: tokio::sync::Mutex::new(()),
            last_heartbeat: Mutex::new(None),
            stop_seen: Mutex::new(None),
            wake_failure: Mutex::new(None),
            projects: Mutex::new(Vec::new()),
            adding: Mutex::new(Vec::new()),
            ports: ports::Ports::default(),
            warned_ahead: Mutex::new(None),
        };
        if let Some(shell) = shell {
            this.set_projects_from(&shell);
        }
        this
    }

    pub fn record(&self) -> Machine {
        self.record.read().unwrap().clone()
    }

    pub fn with_record<R>(&self, f: impl FnOnce(&Machine) -> R) -> R {
        f(&self.record.read().unwrap())
    }

    pub fn update_record(&self, f: impl FnOnce(&mut Machine)) {
        f(&mut self.record.write().unwrap());
        self.shared.persist();
        self.shared.changed();
    }

    /// Mutates the record under the write lock, persisting and notifying only when `f` reports
    /// that the record actually changed.
    pub fn mutate_record<R>(&self, f: impl FnOnce(&mut Machine) -> (bool, R)) -> R {
        let (changed, out) = f(&mut self.record.write().unwrap());
        if changed {
            self.shared.persist();
            self.shared.changed();
        }
        out
    }

    pub fn id(&self) -> String {
        self.record.read().unwrap().id.clone()
    }

    /// The Boat sandbox the machine runs on now.
    pub fn sandbox_id(&self) -> String {
        self.record.read().unwrap().sandbox_id.clone()
    }

    /// Where this machine's cache, chats and assets are kept on this computer.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn power(&self) -> Power {
        *self.power.borrow()
    }

    pub fn subscribe_power(&self) -> watch::Receiver<Power> {
        self.power.subscribe()
    }

    /// Deleted by this app: nothing may wake it or write to its dir again. Taken under the cache
    /// lock, so no `save_cache` that missed it is still writing.
    pub fn retire(&self) {
        let _cache = self.cache.lock().unwrap();
        self.set_power(Power::Missing);
    }

    /// Missing is final: a Boat answer or a wake still in flight from before must not revive a
    /// deleted machine.
    fn set_power(&self, p: Power) {
        let changed = self.power.send_if_modified(|cur| {
            if *cur == Power::Missing || *cur == p {
                return false;
            }
            *cur = p;
            true
        });
        if changed {
            if p != Power::Running {
                *self.watchdog.lock().unwrap() = None;
            }
            self.shared.changed();
        }
    }

    pub fn watchdog_status(&self) -> Option<WatchdogStatus> {
        self.watchdog.lock().unwrap().clone()
    }

    /// Long enough for any stop to finish; a stop still pending after this failed.
    const STOP_SETTLE: Duration = Duration::from_secs(120);

    pub fn last_error(&self) -> Option<String> {
        self.wake_failure.lock().unwrap().as_ref().map(|(e, _)| e.clone())
    }

    /// The machine is about to freeze or frozen: user actions must wait for it and wake it again.
    pub fn is_stopping(&self) -> bool {
        matches!(self.power(), Power::Stopping | Power::Stopped) || self.stop_pending()
    }

    fn stop_pending(&self) -> bool {
        self.stop_seen.lock().unwrap().is_some_and(|t| t.elapsed() < Self::STOP_SETTLE)
    }

    pub fn save_cache(&self) {
        if let Err(e) = self.try_save_cache() {
            tracing::warn!("saving cache for {}: {e:#}", self.id());
        }
    }

    pub fn try_save_cache(&self) -> Result<()> {
        // Serialized and written under the lock: one writer at a time for the file.
        let cache = self.cache.lock().unwrap();
        if self.power() == Power::Missing {
            return Ok(());
        }
        let bytes = cache.to_bytes()?;
        crate::store::write_private(&cache::cache_path(&self.dir), &bytes)
    }

    fn follow_size(&self, sb: &crate::boat::Sandbox) {
        let record = self.record.read().unwrap();
        let Some(size) = sb.size.as_deref().filter(|s| *s != record.size && provision::size(s).is_ok()) else { return };
        if record.resize_pending {
            return;
        }
        drop(record);
        self.update_record(|m| m.size = size.to_owned());
    }

    pub async fn refresh_power(&self) -> Result<Power> {
        let id = self.sandbox_id();
        match self.shared.boat.info(&id).await {
            Ok(sb) => {
                // A restore swapped the sandbox meanwhile: this answer is about the old one.
                if self.sandbox_id() != id {
                    return Ok(self.power());
                }
                *self.stopped_at.lock().unwrap() = sb.stopped_at;
                self.follow_size(&sb);
                let mut p = Power::from_sandbox(&sb);
                if p == Power::Stopped {
                    *self.stop_seen.lock().unwrap() = None;
                } else if p == Power::Running && self.stop_pending() {
                    // Boat lags behind the watchdog by a few seconds.
                    p = Power::Stopping;
                } else if p == Power::Running && self.waking.try_lock().is_err() {
                    // A wake is waiting for T3 to answer; it publishes Running once it does.
                    return Ok(p);
                }
                self.set_power(p);
                Ok(p)
            }
            Err(e) => {
                let gone = self.shared.boat.confirmed_gone(&id).await;
                if self.sandbox_id() != id {
                    // Restored meanwhile, and the old sandbox deleted.
                    return Ok(self.power());
                }
                if gone {
                    self.set_power(Power::Missing);
                    return Ok(Power::Missing);
                }
                Err(e)
            }
        }
    }

    /// Waits out transitional states, so callers see Running or Stopped (or Missing). A wake
    /// publishes its own outcome; the run loop resolves the rest.
    pub async fn settled_power(&self) -> Power {
        let mut rx = self.subscribe_power();
        let settled = |p: &Power| matches!(p, Power::Running | Power::Stopped | Power::Missing);
        let _ = tokio::time::timeout(SETTLE_TIMEOUT, rx.wait_for(settled)).await;
        self.power()
    }

    pub fn upstream_url(&self) -> String {
        self.record.read().unwrap().upstream_url.trim_end_matches('/').to_owned()
    }

    pub fn upstream_token(&self) -> String {
        self.record.read().unwrap().upstream_token.clone()
    }

    /// The machine's own descriptor, never Boat's fallback page for an unrouted port or an
    /// error body. Cached for while the machine sleeps.
    pub async fn fetch_descriptor(&self) -> Option<Value> {
        let url = format!("{}/.well-known/t3/environment", self.upstream_url());
        let res = self.shared.http.get(url).timeout(Duration::from_secs(3)).send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        let v: Value = res.json().await.ok()?;
        if v["environmentId"].as_str() != Some(&self.record().environment_id) {
            return None;
        }
        self.cache.lock().unwrap().descriptor = Some(v.clone());
        Some(v)
    }

    /// Boat does not always restore hosted ports on resume.
    async fn rehost(&self) -> Result<()> {
        let (t3, watchdog) = provision::host(&self.shared.boat, &self.sandbox_id()).await?;
        self.update_record(|m| {
            m.upstream_url = t3;
            m.watchdog_url = watchdog;
        });
        Ok(())
    }

    /// Brings the machine up if needed and returns once its T3 server answers and will stay up.
    /// Concurrent callers share one wake.
    pub async fn ensure_awake(&self) -> Result<()> {
        if self.shared.is_stopping() {
            bail!("t3boatd is going to sleep");
        }
        let _guard = self.waking.lock().await;
        if self.shared.is_stopping() {
            bail!("t3boatd is going to sleep");
        }
        // Checked under the lock: a delete holding it retires the machine.
        if self.power() == Power::Missing {
            bail!("the sandbox no longer exists on Boat");
        }
        if let Some((e, at)) = self.wake_failure.lock().unwrap().clone() {
            if at.elapsed() < WAKE_COOLDOWN {
                return Err(anyhow!(e));
            }
        }
        if self.power() == Power::Running && !self.is_stopping() && self.fetch_descriptor().await.is_some() {
            if self.watchdog_status().is_some_and(|w| matches!(w.phase, WatchdogPhase::Saving | WatchdogPhase::Draining)) {
                // Someone needs it after all: call the sleep off before it stops.
                self.send_activity(false).await;
            }
            // Too late if the watchdog is already stopping: wake_locked waits for it and resumes.
            if !self.is_stopping() {
                if self.wake_failure.lock().unwrap().take().is_some() {
                    self.shared.changed();
                }
                // A message accepted while another caller's wake was finishing.
                self.flush_deferred_commands().await;
                return Ok(());
            }
        }
        let result = self.wake_locked().await;
        let was_failing = self.wake_failure.lock().unwrap().is_some();
        match &result {
            Ok(()) => *self.wake_failure.lock().unwrap() = None,
            Err(e) => {
                let msg = format!("{e:#}");
                if !was_failing {
                    self.shared.notify(format!("{} could not wake", self.record().name), msg.clone());
                }
                *self.wake_failure.lock().unwrap() = Some((msg, Instant::now()));
                let _ = self.refresh_power().await;
            }
        }
        self.shared.changed();
        result
    }

    async fn wake_locked(&self) -> Result<()> {
        let id = self.id();
        let started = Instant::now();
        let fresh = self.fresh_capture().is_some();
        self.set_power(Power::Starting);
        let mut resumed = false;
        let mut restored = false;
        loop {
            match self.refresh_power().await? {
                Power::Running => break,
                Power::Stopped => {
                    tracing::info!("resuming {id}");
                    self.set_power(Power::Starting);
                    let record = self.record();
                    if self.shared.boat.resume(&record.sandbox_id, &record.size).await? == Resume::DataExpired {
                        self.restore().await?;
                        restored = true;
                    }
                    // A resize asked for during the resume stays pending.
                    self.update_record(|m| m.resize_pending &= m.size != record.size);
                    resumed = true;
                    break;
                }
                Power::Missing => bail!("the sandbox no longer exists on Boat"),
                _ => {}
            }
            if started.elapsed() > READY_TIMEOUT {
                bail!("the sandbox is stuck between states on Boat");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        // From here on the machine runs: whatever it slept with is history. A sleep the proxy
        // did not see may have outdated the recorded reads too.
        if !fresh {
            self.cache.lock().unwrap().bump_epoch();
        }
        self.drop_capture();
        self.set_power(Power::Starting);
        if self.fetch_descriptor().await.is_none() {
            self.rehost().await?;
        }
        self.wait_ready(started).await?;
        if resumed {
            // T3 serves now and may have started work on its own; restarting it would kill that.
            // A restored machine booted fresh from its snapshot: nothing to kill, and the
            // upgrade drops the snapshot's stale chats.
            let status = self.watchdog_get("/status?fresh").await;
            if let Ok(s) = &status {
                self.learn_t3_version(s);
            }
            if !restored && status.is_ok_and(|s| s.busy) {
                tracing::info!("{id} is busy: upgrade deferred to the next wake");
            } else if let Err(e) = self.upgrade_if_needed().await {
                tracing::warn!("upgrading {id}: {e:#}");
            }
        }
        // A login changed while the machine slept, before any work runs with the old one. The
        // loop retries a failure.
        match tokio::time::timeout(GH_UPLOADS_ON_WAKE, self.apply_gh_uploads()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("bringing the GitHub login to {id}: {e:#}"),
            Err(_) => tracing::warn!("bringing the GitHub login to {id} took too long"),
        }
        // Before Running is published: T3 Code reconnects then, and must find the messages
        // accepted while asleep ahead of anything it sends next.
        self.on_ready().await;
        self.set_power(Power::Running);
        tracing::info!("{id} awake after {:?}", started.elapsed());
        self.send_activity(resumed).await;
        Ok(())
    }

    /// T3 answers after a wake: sends what waited for it and refreshes the session.
    async fn on_ready(&self) {
        self.flush_deferred_commands().await;
        if let Err(e) = self.prime().await {
            tracing::warn!("refreshing {}'s session: {e:#}", self.id());
        }
    }

    /// Boat deleted the stopped sandbox's disk: a new sandbox takes its place, its disk the
    /// snapshot the watchdog saved at the last sleep. Its addresses differ; the wake hosts them.
    async fn restore(&self) -> Result<()> {
        let record = self.record();
        let boat = &self.shared.boat;
        tracing::info!("restoring {} from its snapshot", record.id);
        let sandbox = boat
            .create(
                &provision::sandbox_name(&record.name),
                &record.size,
                Some(&provision::snapshot_name(&record.name, &record.id)),
            )
            .await
            .context("Boat deleted this machine's disk after 14 days asleep, and restoring its snapshot failed")?;
        // The snapshot can predate the last upgrade and the stored chats: the wake provisions it
        // again, on the version its watchdog reports unless that one is newer.
        self.update_record(|m| {
            m.sandbox_id = sandbox;
            m.provision_revision.clear();
        });
        self.forget_recorded();
        if let Err(e) = boat.delete(&record.sandbox_id).await {
            tracing::warn!("deleting {}'s expired sandbox {}: {e:#}", record.id, record.sandbox_id);
        }
        Ok(())
    }

    /// Fills what the proxy must answer before the machine can sleep: descriptor and session.
    pub async fn prime(&self) -> Result<()> {
        self.fetch_descriptor().await.ok_or_else(|| anyhow!("T3 on the machine did not answer"))?;
        let session = self
            .shared
            .http
            .get(format!("{}/api/auth/session", self.upstream_url()))
            .bearer_auth(self.upstream_token())
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json::<Map<String, Value>>()
            .await
            .context("T3 on the machine sent a malformed auth session")?;
        self.cache.lock().unwrap().auth_session = Some(session);
        self.save_cache();
        Ok(())
    }

    async fn wait_ready(&self, started: Instant) -> Result<()> {
        while self.fetch_descriptor().await.is_none() {
            if started.elapsed() > READY_TIMEOUT {
                bail!("T3 on the machine did not answer");
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(())
    }

    /// Leaves the running machine with the settings' GitHub login, or without one.
    async fn bring_gh_uploads(&self) -> Result<()> {
        // The login first: it must not wait behind apt. This fails while `gh` is too old to
        // upload, with the login already in place.
        if self.apply_gh_uploads().await.is_ok() {
            return Ok(());
        }
        // Outside the lock: apt can take minutes, and a disconnect must not wait behind it.
        if self.shared.extras().gh_upload_token.is_some() {
            let (boat, id) = (&self.shared.boat, self.sandbox_id());
            let _ = provision::gh_uploads(boat, &id, "upgrade", &provision::Extras::default()).await;
        }
        self.apply_gh_uploads().await
    }

    async fn apply_gh_uploads(&self) -> Result<()> {
        let (boat, id) = (&self.shared.boat, self.sandbox_id());
        let _one = self.provisioning.lock().await;
        let extras = self.shared.extras();
        let revision = provision::gh_uploads_revision(&extras);
        if self.record().gh_uploads_revision.as_deref() == Some(&revision) {
            return Ok(());
        }
        // A run that fails part way may have changed the token already.
        self.update_record(|m| m.gh_uploads_revision = None);
        provision::gh_uploads(boat, &id, "apply", &extras).await?;
        self.update_record(|m| m.gh_uploads_revision = Some(revision));
        Ok(())
    }

    /// The machine says which T3 version it runs. One this app did not install came from another
    /// computer's t3boat: what was recorded from the old server is in its encoding.
    fn learn_t3_version(&self, status: &WatchdogStatus) {
        let Some(version) = status.t3_version.as_deref() else { return };
        let known = self.record.read().unwrap().t3_version.clone();
        // Mid-upgrade the new watchdog answers before this app has noted its own change.
        if known == version || self.provisioning.try_lock().is_err() {
            return;
        }
        tracing::info!("{} runs T3 {version}, set on another computer (was {known})", self.id());
        self.update_record(|m| {
            m.t3_version = version.to_owned();
            m.t3_set_elsewhere = true;
        });
        self.forget_recorded();
    }

    /// What was recorded describes a server or a disk that is no longer the machine's.
    pub(crate) fn forget_recorded(&self) {
        self.cache.lock().unwrap().bump_epoch();
        self.drop_server_encoding();
    }

    /// Recorded streams and stored chats are in the encoding of the server that sent them.
    fn drop_server_encoding(&self) {
        self.cache.lock().unwrap().streams.clear();
        threads::sweep(&self.dir, &HashSet::new());
        self.save_cache();
    }

    /// Keeps the machine on the desktop app's T3 version, this app's scripts and the fleet setting.
    /// A newer T3 version that another computer installed stays: with two computers on different
    /// versions, each would otherwise change the machine back at every wake.
    pub async fn upgrade_if_needed(&self) -> Result<()> {
        let _one = self.provisioning.lock().await;
        let desktop = self.shared.t3.version().await?;
        let extras = self.shared.extras();
        let revision = provision::revision(&extras);
        let record = self.record();
        let ahead = record.t3_set_elsewhere && provision::newer_t3(&record.t3_version, &desktop);
        if ahead && self.warned_ahead.lock().unwrap().replace(record.t3_version.clone()).as_ref() != Some(&record.t3_version) {
            self.shared.notify(
                format!("Update T3 Code to use {}", record.name),
                format!(
                    "Another computer moved it to T3 Code {}. This computer has {desktop}; chats may not load until it is updated.",
                    record.t3_version
                ),
            );
        }
        let desktop = if ahead { record.t3_version.clone() } else { desktop };
        let new_server = desktop != record.t3_version;
        if !new_server && revision == record.provision_revision {
            return Ok(());
        }
        tracing::info!("upgrading {} (T3 {desktop}, scripts {revision})", self.id());
        // provision.sh runs gh-uploads.sh but does not say whether it worked: the loop finds out.
        self.update_record(|m| m.gh_uploads_revision = None);
        provision::run(
            &self.shared.boat,
            &record.sandbox_id,
            &record.id,
            &record.name,
            &desktop,
            &record.watchdog_secret,
            &provision::snapshot_name(&record.name, &record.id),
            false,
            &extras,
        )
        .await?;
        self.update_record(|m| {
            m.t3_set_elsewhere = ahead;
            m.t3_version = desktop;
            m.provision_revision = revision;
        });
        self.cache.lock().unwrap().bump_epoch();
        if new_server {
            self.drop_server_encoding();
        }
        self.wait_ready(Instant::now()).await
    }

    async fn flush_deferred_commands(&self) {
        // Left queued until delivered: the run loop retries a failed send.
        let commands = self.cache.lock().unwrap().deferred_commands.clone();
        if commands.is_empty() {
            return;
        }
        let requests = commands
            .iter()
            .map(|c| ("orchestration.dispatchCommand".to_owned(), c.clone()))
            .collect();
        match upstream::call_many(&self.upstream_url(), &self.upstream_token(), requests).await {
            Ok(results) => {
                self.cache.lock().unwrap().deferred_commands.retain(|c| !commands.contains(c));
                // T3 Code was told they were accepted: a refused message is said here or nowhere.
                for (command, result) in commands.iter().zip(results) {
                    if let Err(e) = result
                        && command["type"] == "message.dispatch"
                    {
                        tracing::warn!("message to {} refused: {e}", self.id());
                        self.shared.notify(format!("{}: a message was not sent", self.record().name), e);
                    }
                }
            }
            // Resent on the next try; the server dedupes them by commandId.
            Err(e) => {
                tracing::warn!("sending deferred commands: {e:#}");
                return;
            }
        }
        self.save_cache();
    }

    /// Someone used the machine: it stays awake, and recorded answers may be outdated.
    pub fn user_action(self: &Arc<Self>) {
        self.cache.lock().unwrap().bump_epoch();
        self.used();
    }

    /// Someone used the machine in a way that changes nothing T3 Code shows: it stays awake.
    pub fn used(self: &Arc<Self>) {
        self.shared.touch_activity();
        if self.power() != Power::Running {
            return;
        }
        {
            let mut last = self.last_heartbeat.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < HEARTBEAT_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let this = self.clone();
        tokio::spawn(async move { this.send_activity(false).await });
    }

    /// `resumed`: this app just resumed the sandbox. The watchdog was frozen mid-stop and, after
    /// a short sleep, cannot tell: it would refuse activity and report a stop for another 90 s.
    pub(crate) async fn send_activity(&self, resumed: bool) {
        *self.last_heartbeat.lock().unwrap() = Some(Instant::now());
        match self.watchdog_post("/activity", json!({ "resumed": resumed })).await {
            Ok(s) => {
                *self.watchdog.lock().unwrap() = Some(s);
                self.shared.changed();
            }
            // Refused when the watchdog already stops the machine.
            Err(_) => {
                self.stopping_now().await;
            }
        }
    }

    /// Asks the watchdog whether it is stopping the machine, and records it if so.
    async fn stopping_now(&self) -> bool {
        let stopping = self.watchdog_get("/status").await.is_ok_and(|s| s.phase == WatchdogPhase::Stopping);
        if stopping {
            self.mark_stopping();
        }
        stopping
    }

    async fn watchdog_get(&self, path: &str) -> Result<WatchdogStatus> {
        self.watchdog_request(reqwest::Method::GET, path, None).await
    }

    async fn watchdog_post(&self, path: &str, body: Value) -> Result<WatchdogStatus> {
        self.watchdog_request(reqwest::Method::POST, path, Some(body)).await
    }

    async fn watchdog_request(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<WatchdogStatus> {
        let (url, secret) = {
            let m = self.record.read().unwrap();
            (format!("{}{path}", m.watchdog_url.trim_end_matches('/')), m.watchdog_secret.clone())
        };
        // These routes reply after the watchdog's own T3 check, which allows itself 10 s.
        let timeout = Duration::from_secs(match path {
            // Waits up to 10 s for another computer's capture, then checks T3.
            "/sleep-ack" => 25,
            "/status?fresh" | "/drain" => 15,
            _ => 4,
        });
        let mut req = self
            .shared
            .http
            .request(method, url)
            .bearer_auth(secret)
            .header("x-t3boat-client", &self.shared.client)
            .timeout(timeout);
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req.send().await?;
        let status = res.status();
        if status.as_u16() == 409 {
            let value: Value = res.json().await.unwrap_or_default();
            bail!("{}", value["error"].as_str().unwrap_or("not now"));
        }
        if !status.is_success() {
            bail!("watchdog {path}: HTTP {status}");
        }
        Ok(res.json().await?)
    }

    /// "Sleep now": asks the watchdog to drain. Refused while an agent works.
    pub async fn sleep_now(&self) -> Result<()> {
        if !self.awake_now().await? {
            return Ok(());
        }
        let s = self.watchdog_post("/drain", json!({})).await?;
        // Any use now calls the sleep off: the next user action must reach the watchdog.
        *self.last_heartbeat.lock().unwrap() = None;
        self.on_watchdog_status(s).await;
        Ok(())
    }

    /// Keeps the machine up for `minutes` whether or not anyone uses it (`None`: until stopped);
    /// `Some(0)` ends it. The watchdog enforces it, so it holds with the laptop closed too. Wakes a
    /// sleeping machine to start.
    pub async fn keep_awake(&self, minutes: Option<u32>) -> Result<()> {
        if minutes.is_some_and(|m| m > MAX_KEEP_AWAKE_MINUTES) {
            bail!("keep awake takes at most {MAX_KEEP_AWAKE_MINUTES} minutes, or until stopped");
        }
        // A stop ends any hold, so a machine that isn't running has none to end.
        if minutes == Some(0) && !self.awake_now().await? {
            return Ok(());
        }
        self.ensure_awake().await?;
        // Older scripts lack keep-awake: take this app's now, which restarts T3, so never
        // mid-turn. Ending a hold works with any watchdog that has one.
        if minutes != Some(0) && self.record().provision_revision != provision::revision(&self.shared.extras()) {
            if self.watchdog_get("/status?fresh").await?.busy {
                bail!("needs a quick update first; try again once its agents finish");
            }
            self.kept_awake(self.upgrade_if_needed()).await?;
        }
        let body = json!({ "minutes": minutes });
        let s = match self.watchdog_post("/keep-awake", body.clone()).await {
            Ok(s) => s,
            // Refused mid-stop: ending it is then already done; anything else waits for the stop,
            // wakes the machine and asks again.
            Err(_) if self.stopping_now().await => {
                if minutes == Some(0) {
                    return Ok(());
                }
                self.ensure_awake().await?;
                self.watchdog_post("/keep-awake", body).await?
            }
            Err(e) => return Err(e),
        };
        self.on_watchdog_status(s).await;
        Ok(())
    }

    /// New size: applied on the next resume. An idle machine is restarted onto it right away,
    /// keeping any "keep awake"; a working one (an agent or a build) moves when it next sleeps.
    pub async fn resize(&self, size: &str) -> Result<()> {
        provision::size(size)?;
        if self.record().size == size {
            return Ok(());
        }
        self.update_record(|m| {
            m.size = size.to_owned();
            m.resize_pending = true;
        });
        if self.settled_power().await != Power::Running {
            return Ok(());
        }
        let status = match self.watchdog_get("/status").await {
            Ok(s) if !s.busy => s,
            _ => return Ok(()),
        };
        // The sleep ends any hold: it is taken again after the restart.
        let hold = match (status.keep_awake_forever, status.keep_awake_secs) {
            (true, _) => Some(None),
            (false, 0) => None,
            (false, secs) => Some(Some(secs.div_ceil(60) as u32)),
        };
        if self.sleep_now().await.is_err() {
            return Ok(());
        }
        // The drain dropped the hold even if the sleep was called off.
        let slept = self.slept().await;
        match hold {
            Some(minutes) => self.keep_awake(minutes).await?,
            None if slept => self.ensure_awake().await?,
            None => {}
        }
        Ok(())
    }

    /// For actions on a running machine: true when it runs, false when it sleeps (or is gone).
    async fn awake_now(&self) -> Result<bool> {
        match self.settled_power().await {
            Power::Running => Ok(true),
            Power::Stopped | Power::Missing => Ok(false),
            _ => bail!("still waking; try again in a moment"),
        }
    }

    /// Waits for a sleep just asked for, snapshot save included: true once the machine is
    /// stopped, false if it was used meanwhile and stays up.
    async fn slept(&self) -> bool {
        let deadline = Instant::now() + SLEEP_TIMEOUT;
        while Instant::now() < deadline {
            match self.power() {
                Power::Stopped => return true,
                Power::Missing => return false,
                Power::Running
                    if !self.is_stopping()
                        && self.watchdog_status().is_none_or(|w| w.phase == WatchdogPhase::Active) =>
                {
                    return false
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        false
    }

    async fn on_watchdog_status(&self, s: WatchdogStatus) {
        if s.busy {
            self.cache.lock().unwrap().bump_epoch();
        }
        self.learn_t3_version(&s);
        if s.phase == WatchdogPhase::Active {
            // Back in use after a drain: an earlier capture no longer describes the machine.
            self.drop_capture();
            *self.stop_seen.lock().unwrap() = None;
        }
        if self.record.read().unwrap().next_due_at != s.next_due_at {
            self.update_record(|r| r.next_due_at = s.next_due_at);
        }
        let manifest = s.manifest.clone();
        let phase = s.phase;
        *self.watchdog.lock().unwrap() = Some(s);
        self.shared.changed();
        match (phase, manifest) {
            (WatchdogPhase::Draining, Some(m)) => {
                {
                    // Whoever waited here finds the capture done, and must not redo it: it may
                    // be confirmed by now.
                    let _capturing = self.capturing.lock().await;
                    if self.captured_sequence() != Some(m.sequence) {
                        self.drop_capture();
                        if let Err(e) = self.capture(&m).await {
                            tracing::warn!("capture failed for {}: {e:#}", self.id());
                            return;
                        }
                    }
                }
                // Still draining means the watchdog couldn't stop yet (T3 not answering): leave the
                // retry to the regular poll instead of re-acking in a loop.
                match self.watchdog_post("/sleep-ack", json!({ "sequence": m.sequence })).await {
                    Ok(s) if s.phase != WatchdogPhase::Draining => Box::pin(self.on_watchdog_status(s)).await,
                    _ => {}
                }
            }
            // Stopping without a manifest (the watchdog could not read it) still stops: only
            // the capture goes unconfirmed.
            (WatchdogPhase::Stopping, m) => {
                if m.is_some_and(|m| self.captured_sequence() == Some(m.sequence)) {
                    self.confirm_capture();
                }
                self.mark_stopping();
            }
            _ => {}
        }
    }

    /// Known before Boat says so: new sockets and wakes wait for the stop instead of reaching a
    /// machine that is about to freeze.
    fn mark_stopping(&self) {
        self.stop_seen.lock().unwrap().get_or_insert_with(Instant::now);
        self.set_power(Power::Stopping);
    }

    /// Background loop: follows the machine's power state and its watchdog.
    pub async fn run(self: Arc<Self>) {
        let unprimed = {
            let c = self.cache.lock().unwrap();
            c.descriptor.is_none() || c.auth_session.is_none()
        };
        if unprimed {
            // Without these T3 Code cannot even connect while the machine sleeps.
            let primed = async {
                self.ensure_awake().await?;
                self.prime().await
            };
            if let Err(e) = primed.await {
                tracing::warn!("priming {}: {e:#}", self.id());
            }
        }
        let mut last_boat_poll: Option<Instant> = None;
        let mut last_keep_alive: Option<Instant> = None;
        let mut last_due_wake: Option<Instant> = None;
        let mut last_link_check: Option<Instant> = None;
        let mut watchdog_failures = 0u32;
        let mut was_busy = false;
        let mut was_running = false;
        loop {
            let transitioning = matches!(self.power(), Power::Stopping | Power::Starting | Power::Unknown);
            if transitioning || last_boat_poll.is_none_or(|t| t.elapsed() >= BOAT_POLL) || watchdog_failures >= 2 {
                last_boat_poll = Some(Instant::now());
                watchdog_failures = 0;
                if let Err(e) = self.refresh_power().await {
                    tracing::warn!("boat info {}: {e:#}", self.id());
                }
            }
            // Before a deleted machine's loop ends: its forwards end with it.
            if self.forwards_to_sync() {
                let this = self.clone();
                tokio::spawn(async move { this.sync_forwards().await });
            }
            if self.power() == Power::Missing {
                return;
            }
            if self.needs_keep_alive() && last_keep_alive.is_none_or(|t| t.elapsed() >= KEEP_ALIVE_RETRY) {
                // Boat keeps a stopped disk 14 days; a short wake renews it. The watchdog
                // puts the machine back to sleep after the usual delay. With this app not
                // running that long, the machine is restored from its snapshot instead.
                last_keep_alive = Some(Instant::now());
                tracing::info!("keep-alive wake for {}", self.id());
                if let Err(e) = self.ensure_awake().await {
                    tracing::warn!("keep-alive wake for {}: {e:#}", self.id());
                }
            }
            let due = self.scheduled_wake().is_some_and(|due| chrono::Utc::now() >= due - DUE_LEAD);
            if (due || self.has_queued_wake())
                && last_due_wake.is_none_or(|t| t.elapsed() >= DUE_WAKE_RETRY)
            {
                last_due_wake = Some(Instant::now());
                tracing::info!("waking {} for work due", self.id());
                if let Err(e) = self.ensure_awake().await {
                    tracing::warn!("waking {} for work due: {e:#}", self.id());
                }
            }
            let running = self.power() == Power::Running;
            let came_up = running && !was_running;
            was_running = running;
            if running {
                if came_up || last_link_check.is_none_or(|t| t.elapsed() >= LINK_CHECK) {
                    last_link_check = Some(Instant::now());
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.ensure_linked().await {
                            tracing::warn!("linking {} into T3 Code: {e:#}", this.id());
                        }
                        if came_up {
                            this.refresh_projects().await;
                            this.refresh_connect().await;
                        }
                    });
                }
                // The same settings wait for their try to end, then to be retried; a newer change
                // or a wake goes at once.
                let gh_uploads = provision::gh_uploads_revision(&self.shared.extras());
                let start = {
                    let mut last = self.gh_uploads_try.lock().unwrap();
                    if came_up && last.as_ref().is_some_and(|(_, failed)| failed.is_some()) {
                        *last = None;
                    }
                    let waits = last.as_ref().is_some_and(|(revision, failed)| {
                        *revision == gh_uploads && failed.is_none_or(|at| at.elapsed() < GH_UPLOADS_RETRY)
                    });
                    let start = !waits && self.record().gh_uploads_revision.as_deref() != Some(&gh_uploads);
                    if start {
                        *last = Some((gh_uploads.clone(), None));
                    }
                    start
                };
                if start {
                    let this = self.clone();
                    tokio::spawn(async move {
                        let result = this.bring_gh_uploads().await;
                        if let Err(e) = &result {
                            tracing::warn!("bringing the GitHub login to {}: {e:#}", this.id());
                        }
                        let mut last = this.gh_uploads_try.lock().unwrap();
                        if last.as_ref().is_some_and(|(revision, _)| *revision == gh_uploads) {
                            *last = result.is_err().then(|| (gh_uploads, Some(Instant::now())));
                        }
                    });
                }
                // Retries a send that failed during the wake.
                self.flush_deferred_commands().await;
                match self.watchdog_get("/status").await {
                    Ok(s) => {
                        watchdog_failures = 0;
                        if was_busy && !s.busy {
                            // The turn's last edits may have landed after the last busy poll.
                            self.cache.lock().unwrap().bump_epoch();
                            let this = self.clone();
                            tokio::spawn(async move { this.refresh_projects().await });
                        }
                        was_busy = s.busy;
                        if !s.agents_unknown && self.wake_failure.lock().unwrap().take().is_some() {
                            // A failed wake came up after all: T3 answers.
                            self.shared.changed();
                            let this = self.clone();
                            tokio::spawn(async move { this.on_ready().await });
                        }
                        self.on_watchdog_status(s).await;
                    }
                    Err(_) => watchdog_failures += 1,
                }
            }
            tokio::time::sleep(WATCHDOG_POLL).await;
        }
    }

    /// When work T3 starts on its own (an automation, ...) comes due on a sleeping machine, which
    /// wakes for it as an always-on server would run it. Only work due after it went to sleep:
    /// what came due while it ran had its chance.
    pub fn scheduled_wake(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.scheduled_wake_for(self.power())
    }

    /// Same as `scheduled_wake`, using a caller-captured `power` reading so callers inspecting
    /// both power and scheduled wake observe one consistent state.
    pub fn scheduled_wake_for(&self, power: Power) -> Option<chrono::DateTime<chrono::Utc>> {
        if power != Power::Stopped {
            return None;
        }
        let due = chrono::DateTime::from_timestamp_millis(self.record.read().unwrap().next_due_at?)?;
        let stopped_at = (*self.stopped_at.lock().unwrap())?;
        (due > stopped_at).then_some(due)
    }

    /// A message T3 Code was told is sent, still queued on a sleeping machine: its wake failed
    /// or never ran.
    fn has_queued_wake(&self) -> bool {
        self.power() == Power::Stopped
            && self.cache.lock().unwrap().deferred_commands.iter().any(|c| {
                c["type"].as_str().is_some_and(|k| classify::WAKING_COMMANDS.contains(&k))
            })
    }

    fn needs_keep_alive(&self) -> bool {
        self.power() == Power::Stopped
            && self
                .stopped_at
                .lock()
                .unwrap()
                .is_some_and(|t| chrono::Utc::now() - t > KEEP_ALIVE_AFTER)
    }
}
