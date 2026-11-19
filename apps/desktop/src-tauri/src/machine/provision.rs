//! Creating and upgrading machines: everything that runs on the sandbox goes through
//! `provision.sh`, which is idempotent.

use crate::boat::Boat;
use crate::proxy::auth::random_token;
use crate::store::Machine;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;

const PROVISION_SH: &str = include_str!("provision.sh");
const WATCHDOG_MJS: &str = include_str!("watchdog.mjs");
const GH_UPLOADS_SH: &str = include_str!("gh-uploads.sh");
const T3_PORT: u16 = 3773;
const WATCHDOG_PORT: u16 = 3774;

/// What the user's settings put on every machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extras {
    /// The fleet repo (`owner/name`) machines clone and install.
    pub fleet: Option<String>,
    /// The user's own GitHub token, which `gh` on a machine uses for uploads only.
    pub gh_upload_token: Option<String>,
}

/// Identifies the provisioning scripts and the fleet repo; a machine provisioned with others gets
/// them on wake, so a changed fleet setting reaches every machine. Not "GitHub uploads": that has
/// `gh_uploads_revision`, so a new token never asks for an upgrade, which may restart T3.
pub fn revision(extras: &Extras) -> String {
    let hash = fnv1a(
        PROVISION_SH
            .bytes()
            .chain(WATCHDOG_MJS.bytes())
            .chain(SLEEP_AFTER_SECS.to_string().bytes())
            .chain(extras.fleet.as_deref().unwrap_or_default().bytes()),
    );
    format!("{hash:016x}")
}

/// Identifies the "GitHub uploads" setup `gh-uploads.sh` leaves on a machine: its script and the
/// token, or that there is none. Kept per machine, apart from `revision`, because this part goes
/// to a machine at once, agents working or not, where the rest of an upgrade waits for a wake.
pub fn gh_uploads_revision(extras: &Extras) -> String {
    let token = extras.gh_upload_token.as_deref().unwrap_or_default();
    format!("{:016x}", fnv1a(GH_UPLOADS_SH.bytes().chain([0]).chain(token.bytes())))
}

/// The fleet repo as `provision.sh` takes it: `owner/name`, or `-` for none.
fn fleet_word(fleet: Option<&str>) -> Result<&str> {
    match fleet {
        None => Ok("-"),
        Some(repo) if crate::store::valid_fleet_repo(repo) => Ok(repo),
        Some(repo) => bail!("not a GitHub owner/name: {repo}"),
    }
}

/// FNV-1a: stable across builds, unlike std's hasher.
pub fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provisioned {
    pub environment_id: String,
    pub upstream_token: Option<String>,
}

/// Machine sizes, as Boat sells them.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SizeInfo {
    pub id: &'static str,
    pub spec: &'static str,
    pub hourly: f64,
}

pub const SIZES: &[SizeInfo] = &[
    SizeInfo { id: "small", spec: "2 vCPU · 4 GB", hourly: 0.018 },
    SizeInfo { id: "default", spec: "4 vCPU · 8 GB", hourly: 0.036 },
    SizeInfo { id: "large", spec: "8 vCPU · 16 GB", hourly: 0.072 },
];

pub fn size(id: &str) -> Result<&'static SizeInfo> {
    SIZES.iter().find(|s| s.id == id).ok_or_else(|| anyhow!("unknown size {id}"))
}

/// How long a machine stays up after the last agent turn and the last action through t3boat.
/// Long enough to read a result and type a follow-up without waiting for a wake; idle minutes
/// cost under a cent even on the large size.
pub const SLEEP_AFTER_SECS: u64 = 300;

/// The machine's sandbox, as Boat lists it: `t3boat_jett`.
pub fn sandbox_name(name: &str) -> String {
    format!("t3boat_{name}")
}

/// The saved snapshot the watchdog replaces at every sleep. Boat deletes a stopped sandbox's
/// disk after 14 days; this stays, so the machine can be restored from it. Unique per machine,
/// whoever else uses the Boat account. Boat takes lowercase letters, digits and dashes.
pub fn snapshot_name(name: &str, machine_id: &str) -> String {
    let id: String = machine_id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    format!("t3boat-{name}-{id}")
}

/// Copies the scripts over as a file (the watchdog secret must not appear in any command line)
/// and runs `provision.sh`.
pub async fn run(
    boat: &Boat,
    sandbox_id: &str,
    name: &str,
    t3_version: &str,
    watchdog_secret: &str,
    snapshot: &str,
    mint_upstream_token: bool,
    extras: &Extras,
) -> Result<Provisioned> {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let script = format!(
        "set -e; d=$(mktemp -d); trap 'rm -rf \"$d\" \"$0\"' EXIT\n\
         echo {p} | base64 -d > \"$d/provision.sh\"; echo {w} | base64 -d > \"$d/watchdog.mjs\"\n\
         echo {g} | base64 -d > \"$d/gh-uploads.sh\"\n\
         T3BOAT_SECRET={s} T3BOAT_GH_UPLOAD_TOKEN={gh} bash \"$d/provision.sh\" {v} {sleep} {snapshot} {mint} {name} {fleet}\n",
        p = b64(PROVISION_SH),
        w = b64(WATCHDOG_MJS),
        g = b64(GH_UPLOADS_SH),
        v = shell_word(t3_version)?,
        s = shell_word(watchdog_secret)?,
        sleep = SLEEP_AFTER_SECS,
        snapshot = shell_word(snapshot)?,
        mint = u8::from(mint_upstream_token),
        name = shell_word(name)?,
        fleet = fleet_word(extras.fleet.as_deref())?,
        // In the uploaded file, not in the command Boat runs: `boat history` keeps commands.
        gh = shell_word(extras.gh_upload_token.as_deref().unwrap_or_default())?,
    );
    let out = run_uploaded(boat, sandbox_id, &script, 600).await?;
    let line = out
        .lines()
        .rev()
        .find(|l| l.starts_with('{'))
        .ok_or_else(|| anyhow!("provisioning printed no result"))?;
    Ok(serde_json::from_str(line)?)
}

/// Runs `script` on the sandbox from an uploaded file, which the script itself removes: what it
/// holds (a token) is then never on a command line, and `boat history` keeps those. Returns stdout.
async fn run_uploaded(boat: &Boat, sandbox_id: &str, script: &str, timeout_secs: u64) -> Result<String> {
    let name = format!("t3boat-provision-{}.sh", random_token());
    let local = std::env::temp_dir().join(&name);
    crate::store::write_private(&local, script.as_bytes())?;
    let remote = format!("/tmp/{name}");
    let uploaded = boat.upload(sandbox_id, &local, &remote).await;
    let _ = std::fs::remove_file(&local);
    uploaded?;
    boat.exec_ok(sandbox_id, &format!("bash {remote}"), timeout_secs).await
}

/// Runs one part of `gh-uploads.sh` on a running machine. `upgrade`: the newer `gh`, which takes
/// no token. `apply`: the wrapper and the token of `extras`, or the token's removal. Unlike `run`
/// it restarts nothing, so it is safe while agents work.
pub async fn gh_uploads(boat: &Boat, sandbox_id: &str, part: &str, extras: &Extras) -> Result<()> {
    let script = format!(
        "set -e; d=$(mktemp -d); trap 'rm -rf \"$d\" \"$0\"' EXIT\n\
         echo {g} | base64 -d > \"$d/gh-uploads.sh\"\n\
         T3BOAT_GH_UPLOAD_TOKEN={gh} bash \"$d/gh-uploads.sh\" {part}\n",
        part = shell_word(part)?,
        g = base64::engine::general_purpose::STANDARD.encode(GH_UPLOADS_SH),
        gh = shell_word(extras.gh_upload_token.as_deref().unwrap_or_default())?,
    );
    run_uploaded(boat, sandbox_id, &script, 400).await.map(drop)
}

/// Only `[A-Za-z0-9._-]` reaches the remote shell.
fn shell_word(s: &str) -> Result<&str> {
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        // Not the value itself: it may be a token.
        bail!("unsafe value for the remote shell ({} characters)", s.chars().count());
    }
    Ok(s)
}

/// Hosts the T3 server's and the watchdog's ports publicly: their URLs, in that order.
/// Idempotent: hosting again keeps the URLs.
pub async fn host(boat: &Boat, sandbox_id: &str) -> Result<(String, String)> {
    tokio::try_join!(boat.host_public(sandbox_id, T3_PORT), boat.host_public(sandbox_id, WATCHDOG_PORT))
}

/// Creates a new machine end to end. Takes about a minute, most of it Boat and the T3 install.
/// On failure after `created`, the caller deletes the sandbox.
pub async fn create(
    boat: &Boat,
    name: &str,
    size: &str,
    t3_version: &str,
    local_port: u16,
    extras: &Extras,
    created: impl FnOnce(&str),
    progress: impl Fn(&str),
) -> Result<Machine> {
    self::size(size)?;
    progress("Creating sandbox on Boat");
    let id = boat
        .create(&sandbox_name(name), size, None)
        .await
        .context("creating the Boat sandbox")?;
    created(&id);
    progress("Installing T3 Code");
    let secret = random_token();
    let provisioned = run(boat, &id, name, t3_version, &secret, &snapshot_name(name, &id), true, extras).await?;
    progress("Opening the machine's address");
    let (upstream_url, watchdog_url) = host(boat, &id).await?;
    Ok(Machine {
        id: id.clone(),
        sandbox_id: id,
        name: name.to_owned(),
        size: size.to_owned(),
        upstream_url,
        upstream_token: provisioned
            .upstream_token
            .ok_or_else(|| anyhow!("provisioning issued no token"))?,
        watchdog_url,
        watchdog_secret: secret,
        local_port,
        t3_version: t3_version.to_owned(),
        provision_revision: revision(extras),
        // The machine's loop brings "GitHub uploads" over by itself and learns whether it worked.
        gh_uploads_revision: None,
        environment_id: provisioned.environment_id,
        client_tokens: Vec::new(),
        link_tokens: Vec::new(),
        created_at: chrono::Utc::now().timestamp(),
        next_due_at: None,
        connect: false,
    })
}
