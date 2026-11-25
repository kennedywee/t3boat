//! The proxy is its own auth server towards T3 Code: it pairs the desktop app with a one-time
//! code, issues it a token and short-lived tickets for its sockets and streams. Upstream it
//! always uses its own token, so pairing and connecting work while the machine sleeps.
//!
//! On loopback a token alone is the access, as it always was. Through this computer's Tailscale
//! name, a token must be bound to a DPoP key (RFC 9449) when it is issued, and every use must
//! carry a fresh proof signed with that key.

use crate::store::ClientToken;
use axum::http::{HeaderMap, Method, Uri};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PAIRING_TTL: Duration = Duration::from_secs(15 * 60);
const TICKET_TTL: Duration = Duration::from_secs(5 * 60);
/// Ten years: a token of the desktop app on this computer never makes it re-pair.
pub const LOOPBACK_TOKEN_TTL_SECS: i64 = 10 * 365 * 24 * 3600;
/// Thirty days, as T3's own server: a DPoP client renews before then with the token itself.
pub const TAILNET_TOKEN_TTL_SECS: i64 = 30 * 24 * 3600;
/// How far a proof's `iat` may be from now either way, clock skew included.
const PROOF_MAX_AGE_SECS: i64 = 5 * 60;
/// Maximum number of live `(jkt, jti)` entries kept in the replay cache at once.
const MAX_SEEN_JTIS: usize = 4096;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// What a ticket opens, as T3 Code names it when it asks for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketUse {
    /// The RPC socket.
    Rpc,
    /// Device and browser-preview streams.
    Media,
}

struct Ticket {
    opens: TicketUse,
    reach: Reach,
    token_id: String,
    jkt: Option<String>,
    expires: Instant,
}

pub struct Auth {
    pairing: Mutex<Option<(String, Instant)>>,
    tickets: Mutex<HashMap<String, Ticket>>,
    /// `(jkt, jti) -> expiry (unix secs)` so a signed proof cannot be replayed in its window.
    seen_jtis: Mutex<HashMap<(String, String), i64>>,
    /// Broadcasts revoked `ClientToken::id` values so active sockets close immediately.
    revoked: tokio::sync::broadcast::Sender<String>,
}

impl Default for Auth {
    fn default() -> Self {
        let (revoked, _) = tokio::sync::broadcast::channel(32);
        Self {
            pairing: Mutex::new(None),
            tickets: Mutex::new(HashMap::new()),
            seen_jtis: Mutex::new(HashMap::new()),
            revoked,
        }
    }
}

impl Auth {
    pub fn new_pairing_code(&self) -> String {
        let code = random_token();
        *self.pairing.lock().unwrap() = Some((code.clone(), Instant::now() + PAIRING_TTL));
        code
    }

    /// One-time: a code works once. When a DPoP `proof` is supplied, its `jti` is recorded under
    /// the pairing lock *before* consuming the code so a full replay cache or replayed proof
    /// never burns the user's one-time pairing code.
    pub fn redeem_pairing_code_with_proof(
        &self,
        code: &str,
        proof: Option<&Proof>,
        now: i64,
    ) -> Result<(), &'static str> {
        let mut slot = self.pairing.lock().unwrap();
        match slot.as_ref() {
            Some((c, exp)) if c == code && Instant::now() < *exp => {
                if let Some(p) = proof
                    && !self.record_jti(&p.jkt, &p.jti, now)
                {
                    return Err("replayed jti");
                }
                *slot = None;
                Ok(())
            }
            _ => Err("invalid_grant"),
        }
    }

    pub fn issue_ticket(&self, opens: TicketUse, reach: Reach, token: &ClientToken) -> (String, Duration) {
        let ticket = random_token();
        let mut tickets = self.tickets.lock().unwrap();
        let now = Instant::now();
        tickets.retain(|_, t| t.expires > now);
        tickets.insert(
            ticket.clone(),
            Ticket {
                opens,
                reach,
                token_id: token.id.clone(),
                jkt: token.jkt.clone(),
                expires: now + TICKET_TTL,
            },
        );
        (ticket, TICKET_TTL)
    }

    /// A ticket opens only what it was issued for, on the same reach. For a DPoP-bound RPC ticket,
    /// the WebSocket upgrade must carry a valid proof of the issuing key with `ath` matching the
    /// ticket before the one-time ticket is consumed. Returns the owning device's `token_id`.
    pub fn check_ticket(&self, ticket: &str, opens: TicketUse, call: &Call, now: i64) -> Option<String> {
        let mut tickets = self.tickets.lock().unwrap();
        let entry = tickets.get(ticket)?;
        if entry.opens != opens || entry.reach != call.reach || entry.expires <= Instant::now() {
            return None;
        }
        if opens == TicketUse::Rpc
            && let Some(expected_jkt) = entry.jkt.as_deref()
        {
            let proof = call.verify_proof(now).ok()??;
            if proof.jkt != expected_jkt
                || proof.ath.as_deref() != Some(sha256_b64(ticket).as_str())
                || !self.record_jti(&proof.jkt, &proof.jti, now)
            {
                return None;
            }
        }
        let token_id = entry.token_id.clone();
        if opens == TicketUse::Rpc {
            tickets.remove(ticket);
        }
        Some(token_id)
    }

    /// Records `(jkt, jti)` until the proof's validity window passes. Returns `false` on replay
    /// or when the cache is at capacity until older entries expire.
    pub fn record_jti(&self, jkt: &str, jti: &str, now: i64) -> bool {
        let mut seen = self.seen_jtis.lock().unwrap();
        seen.retain(|_, exp| *exp > now);
        let key = (jkt.to_owned(), jti.to_owned());
        if seen.contains_key(&key) || seen.len() >= MAX_SEEN_JTIS {
            return false;
        }
        seen.insert(key, now.saturating_add(PROOF_MAX_AGE_SECS * 2));
        true
    }

    /// Subscribes to device revocations so open sockets for a revoked device close immediately.
    pub fn subscribe_revoked(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.revoked.subscribe()
    }

    /// Drops all tickets issued to `token_id` and signals active sockets to close.
    pub fn revoke_device(&self, token_id: &str) {
        self.tickets.lock().unwrap().retain(|_, t| t.token_id != token_id);
        let _ = self.revoked.send(token_id.to_owned());
    }

    /// The token presented with `call`, if one of `tokens` and valid for it: on loopback the token
    /// alone unless it is DPoP-bound; through the tailnet only a bound token, with a fresh proof
    /// of its key and matching `ath`.
    pub fn authorize<'t>(&self, tokens: &'t [ClientToken], call: &Call, now: i64) -> Option<&'t ClientToken> {
        let presented = client_token(call.headers)?;
        let token = tokens.iter().find(|t| same_secret(&t.token, presented) && !t.expired(now))?;
        if token.jkt.is_none() && call.reach == Reach::Loopback {
            return Some(token);
        }
        let jkt = token.jkt.as_deref()?;
        let dpop_scheme =
            call.headers.get(axum::http::header::AUTHORIZATION).is_some_and(|v| v.as_bytes().starts_with(b"DPoP "));
        let proof = call.verify_proof(now).ok()??;
        let expected_ath = sha256_b64(presented);
        if !(dpop_scheme && proof.jkt == jkt && proof.ath.as_deref() == Some(expected_ath.as_str())) {
            return None;
        }
        self.record_jti(&proof.jkt, &proof.jti, now).then_some(token)
    }
}

fn sha256_b64(value: &str) -> String {
    B64.encode(Sha256::digest(value.as_bytes()))
}

/// Whether the request carries reverse-proxy / Tailscale Serve forwarding headers. Forwarded
/// requests must never be treated as local loopback, even if the caller spoofed `Host: localhost`.
pub fn is_forwarded(headers: &HeaderMap) -> bool {
    const FORWARDED: &[&str] = &[
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "forwarded",
        "tailscale-user-login",
        "tailscale-user-name",
    ];
    FORWARDED.iter().any(|h| headers.contains_key(*h))
}

/// How a request reached the proxy, by the `Host` it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// `localhost`, `127.0.0.1` or `[::1]`: T3 Code on this computer.
    Loopback,
    /// This computer's Tailscale name: another device on the tailnet, through Tailscale Serve.
    Tailnet,
}

/// `Host` split in name and port. `None`: not a well-formed host.
fn split_host(host: &str) -> Option<(&str, Option<u16>)> {
    let (name, port) = match host.strip_prefix('[') {
        Some(rest) => {
            let end = rest.find(']')? + 2;
            (&host[..end], host[end..].strip_prefix(':'))
        }
        None => match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        },
    };
    let port = match port {
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => Some(p.parse().ok()?),
        Some(_) => return None,
        None => None,
    };
    (!name.is_empty() && (port.is_some() || name.len() == host.len())).then_some((name, port))
}

/// Which of the names the proxy answers to `host` (a `Host` header) is, if any. `forwarded`:
/// whether reverse-proxy / Tailscale Serve headers were present; forwarded requests never get
/// `Loopback` even if the caller spoofed `Host: localhost`. `tailnet`: the configured Tailscale
/// name, normalized. Matches the whole name only, in any case, with or without a port.
pub fn reach(host: &str, forwarded: bool, tailnet: Option<&str>) -> Option<Reach> {
    let (name, _) = split_host(host)?;
    if !forwarded && ["localhost", "127.0.0.1", "[::1]"].iter().any(|l| name.eq_ignore_ascii_case(l)) {
        Some(Reach::Loopback)
    } else if tailnet.is_some_and(|t| name.eq_ignore_ascii_case(t)) {
        Some(Reach::Tailnet)
    } else {
        None
    }
}

/// The parts of a request its credentials are checked against.
pub struct Call<'a> {
    pub method: &'a Method,
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
    /// Set by the host check on every request; absent counts as remote.
    pub reach: Reach,
}

impl<'a> Call<'a> {
    fn new(method: &'a Method, uri: &'a Uri, headers: &'a HeaderMap, extensions: &axum::http::Extensions) -> Self {
        Self { method, uri, headers, reach: extensions.get::<Reach>().copied().unwrap_or(Reach::Tailnet) }
    }

    pub fn of(parts: &'a axum::http::request::Parts) -> Self {
        Self::new(&parts.method, &parts.uri, &parts.headers, &parts.extensions)
    }

    pub fn of_request(req: &'a axum::extract::Request) -> Self {
        Self::new(req.method(), req.uri(), req.headers(), req.extensions())
    }

    /// The DPoP proof: the `dpop` header, or on a socket, which cannot send one, the `dpop`
    /// query parameter.
    fn proof(&self) -> Option<String> {
        let header = self.headers.get("dpop").and_then(|v| v.to_str().ok()).map(str::to_owned);
        header.or_else(|| {
            let query = self.uri.query()?;
            url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "dpop").map(|(_, v)| v.into_owned())
        })
    }

    /// Checks the call's proof. `Ok(None)`: it carries none.
    pub fn verify_proof(&self, now: i64) -> Result<Option<Proof>, ProofError> {
        let Some(proof) = self.proof() else { return Ok(None) };
        let host = self.headers.get(axum::http::header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
        verify_proof(&proof, &Target { method: self.method.as_str(), host, path: self.uri.path(), reach: self.reach }, now)
            .map(Some)
    }
}

/// Compares digests, so the time taken says nothing of the secret.
pub fn same_secret(a: &str, b: &str) -> bool {
    Sha256::digest(a.as_bytes()) == Sha256::digest(b.as_bytes())
}

/// Takes the token with `id` out of `tokens`, unpairing its device. `false`: none had it.
pub fn revoke(tokens: &mut Vec<ClientToken>, id: &str) -> bool {
    let before = tokens.len();
    tokens.retain(|t| t.id != id);
    tokens.len() != before
}

/// What a valid DPoP proof says.
#[derive(Debug, PartialEq, Eq)]
pub struct Proof {
    /// RFC 7638 thumbprint of the key that signed it.
    pub jkt: String,
    /// Unique proof id, used to prevent replay within the `iat` window.
    pub jti: String,
    /// Hash of the access token or ticket it was made for, if it names one.
    pub ath: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProofError(pub &'static str);

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid DPoP proof: {}", self.0)
    }
}

/// The request a proof must be made for.
pub struct Target<'a> {
    pub method: &'a str,
    /// The `Host` header.
    pub host: &'a str,
    pub path: &'a str,
    pub reach: Reach,
}

impl Target<'_> {
    /// `htu`: the request's URL without query or fragment. Tailscale Serve answers HTTPS only.
    fn matches(&self, htu: &str) -> bool {
        let Ok(url) = url::Url::parse(htu) else { return false };
        let Some((name, port)) = split_host(self.host) else { return false };
        let schemes: &[&str] = match self.reach {
            Reach::Loopback => &["http", "ws"],
            Reach::Tailnet => &["https", "wss"],
        };
        let default_port = if self.reach == Reach::Loopback { 80 } else { 443 };
        schemes.contains(&url.scheme())
            && url.host_str().is_some_and(|h| h.eq_ignore_ascii_case(name))
            && url.port_or_known_default() == Some(port.unwrap_or(default_port))
            && url.path() == self.path
    }
}

#[derive(Deserialize)]
struct ProofHeader {
    typ: String,
    alg: String,
    jwk: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
struct ProofClaims {
    htm: String,
    htu: String,
    iat: i64,
    jti: String,
    ath: Option<String>,
}

/// Verifies a DPoP proof JWT for `target` at `now` (unix seconds): its type, its ES256 or EdDSA
/// signature by the public key it carries, and that it was made just now for this request.
pub fn verify_proof(jwt: &str, target: &Target, now: i64) -> Result<Proof, ProofError> {
    let err = ProofError;
    let mut parts = jwt.split('.');
    let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(err("not a JWS"));
    };
    let decode = |part: &str| B64.decode(part).map_err(|_| err("bad base64url"));
    let header: ProofHeader = serde_json::from_slice(&decode(head)?).map_err(|_| err("bad header"))?;
    if header.typ != "dpop+jwt" {
        return Err(err("typ is not dpop+jwt"));
    }
    if header.jwk.contains_key("d") {
        return Err(err("jwk holds a private key"));
    }
    let (algorithm, key, jkt) = public_key(&header.alg, &header.jwk)?;
    let signed = &jwt[..head.len() + 1 + body.len()];
    ring::signature::UnparsedPublicKey::new(algorithm, key)
        .verify(signed.as_bytes(), &decode(sig)?)
        .map_err(|_| err("bad signature"))?;
    let claims: ProofClaims = serde_json::from_slice(&decode(body)?).map_err(|_| err("bad claims"))?;
    if claims.htm != target.method {
        return Err(err("htm does not match"));
    }
    if !target.matches(&claims.htu) {
        return Err(err("htu does not match"));
    }
    if now.abs_diff(claims.iat) > PROOF_MAX_AGE_SECS as u64 {
        return Err(err("iat is not recent"));
    }
    if claims.jti.is_empty() {
        return Err(err("no jti"));
    }
    Ok(Proof { jkt, jti: claims.jti, ath: claims.ath })
}

/// The key a JWK names, as `ring` verifies with it, and its RFC 7638 thumbprint.
fn public_key(
    alg: &str,
    jwk: &serde_json::Map<String, Value>,
) -> Result<(&'static dyn ring::signature::VerificationAlgorithm, Vec<u8>, String), ProofError> {
    let field = |k: &str, len: usize| {
        let bytes = B64.decode(jwk.get(k).and_then(Value::as_str).unwrap_or_default()).ok();
        bytes.filter(|b| b.len() == len).ok_or(ProofError("bad jwk"))
    };
    let member = |k: &str| jwk.get(k).and_then(Value::as_str).unwrap_or_default();
    let (algorithm, key, canonical): (&'static dyn ring::signature::VerificationAlgorithm, _, _) =
        match (alg, member("kty"), member("crv")) {
            ("ES256", "EC", "P-256") => {
                let (x, y) = (field("x", 32)?, field("y", 32)?);
                let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#, B64.encode(&x), B64.encode(&y));
                (&ring::signature::ECDSA_P256_SHA256_FIXED, [&[4u8][..], &x, &y].concat(), canonical)
            }
            ("EdDSA", "OKP", "Ed25519") => {
                let x = field("x", 32)?;
                let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{}"}}"#, B64.encode(&x));
                (&ring::signature::ED25519, x, canonical)
            }
            _ => return Err(ProofError("unsupported alg or key")),
        };
    Ok((algorithm, key, B64.encode(Sha256::digest(canonical.as_bytes()))))
}

pub fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// T3 Code's own token, under either scheme it sends it with. A T3 Code that pairs with a
/// server offering `accessTokenRenewal` keeps the token as a DPoP credential and sends
/// `DPoP <token>` with a proof. `authorize` decides whether the proof must be checked.
pub fn client_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("DPoP "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t3_code_sends_its_token_as_bearer_or_dpop() {
        let with = |value: &'static str| {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(axum::http::header::AUTHORIZATION, axum::http::HeaderValue::from_static(value));
            headers
        };
        assert_eq!(client_token(&with("Bearer t")), Some("t"));
        assert_eq!(client_token(&with("DPoP t")), Some("t"));
        assert_eq!(client_token(&with("Basic t")), None);
        // A link's token is a bearer token only.
        assert_eq!(bearer(&with("DPoP t")), None);
    }

    #[test]
    fn a_ticket_opens_only_what_it_was_issued_for() {
        let auth = Auth::default();
        let desktop = ClientToken::legacy("desktop".into());
        let headers = HeaderMap::new();
        let uri: Uri = "/ws".parse().unwrap();
        let get = Method::GET;
        let loopback = Call { method: &get, uri: &uri, headers: &headers, reach: Reach::Loopback };
        let tailnet = Call { method: &get, uri: &uri, headers: &headers, reach: Reach::Tailnet };

        let (rpc, _) = auth.issue_ticket(TicketUse::Rpc, Reach::Loopback, &desktop);
        assert!(auth.check_ticket(&rpc, TicketUse::Media, &loopback, NOW).is_none());
        assert!(auth.check_ticket(&rpc, TicketUse::Rpc, &tailnet, NOW).is_none(), "loopback ticket rejected on tailnet");
        assert_eq!(auth.check_ticket(&rpc, TicketUse::Rpc, &loopback, NOW).as_deref(), Some(desktop.id.as_str()));
        assert!(auth.check_ticket(&rpc, TicketUse::Rpc, &loopback, NOW).is_none(), "one socket per RPC ticket");

        let (media, _) = auth.issue_ticket(TicketUse::Media, Reach::Loopback, &desktop);
        assert!(auth.check_ticket(&media, TicketUse::Rpc, &loopback, NOW).is_none());
        assert!(auth.check_ticket(&media, TicketUse::Media, &tailnet, NOW).is_none());
        assert!(auth.check_ticket(&media, TicketUse::Media, &loopback, NOW).is_some());
        assert!(auth.check_ticket(&media, TicketUse::Media, &loopback, NOW).is_some(), "a stream makes many requests");

        // Revoking the device drops its tickets and notifies active socket listeners.
        let mut rx = auth.subscribe_revoked();
        auth.revoke_device(&desktop.id);
        assert_eq!(rx.try_recv().ok().as_deref(), Some(desktop.id.as_str()));
        assert!(auth.check_ticket(&media, TicketUse::Media, &loopback, NOW).is_none());
    }

    #[test]
    fn dpop_rpc_ticket_requires_matching_upgrade_proof_before_consuming() {
        let auth = Auth::default();
        let key = Key::es256();
        let other = Key::es256();
        let phone = ClientToken::new("bound".into(), Some("Phone".into()), Some(key.jkt()), NOW, TAILNET_TOKEN_TTL_SECS);
        let (ticket, _) = auth.issue_ticket(TicketUse::Rpc, Reach::Tailnet, &phone);
        let get = Method::GET;
        let ws_url = format!("wss://{TS}/ws");
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::HOST, TS.parse().unwrap());

        // Missing proof, wrong key, or missing/wrong ath must fail WITHOUT consuming the ticket.
        let bare_uri: Uri = format!("/ws?wsTicket={ticket}").parse().unwrap();
        assert!(auth.check_ticket(&ticket, TicketUse::Rpc, &Call { method: &get, uri: &bare_uri, headers: &headers, reach: Reach::Tailnet }, NOW).is_none());

        let no_ath = key.proof(claims("GET", &ws_url));
        let no_ath_uri: Uri = format!("/ws?wsTicket={ticket}&dpop={}", urlencoding::encode(&no_ath)).parse().unwrap();
        assert!(auth.check_ticket(&ticket, TicketUse::Rpc, &Call { method: &get, uri: &no_ath_uri, headers: &headers, reach: Reach::Tailnet }, NOW).is_none());

        let wrong_key = other.proof(serde_json::json!({"htm": "GET", "htu": ws_url, "iat": NOW, "jti": random_token(), "ath": sha256_b64(&ticket)}));
        let wrong_key_uri: Uri = format!("/ws?wsTicket={ticket}&dpop={}", urlencoding::encode(&wrong_key)).parse().unwrap();
        assert!(auth.check_ticket(&ticket, TicketUse::Rpc, &Call { method: &get, uri: &wrong_key_uri, headers: &headers, reach: Reach::Tailnet }, NOW).is_none());

        // Valid upgrade proof with matching key + ath succeeds and consumes the one-time ticket.
        let good = key.proof(serde_json::json!({"htm": "GET", "htu": ws_url, "iat": NOW, "jti": random_token(), "ath": sha256_b64(&ticket)}));
        let good_uri: Uri = format!("/ws?wsTicket={ticket}&dpop={}", urlencoding::encode(&good)).parse().unwrap();
        assert_eq!(
            auth.check_ticket(&ticket, TicketUse::Rpc, &Call { method: &get, uri: &good_uri, headers: &headers, reach: Reach::Tailnet }, NOW).as_deref(),
            Some(phone.id.as_str())
        );
        // Replaying the consumed ticket fails.
        assert!(auth.check_ticket(&ticket, TicketUse::Rpc, &Call { method: &get, uri: &good_uri, headers: &headers, reach: Reach::Tailnet }, NOW).is_none());
    }

    const TS: &str = "mybox.tail1234.ts.net";

    #[test]
    fn answers_loopback_and_the_tailscale_name_only() {
        for host in ["localhost", "localhost:4000", "127.0.0.1:4000", "[::1]", "[::1]:4000", "LOCALHOST:4000"] {
            assert_eq!(reach(host, false, Some(TS)), Some(Reach::Loopback), "{host}");
            assert_eq!(reach(host, false, None), Some(Reach::Loopback), "{host}");
            // A forwarded request (e.g. from Tailscale Serve) must never get Loopback.
            assert_eq!(reach(host, true, Some(TS)), None, "forwarded {host}");
            assert_eq!(reach(host, true, None), None, "forwarded {host}");
        }
        for host in [TS, "mybox.tail1234.ts.net:443", "MyBox.Tail1234.TS.NET", "MYBOX.TAIL1234.TS.NET:8443"] {
            assert_eq!(reach(host, false, Some(TS)), Some(Reach::Tailnet), "{host}");
            assert_eq!(reach(host, true, Some(TS)), Some(Reach::Tailnet), "forwarded {host}");
            assert_eq!(reach(host, false, None), None, "{host}: no name configured");
        }
        for host in [
            "",
            "evil.com",
            "localhost.evil.com",
            "evil.localhost",
            "127.0.0.1.nip.io",
            "foo.ts.net.evil.com",
            "mybox.tail1234.ts.net.evil.com",
            "evil-mybox.tail1234.ts.net",
            "x.mybox.tail1234.ts.net",
            "tail1234.ts.net",
            "other.tail1234.ts.net",
            "mybox.tail1234.ts.net.",
            "localhost:",
            "localhost:80x",
            "localhost:99999",
            "[::1]x",
            "[::2]:4000",
            "0.0.0.0",
        ] {
            assert_eq!(reach(host, false, Some(TS)), None, "{host}");
        }
        let mut h = HeaderMap::new();
        assert!(!is_forwarded(&h));
        h.insert("x-forwarded-for", "100.64.0.1".parse().unwrap());
        assert!(is_forwarded(&h));
    }

    /// A DPoP key of a test client.
    enum Key {
        Ec(ring::signature::EcdsaKeyPair),
        Ed(ring::signature::Ed25519KeyPair),
    }

    impl Key {
        fn es256() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let alg = &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
            let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
            Key::Ec(ring::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap())
        }

        fn ed25519() -> Self {
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
            Key::Ed(ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
        }

        fn jwk(&self) -> Value {
            use ring::signature::KeyPair;
            match self {
                Key::Ec(k) => {
                    let point = k.public_key().as_ref();
                    serde_json::json!({"kty": "EC", "crv": "P-256", "x": B64.encode(&point[1..33]), "y": B64.encode(&point[33..])})
                }
                Key::Ed(k) => serde_json::json!({"kty": "OKP", "crv": "Ed25519", "x": B64.encode(k.public_key().as_ref())}),
            }
        }

        fn jkt(&self) -> String {
            let jwk = self.jwk();
            public_key(if matches!(self, Key::Ec(_)) { "ES256" } else { "EdDSA" }, jwk.as_object().unwrap()).unwrap().2
        }

        fn proof(&self, claims: Value) -> String {
            let alg = if matches!(self, Key::Ec(_)) { "ES256" } else { "EdDSA" };
            let header = serde_json::json!({"typ": "dpop+jwt", "alg": alg, "jwk": self.jwk()});
            let signed = format!("{}.{}", B64.encode(header.to_string()), B64.encode(claims.to_string()));
            let sig = match self {
                Key::Ec(k) => k.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap().as_ref().to_vec(),
                Key::Ed(k) => k.sign(signed.as_bytes()).as_ref().to_vec(),
            };
            format!("{signed}.{}", B64.encode(sig))
        }
    }

    const NOW: i64 = 1_800_000_000;

    fn claims(htm: &str, htu: &str) -> Value {
        serde_json::json!({"htm": htm, "htu": htu, "iat": NOW, "jti": random_token()})
    }

    fn target(method: &'static str, path: &'static str) -> Target<'static> {
        Target { method, host: TS, path, reach: Reach::Tailnet }
    }

    #[test]
    fn a_proof_made_for_the_request_passes() {
        for key in [Key::es256(), Key::ed25519()] {
            let proof = key.proof(claims("POST", &format!("https://{TS}/oauth/token")));
            let ok = verify_proof(&proof, &target("POST", "/oauth/token"), NOW).unwrap();
            assert_eq!(ok.jkt, key.jkt());
            assert_eq!(ok.ath, None);
            assert!(!ok.jti.is_empty());
            // The query is not part of `htu`; clocks may disagree a little.
            let socket = key.proof(claims("GET", &format!("wss://{TS}:443/ws?x=1")));
            assert!(verify_proof(&socket, &target("GET", "/ws"), NOW + 60).is_ok());
        }
        let loopback = Target { method: "GET", host: "localhost:4000", path: "/ws", reach: Reach::Loopback };
        assert!(verify_proof(&Key::es256().proof(claims("GET", "http://localhost:4000/ws")), &loopback, NOW).is_ok());
    }

    #[test]
    fn a_proof_for_anything_else_fails() {
        let key = Key::es256();
        let at = |htm: &str, htu: &str| verify_proof(&key.proof(claims(htm, htu)), &target("POST", "/oauth/token"), NOW);
        let htu = format!("https://{TS}/oauth/token");
        assert_eq!(at("GET", &htu), Err(ProofError("htm does not match")));
        for wrong in [
            format!("https://{TS}/api/auth/session"),
            format!("http://{TS}/oauth/token"),
            format!("https://{TS}:8443/oauth/token"),
            format!("https://{TS}.evil.com/oauth/token"),
            "https://localhost/oauth/token".to_owned(),
            "not a url".to_owned(),
        ] {
            assert_eq!(at("POST", &wrong), Err(ProofError("htu does not match")), "{wrong}");
        }
        let old = serde_json::json!({"htm": "POST", "htu": htu, "iat": NOW - 3600, "jti": "j"});
        assert_eq!(verify_proof(&key.proof(old), &target("POST", "/oauth/token"), NOW), Err(ProofError("iat is not recent")));
        for extreme in [i64::MIN, i64::MAX] {
            let ex = serde_json::json!({"htm": "POST", "htu": htu, "iat": extreme, "jti": "j"});
            assert_eq!(verify_proof(&key.proof(ex), &target("POST", "/oauth/token"), NOW), Err(ProofError("iat is not recent")));
        }

        let proof = key.proof(claims("POST", &htu));
        let (signed, sig) = proof.rsplit_once('.').unwrap();
        let mut sig = B64.decode(sig).unwrap();
        sig[10] ^= 1;
        let tampered = format!("{signed}.{}", B64.encode(&sig));
        assert_eq!(verify_proof(&tampered, &target("POST", "/oauth/token"), NOW), Err(ProofError("bad signature")));
        // Claims swapped under another key's signature.
        let other = Key::es256().proof(claims("POST", &htu));
        let (head, rest) = other.split_once('.').unwrap();
        let swapped = format!("{head}.{}", proof.split_once('.').unwrap().1.split_once('.').unwrap().1);
        assert!(rest != swapped && verify_proof(&swapped, &target("POST", "/oauth/token"), NOW).is_err());

        let unsigned = format!(
            "{}.{}.",
            B64.encode(serde_json::json!({"typ": "dpop+jwt", "alg": "none", "jwk": key.jwk()}).to_string()),
            B64.encode(claims("POST", &htu).to_string())
        );
        assert_eq!(verify_proof(&unsigned, &target("POST", "/oauth/token"), NOW), Err(ProofError("unsupported alg or key")));
        assert_eq!(verify_proof("a.b", &target("POST", "/oauth/token"), NOW), Err(ProofError("not a JWS")));
    }

    fn call_headers(authorization: &str, proof: Option<&str>, host: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, authorization.parse().unwrap());
        headers.insert(axum::http::header::HOST, host.parse().unwrap());
        if let Some(proof) = proof {
            headers.insert("dpop", proof.parse().unwrap());
        }
        headers
    }

    fn check(auth: &Auth, tokens: &[ClientToken], method: &Method, uri: &str, headers: &HeaderMap, reach: Reach) -> bool {
        let uri: Uri = uri.parse().unwrap();
        auth.authorize(tokens, &Call { method, uri: &uri, headers, reach }, NOW).is_some()
    }

    #[test]
    fn the_tailnet_takes_only_bound_tokens_with_a_proof_of_their_key() {
        let auth = Auth::default();
        let key = Key::es256();
        let bound = ClientToken::new("bound".into(), Some("Phone".into()), Some(key.jkt()), NOW, TAILNET_TOKEN_TTL_SECS);
        let desktop = ClientToken::legacy("desktop".into());
        let mut tokens = vec![bound.clone(), desktop];
        let get = Method::GET;
        let url = format!("https://{TS}/api/auth/session");
        let ath = sha256_b64("bound");
        let make_proof = |htm: &str, htu: &str| {
            key.proof(serde_json::json!({"htm": htm, "htu": htu, "iat": NOW, "jti": random_token(), "ath": ath}))
        };
        let no_ath = key.proof(claims("GET", &url));
        let with_ath = make_proof("GET", &url);
        let wrong_ath = key.proof(serde_json::json!({"htm": "GET", "htu": url, "iat": NOW, "jti": random_token(), "ath": "x"}));
        let path = "/api/auth/session";
        let tailnet = |hdr: &str, proof: Option<&str>| check(&auth, &tokens, &get, path, &call_headers(hdr, proof, TS), Reach::Tailnet);

        assert!(!tailnet("DPoP bound", Some(&no_ath)), "protected requests must carry ath");
        assert!(tailnet("DPoP bound", Some(&with_ath)));
        assert!(!tailnet("DPoP bound", Some(&with_ath)), "replayed jti must be rejected");
        assert!(!tailnet("DPoP bound", Some(&wrong_ath)));
        assert!(!tailnet("DPoP bound", None), "no proof");
        assert!(!tailnet("Bearer bound", Some(&make_proof("GET", &url))), "a bound token is sent as DPoP");
        assert!(!tailnet("DPoP bound", Some(&Key::es256().proof(serde_json::json!({"htm": "GET", "htu": url, "iat": NOW, "jti": random_token(), "ath": ath})))), "another key");
        assert!(!tailnet("DPoP bound", Some(&make_proof("POST", &url))), "wrong htm");
        assert!(!tailnet("DPoP bound", Some(&make_proof("GET", &format!("https://{TS}/ws")))), "wrong htu");
        assert!(!tailnet("Bearer desktop", None), "loopback token through the tailnet");
        assert!(!tailnet("DPoP desktop", Some(&make_proof("GET", &url))), "loopback token through the tailnet");
        assert!(!tailnet("DPoP nope", Some(&make_proof("GET", &url))));

        // A socket carries its proof in the URL.
        let ws = make_proof("GET", &format!("wss://{TS}/ws"));
        let ws_uri = format!("/ws?wsTicket=t&dpop={}", urlencoding::encode(&ws));
        assert!(check(&auth, &tokens, &get, &ws_uri, &call_headers("DPoP bound", None, TS), Reach::Tailnet));

        // Loopback: the desktop's token alone, a bound token still with its proof + ath.
        let local = |hdr: &str, proof: Option<&str>| {
            check(&auth, &tokens, &get, path, &call_headers(hdr, proof, "localhost:4000"), Reach::Loopback)
        };
        assert!(local("Bearer desktop", None) && local("DPoP desktop", Some("not checked")));
        assert!(!local("DPoP bound", None));
        assert!(local("DPoP bound", Some(&make_proof("GET", "http://localhost:4000/api/auth/session"))));

        // Expired, then revoked.
        let expired = ClientToken { expires_at: Some(NOW), ..bound.clone() };
        assert!(!check(&auth, &[expired], &get, path, &call_headers("DPoP bound", Some(&make_proof("GET", &url)), TS), Reach::Tailnet));
        assert!(revoke(&mut tokens, &bound.id));
        assert!(!revoke(&mut tokens, &bound.id));
        assert!(!check(&auth, &tokens, &get, path, &call_headers("DPoP bound", Some(&make_proof("GET", &url)), TS), Reach::Tailnet));
        assert!(check(&auth, &tokens, &get, path, &call_headers("Bearer desktop", None, "localhost"), Reach::Loopback));
    }

    #[test]
    fn replay_cache_caps_entries_until_they_expire() {
        let auth = Auth::default();
        for i in 0..MAX_SEEN_JTIS {
            assert!(auth.record_jti("k", &format!("j{i}"), NOW));
        }
        assert!(!auth.record_jti("k", "overflow", NOW), "at capacity");
        // A full replay cache must not consume a valid one-time pairing code.
        let code = auth.new_pairing_code();
        let proof = Proof { jkt: "k".into(), jti: "overflow".into(), ath: None };
        assert_eq!(auth.redeem_pairing_code_with_proof(&code, Some(&proof), NOW), Err("replayed jti"));
        assert!(auth.redeem_pairing_code_with_proof(&code, Some(&proof), NOW + PROOF_MAX_AGE_SECS * 2 + 1).is_ok());
    }
}
