//! Reaching the server's lane sockets through a proxy: an HTTP CONNECT stream, or a SOCKS5
//! UDP association (RFC 1928) whose datagrams go to the same destination.

use super::description::{Endpoint, Host};
use crate::lanes::wire::DESTINATION;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UdpSocket};

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
    /// The SOCKS5 proxy refused: the step and its reply code.
    Socks { step: &'static str, code: u8 },
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProxyError::Io(e) => write!(f, "the proxy: {e}"),
            ProxyError::Refused(status) => write!(f, "the proxy refused the lane: {status}"),
            ProxyError::Answer => write!(f, "the proxy's answer to CONNECT is not HTTP"),
            ProxyError::Socks { step, code } => {
                write!(f, "the SOCKS5 proxy refused the {step} (reply {code})")
            }
        }
    }
}

impl From<std::io::Error> for ProxyError {
    fn from(e: std::io::Error) -> Self {
        ProxyError::Io(e)
    }
}

async fn reach(proxy: &Endpoint) -> std::io::Result<TcpStream> {
    let tcp = match &proxy.host {
        Host::V4(ip) => TcpStream::connect((*ip, proxy.port.get())).await?,
        Host::V6(ip) => TcpStream::connect((*ip, proxy.port.get())).await?,
        Host::Name(name) => TcpStream::connect((name.as_str(), proxy.port.get())).await?,
    };
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

/// A stream to [`DESTINATION`] through `proxy`'s HTTP CONNECT. What the proxy sent past
/// its answer's head stays in the reader, as the stream's first bytes.
pub async fn connect(proxy: &Endpoint) -> Result<BufReader<TcpStream>, ProxyError> {
    let mut tcp = reach(proxy).await?;
    tcp.write_all(
        format!("CONNECT {DESTINATION} HTTP/1.1\r\nHost: {DESTINATION}\r\n\r\n").as_bytes(),
    )
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

/// A SOCKS5 UDP association: datagrams to and from [`DESTINATION`]. It lasts as long as
/// the TCP connection that asked for it (RFC 1928 §7), which `control` holds; a read of
/// `control` ending is the association ending.
pub struct Association {
    pub control: TcpStream,
    /// Connected to the proxy's relay address, so the kernel drops datagrams from anyone
    /// else.
    pub socket: UdpSocket,
}

/// The SOCKS5 header a datagram to [`DESTINATION`] carries: RSV, FRAG 0, ATYP 3 (a domain
/// name), its length, the name, the port.
fn header() -> Vec<u8> {
    let (name, port) = DESTINATION
        .rsplit_once(':')
        .expect("the lanes' destination is host:port");
    let port: u16 = port
        .parse()
        .expect("the lanes' destination's port is a port");
    let mut h = vec![0, 0, 0, 3, name.len() as u8];
    h.extend_from_slice(name.as_bytes());
    h.extend_from_slice(&port.to_be_bytes());
    h
}

/// The longest SOCKS5 UDP header: RSV, FRAG, ATYP, a 255-byte name with its length, the
/// port (RFC 1928 §7). Derived.
pub const UDP_HEADER_MOST: usize = 2 + 1 + 1 + 1 + 255 + 2;

/// The payload of a datagram from the proxy, without its SOCKS5 header; `None` for one
/// that is not a whole unfragmented SOCKS5 datagram (RFC 1928 §7: a datagram with FRAG
/// other than 0 is dropped by an implementation that does not reassemble).
pub fn payload(d: &[u8]) -> Option<&[u8]> {
    let [0, 0, 0, atyp, rest @ ..] = d else {
        return None;
    };
    let addr = match atyp {
        1 => 4,
        4 => 16,
        3 => 1 + usize::from(*rest.first()?),
        _ => return None,
    };
    rest.get(addr + 2..)
}

impl Association {
    /// Asks `proxy` for an association.
    pub async fn open(proxy: &Endpoint) -> Result<Association, ProxyError> {
        let mut control = reach(proxy).await?;
        // No authentication: the proxy is the user's own, on their machine.
        control.write_all(&[5, 1, 0]).await?;
        let mut choice = [0u8; 2];
        control.read_exact(&mut choice).await?;
        if choice != [5, 0] {
            return Err(ProxyError::Socks {
                step: "greeting",
                code: choice[1],
            });
        }
        // The address datagrams come from is not known yet: zeros (RFC 1928 §6).
        let peer = control.peer_addr()?;
        let unspecified = match peer.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        control.write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        let mut reply = [0u8; 4];
        control.read_exact(&mut reply).await?;
        if reply[0] != 5 || reply[1] != 0 {
            return Err(ProxyError::Socks {
                step: "UDP ASSOCIATE",
                code: reply[1],
            });
        }
        let ip = match reply[3] {
            1 => {
                let mut a = [0u8; 4];
                control.read_exact(&mut a).await?;
                IpAddr::from(a)
            }
            4 => {
                let mut a = [0u8; 16];
                control.read_exact(&mut a).await?;
                IpAddr::from(a)
            }
            atyp => {
                return Err(ProxyError::Socks {
                    step: "UDP ASSOCIATE's address type",
                    code: atyp,
                })
            }
        };
        let mut port = [0u8; 2];
        control.read_exact(&mut port).await?;
        // An unspecified relay address is the proxy's own (common practice).
        let ip = match ip.is_unspecified() {
            true => peer.ip(),
            false => ip,
        };
        let socket = UdpSocket::bind(SocketAddr::new(unspecified, 0)).await?;
        socket
            .connect(SocketAddr::new(ip, u16::from_be_bytes(port)))
            .await?;
        Ok(Association { control, socket })
    }

    /// Waits on the association's TCP connection, on which the proxy says nothing: its end
    /// (or anything else on it) is the association's end.
    pub async fn control_read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.control.readable().await?;
            match self.control.try_read(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                r => return r,
            }
        }
    }

    /// Sends `payload` to [`DESTINATION`].
    pub async fn send(&self, payload: &[u8]) -> std::io::Result<()> {
        let mut d = header();
        d.extend_from_slice(payload);
        self.socket.send(&d).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A CONNECT asks the proxy for the lanes' destination, takes a `200` answer with any
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
            let got = connect(&endpoint).await;
            let asked = proxy.await.unwrap();
            assert_eq!(
                asked,
                format!("CONNECT {DESTINATION} HTTP/1.1\r\nHost: {DESTINATION}\r\n\r\n").as_bytes()
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

    /// A datagram's payload is what follows its header, for every address type and every
    /// length of name, and nothing is read from a datagram cut anywhere inside its header,
    /// fragmented, or of an address type RFC 1928 does not have.
    #[test]
    fn a_datagrams_payload_follows_its_header() {
        let mut rng = Xorshift::new(0xD1B54A32D192ED03);
        for _ in 0..50_000 {
            let addr: Vec<u8> = match rng.below(3) {
                0 => vec![1, 127, 0, 0, 1],
                1 => [vec![4], vec![0; 15], vec![1]].concat(),
                _ => {
                    let n = rng.below(256) as u8;
                    [vec![3, n], vec![b'a'; usize::from(n)]].concat()
                }
            };
            let body: Vec<u8> = (0..rng.below(64)).map(|_| rng.next() as u8).collect();
            let d = [vec![0, 0, 0], addr.clone(), vec![0x1a, 0xf5], body.clone()].concat();
            assert_eq!(payload(&d), Some(body.as_slice()));
            let head = 3 + addr.len() + 2;
            assert!(head <= UDP_HEADER_MOST);
            let cut = rng.below(head as u64) as usize;
            assert_eq!(payload(&d[..cut]), None, "cut at {cut} of {head}");
            let mut frag = d.clone();
            frag[2] = 1 + rng.below(255) as u8;
            assert_eq!(payload(&frag), None);
            let mut atyp = d.clone();
            atyp[3] = [0, 2, 5, 255][rng.below(4) as usize];
            assert_eq!(payload(&atyp), None);
        }
        assert_eq!(
            payload(&[header(), b"x".to_vec()].concat()),
            Some(&b"x"[..])
        );
    }
}
