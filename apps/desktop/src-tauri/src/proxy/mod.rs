//! The localhost server T3 Code knows the machine by. One per machine, on a fixed port.

pub mod assets;
pub mod auth;
pub mod cache;
pub mod classify;
pub mod link;
pub mod media;
pub mod session;
pub mod threads;
pub mod upstream;

use crate::machine::{MachineRuntime, Power};
use anyhow::Result;
use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Form, Json, Router};
use auth::{Call, Reach, TicketUse};
use cache::Shell;
use crate::store::ClientToken;
use threads::ThreadFlags;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

type M = State<Arc<MachineRuntime>>;

/// A T3 server linking to the machine gives its descriptor 3 s on loopback. The refresh from a
/// running machine gets half of that; the last one seen answers after it.
const DESCRIPTOR_REFRESH: std::time::Duration = std::time::Duration::from_millis(1500);
/// T3's largest request: a 100 MB file into the preview browser.
const MAX_BODY: usize = 128 * 1024 * 1024;

pub async fn serve(machine: Arc<MachineRuntime>) -> Result<()> {
    let port = machine.record().local_port;
    let app = Router::new()
        .route("/.well-known/t3/environment", get(descriptor))
        .route("/oauth/token", post(oauth_token))
        .route("/api/auth/session", get(auth_session))
        .route("/api/auth/websocket-ticket/{opens}", post(ticket))
        .route("/ws", get(ws))
        .route("/api/device-hub/{*path}", any(media::stream))
        .route("/api/preview-stream/{*path}", any(media::stream))
        .route("/oauth/mcp/register", post(link::register))
        .route("/oauth/mcp/decision", post(link::decision))
        .route("/oauth/mcp/token", post(link::token))
        .route("/mcp", any(link::mcp))
        .route("/api/orchestration/shell", get(shell_snapshot))
        .route("/api/orchestration/threads/{thread}/bounded", get(thread_snapshot))
        .route("/api/voice/status", get(voice_status))
        .route("/api/observability/v1/traces", post(traces))
        .fallback(fallback)
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn_with_state(machine.clone(), local_host_only))
        .with_state(machine);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Web pages can reach 127.0.0.1 too, through DNS rebinding; only requests addressed to this
/// computer by name are T3 Code's: loopback, or its Tailscale name when one is set, which
/// Tailscale Serve passes on. The request carries which, for the auth checks.
async fn local_host_only(State(m): M, mut req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
    let forwarded = auth::is_forwarded(req.headers());
    let reach = match auth::reach(host, forwarded, tailscale_host(&m).as_deref()) {
        // Linking is for T3 servers on this computer: see `link::route`.
        Some(Reach::Tailnet) if link::route(req.uri().path()) => None,
        reach => reach,
    };
    let Some(reach) = reach else {
        return StatusCode::FORBIDDEN.into_response();
    };
    req.extensions_mut().insert(reach);
    next.run(req).await
}

/// This computer's Tailscale name, if set and valid.
fn tailscale_host(m: &MachineRuntime) -> Option<String> {
    m.shared.settings.read().unwrap().tailscale_host.as_deref().and_then(crate::store::normalize_tailscale_host)
}

/// The renderer runs at `t3code://app`; the real server allows any origin, so does the proxy.
async fn cors(req: Request, next: Next) -> Response {
    let mut res = if req.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
    };
    set(h, "access-control-allow-origin", "*");
    set(h, "access-control-allow-methods", "GET, POST, OPTIONS");
    set(
        h,
        "access-control-allow-headers",
        "authorization, b3, traceparent, content-type, dpop, x-t3-orchestration-protocol",
    );
    set(h, "access-control-max-age", "600");
    res
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The paired device making `call`, if its credentials hold.
fn client(m: &MachineRuntime, call: &Call) -> Option<ClientToken> {
    m.auth.authorize(&m.record().client_tokens, call, now_secs()).cloned()
}

fn client_authorized(m: &MachineRuntime, parts: &Parts) -> bool {
    client(m, &Call::of(parts)).is_some()
}

impl MachineRuntime {
    /// The devices paired with this machine's proxy.
    pub fn client_tokens(&self) -> Vec<ClientToken> {
        self.record().client_tokens
    }

    /// Whether `id` is still a live, unrevoked client token on this machine.
    pub fn is_client_token_active(&self, id: &str, now: i64) -> bool {
        self.with_record(|r| r.client_tokens.iter().any(|t| t.id == id && !t.expired(now)))
    }

    /// Unpairs one device: its token, tickets, and open sockets stop working at once.
    /// `false`: no token has `id`.
    pub fn revoke_client_token(&self, id: &str) -> bool {
        self.mutate_record(|r| {
            let revoked = auth::revoke(&mut r.client_tokens, id);
            if revoked {
                // Clear tickets and notify listeners while holding the record write lock so an
                // in-flight ticket request cannot insert a ticket after revocation clears the map.
                self.auth.revoke_device(id);
            }
            (revoked, revoked)
        })
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
}

fn iso_in(secs: i64) -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn iso_at(unix_secs: i64) -> String {
    chrono::DateTime::from_timestamp(unix_secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn descriptor(State(m): M) -> Response {
    if m.power() == Power::Running {
        let _ = tokio::time::timeout(DESCRIPTOR_REFRESH, m.fetch_descriptor()).await;
    }
    let cached = m.cache.lock().unwrap().descriptor.clone();
    match cached {
        Some(mut d) => {
            // The sidebar shows the machine's name, not the sandbox hostname.
            d["label"] = Value::String(m.record().name);
            Json(d).into_response()
        }
        // Transient, so T3 Code keeps retrying instead of blocking the connection.
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// Pairs a device with a one-time code. Through the tailnet the device must send a DPoP proof,
/// and its token is bound to the proof's key. A bound token also renews itself here, with a
/// proof of its key, before it expires.
async fn oauth_token(State(m): M, parts: Parts, Form(form): Form<HashMap<String, String>>) -> Response {
    let call = Call::of(&parts);
    let now = now_secs();
    let tailnet = call.reach == Reach::Tailnet;
    let proof = match call.verify_proof(now) {
        Ok(proof) => proof,
        // On loopback the proof never mattered for pairing: only a renewal needs it.
        Err(_) if !tailnet => None,
        Err(e) => return invalid_dpop_proof(&e.to_string()),
    };
    if tailnet && proof.is_none() {
        return invalid_dpop_proof("a DPoP proof is required");
    }
    let subject = form.get("subject_token").map(String::as_str).unwrap_or_default();
    let client_label: Option<String> = form.get("client_label").map(|l| l.chars().take(100).collect());
    let fresh_token = auth::random_token();

    // Try atomic renewal first under the record lock so concurrent renewals or a race with
    // revocation cannot both succeed, resurrect a revoked device, or persist on a miss.
    // Proof jti is only recorded for eligible exchanges so unpaired callers cannot grow the cache.
    let renewal_result: Result<Option<ClientToken>, &'static str> = match &proof {
        Some(p) => m.mutate_record(|r| {
            let pos = r
                .client_tokens
                .iter()
                .position(|t| t.jkt.as_deref() == Some(p.jkt.as_str()) && auth::same_secret(&t.token, subject) && !t.expired(now));
            let Some(pos) = pos else {
                return (false, Ok(None));
            };
            if !m.auth.record_jti(&p.jkt, &p.jti, now) {
                return (false, Err("replayed jti"));
            }
            let old = r.client_tokens.remove(pos);
            r.client_tokens.retain(|t| !t.expired(now));
            let issued = ClientToken::renewed(
                old.id,
                fresh_token.clone(),
                old.label,
                old.jkt,
                now,
                auth::TAILNET_TOKEN_TTL_SECS,
            );
            r.client_tokens.push(issued.clone());
            (true, Ok(Some(issued)))
        }),
        None => Ok(None),
    };

    let (issued, was_renewed) = match renewal_result {
        Err(why) => return invalid_dpop_proof(why),
        Ok(Some(issued)) => (issued, true),
        Ok(None) => {
            if let Err(err) = m.auth.redeem_pairing_code_with_proof(subject, proof.as_ref(), now) {
                return if err == "invalid_grant" {
                    (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"}))).into_response()
                } else {
                    invalid_dpop_proof(err)
                };
            }
            let jkt = proof.filter(|_| tailnet).map(|p| p.jkt);
            let ttl = if jkt.is_some() { auth::TAILNET_TOKEN_TTL_SECS } else { auth::LOOPBACK_TOKEN_TTL_SECS };
            let issued = ClientToken::new(fresh_token, client_label, jkt, now, ttl);
            m.update_record(|r| {
                r.client_tokens.retain(|t| !t.expired(now));
                r.client_tokens.push(issued.clone());
            });
            (issued, false)
        }
    };

    let token = issued.token.clone();
    let token_type = if issued.jkt.is_some() { "DPoP" } else { "Bearer" };
    let ttl = if issued.jkt.is_some() { auth::TAILNET_TOKEN_TTL_SECS } else { auth::LOOPBACK_TOKEN_TTL_SECS };
    let session = m.cache.lock().unwrap().auth_session.clone().unwrap_or_default();
    let scope = session
        .get("scopes")
        .and_then(Value::as_array)
        .map(|s| s.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let mut body = json!({
        "access_token": token,
        "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
        "token_type": token_type,
        "expires_in": ttl,
        "scope": scope,
    });
    if let Some(p) = session.get("permissions") {
        body["permissions"] = p.clone();
    }
    if was_renewed {
        tracing::info!("T3 Code renewed its token for {}", m.id());
    } else {
        tracing::info!("T3 Code paired with {} ({call_reach:?})", m.id(), call_reach = call.reach);
    }
    Json(body).into_response()
}

fn invalid_dpop_proof(why: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_dpop_proof", "error_description": why}))).into_response()
}

async fn auth_session(State(m): M, parts: Parts) -> Response {
    let Some(token) = client(&m, &Call::of(&parts)) else {
        return unauthorized();
    };
    let cached = m.cache.lock().unwrap().auth_session.clone();
    match cached {
        Some(mut s) => {
            let expires = token.expires_at.map_or_else(|| iso_in(auth::LOOPBACK_TOKEN_TTL_SECS), iso_at);
            s.insert("expiresAt".into(), Value::String(expires));
            Json(s).into_response()
        }
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// The proxy's own tickets, for the RPC socket and for media streams alike: asking for one
/// never reaches the machine.
async fn ticket(State(m): M, Path(opens): Path<TicketUse>, parts: Parts) -> Response {
    let call = Call::of(&parts);
    let Some(token) = client(&m, &call) else {
        return unauthorized();
    };
    let now = now_secs();
    // Issue under the record read lock so revocation (under the write lock) cannot race with
    // inserting a ticket for a device that was just revoked.
    let issued = m.with_record(|r| {
        r.client_tokens
            .iter()
            .any(|t| t.id == token.id && !t.expired(now))
            .then(|| m.auth.issue_ticket(opens, call.reach, &token))
    });
    let Some((ticket, ttl)) = issued else {
        return unauthorized();
    };
    Json(json!({"ticket": ticket, "expiresAt": iso_in(ttl.as_secs() as i64)})).into_response()
}

async fn ws(State(m): M, parts: Parts, upgrade: WebSocketUpgrade) -> Response {
    let call = Call::of(&parts);
    let now = now_secs();
    let query = parts.uri.query().unwrap_or_default().to_owned();
    // Subscribe before checking active state so a revocation landing during upgrade is never missed.
    let revoked = m.auth.subscribe_revoked();
    let token_id = query_param(&query, "wsTicket")
        .and_then(|t| m.auth.check_ticket(&t, TicketUse::Rpc, &call, now))
        .or_else(|| client(&m, &call).map(|t| t.id));
    let Some(token_id) = token_id else {
        return unauthorized();
    };
    if !m.is_client_token_active(&token_id, now) {
        return unauthorized();
    }
    m.shared.touch_activity();
    upgrade.on_upgrade(move |socket| session::run(m, socket, query, token_id, revoked))
}

/// The HTTP shell snapshot T3 Code loads on start: answered from the capture while asleep.
async fn shell_snapshot(State(m): M, Query(q): Query<HashMap<String, String>>, req: Request) -> Response {
    if client(&m, &Call::of_request(&req)).is_none() {
        return unauthorized();
    }
    m.shared.touch_activity();
    // Right after the app starts the power is not known yet: the capture would look stale, and
    // the machine be woken for what it holds.
    m.settled_power().await;
    if let Some(capture) = m.fresh_capture() {
        let resume_from = q.get("resumeFrom").and_then(|v| v.parse().ok());
        match capture.shell(resume_from) {
            Some(Shell::UpToDate) => return Json(json!({"resumable": true})).into_response(),
            Some(Shell::Snapshot(snapshot)) => return Json(snapshot).into_response(),
            None => {}
        }
    }
    forward_authorized(m, req, true).await
}

/// The HTTP snapshot T3 Code opens a chat with: answered from the thread store while asleep.
async fn thread_snapshot(
    State(m): M,
    Path(thread): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    if client(&m, &Call::of_request(&req)).is_none() {
        return unauthorized();
    }
    m.shared.touch_activity();
    let flags = ThreadFlags::from_query(&q);
    m.cache.lock().unwrap().see_thread_encoding(flags);
    // As for the shell: the capture counts once the power is known.
    m.settled_power().await;
    if let Some(body) = m.fresh_capture().and_then(|c| m.stored_thread(&c, &thread, flags)) {
        return ([(header::CONTENT_TYPE, "application/json")], body).into_response();
    }
    forward_authorized(m, req, true).await
}

/// The voice status T3 Code reads on its own as it starts, when this machine takes its
/// dictation. A mic click reads the same, so it is answered as last seen while asleep and wakes
/// the machine only when never seen.
async fn voice_status(State(m): M, mut req: Request) -> Response {
    if client(&m, &Call::of_request(&req)).is_none() {
        return unauthorized();
    }
    if m.settled_power().await != Power::Running
        && let Some(status) = m.cache.lock().unwrap().voice_status.clone()
    {
        return Json(status).into_response();
    }
    // The answer is read here, so it must come uncompressed.
    req.headers_mut().remove(header::ACCEPT_ENCODING);
    let answer = forward_authorized(m.clone(), req, true).await;
    if answer.status() != StatusCode::OK {
        return answer;
    }
    let (head, body) = answer.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if let Ok(status) = serde_json::from_slice(&body) {
        m.cache.lock().unwrap().voice_status = Some(status);
        m.save_cache();
    }
    Response::from_parts(head, Body::from(body))
}

/// Renderer traces: periodic, so forwarding them would keep the machine from sleeping.
async fn traces() -> Response {
    StatusCode::OK.into_response()
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

/// Routes whose URL carries its own one-time token instead of a bearer header.
const URL_TOKEN_ROUTES: &[&str] = &[assets::ROUTE, "/api/attachments/upload/"];

fn query_param(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

/// `query` without the parameters named in `drop`, the rest byte for byte.
fn query_without(query: &str, drop: &[&str]) -> String {
    query
        .split('&')
        .filter(|pair| !pair.is_empty() && !drop.contains(&pair.split('=').next().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("&")
}

fn target(uri: &Uri) -> String {
    uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_owned()
}

/// The proxy's own credential on the machine's server.
fn upstream_bearer(m: &MachineRuntime) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!("Bearer {}", m.upstream_token())).ok()
}

/// How a request goes on to the T3 server on the machine.
struct Relay {
    /// Path and query there.
    target: String,
    /// The credential it is sent with, if any: never the caller's own for the proxy.
    authorization: Option<HeaderValue>,
    /// Whether it may wake a sleeping machine. If not, a sleeping machine answers 503.
    wake: bool,
}

/// Everything without a route of its own. Its sockets (dictation, and whatever T3 adds) are
/// opened by a person, so they wake the machine.
async fn fallback(State(m): M, upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>, req: Request) -> Response {
    match upgrade {
        Ok(upgrade) => media::socket(m, upgrade, &req.into_parts().0, true).await,
        Err(_) => forward(State(m), req).await,
    }
}

/// Plain requests without a route of their own need the live machine: wake it if needed, swap in
/// the upstream token. URL-token requests without a bearer may come from any web page: they reach a
/// running machine but never wake it or keep it awake. The assets a recorded answer points to
/// are kept as they pass, and answered from what was kept while the machine does not run.
async fn forward(State(m): M, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let url_token = URL_TOKEN_ROUTES.iter().any(|r| parts.uri.path().starts_with(r));
    let bearer = parts.headers.contains_key(header::AUTHORIZATION);
    let authorized = client_authorized(&m, &parts);
    if (bearer || !url_token) && !authorized {
        return unauthorized();
    }
    forward_parts(m, parts, body, authorized).await
}

async fn forward_authorized(m: Arc<MachineRuntime>, req: Request, authorized: bool) -> Response {
    let (parts, body) = req.into_parts();
    forward_parts(m, parts, body, authorized).await
}

async fn forward_parts(m: Arc<MachineRuntime>, parts: Parts, body: Body, authorized: bool) -> Response {
    let bearer = parts.headers.contains_key(header::AUTHORIZATION);
    if authorized {
        m.shared.touch_activity();
        if parts.method == Method::POST {
            m.user_action();
        }
    }
    let asset = (parts.method == Method::GET && parts.uri.path().starts_with(assets::ROUTE))
        .then(|| assets::path(m.dir(), parts.uri.path()));
    if m.power() != Power::Running && let Some(kept) = asset.as_deref().and_then(assets::read) {
        return kept;
    }
    let route = Relay {
        target: target(&parts.uri),
        authorization: if bearer { upstream_bearer(&m) } else { None },
        wake: authorized,
    };
    let answer = relay(&m, parts, body, route).await;
    // A range of it (206) is not the asset.
    let keep = asset.filter(|file| {
        answer.status() == StatusCode::OK && assets::recorded(m.dir(), &m.cache.lock().unwrap()).contains(file)
    });
    match keep {
        Some(file) => assets::keep(file, answer),
        None => answer,
    }
}

/// Sends the request to the machine's server and streams its answer back, unbuffered.
async fn relay(m: &MachineRuntime, parts: Parts, body: Body, route: Relay) -> Response {
    if m.power() != Power::Running {
        if let Err(res) = wake(m, route.wake).await {
            return res;
        }
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let send = || {
        let url = format!("{}{}", m.upstream_url(), route.target);
        let mut rb = m.shared.http.request(parts.method.clone(), url).body(body.clone());
        for (k, v) in &parts.headers {
            // The credential is the route's, and the body's length is what is sent here.
            let replaced = k == header::AUTHORIZATION || k == header::CONTENT_LENGTH;
            if !replaced && !HOP_BY_HOP.contains(&k.as_str()) {
                rb = rb.header(k, v);
            }
        }
        if let Some(authorization) = &route.authorization {
            rb = rb.header(header::AUTHORIZATION, authorization);
        }
        rb.send()
    };
    let first = send().await;
    // Maybe never reached the machine: Boat answers for a sandbox that does not run itself (a
    // redirect to its fallback page, a gateway error). If it fell asleep since the last poll,
    // wake it and send once more.
    let unreached = match &first {
        Ok(r) => {
            let s = r.status();
            (s.is_redirection() && s != StatusCode::NOT_MODIFIED)
                || matches!(s, StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT)
        }
        Err(e) => e.is_connect(),
    };
    let res = if unreached && !matches!(m.refresh_power().await, Ok(Power::Running)) {
        if let Err(res) = wake(m, route.wake).await {
            return res;
        }
        send().await
    } else {
        first
    };
    let res = match res {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    };
    let mut out = Response::builder().status(res.status());
    for (k, v) in res.headers() {
        if !HOP_BY_HOP.contains(&k.as_str()) {
            out = out.header(k, v);
        }
    }
    out.body(Body::from_stream(res.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Wakes the machine for a request that may, and answers 503 for one that may not.
async fn wake(m: &MachineRuntime, allowed: bool) -> Result<(), Response> {
    if !allowed {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    m.ensure_awake()
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response())
}
