//! Reaching the server's lane socket through a proxy: an HTTP CONNECT stream.

use crate::wire::DESTINATION;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU16;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// The proxy's address, as `--proxy` gives it: `host:port`, one spelling for each (a number
/// without sign or leading zero, an IPv6 address in brackets and in its shortest form).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub host: Host,
    pub port: NonZeroU16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Host {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    /// A DNS name: dot-separated labels of letters, digits and hyphens, none starting or
    /// ending with a hyphen, the last not all digits.
    Name(String),
}

/// A port from 1 as written without sign or leading zero: its one spelling.
fn port(v: &str) -> Option<NonZeroU16> {
    let digits = !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    match v.parse::<NonZeroU16>() {
        Ok(port) if digits && !v.starts_with('0') => Some(port),
        Ok(_) | Err(_) => None,
    }
}

impl Endpoint {
    /// `host:port` in its one spelling, or the flag's error.
    pub fn parse(v: &str) -> Result<Endpoint, String> {
        let bad = || {
            format!(
                "--proxy {v:?}: not host:port (an IPv6 address in brackets, in its shortest form)"
            )
        };
        let (host, port) = v.rsplit_once(':').ok_or_else(bad)?;
        let port = self::port(port).ok_or_else(bad)?;
        let host = match host.strip_prefix('[') {
            Some(rest) => {
                let ip: Ipv6Addr = rest
                    .strip_suffix(']')
                    .ok_or_else(bad)?
                    .parse()
                    .map_err(|_| bad())?;
                // One spelling: the shortest form, as written back.
                (format!("[{ip}]") == host)
                    .then_some(Host::V6(ip))
                    .ok_or_else(bad)?
            }
            None => match host.parse::<Ipv4Addr>() {
                Ok(ip) => Host::V4(ip),
                Err(_) if is_name(host) => Host::Name(host.to_string()),
                Err(_) => return Err(bad()),
            },
        };
        Ok(Endpoint { host, port })
    }
}

/// The longest DNS label, and name written out, in octets: RFC 1035 §2.3.4 has 63 for a
/// label and 255 for a name on the wire, which written out is 253 (less the first label's
/// length byte and the root's). Published.
const LABEL_MOST: usize = 63;
const NAME_MOST: usize = 253;

/// A DNS name (RFC 1123 labels; RFC 3696 §2: a top-level label is not all digits).
fn is_name(v: &str) -> bool {
    let label = |l: &str| {
        (1..=LABEL_MOST).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    v.len() <= NAME_MOST
        && v.split('.').all(label)
        && !v
            .rsplit('.')
            .next()
            .is_some_and(|top| top.bytes().all(|b| b.is_ascii_digit()))
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.host {
            Host::V4(ip) => write!(f, "{ip}:{}", self.port),
            Host::V6(ip) => write!(f, "[{ip}]:{}", self.port),
            Host::Name(name) => write!(f, "{name}:{}", self.port),
        }
    }
}

/// The longest answer to a CONNECT read: Chromium's bound on a response's head
/// (`net/http/http_stream_parser.h`, `kMaxHeaderBufSize`, 256 KiB), what a browser takes
/// from any server and so from any proxy. Published.
const CONNECT_ANSWER_MOST: u64 = 256 << 10;

/// Why a proxy gave no lane.
#[derive(Debug)]
pub enum ProxyError {
    /// The proxy could not be reached, or the connection to it failed.
    Io(std::io::Error),
    /// The proxy answered a CONNECT with something other than `200`: its status line.
    Refused(String),
    /// The proxy's answer to a CONNECT was not an HTTP head within `CONNECT_ANSWER_MOST`.
    Answer,
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProxyError::Io(e) => write!(f, "the proxy: {e}"),
            ProxyError::Refused(status) => write!(f, "the proxy refused the lane: {status}"),
            ProxyError::Answer => write!(f, "the proxy's answer to CONNECT is not HTTP"),
        }
    }
}

impl From<std::io::Error> for ProxyError {
    fn from(e: std::io::Error) -> Self {
        ProxyError::Io(e)
    }
}

/// The proxy's address, resolved.
async fn address(proxy: &Endpoint) -> std::io::Result<SocketAddr> {
    let port = proxy.port.get();
    match &proxy.host {
        Host::V4(ip) => Ok(SocketAddr::new(IpAddr::V4(*ip), port)),
        Host::V6(ip) => Ok(SocketAddr::new(IpAddr::V6(*ip), port)),
        Host::Name(name) => tokio::net::lookup_host((name.as_str(), port))
            .await?
            .next()
            .ok_or_else(|| std::io::Error::other(format!("{name} has no address"))),
    }
}

/// A TCP connection to the proxy.
async fn reach(proxy: &Endpoint) -> Result<TcpStream, ProxyError> {
    let tcp = TcpStream::connect(address(proxy).await?).await?;
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

/// Where a session's lane goes, as the server's proxy is asked for it (`--place`):
/// `localhost:<port>`, the server's own loopback at the port its proxy maps to one daemon's
/// lane socket. The protocol's is [`DESTINATION`]; a server that runs more than one daemon
/// maps a port to each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    port: std::num::NonZeroU16,
}

impl Place {
    /// The protocol's place, [`DESTINATION`].
    pub fn protocol() -> Place {
        Place::parse(DESTINATION).expect("the lanes' destination is a place")
    }

    /// `localhost:<port>`, the port a decimal number from 1 without sign or leading zero.
    pub fn parse(v: &str) -> Result<Place, String> {
        let port = v
            .strip_prefix("localhost:")
            .and_then(port)
            .ok_or_else(|| format!("--place {v:?}: not localhost:<port>, as {DESTINATION}"))?;
        Ok(Place { port })
    }
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "localhost:{}", self.port)
    }
}

/// A stream to `place` through `proxy`'s HTTP CONNECT. What the proxy sent past its
/// answer's head stays in the reader, as the stream's first bytes.
pub async fn connect(proxy: &Endpoint, place: &Place) -> Result<BufReader<TcpStream>, ProxyError> {
    let mut tcp = reach(proxy).await?;
    tcp.write_all(format!("CONNECT {place} HTTP/1.1\r\nHost: {place}\r\n\r\n").as_bytes())
        .await?;
    let mut reader = BufReader::new(tcp);
    let mut head = (&mut reader).take(CONNECT_ANSWER_MOST);
    let mut line = Vec::new();
    head.read_until(b'\n', &mut line).await?;
    let status = String::from_utf8_lossy(&line).trim_end().to_string();
    // `HTTP/1.x 200 <reason>`: the code is the second field.
    if status.split(' ').nth(1) != Some("200") || !status.starts_with("HTTP/1.") {
        return Err(match line.ends_with(b"\n") {
            true => ProxyError::Refused(status),
            false => ProxyError::Answer,
        });
    }
    // The head ends at its first empty line.
    loop {
        line.clear();
        if head.read_until(b'\n', &mut line).await? == 0 || !line.ends_with(b"\n") {
            return Err(ProxyError::Answer);
        }
        if line == b"\r\n" || line == b"\n" {
            return Ok(reader);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A place drawn as `--place` takes it: a port of 1 to 65535 drawn.
    fn drawn_place(rng: &mut Xorshift) -> Place {
        Place::parse(&format!("localhost:{}", rng.range(1, 65_536))).unwrap()
    }

    /// A place is read exactly when it is `localhost:` and a port from 1 written without sign
    /// or leading zero, and it is read as itself: over drawn strings of that form and near
    /// misses of it (other hosts, ports 0 and past 65535, leading zeros, signs, spaces,
    /// nothing). The protocol's own is one.
    #[test]
    fn a_place_is_localhost_and_a_port() {
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        let mut read = 0;
        for _ in 0..50_000 {
            let host = [
                "localhost",
                "localhost",
                "localhost",
                "127.0.0.1",
                "LOCALHOST",
                "",
            ][rng.below(6) as usize];
            let port = match rng.below(8) {
                0 => format!("0{}", rng.below(1000)),
                1 => format!("+{}", rng.below(1000)),
                2 => format!("{} ", rng.below(1000)),
                3 => String::new(),
                4 => rng.below(100_000).to_string(),
                _ => rng.below(12_000).to_string(),
            };
            let v = format!("{host}:{port}");
            let canonical = host == "localhost"
                && port
                    .parse::<u32>()
                    .is_ok_and(|p| (1..=65_535).contains(&p) && p.to_string() == port);
            match Place::parse(&v) {
                Ok(p) => {
                    assert!(canonical, "{v:?} read");
                    assert_eq!(p.to_string(), v);
                    read += 1;
                }
                Err(_) => assert!(!canonical, "{v:?} refused"),
            }
        }
        assert!(read > 5_000, "{read}");
        assert_eq!(Place::protocol().to_string(), DESTINATION);
    }

    /// A CONNECT asks the proxy for the lane's place, takes a `200` answer with any
    /// headers as the stream, and keeps what the proxy sent past the answer's head as the
    /// stream's first bytes; any other status is the proxy's refusal, and an answer cut
    /// before its head ends is no answer. Against a proxy on 127.0.0.1 that answers as drawn.
    #[tokio::test]
    async fn a_connect_is_answered_by_its_head_and_keeps_what_follows() {
        let mut rng = Xorshift::new(0xBF58476D1CE4E5B9);
        for _ in 0..200 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let status = [
                "200 Connection established",
                "200 OK",
                "403 Forbidden",
                "502 Bad Gateway",
            ][rng.below(4) as usize];
            let headers: String = (0..rng.below(4))
                .map(|i| format!("X-Header-{i}: {}\r\n", rng.next()))
                .collect();
            let after: Vec<u8> = (0..rng.below(40)).map(|_| rng.next() as u8).collect();
            let cut = rng.below(4) == 0;
            let mut answer = format!("HTTP/1.1 {status}\r\n{headers}\r\n").into_bytes();
            // A cut answer ends where it is cut: the proxy closes the connection.
            match cut {
                true => answer.truncate(answer.len() - 2),
                false => answer.extend_from_slice(&after),
            }
            let proxy = tokio::spawn(async move {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut asked = Vec::new();
                while !asked.ends_with(b"\r\n\r\n") {
                    let mut b = [0u8; 1];
                    tcp.read_exact(&mut b).await.unwrap();
                    asked.push(b[0]);
                }
                tcp.write_all(&answer).await.unwrap();
                asked
            });
            let endpoint = Endpoint {
                host: Host::V4(Ipv4Addr::LOCALHOST),
                port: std::num::NonZeroU16::new(port).unwrap(),
            };
            let place = drawn_place(&mut rng);
            let got = connect(&endpoint, &place).await;
            let asked = proxy.await.unwrap();
            assert_eq!(
                asked,
                format!("CONNECT {place} HTTP/1.1\r\nHost: {place}\r\n\r\n").as_bytes()
            );
            match (got, status.starts_with("200"), cut) {
                (Ok(mut stream), true, false) => {
                    let mut first = vec![0u8; after.len()];
                    stream.read_exact(&mut first).await.unwrap();
                    assert_eq!(first, after);
                }
                (Err(ProxyError::Refused(s)), false, _) => {
                    assert_eq!(s, format!("HTTP/1.1 {status}"))
                }
                (Err(ProxyError::Answer), true, true) => {}
                (got, ok, cut) => panic!(
                    "{status} cut {cut}: {:?}",
                    got.map(drop)
                        .map_err(|e| e.to_string())
                        .err()
                        .unwrap_or_else(|| format!("ok {ok}"))
                ),
            }
        }
    }

    fn draw_name(rng: &mut Xorshift) -> String {
        const LDH: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-";
        let labels = rng.range(1, 4);
        let mut out = Vec::new();
        for _ in 0..labels {
            let len = rng.range(1, 12) as usize;
            let mut l: Vec<u8> = (0..len)
                .map(|_| LDH[rng.below(LDH.len() as u64) as usize])
                .collect();
            // A label neither starts nor ends with a hyphen, and the last is not all digits.
            l[0] = b'a' + rng.below(26) as u8;
            let last = l.len() - 1;
            if l[last] == b'-' {
                l[last] = b'z';
            }
            out.push(String::from_utf8(l).unwrap());
        }
        out.join(".")
    }

    fn draw(rng: &mut Xorshift) -> Endpoint {
        let host = match rng.below(3) {
            0 => Host::V4(Ipv4Addr::from(rng.next() as u32)),
            1 => {
                // Mostly zeros, so the shortest form's `::` is drawn often.
                let mut segs = [0u16; 8];
                for s in &mut segs {
                    if rng.below(3) == 0 {
                        *s = rng.next() as u16;
                    }
                }
                Host::V6(Ipv6Addr::from(segs))
            }
            _ => Host::Name(draw_name(rng)),
        };
        Endpoint {
            host,
            port: NonZeroU16::new(rng.range(1, 65536) as u16).unwrap(),
        }
    }

    /// Every endpoint is read back from what it is written as.
    #[test]
    fn an_endpoint_reads_back_from_its_text() {
        let mut rng = Xorshift::new(0x5DEECE66D);
        for _ in 0..50_000 {
            let e = draw(&mut rng);
            assert_eq!(Endpoint::parse(&e.to_string()), Ok(e.clone()), "{e}");
        }
    }

    /// Text is read only if it is the one spelling of what it reads as: over every endpoint's
    /// text with one byte inserted, deleted or replaced (a byte of its own alphabet, or one
    /// around it: a space, a sign, a colon, a bracket), and every text cut short, whatever
    /// reads writes back as exactly the text read. So nothing is read loosely, and nothing
    /// as another.
    #[test]
    fn only_an_endpoints_one_spelling_is_read() {
        const BYTES: &[u8] = b" \t:[]+-.0123456789abcdefxyzADGSTRM";
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        let mut refused = 0;
        for _ in 0..100_000 {
            let text = draw(&mut rng).to_string().into_bytes();
            let at = rng.below(text.len() as u64 + 1) as usize;
            let byte = BYTES[rng.below(BYTES.len() as u64) as usize];
            let mut v = text.clone();
            match rng.below(4) {
                0 => v.insert(at, byte),
                1 if at < v.len() => {
                    v.remove(at);
                }
                2 if at < v.len() => v[at] = byte,
                _ => v.truncate(at),
            }
            let text = String::from_utf8(v).unwrap();
            match Endpoint::parse(&text) {
                Ok(e) => assert_eq!(e.to_string(), text, "{text:?} read loosely"),
                Err(_) => refused += 1,
            }
        }
        // The oracle above holds vacuously if nothing is refused.
        assert!(refused > 30_000, "{refused}");
    }
}
