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
const ADOPT_SH: &str = include_str!("adopt.sh");
/// How Boat lists a machine's sandbox: `t3boat_<name>`.
const SANDBOX_PREFIX: &str = "t3boat_";
const T3_PORT: u16 = 3773;
const WATCHDOG_PORT: u16 = 3774;

/// A port t3boat's own services listen on there.
pub fn own_port(port: u16) -> bool {
    matches!(port, T3_PORT | WATCHDOG_PORT)
}

/// What the settings put on every machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extras {
    pub fleet: Option<String>,
    pub gh_upload_token: Option<String>,
    /// The token is for all of `gh` and git on the machine, not only for uploads.
    pub gh_everything: bool,
}

/// Identifies the provisioning scripts and the fleet repo; a machine provisioned with others gets
/// them on wake, so a changed fleet setting reaches every machine. The upload token is not part
/// of it: a new one must not ask for an upgrade, which may restart T3.
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

/// Identifies what `gh-uploads.sh` leaves on a machine: the script, and the token with what it
/// is for, or none.
pub fn gh_uploads_revision(extras: &Extras) -> String {
    let token = extras.gh_upload_token.as_deref().unwrap_or_default();
    let everything = u8::from(extras.gh_everything && !token.is_empty());
    format!("{:016x}", fnv1a(GH_UPLOADS_SH.bytes().chain([0, everything]).chain(token.bytes())))
}

/// The fleet repo as `provision.sh` takes it: `owner/name`, or `-` for none.
fn fleet_word(fleet: Option<&str>) -> Result<&str> {
    match fleet {
        None => Ok("-"),
        Some(repo) if crate::store::valid_fleet_repo(repo) => Ok(repo),
        Some(repo) => bail!("not a GitHub owner/name: {repo}"),
    }
}

/// Whether T3 version `a` is later than `b`, a release being later than its own nightlies
/// (`0.1.10` after `0.1.10-nightly.20261114.4669`). False when either is not a version.
pub fn newer_t3(a: &str, b: &str) -> bool {
    fn key(version: &str) -> Option<(Vec<u64>, bool, Vec<u64>)> {
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let core = core.split('.').map(|n| n.parse().ok()).collect::<Option<Vec<u64>>>()?;
        let digits = |pre: &str| pre.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect();
        Some((core, pre.is_none(), pre.map(digits).unwrap_or_default()))
    }
    matches!((key(a), key(b)), (Some(a), Some(b)) if a > b)
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
    format!("{SANDBOX_PREFIX}{name}")
}

/// The machine name in a sandbox name: `None` for a sandbox t3boat did not make.
pub fn machine_name(sandbox_name: &str) -> Option<&str> {
    sandbox_name.strip_prefix(SANDBOX_PREFIX)
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
    machine_id: &str,
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
         T3BOAT_SECRET={s} T3BOAT_GH_UPLOAD_TOKEN={gh} T3BOAT_GH_EVERYTHING={everything} bash \"$d/provision.sh\" {v} {sleep} {snapshot} {mint} {name} {fleet} {machine} {revision}\n",
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
        machine = shell_word(machine_id)?,
        revision = revision(extras),
        gh = shell_word(extras.gh_upload_token.as_deref().unwrap_or_default())?,
        everything = u8::from(extras.gh_everything),
    );
    let out = run_uploaded(boat, sandbox_id, &script, 600).await?;
    let line = out
        .lines()
        .rev()
        .find(|l| l.starts_with('{'))
        .ok_or_else(|| anyhow!("provisioning printed no result"))?;
    Ok(serde_json::from_str(line)?)
}

/// Runs `script` from an uploaded file that removes itself: a token in it never reaches a command
/// line, which `boat history` keeps. Returns stdout.
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

/// Runs one part of `gh-uploads.sh` (`upgrade` or `apply`) on a running machine.
pub async fn gh_uploads(boat: &Boat, sandbox_id: &str, part: &str, extras: &Extras) -> Result<()> {
    let script = format!(
        "set -e; d=$(mktemp -d); trap 'rm -rf \"$d\" \"$0\"' EXIT\n\
         echo {g} | base64 -d > \"$d/gh-uploads.sh\"\n\
         T3BOAT_GH_UPLOAD_TOKEN={gh} T3BOAT_GH_EVERYTHING={everything} bash \"$d/gh-uploads.sh\" {part}\n",
        part = shell_word(part)?,
        g = base64::engine::general_purpose::STANDARD.encode(GH_UPLOADS_SH),
        gh = shell_word(extras.gh_upload_token.as_deref().unwrap_or_default())?,
        everything = u8::from(extras.gh_everything),
    );
    run_uploaded(boat, sandbox_id, &script, 400).await.map(drop)
}

/// A machine another computer created, as it describes itself, with a T3 token for this computer.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Adopted {
    /// `None`: provisioned before machines kept a record of themselves.
    pub machine_id: Option<String>,
    pub name: Option<String>,
    pub watchdog_secret: String,
    pub snapshot: String,
    pub revision: Option<String>,
    pub t3_version: String,
    pub environment_id: String,
    pub upstream_token: String,
}

/// Reads a running machine's record and issues this computer its own T3 token. The machine is
/// left as it is: no restart, whatever works on it.
pub async fn adopt(boat: &Boat, sandbox_id: &str) -> Result<Adopted> {
    let script = format!(
        "set -e; d=$(mktemp -d); trap 'rm -rf \"$d\" \"$0\"' EXIT\n\
         echo {a} | base64 -d > \"$d/adopt.sh\"; bash \"$d/adopt.sh\"\n",
        a = base64::engine::general_purpose::STANDARD.encode(ADOPT_SH),
    );
    let out = run_uploaded(boat, sandbox_id, &script, 120).await?;
    let line = out.lines().rev().find(|l| l.starts_with('{')).ok_or_else(|| anyhow!("the machine printed no record"))?;
    Ok(serde_json::from_str(line)?)
}

/// The machine's id from what it says of itself. One provisioned before machines kept their id
/// names it only in its snapshot, and only while it runs on its first sandbox.
pub fn adopted_id(adopted: &Adopted, name: &str, sandbox_id: &str) -> Result<String> {
    if let Some(id) = adopted.machine_id.as_deref().filter(|id| !id.is_empty()) {
        // The id names a folder on this computer, and the machine is not trusted with a path.
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
            bail!("the machine reports an id that is not a Boat sandbox id");
        }
        return Ok(id.to_owned());
    }
    if adopted.snapshot == snapshot_name(name, sandbox_id) {
        return Ok(sandbox_id.to_owned());
    }
    bail!("this machine is from an older t3boat: wake it once on the computer that created it, then add it here")
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
    let provisioned = run(boat, &id, &id, name, t3_version, &secret, &snapshot_name(name, &id), true, extras).await?;
    progress("Opening the machine's address");
    let (upstream_url, watchdog_url) = host(boat, &id).await?;
    Ok(Machine {
        id: id.clone(),
        sandbox_id: id,
        name: name.to_owned(),
        size: size.to_owned(),
        resize_pending: false,
        upstream_url,
        upstream_token: provisioned
            .upstream_token
            .ok_or_else(|| anyhow!("provisioning issued no token"))?,
        watchdog_url,
        watchdog_secret: secret,
        local_port,
        t3_version: t3_version.to_owned(),
        t3_set_elsewhere: false,
        provision_revision: revision(extras),
        gh_uploads_revision: None,
        environment_id: provisioned.environment_id,
        client_tokens: Vec::new(),
        link_tokens: Vec::new(),
        created_at: chrono::Utc::now().timestamp(),
        next_due_at: None,
        connect: false,
        forwards: Vec::new(),
        links: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_t3_orders_releases_and_nightlies() {
        assert!(newer_t3("0.1.11", "0.1.10"));
        assert!(newer_t3("0.1.10", "0.1.10-nightly.20261114.4669"));
        assert!(newer_t3("0.1.10-nightly.20261115.1", "0.1.10-nightly.20261114.4669"));
        assert!(newer_t3("0.2.0-nightly.20261101.1", "0.1.10"));
        for (a, b) in [("0.1.10", "0.1.10"), ("0.1.9", "0.1.10"), ("", "0.1.10"), ("0.1.10", "main")] {
            assert!(!newer_t3(a, b), "{a} {b}");
        }
    }

    #[test]
    fn gh_uploads_revision_follows_the_token_and_what_it_is_for() {
        let extras = |token: Option<&str>, gh_everything| Extras {
            fleet: None,
            gh_upload_token: token.map(str::to_owned),
            gh_everything,
        };
        let uploads = gh_uploads_revision(&extras(Some("gho_a"), false));
        assert_ne!(uploads, gh_uploads_revision(&extras(Some("gho_a"), true)));
        assert_ne!(uploads, gh_uploads_revision(&extras(Some("gho_b"), false)));
        assert_ne!(uploads, gh_uploads_revision(&extras(None, false)));
        assert_eq!(gh_uploads_revision(&extras(None, false)), gh_uploads_revision(&extras(None, true)));
    }

    fn adopted(machine_id: Option<&str>, snapshot: &str) -> Adopted {
        Adopted {
            machine_id: machine_id.map(str::to_owned),
            name: None,
            watchdog_secret: String::new(),
            snapshot: snapshot.to_owned(),
            revision: None,
            t3_version: String::new(),
            environment_id: String::new(),
            upstream_token: String::new(),
        }
    }

    #[test]
    fn adopted_id_comes_from_the_record_or_the_first_sandbox() {
        assert_eq!(adopted_id(&adopted(Some("bx_old"), "x"), "skye", "bx_new").unwrap(), "bx_old");
        assert_eq!(adopted_id(&adopted(None, "t3boat-skye-bx-new"), "skye", "bx_new").unwrap(), "bx_new");
        // Restored onto another sandbox before it kept its id: nothing says which machine it is.
        assert!(adopted_id(&adopted(None, "t3boat-skye-bx-old"), "skye", "bx_new").is_err());
        for id in ["..", "../x", "/tmp/x", "a/b", "a\\b", "."] {
            assert!(adopted_id(&adopted(Some(id), "x"), "skye", "bx_new").is_err(), "{id}");
        }
    }

    #[test]
    fn machine_name_only_for_t3boat_sandboxes() {
        assert_eq!(machine_name(&sandbox_name("skye")), Some("skye"));
        assert_eq!(machine_name("scratch"), None);
    }
}
