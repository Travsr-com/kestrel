//! The two checks in front of every route: `Host` and `Origin` must name the bound address, and
//! `/api` and `/ws` need the token.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use kestrel_types::WS_TOKEN_PREFIX;

use crate::AppState;
use crate::api::error;

/// Refuses (403) a request whose `Host` does not name the bound address, or whose `Origin`
/// (sent by browsers) is not that same host. This stops a page on another site, or a DNS name
/// rebound to 127.0.0.1, from calling the server.
pub(crate) async fn origin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| allowed_host(h, state.config.bind));
    let Some(host) = host else {
        return error(StatusCode::FORBIDDEN, "host not allowed");
    };
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let same = origin.to_str().is_ok_and(|o| o == format!("http://{host}"));
        if !same {
            return error(StatusCode::FORBIDDEN, "origin not allowed");
        }
    }
    next.run(req).await
}

/// The name and port of a `Host` value. Browsers leave out the port for 80.
fn split_host(host: &str) -> Option<(&str, u16)> {
    let (name, port) = match host.strip_prefix('[') {
        Some(rest) => {
            let (inside, after) = rest.split_once(']')?;
            (inside, after.strip_prefix(':'))
        }
        None => match host.rsplit_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        },
    };
    let port = match port {
        Some(p) => p.parse().ok()?,
        None => 80,
    };
    Some((name, port))
}

/// `host[:port]` names `bind` when the port matches and the name is: `localhost` or a loopback
/// IP for a loopback bind; the bound IP for a specific bind; any IP literal for an unspecified
/// bind (0.0.0.0 or ::). Never a DNS name other than `localhost`, so rebinding cannot pass.
fn allowed_host(host: &str, bind: SocketAddr) -> bool {
    let Some((name, port)) = split_host(host) else {
        return false;
    };
    if port != bind.port() {
        return false;
    }
    let ip = name.parse::<IpAddr>().ok();
    let ip_bind = bind.ip();
    if ip_bind.is_loopback() {
        name == "localhost" || ip.is_some_and(|ip| ip.is_loopback())
    } else if ip_bind.is_unspecified() {
        ip.is_some()
    } else {
        ip == Some(ip_bind)
    }
}

/// Refuses (401) a request without the token. HTTP routes take it as `Authorization: Bearer`.
/// Event sockets may also send it as the subprotocol `kestrel.token.<token>` or as `?token=`,
/// since browsers cannot set headers on a WebSocket.
pub(crate) async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let token = state.config.token.as_str();
    let socket = req.uri().path().starts_with("/ws/");
    let ok = bearer(req.headers()).is_some_and(|t| same(t, token))
        || (socket && subprotocol(req.headers()).is_some_and(|t| same(t, token)))
        || (socket && query(req.uri().query()).is_some_and(|t| same(t, token)));
    if !ok {
        return error(StatusCode::UNAUTHORIZED, "token required");
    }
    next.run(req).await
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

fn subprotocol(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .find_map(|p| p.trim().strip_prefix(WS_TOKEN_PREFIX))
}

fn query(query: Option<&str>) -> Option<&str> {
    query?.split('&').find_map(|kv| kv.strip_prefix("token="))
}

/// Compares in time that depends only on the length, so the token cannot be guessed byte by byte
/// from response times.
fn same(given: &str, token: &str) -> bool {
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn hosts_for_a_loopback_bind() {
        let bind = addr("127.0.0.1:7171");
        assert!(allowed_host("127.0.0.1:7171", bind));
        assert!(allowed_host("localhost:7171", bind));
        assert!(allowed_host("[::1]:7171", bind));
        assert!(!allowed_host("localhost:7172", bind));
        assert!(!allowed_host("evil.example:7171", bind));
        assert!(!allowed_host("10.0.0.5:7171", bind));
        assert!(!allowed_host("localhost", bind));
        assert!(!allowed_host("localhost:port", bind));
    }

    // Browsers leave the port out of Host and Origin for port 80.
    #[test]
    fn a_host_without_a_port_means_port_80() {
        let bind = addr("127.0.0.1:80");
        assert!(allowed_host("localhost", bind));
        assert!(allowed_host("127.0.0.1", bind));
        assert!(allowed_host("localhost:80", bind));
        assert!(!allowed_host("localhost", addr("127.0.0.1:7171")));
        assert!(!allowed_host("[::1]", addr("127.0.0.1:7171")));
        assert!(allowed_host("[::1]", bind));
    }

    #[test]
    fn hosts_for_specific_and_unspecified_binds() {
        let lan = addr("192.168.1.20:7171");
        assert!(allowed_host("192.168.1.20:7171", lan));
        assert!(!allowed_host("192.168.1.21:7171", lan));
        assert!(!allowed_host("localhost:7171", lan));
        let any = addr("0.0.0.0:7171");
        assert!(allowed_host("192.168.1.20:7171", any));
        assert!(allowed_host("[fe80::1]:7171", any));
        assert!(!allowed_host("kestrel.example:7171", any));
    }

    #[test]
    fn token_sources() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "kestrel.v1, kestrel.token.xyz".parse().unwrap(),
        );
        assert_eq!(bearer(&h), Some("abc"));
        assert_eq!(subprotocol(&h), Some("xyz"));
        assert_eq!(query(Some("after=3&token=q")), Some("q"));
        assert_eq!(query(Some("after=3")), None);
        assert_eq!(query(None), None);
        assert!(same("abc", "abc"));
        assert!(!same("abd", "abc"));
        assert!(!same("ab", "abc"));
    }
}
