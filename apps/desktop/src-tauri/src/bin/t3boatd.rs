//! Headless t3boat for a server with no desktop, such as a Linux "mothership" Boat: the app
//! model, each machine's sleep-aware proxy and loops, and the MCP server, without the tray.
//! Sign-in pages and notifications go to the log. Stops on SIGINT or SIGTERM.
//!
//! With `--auto-sleep` it stops the Boat sandbox it runs on once nothing works and no client used
//! it for a while, and exits. Run it as a service that restarts it (systemd `Restart=always`), so
//! it runs again whether Boat resumes the sandbox warm or boots it anew.
//!
//! Build: `cargo build --release --no-default-features --bin t3boatd`.

use anyhow::{bail, ensure, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use t3boat_lib::app::App;
use t3boat_lib::boat::{Boat, Resume};
use t3boat_lib::machine::Event;
use t3boat_lib::mcp;
use t3boat_lib::mothership::{should_sleep, DUE_GUARD_SECS};
use t3boat_lib::store::{normalize_tailscale_host, Settings};
use t3boat_lib::t3desktop::T3Desktop;
use t3boat_lib::tailscale;

const USAGE: &str = "\
Usage: t3boatd [options]

Options:
  --data-dir <path>        Where t3boat keeps its state [env: T3BOAT_DATA_DIR]
                           (default: the user data directory, then t3boat)
  --t3-version <version>   The T3 version machines get, when no `t3` CLI is installed here
                           [env: T3BOAT_T3_VERSION]
  --tailscale-host <host>  This computer's MagicDNS name, box.tailnet.ts.net: puts machines on
                           the tailnet [env: T3BOAT_TAILSCALE_HOST]
  --auto-tailscale         Detect the MagicDNS name at start when none is set
  --auto-sleep             Stop this Boat sandbox and exit once no machine works, no scheduled
                           work is near and no client used it for 300 s
  --auto-sleep-secs <secs> The same after <secs> idle, at least 60 [env: T3BOAT_AUTO_SLEEP_SECS]
  --wake [<sandbox-id>]    Wake that sandbox (a mothership running --auto-sleep) and exit
                           [env: T3BOAT_MOTHERSHIP_ID]
  --github-login           Sign GitHub in via device code (prints URL + code) and exit
  -h, --help               Print this help";

/// How often spend and Boat's daily starts are read, as the tray does.
const COSTS_EVERY: Duration = Duration::from_secs(300);
/// Idle time before `--auto-sleep` stops the sandbox, and the least it takes.
const AUTO_SLEEP_SECS: u64 = 300;
const MIN_AUTO_SLEEP_SECS: u64 = 60;
/// How often `--auto-sleep` checks.
const AUTO_SLEEP_POLL: Duration = Duration::from_secs(15);
/// A check this late means Boat froze the sandbox and resumed it: someone woke it.
const RESUMED_GAP: Duration = Duration::from_secs(60);

#[derive(Debug, Default, PartialEq)]
struct Config {
    data_dir: Option<PathBuf>,
    t3_version: Option<String>,
    /// Normalized.
    tailscale_host: Option<String>,
    auto_tailscale: bool,
    /// Idle seconds before the sandbox stops itself; `None`: never.
    auto_sleep: Option<u64>,
    /// The sandbox to wake, then exit.
    wake: Option<String>,
    github_login: bool,
    help: bool,
}

impl Config {
    /// From the arguments after the program name; a flag wins over its environment variable.
    fn parse(args: impl IntoIterator<Item = String>, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let env = |name: &str| env(name).filter(|v| !v.trim().is_empty());
        let mut config = Config {
            data_dir: env("T3BOAT_DATA_DIR").map(PathBuf::from),
            t3_version: env("T3BOAT_T3_VERSION"),
            ..Config::default()
        };
        let mut host = env("T3BOAT_TAILSCALE_HOST");
        let mut auto_sleep_secs = env("T3BOAT_AUTO_SLEEP_SECS");
        let mut auto_sleep = auto_sleep_secs.is_some();
        let mut args = args.into_iter().peekable();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
                _ => (arg, None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| args.next())
                    .filter(|v| !v.is_empty())
                    .with_context(|| format!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--data-dir" => config.data_dir = Some(value()?.into()),
                "--t3-version" => config.t3_version = Some(value()?),
                "--tailscale-host" => host = Some(value()?),
                "--auto-tailscale" => config.auto_tailscale = true,
                "--auto-sleep" => auto_sleep = true,
                "--auto-sleep-secs" => {
                    auto_sleep_secs = Some(value()?);
                    auto_sleep = true;
                }
                // The id is optional: the next argument, unless it is another option.
                "--wake" => {
                    let id = inline.clone().or_else(|| args.next_if(|a| !a.starts_with('-')));
                    let id = id.filter(|id| !id.trim().is_empty()).or_else(|| env("T3BOAT_MOTHERSHIP_ID"));
                    config.wake = Some(id.context("--wake needs a sandbox id, or T3BOAT_MOTHERSHIP_ID")?.trim().to_owned());
                }
                "--github-login" => config.github_login = true,
                "-h" | "--help" => config.help = true,
                _ => bail!("unknown option {flag}\n\n{USAGE}"),
            }
        }
        if auto_sleep {
            let secs = match auto_sleep_secs {
                Some(secs) => secs.trim().parse().with_context(|| format!("auto-sleep seconds must be a number: {secs}"))?,
                None => AUTO_SLEEP_SECS,
            };
            ensure!(secs >= MIN_AUTO_SLEEP_SECS, "auto-sleep needs at least {MIN_AUTO_SLEEP_SECS} s idle, not {secs}");
            config.auto_sleep = Some(secs);
        }
        if let Some(host) = host {
            config.tailscale_host = Some(
                normalize_tailscale_host(&host)
                    .with_context(|| format!("{host} is not a MagicDNS machine name like box.tailnet.ts.net"))?,
            );
        }
        Ok(config)
    }

    fn data_dir(&self) -> Result<PathBuf> {
        match &self.data_dir {
            Some(dir) => Ok(dir.clone()),
            None => dirs::data_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
                .map(|d| d.join("t3boat"))
                .context("no data directory: pass --data-dir"),
        }
    }
}

/// The local `t3` CLI if there is one, else the pinned version alone; an explicit pin wins.
fn t3(pinned: Option<String>) -> Result<T3Desktop> {
    Ok(match (T3Desktop::locate(), pinned) {
        (Some(t3), Some(version)) => t3.with_version(version),
        (Some(t3), None) => t3,
        (None, Some(version)) => T3Desktop::pinned(version),
        (None, None) => bail!(
            "no `t3` CLI in ~/.t3/bin, ~/.local/bin or on PATH: install T3 Code, or pass --t3-version \
             (T3BOAT_T3_VERSION) for the T3 version machines get"
        ),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse(std::env::args().skip(1), |name| std::env::var(name).ok())?;
    if config.help {
        println!("{USAGE}");
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    if let Some(id) = &config.wake {
        let boat = Boat::locate().context("the Boat CLI is not installed: no boat in ~/.ascii/bin or on PATH")?;
        if boat.wake(id).await? == Resume::DataExpired {
            bail!("Boat deleted {id}'s disk after 14 days asleep");
        }
        println!("{id} is awake.");
        return Ok(());
    }

    let data_dir = config.data_dir()?;

    if config.github_login {
        // Refuse if a running t3boatd owns this port/state directory so we never overwrite its state.
        let _guard = mcp::bind()
            .await
            .context("another t3boatd appears to be running on the MCP port; stop it before running --github-login")?;
        let store = t3boat_lib::store::Store::new(&data_dir);
        let mut state = store.load().context("reading t3boat state")?;
        let scratch = data_dir.join(format!("gh-sign-in-{}", t3boat_lib::proxy::auth::random_token()));
        let uploads = t3boat_lib::github::sign_in(&scratch, |code| {
            println!("Open https://github.com/login/device and enter code: {code}");
        })
        .await?;
        state.github_uploads = Some(uploads);
        store.save(&state).context("saving t3boat state")?;
        println!("GitHub signed in.");
        return Ok(());
    }

    let boat = Boat::locate().context("the Boat CLI is not installed: no boat in ~/.ascii/bin or on PATH")?;
    if !boat.account().await.is_ok_and(|a| a.signed_in) {
        bail!("Boat is not signed in: run `boat onboard`");
    }
    let t3 = t3(config.t3_version.clone())?;
    tracing::info!("t3boatd: data in {}, machines get T3 {}", data_dir.display(), t3.version().await?);

    // Bind MCP before starting proxies or reporting readiness so an occupied port fails startup cleanly.
    let mcp_listener = mcp::bind().await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let app = App::new(boat, t3, data_dir, tx)?;
    let (flush_tx, flush_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(events(app.clone(), rx, flush_rx));

    set_tailscale_host(&app, &config).await;
    tokio::spawn(costs(app.clone()));
    let mcp_app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = mcp::serve_listener(mcp_listener, mcp_app).await {
            tracing::error!("coding agents cannot reach t3boat: {e:#}");
        }
    });
    tracing::info!("t3boatd running: {} machines, MCP at {}", app.machines().len(), mcp::url());

    let asleep = async {
        match config.auto_sleep {
            Some(secs) => auto_sleep(&app, secs, &flush_tx).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        stop = shutdown() => {
            stop?;
            tracing::info!("t3boatd stopping");
            app.stop_forwards().await;
            if let Err(e) = flush_and_wait(&flush_tx).await {
                tracing::error!("saving state/caches on shutdown: {e:#}");
            }
        }
        () = asleep => tracing::info!("t3boatd stopping: Boat is putting this sandbox to sleep"),
    }
    Ok(())
}

/// Requests to the event loop to save state and caches now; each is answered once written.
type Flush = tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<Result<()>>>;

/// Saves state and caches with every change sent before, and waits until written.
async fn flush_and_wait(flush: &Flush) -> Result<()> {
    let (done, written) = tokio::sync::oneshot::channel();
    flush.send(done).map_err(|_| anyhow::anyhow!("event loop stopped"))?;
    written.await.map_err(|_| anyhow::anyhow!("event loop dropped flush"))?
}

/// Returns once Boat is stopping the sandbox this runs on: no machine works, no scheduled work is
/// near, and no client used this app for `idle_secs`. A failed stop is tried again on a later check.
async fn auto_sleep(app: &App, idle_secs: u64, flush: &Flush) {
    tracing::info!("auto-sleep: this sandbox stops after {idle_secs} s idle");
    let mut checks = tokio::time::interval(AUTO_SLEEP_POLL);
    checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = SystemTime::now();
    loop {
        checks.tick().await;
        let now = SystemTime::now();
        if now.duration_since(last).unwrap_or_default() > RESUMED_GAP {
            tracing::info!("auto-sleep: resumed, idle time counts anew");
            app.shared.touch_activity();
        }
        last = now;
        let activity = app.activity();
        if !should_sleep(&activity, idle_secs, DUE_GUARD_SECS) {
            continue;
        }
        tracing::info!("auto-sleep: idle for {} s with nothing working or due: stopping this sandbox", activity.idle_for_secs);
        app.stop_forwards().await;
        if let Err(e) = flush_and_wait(flush).await {
            tracing::error!("auto-sleep: saving state/caches failed ({e:#}); skipping stop");
            continue;
        }
        // Recheck immediately before stopping so activity or a worker wake during flush cancels it.
        if !should_sleep(&app.activity(), idle_secs, DUE_GUARD_SECS) {
            tracing::info!("auto-sleep: activity arrived during flush; staying awake");
            continue;
        }
        match app.shared.boat.stop_self().await {
            Ok(()) => return,
            Err(e) => {
                tracing::error!("auto-sleep: {e:#}; resetting idle window and trying again");
                app.shared.touch_activity();
            }
        }
    }
}

/// A host given on the command line replaces the saved one; `--auto-tailscale` fills one in only
/// when none is saved.
async fn set_tailscale_host(app: &Arc<App>, config: &Config) {
    let settings = app.settings();
    let host = match (&config.tailscale_host, &settings.tailscale_host) {
        (Some(host), saved) if saved.as_ref() != Some(host) => host.clone(),
        (None, None) if config.auto_tailscale => match tailscale::detect_host().await {
            Some(host) => host,
            None => {
                tracing::warn!("--auto-tailscale: no MagicDNS name; is Tailscale running?");
                return;
            }
        },
        _ => return,
    };
    tracing::info!("machines on the tailnet at {host}");
    app.update_settings(Settings { tailscale_host: Some(host), ..settings });
}

async fn events(
    app: Arc<App>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    mut flush: tokio::sync::mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<Result<()>>>,
) {
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(Event::Persist) => app.save(),
                Some(Event::Changed) => {}
                Some(Event::Notify { title, body }) => tracing::info!("{title}: {body}"),
                None => break,
            },
            Some(done) = flush.recv() => {
                while let Ok(ev) = rx.try_recv() {
                    if let Event::Notify { title, body } = ev {
                        tracing::info!("{title}: {body}");
                    }
                }
                let _ = done.send(app.try_save());
            }
        }
    }
}

async fn costs(app: Arc<App>) {
    loop {
        app.refresh_costs().await;
        tokio::time::sleep(COSTS_EVERY).await;
    }
}

/// Resolves on SIGINT, or SIGTERM on Unix (as systemd stops a service).
async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn parse(args: &[&str], env: &[(&str, &str)]) -> Result<Config> {
        let env: HashMap<String, String> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::parse(args.iter().map(|a| a.to_string()), |name| env.get(name).cloned())
    }

    #[test]
    fn defaults_to_nothing_set() {
        assert_eq!(parse(&[], &[]).unwrap(), Config::default());
    }

    #[test]
    fn reads_flags() {
        let config = parse(
            &["--data-dir", "/srv/t3boat", "--t3-version=0.1.10", "--tailscale-host", "Box.Tail1.ts.net", "--auto-tailscale"],
            &[],
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                data_dir: Some("/srv/t3boat".into()),
                t3_version: Some("0.1.10".into()),
                tailscale_host: Some("box.tail1.ts.net".into()),
                auto_tailscale: true,
                auto_sleep: None,
                wake: None,
                github_login: false,
                help: false,
            }
        );
    }

    #[test]
    fn reads_the_environment_and_flags_win() {
        let env = [
            ("T3BOAT_DATA_DIR", "/env/dir"),
            ("T3BOAT_T3_VERSION", "0.1.9"),
            ("T3BOAT_TAILSCALE_HOST", "env.tail1.ts.net"),
        ];
        let from_env = parse(&[], &env).unwrap();
        assert_eq!(from_env.data_dir, Some("/env/dir".into()));
        assert_eq!(from_env.t3_version.as_deref(), Some("0.1.9"));
        assert_eq!(from_env.tailscale_host.as_deref(), Some("env.tail1.ts.net"));
        let flagged = parse(&["--t3-version", "0.2.0", "--tailscale-host", "flag.tail1.ts.net"], &env).unwrap();
        assert_eq!(flagged.t3_version.as_deref(), Some("0.2.0"));
        assert_eq!(flagged.tailscale_host.as_deref(), Some("flag.tail1.ts.net"));
        assert_eq!(flagged.data_dir, Some("/env/dir".into()));
    }

    #[test]
    fn an_empty_variable_is_unset() {
        assert_eq!(parse(&[], &[("T3BOAT_T3_VERSION", " "), ("T3BOAT_TAILSCALE_HOST", "")]).unwrap(), Config::default());
    }

    #[test]
    fn refuses_bad_input() {
        assert!(parse(&["--tailscale-host", "100.64.0.1"], &[]).is_err());
        assert!(parse(&[], &[("T3BOAT_TAILSCALE_HOST", "example.com")]).is_err());
        assert!(parse(&["--data-dir"], &[]).is_err());
        assert!(parse(&["--t3-version="], &[]).is_err());
        assert!(parse(&["--verbose"], &[]).is_err());
    }

    #[test]
    fn auto_sleep_defaults_to_300_seconds() {
        assert_eq!(parse(&["--auto-sleep"], &[]).unwrap().auto_sleep, Some(300));
        assert_eq!(parse(&[], &[]).unwrap().auto_sleep, None);
    }

    #[test]
    fn auto_sleep_secs_sets_and_turns_it_on() {
        assert_eq!(parse(&["--auto-sleep-secs", "600"], &[]).unwrap().auto_sleep, Some(600));
        assert_eq!(parse(&["--auto-sleep-secs=60"], &[]).unwrap().auto_sleep, Some(60));
        assert_eq!(parse(&["--auto-sleep", "--auto-sleep-secs", "90"], &[]).unwrap().auto_sleep, Some(90));
    }

    #[test]
    fn auto_sleep_reads_the_environment_and_the_flag_wins() {
        let env = [("T3BOAT_AUTO_SLEEP_SECS", "120")];
        assert_eq!(parse(&[], &env).unwrap().auto_sleep, Some(120));
        assert_eq!(parse(&["--auto-sleep"], &env).unwrap().auto_sleep, Some(120));
        assert_eq!(parse(&["--auto-sleep-secs", "900"], &env).unwrap().auto_sleep, Some(900));
        assert_eq!(parse(&[], &[("T3BOAT_AUTO_SLEEP_SECS", " ")]).unwrap().auto_sleep, None);
    }

    #[test]
    fn auto_sleep_refuses_less_than_a_minute_or_no_number() {
        assert!(parse(&["--auto-sleep-secs", "59"], &[]).is_err());
        assert!(parse(&["--auto-sleep-secs", "soon"], &[]).is_err());
        assert!(parse(&["--auto-sleep-secs"], &[]).is_err());
        assert!(parse(&[], &[("T3BOAT_AUTO_SLEEP_SECS", "30")]).is_err());
    }

    #[test]
    fn wake_takes_an_id_or_the_mothership_variable() {
        let env = [("T3BOAT_MOTHERSHIP_ID", "bx_env")];
        assert_eq!(parse(&["--wake", "bx_flag"], &env).unwrap().wake.as_deref(), Some("bx_flag"));
        assert_eq!(parse(&["--wake=bx_inline"], &[]).unwrap().wake.as_deref(), Some("bx_inline"));
        assert_eq!(parse(&["--wake"], &env).unwrap().wake.as_deref(), Some("bx_env"));
        // The next option is not the id.
        let config = parse(&["--wake", "--data-dir", "/d"], &env).unwrap();
        assert_eq!((config.wake.as_deref(), config.data_dir), (Some("bx_env"), Some("/d".into())));
        // The variable alone wakes nothing: a running t3boatd may have it set.
        assert_eq!(parse(&[], &env).unwrap().wake, None);
    }

    #[test]
    fn wake_needs_an_id() {
        assert!(parse(&["--wake"], &[]).is_err());
        assert!(parse(&["--wake="], &[]).is_err());
        assert!(parse(&["--wake"], &[("T3BOAT_MOTHERSHIP_ID", " ")]).is_err());
    }

    #[test]
    fn asks_for_help() {
        assert!(parse(&["--help"], &[]).unwrap().help);
        assert!(parse(&["-h"], &[]).unwrap().help);
    }

    #[test]
    fn pins_the_t3_version() {
        let t3 = t3(Some("0.1.10".into())).unwrap();
        let version = tokio::runtime::Runtime::new().unwrap().block_on(t3.version()).unwrap();
        assert_eq!(version, "0.1.10");
    }

    #[test]
    fn the_default_data_dir_is_named_t3boat() {
        assert!(Config::default().data_dir().unwrap().ends_with("t3boat"));
    }
}
