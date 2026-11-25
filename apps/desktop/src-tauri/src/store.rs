//! Persistent app state: machines, settings and this month's spend, one JSON file in the app
//! data dir.

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};
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
    /// `size` was chosen here and Boat does not run it yet. Otherwise `size` follows Boat, where
    /// another computer may have changed it.
    #[serde(default)]
    pub resize_pending: bool,
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
    /// Another computer's t3boat installed that version, not this one.
    #[serde(default)]
    pub t3_set_elsewhere: bool,
    /// `provision::revision()` of the scripts the machine runs.
    pub provision_revision: String,
    /// `provision::gh_uploads_revision()` of what the machine has. `None`: unknown.
    #[serde(default)]
    pub gh_uploads_revision: Option<String>,
    pub environment_id: String,
    /// Tokens the proxy issued to T3 Code when it paired, one per device. Read from the plain
    /// strings earlier versions stored as well.
    #[serde(deserialize_with = "client_tokens")]
    pub client_tokens: Vec<ClientToken>,
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
    /// Ports forwarded to this computer at the same number while the machine runs.
    #[serde(default)]
    pub forwards: Vec<u16>,
    /// Ports with a Boat link: an HTTPS address of their own, for any device.
    #[serde(default)]
    pub links: Vec<PortLink>,
}

/// A token the proxy issued to one T3 Code. Revoking it unpairs that device only.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientToken {
    /// Names the token without giving it away: derived from it, so stable across versions.
    pub id: String,
    pub token: String,
    /// What the device called itself when it paired (`client_label`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// RFC 7638 thumbprint of the DPoP key the token is bound to: every use must carry a proof
    /// signed with that key. `None`: a bearer token, good on loopback only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jkt: Option<String>,
    /// Unix seconds. 0: paired before tokens had metadata.
    #[serde(default)]
    pub created_at: i64,
    /// Unix seconds. `None`: never.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl ClientToken {
    pub fn new(token: String, label: Option<String>, jkt: Option<String>, created_at: i64, ttl_secs: i64) -> Self {
        Self { id: client_token_id(&token), token, label, jkt, created_at, expires_at: Some(created_at + ttl_secs) }
    }

    /// Replaces the secret of an existing device token on renewal, keeping its stable device `id`.
    pub fn renewed(
        id: String,
        token: String,
        label: Option<String>,
        jkt: Option<String>,
        created_at: i64,
        ttl_secs: i64,
    ) -> Self {
        Self { id, token, label, jkt, created_at, expires_at: Some(created_at + ttl_secs) }
    }

    /// A token stored as a plain string by an earlier version: loopback, unbound, no expiry.
    pub fn legacy(token: String) -> Self {
        Self { id: client_token_id(&token), token, label: None, jkt: None, created_at: 0, expires_at: None }
    }

    /// `now`: unix seconds.
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// The first 8 bytes of the token's SHA-256, in hex.
fn client_token_id(token: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes())[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn client_tokens<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ClientToken>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Full(ClientToken),
        Legacy(String),
    }
    Ok(Vec::<Stored>::deserialize(d)?
        .into_iter()
        .map(|t| match t {
            Stored::Full(t) => t,
            Stored::Legacy(token) => ClientToken::legacy(token),
        })
        .collect())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PortLink {
    pub port: u16,
    /// Carries its access token: whoever has it opens the port.
    pub url: String,
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
    /// This computer's Tailscale MagicDNS name (`mybox.tailnet.ts.net`). The proxies answer
    /// requests addressed to it as well as loopback ones, with stricter auth. `None`: loopback only.
    pub tailscale_host: Option<String>,
    /// The Boat sandbox that runs `t3boatd --auto-sleep` for the user's machines. This app wakes
    /// it as it starts. `None`: no mothership.
    pub mothership_id: Option<String>,
}

/// The user's own GitHub login, which machines use for `gh` uploads, or with `everything` for all
/// of `gh` and git in place of Boat's login. The token acts as the user until they revoke it on
/// GitHub.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GithubUploads {
    pub token: String,
    pub login: Option<String>,
    #[serde(default)]
    pub everything: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { start_at_login: true, fleet_repo: None, tailscale_host: None, mothership_id: None }
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

/// `host` as a Tailscale MagicDNS machine name, `machine.tailnet.ts.net`, lowercased. `None` for
/// anything else: an IP, a port, a bare tailnet, another domain.
pub fn normalize_tailscale_host(host: &str) -> Option<String> {
    let host = host.trim().strip_suffix('.').unwrap_or(host.trim()).to_ascii_lowercase();
    let label = |l: &&str| {
        (1..=63).contains(&l.len())
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let labels: Vec<&str> = host.split('.').collect();
    (labels.len() == 4 && labels[2..] == ["ts", "net"] && labels.iter().all(label)).then_some(host)
}

#[cfg(test)]
mod tailscale_host_tests {
    use super::normalize_tailscale_host;

    #[test]
    fn accepts_only_a_magicdns_machine_name() {
        assert_eq!(normalize_tailscale_host("mybox.tail1234.ts.net").as_deref(), Some("mybox.tail1234.ts.net"));
        assert_eq!(normalize_tailscale_host(" MyBox.Tail-1234.TS.net. ").as_deref(), Some("mybox.tail-1234.ts.net"));
        for bad in [
            "",
            "ts.net",
            "tail1234.ts.net",
            "a.b.c.ts.net",
            "mybox.tail1234.ts.net.evil.com",
            "mybox.tail1234.ts.net:443",
            "mybox.tail1234.ts.com",
            "-box.tail1234.ts.net",
            "my_box.tail1234.ts.net",
            "my box.tail1234.ts.net",
            "100.64.0.1",
            "mybox..ts.net",
        ] {
            assert_eq!(normalize_tailscale_host(bad), None, "{bad}");
        }
    }
}

#[cfg(test)]
mod client_token_tests {
    use super::*;

    #[test]
    fn reads_the_plain_tokens_of_earlier_versions() {
        let json = r#"["old", {"id": "x", "token": "new", "label": "Phone", "jkt": "k", "createdAt": 5, "expiresAt": 9}]"#;
        let mut d = serde_json::Deserializer::from_str(json);
        let tokens = client_tokens(&mut d).unwrap();
        assert_eq!(tokens[0], ClientToken::legacy("old".into()));
        assert_eq!(tokens[0].id.len(), 16);
        assert!(!tokens[0].expired(i64::MAX));
        assert_eq!(tokens[1].label.as_deref(), Some("Phone"));
        assert_eq!(tokens[1].jkt.as_deref(), Some("k"));
        assert!(tokens[1].expired(9) && !tokens[1].expired(8));
    }

    #[test]
    fn a_legacy_machine_record_loads_and_saves_with_metadata() {
        let mut machine = serde_json::json!({
            "id": "m", "sandboxId": "s", "name": "n", "size": "z", "upstreamUrl": "u", "upstreamToken": "t",
            "watchdogUrl": "w", "watchdogSecret": "ws", "localPort": 4000, "t3Version": "v",
            "provisionRevision": "r", "environmentId": "e", "clientTokens": ["tok"], "linkTokens": [], "createdAt": 1,
        });
        let m: Machine = serde_json::from_value(machine.clone()).unwrap();
        assert_eq!(m.client_tokens, vec![ClientToken::legacy("tok".into())]);
        let saved = serde_json::to_value(&m).unwrap();
        machine["clientTokens"] = saved["clientTokens"].clone();
        assert_eq!(saved["clientTokens"][0]["token"], "tok");
        let again: Machine = serde_json::from_value(machine).unwrap();
        assert_eq!(again, m);
    }
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
    /// Tailscale Serve HTTPS ports whose removal failed (e.g. Tailscale was stopped) and should be
    /// retried on startup or when Tailscale becomes available.
    #[serde(default)]
    pub pending_unexpose_ports: HashSet<u16>,
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
