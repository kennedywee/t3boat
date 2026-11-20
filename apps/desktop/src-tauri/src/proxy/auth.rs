//! The proxy is its own auth server towards T3 Code: it pairs the desktop app with a one-time
//! code, issues it a bearer token and short-lived tickets for its sockets and streams. Upstream
//! it always uses its own token, so pairing and connecting work while the machine sleeps.

use base64::Engine;
use rand::RngCore;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PAIRING_TTL: Duration = Duration::from_secs(15 * 60);
const TICKET_TTL: Duration = Duration::from_secs(5 * 60);

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

#[derive(Default)]
pub struct Auth {
    pairing: Mutex<Option<(String, Instant)>>,
    tickets: Mutex<HashMap<String, (TicketUse, Instant)>>,
}

impl Auth {
    pub fn new_pairing_code(&self) -> String {
        let code = random_token();
        *self.pairing.lock().unwrap() = Some((code.clone(), Instant::now() + PAIRING_TTL));
        code
    }

    /// One-time: a code works once.
    pub fn redeem_pairing_code(&self, code: &str) -> bool {
        let mut slot = self.pairing.lock().unwrap();
        match slot.as_ref() {
            Some((c, exp)) if c == code && Instant::now() < *exp => {
                *slot = None;
                true
            }
            _ => false,
        }
    }

    pub fn issue_ticket(&self, opens: TicketUse) -> (String, Duration) {
        let ticket = random_token();
        let mut tickets = self.tickets.lock().unwrap();
        let now = Instant::now();
        tickets.retain(|_, (_, exp)| *exp > now);
        tickets.insert(ticket.clone(), (opens, now + TICKET_TTL));
        (ticket, TICKET_TTL)
    }

    /// A ticket opens only what it was issued for. An RPC ticket opens one socket; a media
    /// ticket serves every request of a stream until it expires, as T3's own does.
    pub fn check_ticket(&self, ticket: &str, opens: TicketUse) -> bool {
        let mut tickets = self.tickets.lock().unwrap();
        let valid = tickets.get(ticket).is_some_and(|(t, exp)| *t == opens && *exp > Instant::now());
        if valid && opens == TicketUse::Rpc {
            tickets.remove(ticket);
        }
        valid
    }
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
/// `DPoP <token>` with a proof. The proof is not checked: the proxy answers loopback only,
/// where the token alone is the access, as it is under `Bearer`.
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
        let (rpc, _) = auth.issue_ticket(TicketUse::Rpc);
        assert!(!auth.check_ticket(&rpc, TicketUse::Media));
        assert!(auth.check_ticket(&rpc, TicketUse::Rpc));
        assert!(!auth.check_ticket(&rpc, TicketUse::Rpc), "one socket per RPC ticket");

        let (media, _) = auth.issue_ticket(TicketUse::Media);
        assert!(!auth.check_ticket(&media, TicketUse::Rpc));
        assert!(auth.check_ticket(&media, TicketUse::Media));
        assert!(auth.check_ticket(&media, TicketUse::Media), "a stream makes many requests");
    }
}
