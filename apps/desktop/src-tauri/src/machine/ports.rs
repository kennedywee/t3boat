//! Ports: a machine's dev servers in this computer's own browser. A forwarded port is a listener
//! on this computer's loopback at the same number, so an app that names `localhost:<port>` in its
//! code works unchanged. The listener is t3boat's and relays to a `boat forward` behind it: that
//! way a connection counts as using the machine, and a port taken here is told apart from a
//! tunnel that failed.

use super::{provision, MachineRuntime, Power};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// Each forward is a `boat forward` process of its own.
pub const MAX_FORWARDS: usize = 20;

/// Sockets that listen on the machine, then its containers' published ports. Root sees the
/// process names of other users' sockets (a container's `docker-proxy`).
const SCAN: &str = "(sudo -n ss -ltnpH 2>/dev/null || ss -ltnpH); echo ---; \
                    docker ps --format '{{.Names}} {{.Ports}}' 2>/dev/null; true";

/// What the machine's image runs for Boat itself: its agent, desktop stream, name services.
const SYSTEM: &[&str] =
    &["ascii-agents-se", "socat", "sunshine", "web-server", "sshd", "cupsd", "systemd-resolve", "epmd"];

#[derive(Default)]
pub struct Ports {
    /// As last scanned: port and what listens there.
    listening: Mutex<BTreeMap<u16, Option<String>>>,
    forwards: Mutex<HashMap<u16, Forward>>,
    /// Why a wanted port is not forwarded.
    errors: Mutex<HashMap<u16, String>>,
    syncing: tokio::sync::Mutex<()>,
}

/// The listener, its tunnel and its open connections: all end with the task.
struct Forward(JoinHandle<()>);

impl Drop for Forward {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortView {
    pub port: u16,
    /// What listens there: a container's name, else the process's.
    pub name: Option<String>,
    /// Seen listening at the last scan.
    pub listening: bool,
    /// Turned on: kept across sleeps.
    pub forwarded: bool,
    /// Reachable now at `localhost:<port>` on this computer.
    pub live: bool,
    pub error: Option<String>,
}

impl MachineRuntime {
    pub fn port_views(&self) -> Vec<PortView> {
        let wanted = self.record().forwards;
        let listening = self.ports.listening.lock().unwrap().clone();
        let forwards = self.ports.forwards.lock().unwrap();
        let errors = self.ports.errors.lock().unwrap();
        let mut ports: Vec<u16> = listening.keys().copied().chain(wanted.iter().copied()).collect();
        ports.sort_unstable();
        ports.dedup();
        ports
            .into_iter()
            .map(|port| PortView {
                port,
                name: listening.get(&port).cloned().flatten(),
                listening: listening.contains_key(&port),
                forwarded: wanted.contains(&port),
                live: forwards.get(&port).is_some_and(|f| !f.0.is_finished()),
                error: errors.get(&port).cloned(),
            })
            .collect()
    }

    /// Looks for what listens on the machine. Never wakes it: asleep, the last scan stays.
    pub async fn scan_ports(&self) -> Result<()> {
        if self.power() != Power::Running || self.is_stopping() {
            return Ok(());
        }
        let out = self.shared.boat.exec_ok(&self.sandbox_id(), SCAN, 20).await?;
        *self.ports.listening.lock().unwrap() = parse_scan(&out);
        self.shared.changed();
        Ok(())
    }

    /// Turns forwarding of `ports` on or off. On wakes a sleeping machine and returns once they
    /// are reachable; a port that could not be forwarded is left off and named in the error.
    pub async fn set_forwards(self: &Arc<Self>, ports: &[u16], on: bool) -> Result<()> {
        if !on {
            self.update_record(|m| m.forwards.retain(|p| !ports.contains(p)));
            self.sync_forwards().await;
            return Ok(());
        }
        if let Some(port) = ports.iter().find(|p| **p == 0) {
            bail!("{port} is not a port");
        }
        let mut wanted = self.record().forwards;
        for port in ports {
            if !wanted.contains(port) {
                wanted.push(*port);
            }
        }
        if wanted.len() > MAX_FORWARDS {
            bail!("a machine forwards at most {MAX_FORWARDS} ports");
        }
        self.ensure_awake().await?;
        self.update_record(|m| m.forwards = wanted);
        self.sync_forwards().await;
        let failed: Vec<String> = {
            let errors = self.ports.errors.lock().unwrap();
            ports.iter().filter_map(|p| errors.get(p).cloned()).collect()
        };
        if failed.is_empty() {
            return Ok(());
        }
        self.update_record(|m| m.forwards.retain(|p| !ports.contains(p) || self.forward_live(*p)));
        self.sync_forwards().await;
        bail!("{}", failed.join("; "))
    }

    /// Ends every tunnel now, for when the app quits: its tasks are not dropped then.
    pub async fn stop_forwards(&self) {
        let forwards: Vec<Forward> = self.ports.forwards.lock().unwrap().drain().map(|(_, f)| f).collect();
        for mut forward in forwards {
            forward.0.abort();
            let _ = (&mut forward.0).await;
        }
    }

    fn forward_live(&self, port: u16) -> bool {
        self.ports.forwards.lock().unwrap().get(&port).is_some_and(|f| !f.0.is_finished())
    }

    /// Whether the background loop has anything to bring in line.
    pub(super) fn forwards_to_sync(&self) -> bool {
        !self.record().forwards.is_empty() || !self.ports.forwards.lock().unwrap().is_empty()
    }

    /// Brings the forwards in line with the record: up while the machine runs, gone otherwise.
    /// One whose tunnel ended is started again.
    pub(super) async fn sync_forwards(self: &Arc<Self>) {
        let _one = self.ports.syncing.lock().await;
        let wanted = self.record().forwards;
        let up = self.power() == Power::Running && !self.is_stopping();
        let missing: Vec<u16> = {
            let mut forwards = self.ports.forwards.lock().unwrap();
            let before = forwards.len();
            forwards.retain(|port, f| up && wanted.contains(port) && !f.0.is_finished());
            let mut errors = self.ports.errors.lock().unwrap();
            errors.retain(|port, _| up && wanted.contains(port));
            if forwards.len() != before {
                self.shared.changed();
            }
            wanted.iter().filter(|p| up && !forwards.contains_key(p)).copied().collect()
        };
        if missing.is_empty() {
            return;
        }
        let started = futures_util::future::join_all(missing.iter().map(|port| self.start_forward(*port))).await;
        let mut forwards = self.ports.forwards.lock().unwrap();
        let mut errors = self.ports.errors.lock().unwrap();
        for (port, result) in missing.into_iter().zip(started) {
            match result {
                Ok(forward) => {
                    errors.remove(&port);
                    forwards.insert(port, forward);
                }
                Err(e) => {
                    tracing::warn!("forwarding port {port} of {}: {e:#}", self.id());
                    errors.insert(port, format!("{e:#}"));
                }
            }
        }
        self.shared.changed();
    }

    async fn start_forward(self: &Arc<Self>, port: u16) -> Result<Forward> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|_| anyhow!("port {port} is in use on this computer"))?;
        let tunnel = std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port();
        let mut child = self.shared.boat.forward(&self.sandbox_id(), port, tunnel).await?;
        let machine = Arc::downgrade(self);
        Ok(Forward(tokio::spawn(async move {
            let mut relays = JoinSet::new();
            loop {
                tokio::select! {
                    _ = child.wait() => return,
                    Some(_) = relays.join_next() => {}
                    accepted = listener.accept() => {
                        let (Ok((mut client, _)), Some(machine)) = (accepted, machine.upgrade()) else { return };
                        machine.used();
                        relays.spawn(async move {
                            if let Ok(mut remote) = TcpStream::connect(("127.0.0.1", tunnel)).await {
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut remote).await;
                            }
                        });
                    }
                }
            }
        })))
    }
}

/// Ports worth showing from `SCAN`'s output: unprivileged, not T3's own, not the image's.
fn parse_scan(out: &str) -> BTreeMap<u16, Option<String>> {
    let (sockets, containers) = out.split_once("---").unwrap_or((out, ""));
    let mut published = HashMap::new();
    for line in containers.lines() {
        let Some((name, ports)) = line.trim().split_once(' ') else { continue };
        // `0.0.0.0:4001->4001/tcp, [::]:4001->4001/tcp`
        for mapping in ports.split(',') {
            let host = mapping.split("->").next().unwrap_or_default();
            if let Some(port) = mapping.contains("->").then(|| port_of(host)).flatten() {
                published.insert(port, name.to_owned());
            }
        }
    }
    let mut found = BTreeMap::new();
    for line in sockets.lines() {
        let mut fields = line.split_whitespace();
        let Some(port) = fields.nth(3).and_then(port_of) else { continue };
        let process = line.split_once("users:((\"").and_then(|(_, rest)| rest.split('"').next());
        if port < 1024 || provision::own_port(port) || process.is_some_and(|p| SYSTEM.contains(&p)) {
            continue;
        }
        let name = published.get(&port).cloned().or_else(|| process.map(str::to_owned));
        found.entry(port).or_insert(name);
    }
    found
}

/// The port of `0.0.0.0:4000`, `[::]:22` or `*:8090`.
fn port_of(address: &str) -> Option<u16> {
    address.rsplit_once(':')?.1.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_scan;

    #[test]
    fn scan_keeps_dev_servers_and_names_containers() {
        let out = r#"LISTEN 0 4096 0.0.0.0:22 0.0.0.0:* users:(("sshd",pid=1772,fd=3),("systemd",pid=1,fd=120))
LISTEN 0 4096 0.0.0.0:4000 0.0.0.0:* users:(("docker-proxy",pid=55966,fd=8))
LISTEN 0 4096 [::]:4000 [::]:* users:(("docker-proxy",pid=55970,fd=8))
LISTEN 0 4096 0.0.0.0:4001 0.0.0.0:* users:(("docker-proxy",pid=56148,fd=8))
LISTEN 0 512 0.0.0.0:8911 0.0.0.0:* users:(("ascii-agents-se",pid=4601,fd=15))
LISTEN 0 512 10.77.27.114:3773 0.0.0.0:* users:(("ascii-agents-se",pid=7064,fd=7))
LISTEN 0 511 127.0.0.1:3773 0.0.0.0:* users:(("node-MainThread",pid=9828,fd=24))
LISTEN 0 511 127.0.0.1:3774 0.0.0.0:* users:(("MainThread",pid=8501,fd=22))
LISTEN 0 4096 127.0.0.1:47984 0.0.0.0:* users:(("sunshine",pid=8217,fd=25))
LISTEN 0 4096 127.0.0.1:54329 0.0.0.0:* users:(("docker-proxy",pid=55340,fd=8))
LISTEN 0 4096 *:4369 *:* users:(("epmd",pid=1154,fd=3),("systemd",pid=1,fd=198))
LISTEN 0 1024 *:8090 *:* users:(("web-server",pid=8583,fd=9))
LISTEN 0 511 *:5173 *:* users:(("node",pid=900,fd=20))
LISTEN 0 511 127.0.0.1:9000 0.0.0.0:*
---
flowfokus-portal 0.0.0.0:4001->4001/tcp, [::]:4001->4001/tcp
flowfokus-jobs 4000/tcp
flowfokus-server 0.0.0.0:4000->4000/tcp
flowfokus-db 127.0.0.1:54329->5432/tcp
"#;
        let scan = parse_scan(out);
        let found: Vec<(u16, Option<&str>)> = scan.iter().map(|(p, n)| (*p, n.as_deref())).collect();
        assert_eq!(
            found,
            vec![
                (4000, Some("flowfokus-server")),
                (4001, Some("flowfokus-portal")),
                (5173, Some("node")),
                (9000, None),
                (54329, Some("flowfokus-db")),
            ]
        );
    }
}
