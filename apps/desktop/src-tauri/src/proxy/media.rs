//! WebSockets and streams relayed to the machine, with a media ticket the proxy issues itself in
//! the URL (an image or a WebSocket cannot send a header) or its bearer token.
//!
//! Device and browser-preview streams are live pictures of what runs on the machine, so they
//! exist only while it runs. T3 Code keeps retrying one while it is on screen, so they never wake
//! the machine: a sleeping one answers 503. Any other socket (dictation, and whatever T3 adds) is
//! opened by a person and wakes it.

use super::auth::{Call, TicketUse};
use super::{client, now_secs, query_param, query_without, relay, unauthorized, upstream, upstream_bearer, wake, Relay, M};
use crate::machine::{MachineRuntime, Power};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame as DownClose, Message as DownMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as UpClose;
use tokio_tungstenite::tungstenite::{Error as UpError, Message as UpMsg};

/// T3 Code acknowledges every preview frame with this: the one message a stream sends with
/// nobody touching it.
const FRAME_ACK: &str = r#"{"type":"ack"}"#;

/// `/api/device-hub/*` and `/api/preview-stream/*`: plain requests (snapshots, MJPEG and
/// event streams, uploads) and WebSockets alike.
pub async fn stream(
    State(m): M,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    if let Ok(upgrade) = upgrade {
        return socket(m, upgrade, &parts, false).await;
    }
    let Some((target, _)) = authorized_target(&m, &parts) else {
        return unauthorized();
    };
    // Anything but a read is someone working the device or the browser.
    if parts.method != Method::GET && parts.method != Method::HEAD {
        m.user_action();
    }
    let route = Relay { target, authorization: upstream_bearer(&m), wake: false };
    relay(&m, parts, body, route).await
}

/// Relays a WebSocket to the same path on the machine. `wakes`: a sleeping machine is woken for
/// it; otherwise it answers 503.
pub(super) async fn socket(m: Arc<MachineRuntime>, upgrade: WebSocketUpgrade, parts: &Parts, wakes: bool) -> Response {
    // Subscribe before checking active state and before awaiting upstream connect so a revocation
    // landing in that window is never missed.
    let revoked = m.auth.subscribe_revoked();
    let Some((target, token_id)) = authorized_target(&m, parts) else {
        return unauthorized();
    };
    if !m.is_client_token_active(&token_id, now_secs()) {
        return unauthorized();
    }
    if m.power() != Power::Running
        && let Err(res) = wake(&m, wakes).await
    {
        return res;
    }
    if !m.is_client_token_active(&token_id, now_secs()) {
        return unauthorized();
    }
    match upstream::connect(&m.upstream_url(), &m.upstream_token(), &target).await {
        Ok(up) => {
            if !m.is_client_token_active(&token_id, now_secs()) {
                return unauthorized();
            }
            upgrade.on_upgrade(move |down| pump(m, down, up, token_id, revoked))
        }
        // The machine's own refusal (no device hub, say) reaches T3 Code as it was.
        Err(e) => match e.downcast_ref() {
            Some(UpError::Http(res)) => res.status().into_response(),
            _ => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
        },
    }
}

/// Path and query on the machine, minus the proxy's ticket, plus the owning device's token id;
/// `None` if the caller holds neither a media ticket nor a token of this proxy.
fn authorized_target(m: &MachineRuntime, parts: &Parts) -> Option<(String, String)> {
    let call = Call::of(parts);
    let query = parts.uri.query().unwrap_or_default();
    let token_id = query_param(query, "wsTicket")
        .and_then(|t| m.auth.check_ticket(&t, TicketUse::Media, &call, now_secs()))
        .or_else(|| client(m, &call).map(|t| t.id))?;
    Some((format!("{}?{}", parts.uri.path(), query_without(query, &["wsTicket"])), token_id))
}

/// Passes frames both ways until either side ends, close code and reason included: T3 Code
/// reads them to tell a closed tab from a stream to reopen.
async fn pump(
    m: Arc<MachineRuntime>,
    down: WebSocket,
    up: upstream::Upstream,
    token_id: String,
    mut revoked: tokio::sync::broadcast::Receiver<String>,
) {
    let (mut to_client, mut from_client) = down.split();
    let (mut to_machine, mut from_machine) = up.split();
    let upward = async {
        let mut ping = tokio::time::interval(upstream::PING);
        loop {
            let msg = tokio::select! {
                msg = from_client.next() => {
                    let Some(Ok(msg)) = msg else { break };
                    // A click or a key in the stream is someone using the machine. Watching is
                    // not: an open preview must not keep it from sleeping.
                    if is_input(&msg) {
                        m.user_action();
                    }
                    machine_message(msg)
                }
                _ = ping.tick() => Some(UpMsg::Ping(Default::default())),
            };
            if let Some(msg) = msg {
                if to_machine.send(msg).await.is_err() {
                    break;
                }
            }
        }
    };
    let downward = async {
        // Pongs count: only a machine that stopped answering falls silent.
        while let Ok(Some(Ok(msg))) = tokio::time::timeout(upstream::SILENCE, from_machine.next()).await {
            if let Some(msg) = client_message(msg) {
                if to_client.send(msg).await.is_err() {
                    break;
                }
            }
        }
    };
    let on_revoke = async {
        loop {
            match revoked.recv().await {
                Ok(id) if id == token_id => break,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if !m.is_client_token_active(&token_id, now_secs()) {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    tokio::select! {
        _ = upward => {}
        _ = downward => {}
        _ = on_revoke => {}
    }
}

fn is_input(msg: &DownMsg) -> bool {
    match msg {
        DownMsg::Text(text) => text.as_str() != FRAME_ACK,
        DownMsg::Binary(_) => true,
        _ => false,
    }
}

/// Each side answers its own pings; the rest crosses over as it is.
fn machine_message(msg: DownMsg) -> Option<UpMsg> {
    Some(match msg {
        DownMsg::Text(text) => UpMsg::text(text.as_str()),
        DownMsg::Binary(bytes) => UpMsg::Binary(bytes),
        DownMsg::Close(frame) => {
            UpMsg::Close(frame.map(|f| UpClose { code: f.code.into(), reason: f.reason.as_str().into() }))
        }
        DownMsg::Ping(_) | DownMsg::Pong(_) => return None,
    })
}

fn client_message(msg: UpMsg) -> Option<DownMsg> {
    Some(match msg {
        UpMsg::Text(text) => DownMsg::text(text.as_str()),
        UpMsg::Binary(bytes) => DownMsg::Binary(bytes),
        UpMsg::Close(frame) => {
            DownMsg::Close(frame.map(|f| DownClose { code: f.code.into(), reason: f.reason.as_str().into() }))
        }
        UpMsg::Ping(_) | UpMsg::Pong(_) | UpMsg::Frame(_) => return None,
    })
}
