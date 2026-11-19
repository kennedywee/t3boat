//! Persistent app state: machines, settings and this month's spend, one JSON file in the app
//! data dir.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    /// Never changes: the id of the sandbox the machine was created on.
    pub id: String,
    /// Boat sandbox it runs on now, e.g. `bx_sad32u26`: another one after a restore.
    pub sandbox_id: String,
    pub name: String,
    pub size: String,
    /// Public HTTPS URL of the T3 server on the sandbox.
    pub upstream_url: String,
    /// Bearer token the proxy uses upstream. Never leaves this computer except to that server.
    pub upstream_token: String,
    /// Public HTTPS URL of the watchdog on the sandbox.
    pub watchdog_url: String,
    pub watchdog_secret: String,
    /// Loopback port the proxy serves this machine on. Stable, T3 Code stores it.
    pub local_port: u16,
    /// T3 server version installed on the machine.
    pub t3_version: String,
    /// `provision::revision()` of the scripts the machine runs.
    pub provision_revision: String,
    pub environment_id: String,
    /// Bearer tokens the proxy issued to T3 Code when it paired.
    pub client_tokens: Vec<String>,
    /// Bearer tokens the machine's server issued to T3 servers that linked to it through the
    /// proxy. The proxy only recognizes them: a call bearing one may wake the machine.
    pub link_tokens: Vec<LinkToken>,
    pub created_at: i64,
    /// When T3 on the machine next starts work on its own (an automation, a held webhook run, an
    /// agent resuming after a usage limit), as its watchdog last said: ms since epoch.
    #[serde(default)]
    pub next_due_at: Option<i64>,
    /// T3 Connect is on there: other devices reach it with the user's T3 account.
    #[serde(default)]
    pub connect: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkToken {
    pub token: String,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub start_at_login: bool,
    /// The user's fleet repo on GitHub (`owner/name`): skills and agent setup every machine clones
    /// and installs once with its `install.sh`. `None`: machines get no fleet.
    pub fleet_repo: Option<String>,
}

/// The user's own GitHub login, from "GitHub uploads" in the settings. Machines get the token for
/// `gh` commands that upload a file, which GitHub refuses the GitHub App token Boat signs them in
/// with. It acts as the user until they revoke it on GitHub.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GithubUploads {
    pub token: String,
    /// The account's login name, for the settings to show.
    pub login: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { start_at_login: true, fleet_repo: None }
    }
}

/// Whether `repo` is a GitHub `owner/name`, the only shape that reaches a machine's shell.
pub fn valid_fleet_repo(repo: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    matches!(repo.split_once('/'), Some((owner, name)) if ok(owner) && ok(name))
}

#[cfg(test)]
mod fleet_repo_tests {
    use super::valid_fleet_repo;

    #[test]
    fn accepts_only_owner_slash_name() {
        assert!(valid_fleet_repo("kennedywee/fleet"));
        assert!(valid_fleet_repo("some-org/agent.skills_2"));
        for bad in ["fleet", "a/b/c", "/fleet", "kennedywee/", "../x", "a/..", "a/b c", "a/b;rm", "a/$(x)"] {
            assert!(!valid_fleet_repo(bad), "{bad}");
        }
    }
}

/// One sandbox's spend as last looked up. Kept after the sandbox is gone (its machine deleted, or
/// a restore replaced it) so the totals still count it until its month is over.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    /// Id of the machine that ran on the sandbox.
    pub machine: String,
    /// Start of the local day of the lookup, unix seconds.
    pub day: i64,
    /// Dollars since `day`.
    pub today: f64,
    pub awake_secs_today: u64,
    /// Dollars since the start of `day`'s month.
    pub month: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub machines: Vec<Machine>,
    pub settings: Settings,
    pub github_uploads: Option<GithubUploads>,
    /// By sandbox id.
    pub spend: HashMap<String, Spend>,
    /// Local ports of deleted machines, never given out again: T3 Code may still list one (it
    /// cannot be told to forget a backend), and would reach a new machine there as the old.
    pub retired_ports: Vec<u16>,
    /// Sandboxes of creates still running: deleted at the next start if the app quit or crashed
    /// before the create finished.
    pub pending_sandboxes: Vec<String>,
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("state.json"),
        }
    }

    pub fn load(&self) -> Result<State> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// The file holds bearer tokens.
    pub fn save(&self, state: &State) -> Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(state)?)
    }
}

/// Atomic write of a file only its owner can read, never readable by others even briefly.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().expect("path has a parent"))?;
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp)?.write_all(bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Removes everything in `dir` but `keep`, whatever an interrupted write left behind included.
pub fn sweep(dir: &Path, keep: &HashSet<PathBuf>) {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    for path in entries.map(|e| e.path()).filter(|p| !keep.contains(p)) {
        let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
    }
}
