//! Thin wrapper over the user's own Boat CLI. Uses its existing login; never stores Boat credentials.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use crate::process::command;

#[derive(Clone, Debug)]
pub struct Boat {
    bin: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sandbox {
    pub id: String,
    pub state: String,
    pub stopped_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A sandbox as `boat list` describes it, for sandboxes t3boat shows but does not manage.
/// Field names match Boat's JSON and the UI's `BoatSandbox`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SandboxSummary {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub state: String,
    #[serde(rename = "type", default)]
    pub size: Option<String>,
    #[serde(default)]
    pub vcpu: Option<u32>,
    #[serde(rename = "memoryGB", default)]
    pub memory_gb: Option<f64>,
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub identifier: Option<String>,
    pub signed_in: bool,
    pub plan: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub dollars: f64,
    pub seconds: u64,
}

/// A pending browser sign-in: the page that completes it, and the code that page may ask for.
#[derive(Clone, Debug)]
pub struct SignIn {
    pub url: String,
    pub code: Option<String>,
}

/// Boat's answer when a free trial asks for a sandbox without auto-stop.
const TRIAL_REFUSAL: &str = "trial_auto_stop_required";
/// The longest auto-stop Boat's free trial allows (2 hours); `watchdog.mjs` renews it.
const TRIAL_TTL_SECS: &str = "7200";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    Resumed,
    /// Boat deleted the stopped sandbox's disk: it keeps one 14 days.
    DataExpired,
}

#[derive(Clone, Debug, Default)]
pub struct Starts {
    pub used_today: u64,
    pub limit_today: u64,
}

impl Boat {
    /// Finds the CLI the Boat installer puts in `~/.ascii/bin`, else on PATH.
    pub fn locate() -> Option<Self> {
        let exe = if cfg!(windows) { "boat.exe" } else { "boat" };
        let installed = dirs::home_dir().map(|h| h.join(".ascii").join("bin").join(exe));
        installed
            .filter(|p| p.is_file())
            .or_else(|| which::which("boat").ok())
            .map(|bin| Self { bin })
    }

    /// Runs the CLI; returns whether it succeeded, stdout and stderr.
    async fn output(&self, args: &[&str], timeout: Duration) -> Result<(bool, String, String)> {
        let mut cmd = command(&self.bin);
        // `--json` first: anything after `--` belongs to the remote command, and a nested
        // subcommand (`snapshot rm`) is only read as one with no option before it.
        cmd.arg("--json")
            .args(args)
            .env("BOAT_NO_UPDATE", "1")
            .kill_on_drop(true);
        // Only the subcommand goes into errors: arguments can hold scripts.
        let sub = args.first().copied().unwrap_or_default();
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow!("boat {sub} timed out"))?
            .with_context(|| format!("running boat {sub}"))?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    async fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let (ok, stdout, stderr) = self.output(args, timeout).await?;
        if !ok {
            let sub = args.first().copied().unwrap_or_default();
            bail!("boat {sub} failed: {}", said(&stdout, &stderr));
        }
        Ok(stdout)
    }

    /// The CLI prints JSON lines for long operations; the last line is the result.
    fn last_json(out: &str) -> Result<Value> {
        let line = out
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with('{'))
            .ok_or_else(|| anyhow!("boat printed no JSON: {out}"))?;
        Ok(serde_json::from_str(line)?)
    }

    pub async fn account(&self) -> Result<Account> {
        let v = Self::last_json(&self.run(&["status"], Duration::from_secs(20)).await?)?;
        Ok(serde_json::from_value(v["account"].clone())?)
    }

    /// Returns the new sandbox's id. `snapshot`: the saved snapshot its disk starts from.
    /// Runs `args` without Boat's auto-stop, as machines run: the watchdog decides when they
    /// sleep. Boat's free trial refuses that, so there it runs them with the longest auto-stop
    /// the trial allows; the watchdog keeps pushing that deadline while the machine is up.
    /// Asked of Boat every time, so a trial upgraded to a plan needs nothing more.
    async fn output_without_auto_stop(&self, args: &[&str], timeout: Duration) -> Result<(bool, String, String)> {
        let out = self.output(&[args, &["--no-auto-stop"]].concat(), timeout).await?;
        if out.0 || !said(&out.1, &out.2).contains(TRIAL_REFUSAL) {
            return Ok(out);
        }
        tracing::info!("Boat trial: boat {} with a {TRIAL_TTL_SECS} s auto-stop", args[0]);
        self.output(&[args, &["--ttl", TRIAL_TTL_SECS]].concat(), timeout).await
    }

    pub async fn create(&self, name: &str, size: &str, snapshot: Option<&str>) -> Result<String> {
        let mut args = vec!["new", "--name", name, "--type", size];
        args.extend(snapshot.into_iter().flat_map(|s| ["--from", s]));
        let (ok, stdout, stderr) = self.output_without_auto_stop(&args, Duration::from_secs(300)).await?;
        if !ok {
            bail!("boat new failed: {}", said(&stdout, &stderr));
        }
        let out = stdout;
        let v = Self::last_json(&out)?;
        let id = v["id"].as_str().ok_or_else(|| anyhow!("boat new returned no id"))?;
        Ok(id.to_owned())
    }

    pub async fn info(&self, id: &str) -> Result<Sandbox> {
        let v = Self::last_json(&self.run(&["info", id], Duration::from_secs(20)).await?)?;
        Ok(serde_json::from_value(v["sandbox"].clone())?)
    }

    pub async fn list(&self) -> Result<Vec<Sandbox>> {
        let v = Self::last_json(&self.run(&["list", "--all"], Duration::from_secs(20)).await?)?;
        Ok(serde_json::from_value(v["sandboxes"].clone())?)
    }

    /// Every sandbox on the account, with what the UI shows for one t3boat does not manage.
    pub async fn summaries(&self) -> Result<Vec<SandboxSummary>> {
        let v = Self::last_json(&self.run(&["list", "--all"], Duration::from_secs(20)).await?)?;
        Ok(serde_json::from_value(v["sandboxes"].clone())?)
    }

    /// Starts a stopped sandbox with Boat's own auto-stop, as `boat resume` does by default.
    /// Not for machines: they resume through `resume`, onto their size and without auto-stop.
    pub async fn start(&self, id: &str) -> Result<()> {
        self.run(&["resume", id], Duration::from_secs(300)).await.map(drop)
    }

    /// Stops a sandbox; its disk stays, as for a sleeping machine.
    pub async fn stop(&self, id: &str) -> Result<()> {
        self.run(&["stop", id], Duration::from_secs(120)).await.map(drop)
    }

    /// Gone only when Boat's own list confirms it: one bad answer must not retire a machine.
    pub async fn confirmed_gone(&self, id: &str) -> bool {
        matches!(self.list().await, Ok(l) if l.iter().all(|s| s.id != id))
    }

    /// Resumes onto `size`, which is how a machine changes size.
    pub async fn resume(&self, id: &str, size: &str) -> Result<Resume> {
        let (ok, stdout, stderr) =
            self.output_without_auto_stop(&["resume", id, "--type", size], Duration::from_secs(300)).await?;
        let said = said(&stdout, &stderr);
        match ok {
            true => Ok(Resume::Resumed),
            false if said.contains("snapshot_data_expired") => Ok(Resume::DataExpired),
            false => bail!("boat resume failed: {said}"),
        }
    }

    pub async fn delete_snapshot(&self, name: &str) -> Result<()> {
        self.run(&["snapshot", "rm", name], Duration::from_secs(60)).await.map(drop)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.run(&["delete", id, "--yes"], Duration::from_secs(120))
            .await
            .map(drop)
    }

    /// Exposes `port` (bound to loopback on the sandbox) on a stable public HTTPS URL.
    pub async fn host_public(&self, id: &str, port: u16) -> Result<String> {
        self.host(id, port, "--public", true).await
    }

    /// Exposes `port` on a stable HTTPS URL that only opens with the token it carries.
    /// `loopback`: the service listens on loopback only, so Boat relays to it. Wrong for a
    /// service on every address: the relay would hold its port.
    pub async fn host_private(&self, id: &str, port: u16, loopback: bool) -> Result<String> {
        self.host(id, port, "--private", loopback).await
    }

    async fn host(&self, id: &str, port: u16, access: &str, loopback: bool) -> Result<String> {
        let port = port.to_string();
        let mut args = vec!["host", id, &port, access];
        args.extend(loopback.then_some("--localhost"));
        let v = Self::last_json(&self.run(&args, Duration::from_secs(60)).await?)?;
        v["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("boat host returned no url"))
    }

    pub async fn exec(&self, id: &str, script: &str, timeout_secs: u64) -> Result<ExecResult> {
        let t = timeout_secs.to_string();
        // The CLI exits with the command's code; its JSON result is what matters.
        let (_, stdout, stderr) = self
            .output(
                &["exec", id, "--timeout", &t, "--", "bash", "-lc", script],
                Duration::from_secs(timeout_secs + 60),
            )
            .await?;
        match Self::last_json(&stdout) {
            Ok(v) => Ok(serde_json::from_value(v)?),
            Err(_) => bail!("boat exec failed: {}", stderr.trim()),
        }
    }

    /// Runs `script` and fails unless it exits 0. Returns stdout.
    pub async fn exec_ok(&self, id: &str, script: &str, timeout_secs: u64) -> Result<String> {
        let r = self.exec(id, script, timeout_secs).await?;
        if r.exit_code != Some(0) {
            let detail = if r.stderr.trim().is_empty() { r.stdout.trim() } else { r.stderr.trim() };
            bail!("command on the machine failed ({:?}): {detail}", r.exit_code);
        }
        Ok(r.stdout)
    }

    pub async fn usage_since(&self, id: &str, since_unix: i64) -> Result<Usage> {
        let since = since_unix.to_string();
        let v = Self::last_json(
            &self.run(&["usage", id, "--since", &since], Duration::from_secs(20)).await?,
        )?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn starts(&self) -> Result<Starts> {
        let v = Self::last_json(&self.run(&["limits", "--all"], Duration::from_secs(20)).await?)?;
        let day = &v["starts"]["day"];
        Ok(Starts {
            used_today: day["used"].as_u64().unwrap_or(0),
            limit_today: day["limit"].as_u64().unwrap_or(0),
        })
    }

    /// Starts a browser sign-in, or picks up the one already pending. Without a terminal the
    /// CLI opens no browser, so the caller opens the page.
    pub async fn start_sign_in(&self) -> Result<SignIn> {
        let v = Self::last_json(&self.run(&["onboard"], Duration::from_secs(60)).await?)?;
        let url = v["url"].as_str().ok_or_else(|| anyhow!("boat onboard gave no sign-in page: {v}"))?;
        Ok(SignIn { url: url.to_owned(), code: v["userCode"].as_str().map(str::to_owned) })
    }

    /// Waits until the started sign-in completes or expires.
    pub async fn finish_sign_in(&self) -> Result<()> {
        self.run(&["onboard", "--wait"], Duration::from_secs(900)).await.map(drop)
    }

    /// Copies a local file to `remote_path` on the sandbox.
    pub async fn upload(&self, id: &str, local: &Path, remote_path: &str) -> Result<()> {
        let to = format!("{id}:{remote_path}");
        self.run(&["scp", path_str(local)?, &to], Duration::from_secs(1800)).await.map(drop)
    }

    /// Forwards `local` on this computer to `remote` in the sandbox, for as long as the returned
    /// process lives.
    pub async fn forward(&self, id: &str, remote: u16, local: u16) -> Result<tokio::process::Child> {
        // Else another listener on the port would pass the readiness check below.
        if std::net::TcpListener::bind(("127.0.0.1", local)).is_err() {
            bail!("port {local} is in use on this computer");
        }
        let (port, remote) = (local, remote.to_string());
        let p = port.to_string();
        let mut cmd = command(&self.bin);
        cmd.args(["forward", id, "--remote", &remote, "--local", &p])
            .env("BOAT_NO_UPDATE", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // A tunnel must not outlive this app when it is killed: nothing is dropped then.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("running boat forward")?;
        for _ in 0..100 {
            if let Some(status) = child.try_wait()? {
                bail!("boat forward exited ({status}); is port {port} free on this computer?");
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return Ok(child);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        bail!("boat forward did not open port {port}")
    }
}

/// What a failed command said: its stderr, else its stdout.
fn said<'a>(stdout: &'a str, stderr: &'a str) -> &'a str {
    if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() }
}

/// Command-line form of a path; the Boat CLI and git only take UTF-8 here.
pub(crate) fn path_str(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| anyhow!("path is not UTF-8: {}", path.display()))
}
