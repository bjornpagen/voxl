//! Which requests the relay answers. Browsers let any web page open a WebSocket to the
//! relay or send it requests, and a page whose own domain is made to resolve to 127.0.0.1
//! (DNS rebinding) even counts as the relay's own page. So the relay only answers a `Host`
//! that can't be rebound (an IP address or localhost), and turns away what a browser sends
//! from another site's page (its `Origin`). Programs that aren't browsers send no `Origin`
//! and get through.

use axum::extract::Request;
use axum::http::{header, HeaderName, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::fmt;
use std::net::IpAddr;

/// Why a request is turned away.
#[derive(Debug, PartialEq)]
enum Refused {
    /// A host name that could be rebound.
    Host(String),
    /// Another site's page.
    Origin(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Refused::Host(name) => write!(
                f,
                "voxl doesn't answer for the host {name:?}: open it at 127.0.0.1 or localhost"
            ),
            Refused::Origin(origin) => write!(
                f,
                "voxl only takes requests from its own page, not from {origin}"
            ),
        }
    }
}

fn host_allowed(name: &str) -> bool {
    name.parse::<IpAddr>().is_ok() || name == "localhost" || name.ends_with(".localhost")
}

/// A request header as the access rule reads it.
#[derive(Clone, Copy, Debug)]
enum Header<'a> {
    Absent,
    Text(&'a str),
    /// A value that isn't text names no host or origin.
    NotText,
}

/// A host as a `Host` header or an origin writes it: its lowercase name (an IPv6 address
/// without the brackets) and its port.
struct Authority {
    name: String,
    port: Port,
}

/// A host's port as it is written: a number, or left out, which means the default port of
/// the scheme the host is reached by (RFC 6454 §4).
#[derive(Clone, Copy)]
enum Port {
    Written(u16),
    TheSchemes,
}

impl Port {
    /// The port's number, where the scheme's default port is `default`.
    fn number(self, default: u16) -> u16 {
        match self {
            Port::Written(port) => port,
            Port::TheSchemes => default,
        }
    }
}

/// The default ports of the schemes an origin of the relay's page has: HTTP's 80 and
/// HTTPS's 443 (RFC 9110 §4.2.1, §4.2.2), which WebSocket's `ws` and `wss` share
/// (RFC 6455 §3). Published.
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;

/// The schemes an origin of the relay's page has, each with its default port.
const SCHEMES: [(&str, u16); 4] = [
    ("http", HTTP_PORT),
    ("ws", HTTP_PORT),
    ("https", HTTPS_PORT),
    ("wss", HTTPS_PORT),
];

/// Whether a request with these `Host` and `Origin` headers is answered, or why it is
/// turned away.
fn admit(host: Header<'_>, origin: Header<'_>) -> Result<(), Refused> {
    let host = match host {
        // Browsers always send a Host, so a request without one comes from another kind of
        // client, which sends no Origin either; one that does is not the relay's page.
        Header::Absent => {
            return match origin {
                Header::Absent => Ok(()),
                Header::Text(o) => Err(Refused::Origin(o.to_string())),
                Header::NotText => Err(Refused::Origin("a value that is not text".into())),
            }
        }
        Header::NotText => return Err(Refused::Host("a value that is not text".into())),
        Header::Text(h) => match authority(h) {
            Some(a) if host_allowed(&a.name) => a,
            Some(a) => return Err(Refused::Host(a.name)),
            None => return Err(Refused::Host(h.to_string())),
        },
    };
    let origin = match origin {
        Header::Absent => return Ok(()),
        Header::NotText => return Err(Refused::Origin("a value that is not text".into())),
        Header::Text(o) => o.trim(),
    };
    // The relay's own page: the origin the browser loaded it from is the host it asks for,
    // each port the one written or the origin's scheme's.
    match split_origin(&origin.to_ascii_lowercase()) {
        Some((o, default))
            if o.name == host.name && o.port.number(default) == host.port.number(default) =>
        {
            Ok(())
        }
        Some(_) | None => Err(Refused::Origin(origin.to_string())),
    }
}

/// Splits a `Host` value into its name and port; `None` for one whose port is not a port.
fn authority(v: &str) -> Option<Authority> {
    let v = v.trim();
    let (name, port) = match v.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((ip, after)) => (ip, after.strip_prefix(':')),
            None => (rest, None),
        },
        None => match v.rsplit_once(':') {
            Some((n, p)) if !n.contains(':') => (n, Some(p)),
            Some(_) | None => (v, None),
        },
    };
    let port = match port.map(str::parse) {
        None => Port::TheSchemes,
        Some(Ok(port)) => Port::Written(port),
        Some(Err(_)) => return None,
    };
    Some(Authority {
        name: name.trim_end_matches('.').to_ascii_lowercase(),
        port,
    })
}

/// Splits an origin into its host and its scheme's default port; `None` for one that isn't
/// http(s) or ws(s), such as the "null" of a sandboxed frame, or whose port is not a port.
fn split_origin(v: &str) -> Option<(Authority, u16)> {
    let (scheme, rest) = v.split_once("://")?;
    let (_, default) = SCHEMES.iter().find(|(known, _)| *known == scheme)?;
    Some((authority(rest)?, *default))
}

fn value(req: &Request, name: HeaderName) -> Header<'_> {
    match req.headers().get(name) {
        None => Header::Absent,
        Some(v) => v.to_str().map_or(Header::NotText, Header::Text),
    }
}

/// Middleware for every route: answers 403 instead of running the handler for a request
/// [`admit`] turns away.
pub async fn check(req: Request, next: Next) -> Response {
    // HTTP/2 carries the host as the request's authority, not a Host header.
    let host = match (value(&req, header::HOST), req.uri().authority()) {
        (Header::Absent, Some(a)) => Header::Text(a.as_str()),
        (host @ Header::Absent, None) => host,
        (host @ (Header::Text(_) | Header::NotText), Some(_) | None) => host,
    };
    if let Err(why) = admit(host, value(&req, header::ORIGIN)) {
        log::warn!("refused {} {}: {why}", req.method(), req.uri().path());
        return (StatusCode::FORBIDDEN, why.to_string()).into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A host name as drawn: what it is written as, and whether it can be rebound.
    fn draw_name(rng: &mut Xorshift) -> (String, bool) {
        match rng.below(6) {
            0 => (
                std::net::Ipv4Addr::from(rng.next() as u32).to_string(),
                false,
            ),
            1 => (
                format!(
                    "[{}]",
                    std::net::Ipv6Addr::from(u128::from(rng.next()) << rng.below(64))
                ),
                false,
            ),
            2 => (
                ["localhost", "LocalHost", "localhost."][rng.below(3) as usize].into(),
                false,
            ),
            3 => (format!("app{}.localhost", rng.below(100)), false),
            4 => (format!("rebound{}.example", rng.below(100)), true),
            // Looks like an address, is a name.
            _ => (
                format!("{}.{}.example", rng.below(256), rng.below(256)),
                true,
            ),
        }
    }

    /// Over drawn `Host` and `Origin` headers, each absent, not text, or text: a request is
    /// answered exactly when it carries no Host and no Origin (a client that is no
    /// browser), or a Host that cannot be rebound (an IP address or localhost) with a port
    /// that is a port, and no Origin or its own page's: http(s) or ws(s), the same name, and
    /// the same port once a missing one is the scheme's default. The oracle is the
    /// drawing's own record of what it drew.
    #[test]
    fn only_the_relays_own_page_on_an_unrebindable_host_is_answered() {
        let mut rng = Xorshift::new(0x9E3779B97F4A7C15);
        for _ in 0..100_000 {
            let (name, rebindable) = draw_name(&mut rng);
            let port: Option<u16> = match rng.below(3) {
                0 => None,
                1 => Some(6901),
                _ => Some(rng.below(65536) as u16),
            };
            // A Host as a browser writes it, or one whose port is not a port.
            let (host, port_ok) = match (rng.below(8), port) {
                (0, _) => (format!("{name}:x"), false),
                (_, Some(p)) => (format!("{name}:{p}"), true),
                (_, None) => (name.clone(), true),
            };
            let schemes = [("http", 80), ("ws", 80), ("https", 443), ("wss", 443)];
            let (scheme, default) = schemes[rng.below(4) as usize];
            let (origin, own) = match rng.below(7) {
                0 => (None, true),
                // The page itself, with its port written out or left to the scheme.
                1 => {
                    let p = port.unwrap_or(default);
                    match (port.is_none() || p == default) && rng.below(2) == 0 {
                        true => (Some(format!("{scheme}://{name}")), true),
                        false => (Some(format!("{scheme}://{name}:{p}")), true),
                    }
                }
                2 => (
                    Some(format!(
                        "{scheme}://{name}:{}",
                        port.unwrap_or(default).wrapping_add(1)
                    )),
                    false,
                ),
                3 => (Some(format!("{scheme}://other.example")), false),
                4 => (Some("null".to_string()), false),
                5 => (Some(format!("file://{name}")), false),
                _ => (Some("not text".to_string()), false),
            };
            let origin_header = match origin.as_deref() {
                None => Header::Absent,
                Some("not text") => Header::NotText,
                Some(o) => Header::Text(o),
            };
            let (host_header, answered_if) = match rng.below(10) {
                0 => (Header::Absent, origin.is_none()),
                1 => (Header::NotText, false),
                _ => (Header::Text(&host), port_ok && !rebindable && own),
            };
            let answered = admit(host_header, origin_header).is_ok();
            assert_eq!(
                answered, answered_if,
                "Host {host_header:?} Origin {origin_header:?}"
            );
        }
    }
}
