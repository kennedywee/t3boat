//! The app's model: machines, settings, setup checks. UI-agnostic; the tray and the dev CLI
//! both drive it.

use crate::boat::{Boat, Resume, Starts};
use crate::machine::{provision, Event, MachineRuntime, Power, Shared, WatchdogPhase};
use crate::proxy;
use crate::store::{GithubUploads, Settings, Spend, State, Store};
use crate::t3desktop::T3Desktop;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const FIRST_PORT: u16 = 47731;
/// Machines ever created here: deleted ones keep their port.
const PORTS: u16 = 1000;
/// Between tries to remove Tailscale Serve handlers that failed to go, such as while Tailscale
/// is stopped.
const UNEXPOSE_RETRY: Duration = Duration::from_secs(60);

/// Why the app cannot run yet, in the order the user has to fix it.
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Setup {
    /// The first check is still running.
    #[default]
    Starting,
    Ready,
    InstallBoat,
    SignInBoat,
    InstallT3,
}

pub async fn check_setup() -> (Setup, Option<Boat>, Option<T3Desktop>) {
    let Some(boat) = Boat::locate() else { return (Setup::InstallBoat, None, None) };
    if !boat.account().await.is_ok_and(|a| a.signed_in) {
        return (Setup::SignInBoat, Some(boat), None);
    }
    let Some(t3) = T3Desktop::locate() else { return (Setup::InstallT3, Some(boat), None) };
    (Setup::Ready, Some(boat), Some(t3))
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MachineState {
    Working,
    Idle,
    Sleeping,
    Asleep,
    Waking,
    Creating,
    Error,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineView {
    pub id: String,
    pub name: String,
    pub size: String,
    pub state: MachineState,
    pub detail: String,
    /// Set while `detail` counts down, so the UI can tick it between updates.
    pub countdown: Option<Countdown>,
    pub paired: bool,
    /// Agents that need a sign-in on this machine (done in T3 Code, per machine).
    pub signed_out: Vec<String>,
    /// Its T3 projects as last seen, and those being added (with their progress).
    pub projects: Vec<crate::machine::projects::MachineProject>,
    /// When a "keep awake" runs out (unix ms); none when off or until stopped.
    pub keep_awake_until: Option<u64>,
    pub keep_awake_forever: bool,
}

/// `detail` as `template` with `{left}` filled in: the time left until `ends_at` (unix ms).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Countdown {
    pub template: &'static str,
    pub ends_at: u64,
}

impl Countdown {
    fn new(template: &'static str, secs: u64, from: SystemTime) -> Self {
        Self { template, ends_at: unix_ms_after(from, secs) }
    }

    fn text(&self, secs: u64) -> String {
        self.template.replace("{left}", &human_secs(secs))
    }
}

/// Saturates: `secs` comes from the machine and may be anything.
fn unix_ms_after(from: SystemTime, secs: u64) -> u64 {
    let from_ms = from.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    from_ms.saturating_add(secs.saturating_mul(1000))
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineUsage {
    pub today: f64,
    pub month: f64,
    pub awake_secs_today: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Costs {
    pub today: f64,
    pub month: f64,
    pub starts_today: u64,
    pub starts_limit: u64,
}

/// A machine being created: it holds its name and local port until it exists.
struct Creating {
    name: String,
    step: String,
    port: u16,
    /// Its Boat sandbox, once there is one.
    sandbox: Option<String>,
}

struct Running {
    machine: Arc<MachineRuntime>,
    tasks: Vec<JoinHandle<()>>,
}

pub struct App {
    pub shared: Arc<Shared>,
    store: Store,
    data_dir: PathBuf,
    machines: RwLock<Vec<Running>>,
    creating: Mutex<Vec<Creating>>,
    starts: Mutex<Starts>,
    /// By sandbox id, gone sandboxes included: `Spend`.
    spend: Mutex<HashMap<String, Spend>>,
    retired_ports: Mutex<Vec<u16>>,
    /// Sandboxes of creates the app didn't live to finish, until deleted.
    leftover_sandboxes: Mutex<Vec<String>>,
    /// Tailscale Serve ports whose removal failed and is still pending retry.
    pending_unexpose_ports: Mutex<std::collections::HashSet<u16>>,
    /// The one task retrying `pending_unexpose_ports`.
    unexpose_retry: SingleFlight,
}

/// At most one background job of a kind at a time.
#[derive(Default)]
struct SingleFlight(AtomicBool);

impl SingleFlight {
    /// Whether the caller now runs the job: false while another task runs it.
    fn start(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    /// Ends the job. `true` when `more` work came in as it ended and no other task took it: the
    /// caller then keeps the job.
    fn finish(&self, more: impl FnOnce() -> bool) -> bool {
        self.0.store(false, Ordering::Release);
        more() && self.start()
    }
}

impl App {
    pub fn new(
        boat: Boat,
        t3: T3Desktop,
        data_dir: PathBuf,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Arc<Self>> {
        let store = Store::new(&data_dir);
        let state = store.load().context("reading t3boat state")?;
        // A machine saved meanwhile owns its sandbox.
        let leftover: Vec<String> = state
            .pending_sandboxes
            .into_iter()
            .filter(|s| !state.machines.iter().any(|m| &m.sandbox_id == s))
            .collect();
        let shared = Arc::new(Shared {
            boat,
            t3: t3.with_token_file(data_dir.join("t3-token")),
            settings: RwLock::new(state.settings.clone()),
            github_uploads: RwLock::new(state.github_uploads.clone()),
            events,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                // Boat answers an unrouted port with a redirect to its fallback page.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            client: proxy::auth::random_token(),
        });
        let app = Arc::new(Self {
            shared,
            store,
            data_dir,
            machines: RwLock::new(Vec::new()),
            creating: Mutex::new(Vec::new()),
            starts: Mutex::new(Starts::default()),
            spend: Mutex::new(state.spend),
            retired_ports: Mutex::new(state.retired_ports),
            leftover_sandboxes: Mutex::new(leftover),
            pending_unexpose_ports: Mutex::new(state.pending_unexpose_ports),
            unexpose_retry: SingleFlight::default(),
        });
        for record in state.machines {
            app.spawn(app.runtime(record));
        }
        let pending_unexpose: Vec<u16> = app.pending_unexpose_ports.lock().unwrap().iter().copied().collect();
        if !pending_unexpose.is_empty() {
            app.serve_on_tailnet(pending_unexpose, false, false);
        }
        app.serve_on_tailnet(app.ports(), true, false);
        tokio::spawn(app.clone().delete_leftover_sandboxes());
        Ok(app)
    }

    async fn delete_leftover_sandboxes(self: Arc<Self>) {
        let sandboxes = self.leftover_sandboxes.lock().unwrap().clone();
        for id in sandboxes {
            self.delete_leftover(&id).await;
        }
    }

    /// Unfinished, unlisted and billed without limit: deleted, or the user told. Kept until Boat
    /// confirms it gone, so the next launch tries again.
    async fn delete_leftover(&self, id: &str) {
        let boat = &self.shared.boat;
        if let Err(e) = boat.delete(id).await {
            if !boat.confirmed_gone(id).await {
                self.shared.notify(
                    "A sandbox may be left on Boat",
                    format!("Couldn't delete {id} from an unfinished create ({e:#}); delete it with `boat delete {id}`"),
                );
                return;
            }
        }
        self.leftover_sandboxes.lock().unwrap().retain(|s| s != id);
        self.shared.persist();
    }

    fn machine_dir(&self, id: &str) -> PathBuf {
        self.data_dir.join("machines").join(id)
    }

    fn runtime(&self, record: crate::store::Machine) -> Arc<MachineRuntime> {
        let dir = self.machine_dir(&record.id);
        Arc::new(MachineRuntime::new(self.shared.clone(), record, dir))
    }

    /// Starts the machine's proxy and background loop.
    fn spawn(&self, machine: Arc<MachineRuntime>) {
        let proxy_machine = machine.clone();
        let proxy = tokio::spawn(async move {
            let port = proxy_machine.record().local_port;
            if let Err(e) = proxy::serve(proxy_machine.clone()).await {
                tracing::error!("proxy on port {port} failed: {e:#}");
                proxy_machine.shared.notify(
                    "t3boat",
                    format!("{} cannot listen on port {port}: {e}", proxy_machine.record().name),
                );
            }
        });
        let runner = tokio::spawn(machine.clone().run());
        self.machines.write().unwrap().push(Running { machine, tasks: vec![proxy, runner] });
    }

    /// Before the app quits: no tunnel process is left behind.
    pub async fn stop_forwards(&self) {
        for machine in self.machines() {
            machine.stop_forwards().await;
        }
    }

    pub fn machines(&self) -> Vec<Arc<MachineRuntime>> {
        self.machines.read().unwrap().iter().map(|r| r.machine.clone()).collect()
    }

    /// Whether a Boat sandbox belongs to t3boat: a machine's, one being created, or one left from
    /// a create that did not finish.
    fn owns_sandbox(&self, id: &str) -> bool {
        self.machines().iter().any(|m| m.sandbox_id() == id)
            || self.creating.lock().unwrap().iter().any(|c| c.sandbox.as_deref() == Some(id))
            || self.leftover_sandboxes.lock().unwrap().iter().any(|s| s == id)
    }

    /// The account's other sandboxes: made with the Boat CLI or its website, not by t3boat.
    pub async fn other_sandboxes(&self) -> Result<Vec<crate::boat::SandboxSummary>> {
        let mut all = self.shared.boat.summaries().await?;
        all.retain(|s| !self.owns_sandbox(&s.id));
        for s in &mut all {
            s.adoptable = s.name.as_deref().and_then(provision::machine_name).is_some_and(valid_name);
        }
        Ok(all)
    }

    /// Starts or stops one of `other_sandboxes`; never a machine, which wakes and sleeps itself.
    pub async fn set_other_sandbox_running(&self, id: &str, running: bool) -> Result<()> {
        anyhow::ensure!(!self.owns_sandbox(id), "{id} is a t3boat machine: wake or sleep it instead");
        let boat = &self.shared.boat;
        if running { boat.start(id).await } else { boat.stop(id).await }
    }

    /// By id, or by name ignoring case.
    pub fn machine(&self, machine: &str) -> Result<Arc<MachineRuntime>> {
        self.machines()
            .into_iter()
            .find(|m| m.id() == machine || m.record().name.eq_ignore_ascii_case(machine))
            .ok_or_else(|| anyhow!("no machine {machine}"))
    }

    pub fn settings(&self) -> Settings {
        self.shared.settings.read().unwrap().clone()
    }

    /// Only the event loop calls this on `Event::Persist`: one writer for the state file.
    pub fn save(&self) {
        // One lock at a time: a guard in a field initializer would live until the end of the `let`.
        let spend = self.spend.lock().unwrap().clone();
        let retired_ports = self.retired_ports.lock().unwrap().clone();
        let creating: Vec<_> = self.creating.lock().unwrap().iter().filter_map(|c| c.sandbox.clone()).collect();
        let mut pending_sandboxes = self.leftover_sandboxes.lock().unwrap().clone();
        pending_sandboxes.extend(creating);
        let pending_unexpose_ports = self.pending_unexpose_ports.lock().unwrap().clone();
        let state = State {
            machines: self.machines().iter().map(|m| m.record()).collect(),
            settings: self.settings(),
            github_uploads: self.shared.github_uploads.read().unwrap().clone(),
            spend,
            retired_ports,
            pending_sandboxes,
            pending_unexpose_ports,
        };
        if let Err(e) = self.store.save(&state) {
            tracing::error!("saving state: {e:#}");
        }
    }

    /// A Tailscale name set or changed puts every machine on the tailnet; one cleared takes them off.
    pub fn update_settings(self: &Arc<Self>, settings: Settings) {
        let host = settings.tailscale_host.clone();
        let old = std::mem::replace(&mut *self.shared.settings.write().unwrap(), settings).tailscale_host;
        self.shared.persist();
        let pending: Vec<u16> = self.pending_unexpose_ports.lock().unwrap().iter().copied().collect();
        if !pending.is_empty() {
            self.serve_on_tailnet(pending, false, false);
        }
        match (old, host) {
            (old, Some(host)) if old.as_ref() != Some(&host) => self.serve_on_tailnet(self.ports(), true, true),
            (Some(_), None) => self.serve_on_tailnet(self.ports(), false, false),
            _ => {}
        }
    }

    /// The local ports of all machines.
    fn ports(&self) -> Vec<u16> {
        self.machines().iter().map(|m| m.record().local_port).collect()
    }

    /// Whether `port` should currently be served on the tailnet.
    fn should_expose_port(&self, port: u16) -> bool {
        self.settings().tailscale_host.is_some()
            && self.machines().iter().any(|m| m.record().local_port == port)
    }

    /// Serves `ports` on the tailnet with Tailscale Serve (`on`), or stops, in the background:
    /// Tailscale may be missing or slow, and nothing waits on it. Does nothing without a Tailscale
    /// name when turning on. `tell`: a failure notifies the user once per batch, else it is only
    /// logged. Failed removals stay persisted in `pending_unexpose_ports` and are retried by
    /// `retry_unexpose`.
    fn serve_on_tailnet(self: &Arc<Self>, ports: Vec<u16>, on: bool, tell: bool) {
        if ports.is_empty() || (on && self.settings().tailscale_host.is_none()) {
            return;
        }
        if !on {
            let mut pending = self.pending_unexpose_ports.lock().unwrap();
            let before = pending.len();
            pending.extend(ports.iter().copied());
            let changed = pending.len() != before;
            drop(pending);
            if changed {
                self.shared.persist();
            }
        }
        let app = self.clone();
        tokio::spawn(async move {
            let mut notified = false;
            let mut removal_failed = false;
            for port in ports {
                let result = match on {
                    true => crate::tailscale::expose_port_if(port, || app.should_expose_port(port)).await,
                    false => crate::tailscale::unexpose_port_if(port, || !app.should_expose_port(port)).await,
                };
                match result {
                    // Removed, skipped as wanted again, or served on purpose: no removal is due.
                    Ok(done) if done || !on => app.forget_pending_unexpose(port),
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("tailscale serve on port {port}: {e:#}");
                        removal_failed |= !on;
                        if tell && !notified {
                            notified = true;
                            app.shared.notify("Machines aren't on your tailnet", format!("{e:#}"));
                        }
                    }
                }
            }
            if removal_failed {
                app.retry_unexpose();
            }
        });
    }

    /// `port` has no Serve handler left to remove.
    fn forget_pending_unexpose(&self, port: u16) {
        if self.pending_unexpose_ports.lock().unwrap().remove(&port) {
            self.shared.persist();
        }
    }

    /// Retries `pending_unexpose_ports` every `UNEXPOSE_RETRY` in one background task, until none
    /// is left: otherwise a handler that failed to go, such as while Tailscale was stopped, stays
    /// on the tailnet until the next settings save or launch.
    fn retry_unexpose(self: &Arc<Self>) {
        if !self.unexpose_retry.start() {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(UNEXPOSE_RETRY).await;
                let ports: Vec<u16> = app.pending_unexpose_ports.lock().unwrap().iter().copied().collect();
                for port in ports {
                    match crate::tailscale::unexpose_port_if(port, || !app.should_expose_port(port)).await {
                        // Skipped: the port is wanted on the tailnet again, so its handler stays.
                        Ok(_) => app.forget_pending_unexpose(port),
                        Err(e) => tracing::debug!("retrying tailscale serve removal on port {port}: {e:#}"),
                    }
                }
                let none_left = || app.pending_unexpose_ports.lock().unwrap().is_empty();
                if none_left() && !app.unexpose_retry.finish(|| !none_left()) {
                    return;
                }
            }
        });
    }

    /// `None`: not connected. `Some(None)`: connected, login name unknown.
    pub fn github_uploads(&self) -> Option<Option<String>> {
        self.shared.github_uploads.read().unwrap().as_ref().map(|g| g.login.clone())
    }

    pub async fn connect_github_uploads(self: &Arc<Self>, on_code: impl FnOnce(&str)) -> Result<()> {
        let scratch = self.data_dir.join(format!("gh-sign-in-{}", crate::proxy::auth::random_token()));
        let uploads = crate::github::sign_in(&scratch, on_code).await?;
        self.set_github_uploads(Some(uploads));
        Ok(())
    }

    /// Whether machines use the connected login for all of `gh` and git.
    pub fn github_everything(&self) -> bool {
        self.shared.github_uploads.read().unwrap().as_ref().is_some_and(|g| g.everything)
    }

    /// Each machine's loop brings the change over.
    pub fn set_github_everything(&self, on: bool) -> Result<()> {
        match self.shared.github_uploads.write().unwrap().as_mut() {
            Some(uploads) => uploads.everything = on,
            None => anyhow::bail!("Connect GitHub first"),
        }
        self.shared.persist();
        Ok(())
    }

    /// Each machine's loop brings the change over.
    pub fn set_github_uploads(&self, uploads: Option<GithubUploads>) {
        *self.shared.github_uploads.write().unwrap() = uploads;
        self.shared.persist();
    }

    /// Claims the name and a local port for a new machine, so concurrent creates never share
    /// either. `replaced` is the machine it takes the place of: it keeps that one's port.
    fn reserve(&self, name: &str, replaced: Option<&crate::store::Machine>) -> Result<u16> {
        let mut creating = self.creating.lock().unwrap();
        let records: Vec<_> = self.machines().iter().map(|m| m.record()).collect();
        let names = records
            .iter()
            .filter(|m| replaced.is_none_or(|r| r.id != m.id))
            .map(|m| &m.name)
            .chain(creating.iter().map(|c| &c.name));
        if names.into_iter().any(|n| n.eq_ignore_ascii_case(name)) {
            bail!("a machine called {name} already exists");
        }
        if let Some(r) = replaced {
            creating.push(Creating { name: name.to_owned(), step: "Preparing".into(), port: r.local_port, sandbox: None });
            return Ok(r.local_port);
        }
        let retired = self.retired_ports.lock().unwrap();
        let used: Vec<u16> = records
            .iter()
            .map(|m| m.local_port)
            .chain(creating.iter().map(|c| c.port))
            .chain(retired.iter().copied())
            .collect();
        let port = (FIRST_PORT..FIRST_PORT + PORTS)
            .find(|p| !used.contains(p) && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
            .ok_or_else(|| anyhow!("no free local port"))?;
        creating.push(Creating { name: name.to_owned(), step: "Preparing".into(), port, sandbox: None });
        Ok(port)
    }

    /// Saved before provisioning starts: a quit or crash from here on leaves the sandbox billed.
    fn set_creating_sandbox(&self, name: &str, sandbox: &str) {
        if let Some(c) = self.creating.lock().unwrap().iter_mut().find(|c| c.name == name) {
            c.sandbox = Some(sandbox.to_owned());
        }
        self.shared.persist();
    }

    fn set_creating_step(&self, name: &str, step: &str) {
        if let Some(c) = self.creating.lock().unwrap().iter_mut().find(|c| c.name == name) {
            c.step = step.to_owned();
        }
        self.shared.changed();
    }

    /// Creates a machine, then adds this computer's T3 projects `projects` (ids) to it.
    pub async fn create_machine(self: &Arc<Self>, name: &str, size: &str, projects: Vec<String>) -> Result<Arc<MachineRuntime>> {
        if !valid_name(name) {
            bail!("machine names are 1-32 lowercase letters, digits or dashes, starting with a letter or digit");
        }
        let port = self.reserve(name, None)?;
        self.shared.changed();
        let result = async {
            let version = self.shared.t3.version().await?;
            let record = provision::create(
                &self.shared.boat,
                name,
                size,
                &version,
                port,
                &self.shared.extras(),
                |sandbox| self.set_creating_sandbox(name, sandbox),
                |step| self.set_creating_step(name, step),
            )
            .await?;
            let machine = self.runtime(record);
            // Before pairing: T3 Code's onboarding needs the descriptor and session right away.
            // On failure the machine's loop primes it again.
            if let Err(e) = machine.prime().await {
                tracing::warn!("priming {}: {e:#}", machine.id());
            }
            self.spawn(machine.clone());
            self.serve_on_tailnet(vec![port], true, false);
            Ok::<_, anyhow::Error>(machine)
        }
        .await;
        // A failed create's sandbox becomes a leftover: deleted now, or retried at the next launch.
        let leftover = {
            let mut creating = self.creating.lock().unwrap();
            let sandbox = creating.iter().position(|c| c.name == name).and_then(|i| creating.remove(i).sandbox);
            sandbox.filter(|_| result.is_err())
        };
        self.leftover_sandboxes.lock().unwrap().extend(leftover.clone());
        self.shared.persist();
        self.shared.changed();
        if let Some(id) = leftover {
            self.delete_leftover(&id).await;
        }
        let machine = result?;
        let (app, m) = (self.clone(), machine.clone());
        tokio::spawn(async move {
            for id in projects {
                if let Err(e) = app.add_project(&m, &id).await {
                    app.shared.notify(format!("A project didn't reach {}", m.record().name), format!("{e:#}"));
                }
            }
        });
        // The machine exists either way; its row still offers "Add to T3 Code".
        if let Err(e) = self.pair(&machine) {
            self.shared.notify(format!("Couldn't open T3 Code for {name}"), format!("{e:#}; use Add to T3 Code"));
        }
        Ok(machine)
    }

    /// Adds a machine that t3boat on another computer created: one of `other_sandboxes` marked
    /// `adoptable`. Wakes it if it sleeps, to read what it says of itself. The machine gets a T3
    /// token for this computer and is otherwise left as it is, so both computers use it.
    pub async fn adopt_machine(self: &Arc<Self>, sandbox_id: &str) -> Result<Arc<MachineRuntime>> {
        let boat = &self.shared.boat;
        let sandbox = self
            .other_sandboxes()
            .await?
            .into_iter()
            .find(|s| s.id == sandbox_id && s.adoptable)
            .ok_or_else(|| anyhow!("{sandbox_id} is not a t3boat machine from another computer"))?;
        let name = sandbox.name.as_deref().and_then(provision::machine_name).unwrap_or_default().to_owned();
        // Another computer restored it onto a new sandbox: this one's row says "Deleted on Boat".
        let replaced = self
            .machines()
            .into_iter()
            .find(|m| m.power() == Power::Missing && m.record().name.eq_ignore_ascii_case(&name))
            .map(|m| m.record());
        let port = self.reserve(&name, replaced.as_ref())?;
        self.shared.changed();
        let result = async {
            let size = sandbox.size.clone().unwrap_or_else(|| "default".to_owned());
            let resumed = match sandbox.state.as_str() {
                "stopped" => {
                    self.set_creating_step(&name, "Waking the machine");
                    if boat.resume(sandbox_id, &size).await? == Resume::DataExpired {
                        bail!("Boat deleted its disk after 14 days asleep; wake it on the computer that created it, which restores it");
                    }
                    true
                }
                "stopping" => bail!("{name} is going to sleep; try again in a moment"),
                _ => false,
            };
            self.set_creating_step(&name, "Reading the machine");
            let adopted = provision::adopt(boat, sandbox_id).await?;
            let id = provision::adopted_id(&adopted, &name, sandbox_id)?;
            if let Some(old) = replaced.as_ref().filter(|old| old.id != id) {
                bail!("{} here is another machine, deleted on Boat: delete it here first", old.name);
            }
            if let Some(m) = self.machines().iter().find(|m| m.id() == id && replaced.is_none()) {
                bail!("already on this computer as {}", m.record().name);
            }
            let (upstream_url, watchdog_url) = provision::host(boat, sandbox_id).await?;
            let old = replaced.clone();
            let record = crate::store::Machine {
                id,
                sandbox_id: sandbox_id.to_owned(),
                name: name.clone(),
                size,
                resize_pending: false,
                upstream_url,
                upstream_token: adopted.upstream_token,
                watchdog_url,
                watchdog_secret: adopted.watchdog_secret,
                local_port: port,
                t3_version: adopted.t3_version,
                t3_set_elsewhere: true,
                provision_revision: adopted.revision.unwrap_or_default(),
                gh_uploads_revision: None,
                environment_id: adopted.environment_id,
                // T3 Code stays paired with a machine that was here before.
                client_tokens: old.as_ref().map(|m| m.client_tokens.clone()).unwrap_or_default(),
                link_tokens: old.as_ref().map(|m| m.link_tokens.clone()).unwrap_or_default(),
                created_at: match &old {
                    Some(m) => m.created_at,
                    None => sandbox.created_at.map_or_else(|| chrono::Utc::now().timestamp(), |t| t.timestamp()),
                },
                next_due_at: None,
                connect: old.as_ref().is_some_and(|m| m.connect),
                forwards: old.as_ref().map(|m| m.forwards.clone()).unwrap_or_default(),
                links: old.map(|m| m.links).unwrap_or_default(),
            };
            if let Some(old) = &replaced {
                self.remove_runtime(&old.id).await;
            }
            let machine = self.runtime(record);
            if replaced.is_some() {
                // Saved from the sandbox that is gone; the snapshot may be older.
                machine.forget_recorded();
            }
            // The watchdog was frozen mid-stop: told of the resume, it counts the idle time anew.
            machine.send_activity(resumed).await;
            if let Err(e) = machine.prime().await {
                tracing::warn!("priming {}: {e:#}", machine.id());
            }
            self.spawn(machine.clone());
            self.serve_on_tailnet(vec![port], true, false);
            Ok::<_, anyhow::Error>(machine)
        }
        .await;
        self.creating.lock().unwrap().retain(|c| c.name != name);
        self.shared.persist();
        self.shared.changed();
        let machine = result?;
        if !machine.record().client_tokens.is_empty() {
            return Ok(machine);
        }
        if let Err(e) = self.pair(&machine) {
            self.shared.notify(format!("Couldn't open T3 Code for {name}"), format!("{e:#}; use Add to T3 Code"));
        }
        Ok(machine)
    }

    /// Ends the machine's proxy and loop, and waits until its port is free.
    async fn remove_runtime(&self, id: &str) {
        let running = {
            let mut machines = self.machines.write().unwrap();
            let idx = machines.iter().position(|r| r.machine.id() == id);
            idx.map(|i| machines.remove(i))
        };
        let Some(running) = running else { return };
        running.machine.stop_forwards().await;
        for task in running.tasks {
            task.abort();
            let _ = task.await;
        }
    }

    /// This computer's T3 projects that can go to a machine: those with a git remote.
    pub async fn local_projects(&self) -> Result<Vec<crate::projects::Project>> {
        self.shared.t3.projects().await
    }

    /// Adds this computer's T3 project `project` (its id, path or title) to the machine as T3
    /// Code's "Add to another machine" does: cloned from its remote, with its env files. Resolves
    /// once cloned.
    pub async fn add_project(&self, machine: &MachineRuntime, project: &str) -> Result<crate::projects::Project> {
        let project = self
            .local_projects()
            .await?
            .into_iter()
            .find(|p| p.id == project || p.path == project || p.title.eq_ignore_ascii_case(project))
            .ok_or_else(|| anyhow!("no project {project} with a git remote"))?;
        let (files, mut failed) = self.shared.t3.files_to_copy(&project).await?;
        failed.extend(machine.add_project(&project, files).await?);
        if !failed.is_empty() {
            self.shared.notify(
                format!("Some of {}'s files didn't reach {}", project.title, machine.record().name),
                failed.join("\n"),
            );
        }
        Ok(project)
    }

    /// Opens T3 Code's own "add backend" confirmation for this machine.
    pub fn pair(&self, machine: &MachineRuntime) -> Result<()> {
        let code = machine.auth.new_pairing_code();
        let pairing_url = format!("http://localhost:{}/pair#token={code}", machine.record().local_port);
        open_url(&format!("t3code://pair?pairingUrl={}", urlencoding::encode(&pairing_url)))
    }

    /// A one-time pairing URL for a phone or another device on the tailnet: puts the machine on
    /// the tailnet first. Replaces any pairing code not yet used.
    pub async fn pair_tailnet(&self, machine: &MachineRuntime) -> Result<String> {
        let host = self
            .settings()
            .tailscale_host
            .as_deref()
            .and_then(crate::store::normalize_tailscale_host)
            .context("Set this computer's Tailscale name in Settings first")?;
        let record = machine.record();
        let exposed = crate::tailscale::expose_port_if(record.local_port, || self.should_expose_port(record.local_port))
            .await
            .with_context(|| format!("Tailscale Serve couldn't put {} on your tailnet", record.name))?;
        if !exposed {
            anyhow::bail!("Tailscale access was turned off or {} was deleted", record.name);
        }
        self.forget_pending_unexpose(record.local_port);
        let code = machine.auth.new_pairing_code();
        Ok(crate::tailscale::tailnet_pairing_url(&host, record.local_port, &code))
    }

    /// Refuses while work runs on the machine, unless `discard`. T3 Code offers no way to remove
    /// a backend from outside, so a paired machine ends with T3 Code in front and a note on what
    /// to remove there.
    pub async fn delete_machine(self: &Arc<Self>, machine: &MachineRuntime, discard: bool) -> Result<()> {
        // Boat can't report a deleted sandbox's usage: the last chance to count its tail.
        let (day, month) = periods();
        self.refresh_spend(machine, day, month).await;
        if !discard {
            machine.check_idle_before_delete().await?;
        }
        let boat = &self.shared.boat;
        let record = {
            // Held until retired: a later wake then sees Missing, and none restores meanwhile.
            let _waking = machine.waking.lock().await;
            // Read under the lock: a wake may have restored it onto a new sandbox.
            let record = machine.record();
            if let Err(e) = boat.delete(&record.sandbox_id).await {
                // Already gone on Boat is fine; anything else keeps the machine.
                if !boat.confirmed_gone(&record.sandbox_id).await {
                    return Err(e);
                }
            }
            machine.retire();
            record
        };
        let id = record.id.as_str();
        // None yet if it never slept with this app's watchdog.
        if let Err(e) = boat.delete_snapshot(&provision::snapshot_name(&record.name, &record.id)).await {
            tracing::warn!("deleting {id}'s snapshot: {e:#}");
        }
        let running = {
            let mut machines = self.machines.write().unwrap();
            let idx = machines.iter().position(|r| r.machine.id() == id);
            idx.map(|i| machines.remove(i))
        };
        if let Some(r) = running {
            r.tasks.iter().for_each(JoinHandle::abort);
        }
        let _ = std::fs::remove_dir_all(machine.dir());
        self.retired_ports.lock().unwrap().push(record.local_port);
        if self.settings().tailscale_host.is_some() {
            self.serve_on_tailnet(vec![record.local_port], false, false);
        }
        if let Err(e) = self.shared.t3.unlink(&record.environment_id).await {
            tracing::warn!("unlinking {id} from T3 Code: {e:#}");
        }
        self.shared.persist();
        self.shared.changed();
        if !record.client_tokens.is_empty() {
            self.shared.notify(
                format!("Remove {} from T3 Code", record.name),
                "T3 Code still lists it. Remove it there under Settings → Environments.",
            );
            let _ = self.shared.t3.activate();
        }
        Ok(())
    }

    pub fn views(&self) -> Vec<MachineView> {
        let mut views: Vec<MachineView> = self.machines().iter().map(|m| view(m)).collect();
        for c in self.creating.lock().unwrap().iter() {
            views.push(MachineView {
                id: String::new(),
                name: c.name.clone(),
                size: String::new(),
                state: MachineState::Creating,
                detail: c.step.clone(),
                countdown: None,
                paired: false,
                signed_out: Vec::new(),
                projects: Vec::new(),
                keep_awake_until: None,
                keep_awake_forever: false,
            });
        }
        views
    }

    /// Totals count every sandbox spent on this month, those of deleted machines and those a
    /// restore replaced included.
    pub fn costs(&self) -> Costs {
        let (day, month) = periods();
        let starts = self.starts.lock().unwrap().clone();
        let mut costs =
            Costs { starts_today: starts.used_today, starts_limit: starts.limit_today, ..Costs::default() };
        for spend in self.spend.lock().unwrap().values() {
            let u = usage(spend, day, month);
            costs.today += u.today;
            costs.month += u.month;
        }
        costs
    }

    fn usage(&self, machine: &MachineRuntime) -> MachineUsage {
        let (day, month) = periods();
        let id = machine.id();
        let mut total = MachineUsage::default();
        for spend in self.spend.lock().unwrap().values().filter(|s| s.machine == id) {
            let u = usage(spend, day, month);
            total.today += u.today;
            total.month += u.month;
            total.awake_secs_today += u.awake_secs_today;
        }
        total
    }

    /// Looks up the spend of the machine's current sandbox. A failed lookup keeps the last known
    /// figures, never a fake zero.
    async fn refresh_spend(&self, machine: &MachineRuntime, day: i64, month: i64) {
        if machine.power() == Power::Missing {
            return;
        }
        let boat = &self.shared.boat;
        let sandbox = machine.sandbox_id();
        let Ok((today, this_month)) = tokio::try_join!(boat.usage_since(&sandbox, day), boat.usage_since(&sandbox, month))
        else {
            return;
        };
        let spend = Spend {
            machine: machine.id(),
            day,
            today: today.dollars,
            awake_secs_today: today.seconds,
            month: this_month.dollars,
        };
        self.spend.lock().unwrap().insert(sandbox, spend);
    }

    pub async fn refresh_costs(&self) {
        let (day, month) = periods();
        let boat = &self.shared.boat;
        for m in self.machines() {
            self.refresh_spend(&m, day, month).await;
        }
        self.spend.lock().unwrap().retain(|_, s| s.day >= month);
        if let Ok(starts) = boat.starts().await {
            *self.starts.lock().unwrap() = starts;
        }
        self.shared.persist();
        self.shared.changed();
    }
}

/// "at 14:30" today, "Tue 14:30" on another day.
fn local_time(t: chrono::DateTime<chrono::Utc>) -> String {
    let t = t.with_timezone(&chrono::Local);
    if t.date_naive() == chrono::Local::now().date_naive() {
        t.format("at %H:%M").to_string()
    } else {
        t.format("%a %H:%M").to_string()
    }
}

/// Starts of the current local day and month, unix seconds.
fn periods() -> (i64, i64) {
    use chrono::{Datelike, Local, TimeZone};
    let now = Local::now();
    let start = |day| {
        Local
            .with_ymd_and_hms(now.year(), now.month(), day, 0, 0, 0)
            .single()
            .map(|t| t.timestamp())
            .unwrap_or_default()
    };
    (start(now.day()), start(1))
}

/// What `spend` counts toward the periods starting at `day` and `month`: nothing for a period
/// that began after its lookup.
fn usage(spend: &Spend, day: i64, month: i64) -> MachineUsage {
    let today = spend.day >= day;
    MachineUsage {
        today: if today { spend.today } else { 0.0 },
        month: if spend.day >= month { spend.month } else { 0.0 },
        awake_secs_today: if today { spend.awake_secs_today } else { 0 },
    }
}

/// What the details view (and agents) show about one machine, beyond its MachineView.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineDetails {
    pub id: String,
    pub cost_today: f64,
    pub cost_month: f64,
    pub awake_secs_today: u64,
    pub agents: Vec<AgentLogin>,
    pub t3_version: String,
    pub created_at: i64,
    /// T3 Connect is on: other devices reach it with the user's T3 account.
    pub connect: bool,
    /// What listened there at the last `scan_ports`, and the ports forwarded to this computer.
    pub ports: Vec<crate::machine::ports::PortView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLogin {
    pub name: String,
    pub signed_in: bool,
}

impl App {
    /// Never wakes the machine.
    pub fn details(&self, machine: &str) -> Result<MachineDetails> {
        let machine = self.machine(machine)?;
        let id = machine.id();
        let record = machine.record();
        let usage = self.usage(&machine);
        let agents = machine.cache.lock().unwrap().agents();
        Ok(MachineDetails {
            id,
            cost_today: usage.today,
            cost_month: usage.month,
            awake_secs_today: usage.awake_secs_today,
            // Only agents whose status is known, so this agrees with `signed_out` in the list.
            agents: agents
                .into_iter()
                .filter(|(_, status)| matches!(status.as_str(), "authenticated" | "unauthenticated"))
                .map(|(name, status)| AgentLogin { name, signed_in: status == "authenticated" })
                .collect(),
            t3_version: record.t3_version,
            created_at: record.created_at,
            connect: record.connect,
            ports: machine.port_views(),
        })
    }

    /// Brings T3 Code forward on `thread`, or just to the front; an unpaired machine gets the
    /// "add backend" dialog instead.
    pub fn open_in_t3(&self, machine: &MachineRuntime, thread: Option<&str>) -> Result<()> {
        let record = machine.record();
        if record.client_tokens.is_empty() {
            return self.pair(machine);
        }
        match thread {
            Some(thread) => self.shared.t3.open_thread(&record.environment_id, thread),
            None => self.shared.t3.activate(),
        }
    }
}

fn view(m: &MachineRuntime) -> MachineView {
    let r = m.record();
    let watchdog = m.watchdog_status();
    let mut countdown = None;
    let mut count_down = |template, secs, from| {
        let c = Countdown::new(template, secs, from);
        let text = c.text(secs);
        countdown = Some(c);
        text
    };
    use MachineState::*;
    let (state, detail) = match (m.power(), &watchdog) {
        (Power::Missing, _) => (Missing, "Deleted on Boat".to_owned()),
        (_, _) if m.last_error().is_some() => (Error, m.last_error().unwrap_or_default()),
        (Power::Running, Some(w)) if w.agents_unknown => (Working, "T3 not answering · stays up 30m max".to_owned()),
        (Power::Running, Some(w)) if w.cpu_busy => (Working, "Working · busy CPU".to_owned()),
        (Power::Running, Some(w)) if w.busy => (Working, working_line(w.working_threads, w.working_subagents)),
        (Power::Running, Some(w)) if w.phase == WatchdogPhase::Active && w.keep_awake_forever => {
            (Idle, "Awake until you stop it".to_owned())
        }
        (Power::Running, Some(w)) if w.phase == WatchdogPhase::Active && w.keep_awake_secs > 0 => {
            (Idle, count_down("Kept awake · {left} left", w.keep_awake_secs, w.received))
        }
        (Power::Running, Some(w)) if w.phase == WatchdogPhase::Active && w.work_due => {
            let due = w.next_due_at.and_then(chrono::DateTime::from_timestamp_millis);
            (Idle, due.map_or("Idle · scheduled work due".to_owned(), |t| format!("Idle · scheduled work {}", local_time(t))))
        }
        (Power::Running, Some(w)) if w.phase == WatchdogPhase::Active => {
            let left = w.sleep_after_secs.saturating_sub(w.idle_secs);
            (Idle, count_down("Idle · sleeps in {left}", left, w.received))
        }
        (Power::Running, Some(_)) | (Power::Stopping, _) => (Sleeping, "Going to sleep".to_owned()),
        (Power::Running, None) => (Idle, "Running".to_owned()),
        (Power::Starting, _) => (Waking, "Waking up".to_owned()),
        (Power::Stopped, _) => match m.scheduled_wake() {
            Some(due) => (Asleep, format!("Asleep · wakes {} for scheduled work", local_time(due))),
            None => (Asleep, "Asleep".to_owned()),
        },
        (Power::Unknown, _) => (Unknown, "Checking".to_owned()),
    };
    let running_watchdog = watchdog.as_ref().filter(|_| m.power() == Power::Running);
    MachineView {
        id: r.id.clone(),
        name: r.name.clone(),
        size: r.size.clone(),
        state,
        detail,
        countdown,
        paired: !r.client_tokens.is_empty(),
        signed_out: m.cache.lock().unwrap().signed_out_providers(),
        projects: m.projects(),
        keep_awake_until: running_watchdog
            .filter(|w| w.keep_awake_secs > 0)
            .map(|w| unix_ms_after(w.received, w.keep_awake_secs)),
        keep_awake_forever: running_watchdog.is_some_and(|w| w.keep_awake_forever),
    }
}

/// The UI's rule: `^[a-z0-9][a-z0-9-]{0,31}$`.
fn valid_name(name: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    name.len() <= 32 && name.starts_with(ok) && name.chars().all(|c| ok(c) || c == '-')
}

/// "Working · 2 threads · 3 subagents".
fn working_line(threads: u32, subagents: u32) -> String {
    let count = |n: u32, one: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {one}s")),
    };
    std::iter::once("Working".to_owned())
        .chain(count(threads, "thread"))
        .chain(count(subagents, "subagent"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// "42s", "4m", "1h 5m"; minutes round up. The UI's `timeLeft` matches it.
fn human_secs(s: u64) -> String {
    let m = s.div_ceil(60);
    match (s, m) {
        (0..60, _) => format!("{s}s"),
        (_, 0..60) => format!("{m}m"),
        _ => format!("{}h {}m", m / 60, m % 60),
    }
}

/// On Linux through GLib, as `gio open` does: `xdg-open` outside GNOME and KDE takes a scheme
/// with a digit in it, like `t3code:`, for a file name.
#[cfg(target_os = "linux")]
pub fn open_url(url: &str) -> Result<()> {
    use gtk::gio;
    gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>)
        .map_err(|e| anyhow::anyhow!("could not open {url}: {e}"))
}

#[cfg(not(target_os = "linux"))]
pub fn open_url(url: &str) -> Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else {
        // No shell: `cmd /C start` would run anything after a `&` in the URL.
        std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).status()
    }?;
    if !status.success() {
        bail!("could not open {url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn valid_name() {
        for ok in ["dev", "0", "a-b-", &"a".repeat(32)] {
            assert!(super::valid_name(ok), "{ok}");
        }
        for bad in ["", "-dev", "Dev", "my dev", "dév", &"a".repeat(33)] {
            assert!(!super::valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn spend_counts_only_in_its_periods() {
        let spend =
            crate::store::Spend { machine: "m".into(), day: 100, today: 1.0, awake_secs_today: 60, month: 5.0 };
        let same_day = super::usage(&spend, 100, 0);
        assert_eq!((same_day.today, same_day.awake_secs_today, same_day.month), (1.0, 60, 5.0));
        let later_day = super::usage(&spend, 200, 0);
        assert_eq!((later_day.today, later_day.awake_secs_today, later_day.month), (0.0, 0, 5.0));
        let later_month = super::usage(&spend, 300, 300);
        assert_eq!((later_month.today, later_month.month), (0.0, 0.0));
    }

    #[test]
    fn single_flight_runs_one_job_and_keeps_late_work() {
        let job = super::SingleFlight::default();
        assert!(job.start());
        assert!(!job.start(), "a second task must not start while one runs");
        assert!(!job.finish(|| false), "no work left: the job ends");
        assert!(job.start(), "an ended job can start again");
        assert!(job.finish(|| true), "late work: the ending task keeps the job");
        assert!(!job.start(), "the kept job still blocks others");
        assert!(!job.finish(|| false));
    }
}
