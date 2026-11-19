//! "GitHub uploads": the user's own GitHub login, for the one thing machines cannot do with the
//! login Boat gives them. Boat signs `gh` in with its GitHub App's token, and GitHub's upload
//! server refuses those, so `gh pr edit --attach shot.png` fails there. The sign-in runs here
//! with this computer's `gh`, in a config folder of its own: the user's own `gh` login is neither
//! read nor changed.

use crate::process::command;
use crate::store::GithubUploads;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Where GitHub asks for the code of a device sign-in.
const DEVICE_PAGE: &str = "https://github.com/login/device";
/// GitHub's device codes last 15 minutes.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(900);

/// `gh` with nothing of the user's: no token from the environment, its config in `config_dir`.
fn gh(bin: &Path, config_dir: &Path) -> tokio::process::Command {
    let mut cmd = command(bin);
    cmd.env("GH_CONFIG_DIR", config_dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for var in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN", "GH_HOST"] {
        cmd.env_remove(var);
    }
    cmd
}

/// Signs in to GitHub in the browser and returns the login. `scratch` is a folder that does not
/// exist yet; it is removed again. `on_code` gets the code GitHub's page asks for, before the
/// page opens.
pub async fn sign_in(scratch: &Path, on_code: impl FnOnce(&str)) -> Result<GithubUploads> {
    let bin = which::which("gh").map_err(|_| {
        anyhow!("GitHub's gh is not installed on this computer; the sign-in needs it: https://cli.github.com")
    })?;
    std::fs::create_dir_all(scratch)?;
    #[cfg(unix)]
    std::fs::set_permissions(scratch, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let result = sign_in_with(&bin, scratch, on_code).await;
    let _ = std::fs::remove_dir_all(scratch);
    result
}

async fn sign_in_with(bin: &Path, config_dir: &Path, on_code: impl FnOnce(&str)) -> Result<GithubUploads> {
    // `--insecure-storage`: into the config folder, never the system keyring, where it would
    // replace the user's own gh login.
    let mut login = gh(bin, config_dir)
        .args(["auth", "login", "--hostname", "github.com", "--git-protocol", "https", "--web"])
        .args(["--skip-ssh-key", "--insecure-storage"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running gh auth login")?;
    let mut lines = BufReader::new(login.stderr.take().expect("stderr is piped")).lines();
    let mut said = String::new();
    let code = loop {
        match tokio::time::timeout(Duration::from_secs(30), lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if let Some(code) = device_code(&line) {
                    break code.to_owned();
                }
                said.push_str(&line);
                said.push('\n');
            }
            _ => bail!("gh started no sign-in: {}", said.trim()),
        }
    };
    on_code(&code);
    // Without a terminal gh opens no browser.
    crate::app::open_url(DEVICE_PAGE)?;
    let finished = tokio::time::timeout(SIGN_IN_TIMEOUT, async {
        while let Ok(Some(line)) = lines.next_line().await {
            said.push_str(&line);
            said.push('\n');
        }
        login.wait().await
    })
    .await
    .map_err(|_| anyhow!("the GitHub sign-in was not finished in the browser"))??;
    if !finished.success() {
        bail!("the GitHub sign-in failed: {}", said.trim());
    }
    let token = output(gh(bin, config_dir).args(["auth", "token", "--hostname", "github.com"])).await?;
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("gh gave no token after the sign-in");
    }
    let login = output(gh(bin, config_dir).args(["api", "user", "--jq", ".login"])).await.ok().filter(|l| !l.is_empty());
    Ok(GithubUploads { token, login })
}

async fn output(cmd: &mut tokio::process::Command) -> Result<String> {
    let out = cmd.output().await?;
    if !out.status.success() {
        bail!("gh failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The device code in a line of gh's output: `XXXX-XXXX`.
fn device_code(line: &str) -> Option<&str> {
    let part = |s: &str| s.len() == 4 && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find(|word| matches!(word.split_once('-'), Some((a, b)) if part(a) && part(b)))
}

#[cfg(test)]
mod tests {
    use super::device_code;

    #[test]
    fn finds_the_device_code_in_ghs_lines() {
        assert_eq!(device_code("! One-time code (AB12-CD34) copied to clipboard"), Some("AB12-CD34"));
        assert_eq!(device_code("! First copy your one-time code: 9F3A-00ZZ"), Some("9F3A-00ZZ"));
        assert_eq!(device_code("Open this URL to continue in your web browser: https://github.com/login/device"), None);
        assert_eq!(device_code("one-time code"), None);
    }
}
