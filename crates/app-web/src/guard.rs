//! Stage 15.5: the browser-facing guard on the local control surface.
//!
//! The server is unauthenticated by design (it is a loopback control surface for the person at
//! the keyboard), which makes it reachable by *any web page open in that person's browser*: a
//! page can open `ws://127.0.0.1:4480/api/watch/video` and read the video, or (via DNS rebinding,
//! where an attacker's hostname resolves to 127.0.0.1) call the API as if same-origin. Browsers
//! always attach `Host` and, for cross-site requests and every WebSocket handshake, `Origin`,
//! so two checks close both holes without adding any credential:
//!
//! * **Host** must be a loopback name/address (`localhost`, `127.0.0.0/8`, `::1`) when the server
//!   is bound to loopback. A rebinding page's `Host` is its own hostname and is refused. (When the
//!   operator deliberately binds beyond loopback, host names are not restricted - the README's
//!   existing warning applies - but the Origin check below still is.)
//! * **Origin**, when present, must name the same authority as `Host`. A cross-site page's Origin
//!   is its own site, so it is refused; the served page (same origin) is unaffected.
//!
//! A request with neither header (curl, the in-process tests, `puppeteer`'s `fetch` to itself
//! sends both) is not a browser cross-site request and is allowed.

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// How strictly `Host` is checked; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPolicy {
    /// Bound to loopback: `Host` must be a loopback name or address.
    Loopback,
    /// Bound elsewhere on purpose: any `Host`; the Origin check still applies.
    Any,
}

impl HostPolicy {
    pub fn for_bind(ip: std::net::IpAddr) -> Self {
        if ip.is_loopback() {
            Self::Loopback
        } else {
            Self::Any
        }
    }
}

/// The host part of an authority (`host`, `host:port`, `[v6]`, `[v6]:port`), lowercased.
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    authority.rsplit_once(':').map_or(authority, |(h, _)| h)
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// `Ok(())` when the request may proceed; otherwise the reason it is refused.
pub fn check(
    policy: HostPolicy,
    host: Option<&str>,
    origin: Option<&str>,
) -> Result<(), &'static str> {
    if let (HostPolicy::Loopback, Some(host)) = (policy, host) {
        if !is_loopback_host(host_of(host)) {
            return Err("unexpected Host header");
        }
    }
    if let Some(origin) = origin {
        // `Origin: null` (sandboxed frames, some redirects) and any other-authority origin
        // are refused; only the server's own authority passes.
        let authority = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"));
        match (authority, host) {
            (Some(a), Some(h)) if a.eq_ignore_ascii_case(h) => {}
            _ => return Err("cross-origin request refused"),
        }
    }
    Ok(())
}

/// The two headers as owned strings (`Err` when present but not valid text), read in their own
/// scope so no borrow of the request is held across the `.await` below.
fn headers(req: &Request<Body>) -> Result<(Option<String>, Option<String>), ()> {
    let read = |name| match req.headers().get(name) {
        None => Ok(None),
        Some(v) => v.to_str().map(|s| Some(s.to_string())).map_err(|_| ()),
    };
    Ok((read(header::HOST)?, read(header::ORIGIN)?))
}

pub async fn guard(State(policy): State<HostPolicy>, req: Request<Body>, next: Next) -> Response {
    // A present-but-unreadable header is treated as hostile rather than absent.
    let Ok((host, origin)) = headers(&req) else {
        return (StatusCode::FORBIDDEN, "malformed header").into_response();
    };
    if let Err(why) = check(policy, host.as_deref(), origin.as_deref()) {
        tracing::warn!(target: "security", "{why}");
        return (StatusCode::FORBIDDEN, why).into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: HostPolicy = HostPolicy::Loopback;

    #[test]
    fn loopback_hosts_pass_and_others_fail() {
        for h in [
            "127.0.0.1:4480",
            "localhost:4480",
            "LOCALHOST",
            "[::1]:4480",
            "127.0.0.2",
            "a.localhost:1",
        ] {
            assert_eq!(check(L, Some(h), None), Ok(()), "{h}");
        }
        for h in [
            "evil.example:4480",
            "127.0.0.1.evil.example",
            "192.168.1.5:4480",
            "[2001:db8::1]:80",
            "",
        ] {
            assert!(check(L, Some(h), None).is_err(), "{h}");
        }
        assert_eq!(check(HostPolicy::Any, Some("evil.example"), None), Ok(()));
    }

    #[test]
    fn origin_must_be_the_servers_own_authority() {
        let host = Some("127.0.0.1:4480");
        assert_eq!(check(L, host, Some("http://127.0.0.1:4480")), Ok(()));
        for o in [
            "http://evil.example",
            "http://127.0.0.1:9999",
            "null",
            "https://127.0.0.1:4480x",
            "file://",
        ] {
            assert!(check(L, host, Some(o)).is_err(), "{o}");
        }
        // The Origin check applies even when the Host is unrestricted.
        assert!(check(
            HostPolicy::Any,
            Some("10.0.0.5:4480"),
            Some("http://evil.example")
        )
        .is_err());
        // An Origin with no Host to compare against cannot be same-origin.
        assert!(check(L, None, Some("http://127.0.0.1:4480")).is_err());
    }

    #[test]
    fn requests_without_browser_headers_are_allowed() {
        assert_eq!(check(L, None, None), Ok(()));
    }
}
