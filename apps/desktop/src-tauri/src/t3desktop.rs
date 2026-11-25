//! The user's T3 Code desktop install: its version (machines are pinned to it), the deep links
//! that open it, the environments linked into its server, and that server's own API, read the
//! way a T3 Code client reads it.

use crate::projects::{self, Project};
use crate::proxy::upstream;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use crate::process::command;

const BUNDLE_ID: &str = "com.t3tools.t3code";

/// An environment linked into the desktop app's T3 server.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub environment_id: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// T3 Code's data directory.
pub(crate) fn home() -> Result<PathBuf> {
    std::env::var_os("T3CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".t3")))
        .ok_or_else(|| anyhow!("no home directory"))
}

/// What t3boat may do on the desktop app's server: read its projects and their files.
const LOCAL_SCOPES: &[&str] = &["orchestration:read", "filesystem:read"];

#[derive(Clone, Debug)]
pub struct T3Desktop {
    /// The `t3` launcher the desktop app writes into `~/.t3/bin`; it runs the app's bundled CLI.
    /// `None` on a headless computer without T3 Code: see `pinned`.
    cli: Option<PathBuf>,
    /// The T3 version machines get, when set instead of asking `t3 --version`.
    pinned: Option<String>,
    /// Where t3boat keeps its session on the desktop app's server (T3 Code lists it under
    /// Settings → Connections as "t3boat"). Set by the app before any API call.
    token_file: Option<PathBuf>,
    /// Held across read-or-issue so overlapping calls share one session.
    token: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl T3Desktop {
    /// The `t3` CLI: the desktop app's launcher in `~/.t3/bin`, else a standalone install in
    /// `~/.local/bin` (as on a headless Linux computer), else `t3` on PATH.
    pub fn locate() -> Option<Self> {
        let home = dirs::home_dir()?;
        let names: &[&str] = if cfg!(windows) { &["t3.cmd", "t3.exe"] } else { &["t3"] };
        [home.join(".t3").join("bin"), home.join(".local").join("bin")]
            .iter()
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|p| p.is_file())
            .or_else(|| which::which("t3").ok())
            .map(|cli| Self { cli: Some(cli), pinned: None, token_file: None, token: Arc::default() })
    }

    /// No T3 Code on this computer: machines get T3 `version`. There are no desktop projects or
    /// links, and opening T3 Code fails with a clear error.
    pub fn pinned(version: String) -> Self {
        Self { cli: None, pinned: Some(version), token_file: None, token: Arc::default() }
    }

    /// Machines get T3 `version` instead of the installed CLI's.
    pub fn with_version(mut self, version: String) -> Self {
        self.pinned = Some(version);
        self
    }

    fn cli(&self) -> Result<&PathBuf> {
        self.cli.as_ref().ok_or_else(|| anyhow!("T3 Code is not installed on this computer"))
    }

    pub fn with_token_file(mut self, path: PathBuf) -> Self {
        self.token = Arc::new(std::fs::read_to_string(&path).ok().map(|t| t.trim().to_owned()).into());
        self.token_file = Some(path);
        self
    }

    /// e.g. `0.1.10-nightly.20261114.4669`.
    pub async fn version(&self) -> Result<String> {
        if let Some(version) = &self.pinned {
            return Ok(version.clone());
        }
        let out = command(self.cli()?)
            .arg("--version")
            .output()
            .await
            .context("running t3 --version")?;
        if !out.status.success() {
            bail!("t3 --version failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace()
            .find_map(|w| w.strip_prefix('v'))
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("unexpected t3 --version output: {text}"))
    }

    /// Runs the CLI's `environment` command: links live in the desktop app's database, which the
    /// CLI opens itself, so this works whether or not the app runs.
    async fn environment(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = command(self.cli()?)
            .arg("environment")
            .args(args)
            .output()
            .await
            .context("running t3 environment")?;
        if !out.status.success() {
            bail!("t3 environment {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    }

    /// Whether linked environments are on in T3 Code (Settings → Labs): its own switch for
    /// letting agents here work on other machines.
    pub fn links_enabled(&self) -> bool {
        let settings = home().and_then(|h| Ok(std::fs::read(h.join("userdata").join("settings.json"))?));
        settings
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|s| s["linkedEnvironmentsEnabled"] == true)
    }

    pub async fn links(&self) -> Result<Vec<Link>> {
        if self.cli.is_none() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_slice(&self.environment(&["list", "--json"]).await?)?)
    }

    /// Links the environment answering at `url`, with a one-time pairing code from it. `access`
    /// caps what agents here may do there.
    pub async fn link(&self, url: &str, pairing_code: &str, access: &str) -> Result<()> {
        self.environment(&["link", url, "--pairing-code", pairing_code, "--access", access]).await?;
        Ok(())
    }

    pub async fn unlink(&self, environment_id: &str) -> Result<()> {
        if self.cli.is_none() {
            return Ok(());
        }
        self.environment(&["unlink", environment_id]).await?;
        Ok(())
    }

    /// Brings the desktop app to the front. Elsewhere a link T3 does not handle, a bare
    /// `t3code://`, reaches its running app as a second launch, which shows its window; macOS
    /// delivers links to the running app without that, so there it opens by bundle id.
    pub fn activate(&self) -> Result<()> {
        self.cli()?;
        if cfg!(target_os = "macos") {
            let status = std::process::Command::new("open").args(["-b", BUNDLE_ID]).status()?;
            if !status.success() {
                bail!("could not open T3 Code");
            }
            Ok(())
        } else {
            crate::app::open_url("t3code://")
        }
    }

    /// The desktop app's projects that can go to another machine: those with a git remote,
    /// pinned first, then most recently used. Asks for the trimmed snapshot T3's own client
    /// loads: every project, without the thousands of old threads (MBs, seconds) this ignores.
    pub async fn projects(&self) -> Result<Vec<Project>> {
        if self.cli.is_none() {
            return Ok(Vec::new());
        }
        let shell = self
            .authorized(|origin, token| async move {
                let res = reqwest::Client::new()
                    .get(format!("{origin}/api/orchestration/shell"))
                    .query(&[("omitIdleSubagents", "1"), ("omitOlderSettled", "1")])
                    .bearer_auth(token)
                    .header("x-t3-orchestration-protocol", "2")
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await?;
                if res.status() == reqwest::StatusCode::UNAUTHORIZED {
                    return Err(Unauthorized.into());
                }
                Ok(res.error_for_status()?.json::<Value>().await?)
            })
            .await?;
        Ok(projects::cloneable(&shell))
    }

    /// The ignored files T3 Code's "Add to another machine" dialog selects by default for
    /// `project`, with their contents. A file that can't be read is left out with its reason.
    pub async fn files_to_copy(&self, project: &Project) -> Result<(Vec<(String, String)>, Vec<String>)> {
        let listed = self.call("projects.listIgnoredFiles", json!({ "cwd": project.path })).await?;
        let mut files = Vec::new();
        let mut failed = Vec::new();
        for file in listed["files"].as_array().into_iter().flatten() {
            let (Some(path), Some(len)) = (file["path"].as_str(), file["byteLength"].as_u64()) else { continue };
            if !projects::copied_by_default(path, len) {
                continue;
            }
            match self.call("projects.readFile", json!({ "cwd": project.path, "relativePath": path })).await {
                Ok(read) if read["truncated"] == true => failed.push(format!("{path}: too large to copy")),
                Ok(read) => files.push((path.to_owned(), read["contents"].as_str().unwrap_or_default().to_owned())),
                Err(e) => failed.push(format!("{path}: {e:#}")),
            }
        }
        Ok((files, failed))
    }

    async fn call(&self, tag: &str, payload: Value) -> Result<Value> {
        self.authorized(|origin, token| {
            let payload = payload.clone();
            async move {
                upstream::call(&origin, &token, tag, payload).await.map_err(|e| {
                    let rejected = e.chain().any(|c| {
                        matches!(c.downcast_ref(), Some(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status() == 401)
                    });
                    if rejected { Unauthorized.into() } else { e }
                })
            }
        })
        .await
    }

    /// Runs `f` against the running desktop server with t3boat's session, issuing a new one
    /// when there is none or the server no longer knows it (the user revoked it).
    async fn authorized<T, F: Future<Output = Result<T>>>(&self, f: impl Fn(String, String) -> F) -> Result<T> {
        let origin = local_origin()?;
        let token = self.session(None).await?;
        match f(origin.clone(), token.clone()).await {
            Err(e) if e.is::<Unauthorized>() => f(origin, self.session(Some(&token)).await?).await,
            result => result,
        }
    }

    /// The saved session, or a newly issued one when there is none or it is `rejected`.
    async fn session(&self, rejected: Option<&str>) -> Result<String> {
        let mut saved = self.token.lock().await;
        if let Some(token) = saved.as_deref().filter(|t| Some(*t) != rejected) {
            return Ok(token.to_owned());
        }
        let mut cmd = command(self.cli()?);
        cmd.args(["auth", "session", "issue", "--token-only", "--ttl", "3650d", "--label", "t3boat"]);
        for scope in LOCAL_SCOPES {
            cmd.args(["--scope", scope]);
        }
        let out = cmd.output().await.context("running t3 auth session issue")?;
        if !out.status.success() {
            bail!("T3 Code refused t3boat a session: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if let Some(path) = &self.token_file {
            crate::store::write_private(path, token.as_bytes())?;
        }
        *saved = Some(token.clone());
        Ok(token)
    }

    /// Opens one of an environment's threads in the desktop app.
    pub fn open_thread(&self, environment_id: &str, thread_id: &str) -> Result<()> {
        self.cli()?;
        crate::app::open_url(&format!("t3code://threads/{environment_id}/{thread_id}"))
    }
}

/// The desktop server turned the session down.
#[derive(Debug)]
struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("T3 Code did not accept t3boat's session")
    }
}

impl std::error::Error for Unauthorized {}

/// Where the desktop app's server answers, from the runtime file it writes on start.
fn local_origin() -> Result<String> {
    let runtime = home()?.join("userdata").join("server-runtime.json");
    let runtime: Value = serde_json::from_slice(&std::fs::read(&runtime).map_err(|_| anyhow!("T3 Code is not running"))?)?;
    runtime["origin"].as_str().map(str::to_owned).ok_or_else(|| anyhow!("T3 Code is not running"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pinned_works_without_t3_code() {
        let t3 = T3Desktop::pinned("0.1.10".into());
        assert_eq!(t3.version().await.unwrap(), "0.1.10");
        assert!(t3.projects().await.unwrap().is_empty());
        assert!(t3.links().await.unwrap().is_empty());
        t3.unlink("env").await.unwrap();
        let link = t3.link("http://localhost:1", "code", "full-access").await.unwrap_err();
        assert_eq!(link.to_string(), "T3 Code is not installed on this computer");
        assert!(t3.activate().is_err());
        assert!(t3.open_thread("env", "thread").is_err());
    }

    #[tokio::test]
    async fn a_pinned_version_wins_over_the_cli() {
        let t3 = T3Desktop { cli: Some("/nonexistent/t3".into()), pinned: None, token_file: None, token: Arc::default() };
        assert_eq!(t3.with_version("0.2.0".into()).version().await.unwrap(), "0.2.0");
    }

    /// Against the running desktop app, with t3boat's own session file:
    /// `cargo test reads_the_desktop_projects -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn reads_the_desktop_projects() {
        let dir = dirs::data_dir().unwrap().join("dev.t3boat.app");
        let t3 = T3Desktop::locate().unwrap().with_token_file(dir.join("t3-token"));
        let projects = t3.projects().await.unwrap();
        assert!(!projects.is_empty());
        for p in projects.iter().take(5) {
            println!("{} {} {} {:?}", p.title, p.repo_key, p.path, p.subdirectory);
        }
        let (files, failed) = t3.files_to_copy(&projects[0]).await.unwrap();
        println!("{}: copies {:?}, failed {failed:?}", projects[0].title, files.iter().map(|f| &f.0).collect::<Vec<_>>());
    }
}
