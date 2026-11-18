//! Reaching the server's lane sockets through a proxy: an HTTP CONNECT stream, or a SOCKS5
//! UDP association (RFC 1928) whose datagrams go to the same destination.

use super::description::{Endpoint, Host};
use crate::lanes::wire::DESTINATION;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};

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
    /// The kernel would not hold a lane's `bytes` (`SO_RCVBUF` above its ceiling).
    Buffer { bytes: u32, error: std::io::Error },
    /// The SOCKS5 proxy refused: the step and its reply code.
    Socks { step: &'static str, code: u8 },
}

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProxyError::Io(e) => write!(f, "the proxy: {e}"),
            ProxyError::Refused(status) => write!(f, "the proxy refused the lane: {status}"),
            ProxyError::Answer => write!(f, "the proxy's answer to CONNECT is not HTTP"),
            ProxyError::Buffer { bytes, error } => write!(
                f,
                "the kernel would not hold the lane's {bytes} bytes (raise its socket buffer ceiling): {error}"
            ),
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

/// What the kernel may hold of a lane the relay has not yet read (`SO_RCVBUF`), in bytes: the
/// size the daemon names (`Down::Buffer`, one deadline at the drain rate). A lane socket is
/// built only with one, set before it receives a byte (and, for a stream, before its
/// connect, which fixes TCP's window scale), so no lane is ever read through a buffer of the
/// kernel's guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buffer(u32);

impl Buffer {
    /// The size the daemon named; only the session, reading `Down::Buffer`, makes one.
    pub(in crate::relay) fn named(bytes: u32) -> Buffer {
        Buffer(bytes)
    }

    /// Sets it on the socket `fd`.
    pub fn hold(self, fd: RawFd) -> Result<(), ProxyError> {
        let refused = |error| ProxyError::Buffer {
            bytes: self.0,
            error,
        };
        let bytes = libc::c_int::try_from(self.0).map_err(|e| refused(std::io::Error::other(e)))?;
        // SAFETY: `fd` is a socket the caller holds open, and the option's value is a c_int
        // that outlives the call.
        let set = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&bytes as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        match set {
            0 => Ok(()),
            _ => Err(refused(std::io::Error::last_os_error())),
        }
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

/// A TCP connection to the proxy; for a lane, its receive buffer set before it connects.
async fn reach(proxy: &Endpoint, lane: Option<Buffer>) -> Result<TcpStream, ProxyError> {
    let addr = address(proxy).await?;
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    if let Some(buffer) = lane {
        buffer.hold(socket.as_raw_fd())?;
    }
    let tcp = socket.connect(addr).await?;
    tcp.set_nodelay(true)?;
    Ok(tcp)
}

/// Where a session's lanes go, as the server's proxy is asked for it (`--place`):
/// `localhost:<port>`, the server's own loopback at the port its proxy maps to one daemon's
/// lane sockets. The protocol's is [`DESTINATION`]; a server that runs more than one daemon
/// maps a port to each. Its text is never longer than `DESTINATION`'s: the daemon sizes a
/// lane datagram for a Hysteria 2 message that names `DESTINATION` (`wire::DATAGRAM`), so a
/// longer place would carry a full datagram past what one QUIC datagram holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    port: std::num::NonZeroU16,
}

impl Place {
    /// The protocol's place, [`DESTINATION`].
    pub fn protocol() -> Place {
        Place::parse(DESTINATION).expect("the lanes' destination is a place")
    }

    /// `localhost:<port>`, the port a decimal number from 1 without sign or leading zero,
    /// the whole no longer than [`DESTINATION`].
    pub fn parse(v: &str) -> Result<Place, String> {
        let port = v
            .strip_prefix("localhost:")
            .filter(|p| !p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|p| p.parse::<std::num::NonZeroU16>().ok())
            .filter(|_| v.len() <= DESTINATION.len())
            .ok_or_else(|| {
                format!(
                    "--place {v:?}: not localhost:<port> of at most {} characters, as {DESTINATION}",
                    DESTINATION.len()
                )
            })?;
        Ok(Place { port })
    }

    /// The SOCKS5 header a datagram to the place carries: RSV, FRAG 0, ATYP 3 (a domain
    /// name), its length, the name, the port.
    fn header(&self) -> Vec<u8> {
        let mut h = vec![0, 0, 0, 3, b"localhost".len() as u8];
        h.extend_from_slice(b"localhost");
        h.extend_from_slice(&self.port.get().to_be_bytes());
        h
    }
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "localhost:{}", self.port)
    }
}

/// A stream to `place` through `proxy`'s HTTP CONNECT. What the proxy sent past its
/// answer's head stays in the reader, as the stream's first bytes.
pub async fn connect(
    proxy: &Endpoint,
    place: &Place,
    buffer: Buffer,
) -> Result<BufReader<TcpStream>, ProxyError> {
    let mut tcp = reach(proxy, Some(buffer)).await?;
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

/// A SOCKS5 UDP association: datagrams to and from a [`Place`]. It lasts as long as
/// the TCP connection that asked for it (RFC 1928 §7), which `control` holds; a read of
/// `control` ending is the association ending.
pub struct Association {
    pub control: TcpStream,
    /// Connected to the proxy's relay address, so the kernel drops datagrams from anyone
    /// else.
    pub socket: UdpSocket,
    /// The SOCKS5 header of every datagram it sends: to its place.
    header: Vec<u8>,
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
    /// Asks `proxy` for an association whose datagrams go to `place`.
    pub async fn open(
        proxy: &Endpoint,
        place: &Place,
        buffer: Buffer,
    ) -> Result<Association, ProxyError> {
        let mut control = reach(proxy, None).await?;
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
        let socket = std::net::UdpSocket::bind(SocketAddr::new(unspecified, 0))?;
        buffer.hold(socket.as_raw_fd())?;
        socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(socket)?;
        socket
            .connect(SocketAddr::new(ip, u16::from_be_bytes(port)))
            .await?;
        Ok(Association {
            control,
            socket,
            header: place.header(),
        })
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

    /// Sends `payload` to the association's place.
    pub async fn send(&self, payload: &[u8]) -> std::io::Result<()> {
        let mut d = self.header.clone();
        d.extend_from_slice(payload);
        self.socket.send(&d).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// A place drawn as `--place` takes it: a port of 1 to 9999 drawn, the place the protocol's
    /// own way of writing a lane's port to its proxy.
    fn drawn_place(rng: &mut Xorshift) -> Place {
        Place::parse(&format!("localhost:{}", rng.range(1, 10_000))).unwrap()
    }

    /// A place is read exactly when it is `localhost:` and a port from 1 written without sign
    /// or leading zero, the whole no longer than the protocol's destination, and it is read
    /// as itself: over drawn strings of that form and near misses of it (other hosts, ports 0
    /// and past 65535, leading zeros, signs, spaces, nothing). The protocol's own is one.
    #[test]
    fn a_place_is_localhost_and_a_port_no_longer_than_the_protocols() {
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
                    .is_ok_and(|p| (1..=65_535).contains(&p) && p.to_string() == port)
                && v.len() <= DESTINATION.len();
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

    /// Every lane message goes to the place named: through a SOCKS5 proxy on 127.0.0.1 (its
    /// greeting, UDP ASSOCIATE and relay socket, as RFC 1928 has them), each datagram an
    /// association sends arrives at the proxy's relay whole, its header naming the place's
    /// host and port (ATYP 3) and its payload what was sent, over drawn places and payloads.
    /// The stream lane's CONNECT naming its place is the test below's.
    #[tokio::test]
    async fn every_lane_datagram_goes_to_the_place_named() {
        let mut rng = Xorshift::new(0x94D049BB133111EB);
        for _ in 0..40 {
            let place = drawn_place(&mut rng);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_port = listener.local_addr().unwrap().port();
            let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let relay_port = relay.local_addr().unwrap().port();
            let control = tokio::spawn(async move {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut greeting = [0u8; 3];
                tcp.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1, 0]);
                tcp.write_all(&[5, 0]).await.unwrap();
                let mut ask = [0u8; 10];
                tcp.read_exact(&mut ask).await.unwrap();
                assert_eq!(ask[..2], [5, 3]);
                let mut reply = vec![5, 0, 0, 1, 127, 0, 0, 1];
                reply.extend_from_slice(&relay_port.to_be_bytes());
                tcp.write_all(&reply).await.unwrap();
                tcp
            });
            let endpoint = Endpoint {
                host: Host::V4(Ipv4Addr::LOCALHOST),
                port: std::num::NonZeroU16::new(proxy_port).unwrap(),
            };
            let a = Association::open(&endpoint, &place, Buffer::named(64 << 10))
                .await
                .unwrap();
            let _held = control.await.unwrap();
            for _ in 0..1 + rng.below(8) {
                let sent: Vec<u8> = (0..rng.below(1200)).map(|_| rng.next() as u8).collect();
                a.send(&sent).await.unwrap();
                let mut got = vec![0u8; UDP_HEADER_MOST + 1200];
                let n = relay.recv(&mut got).await.unwrap();
                let got = &got[..n];
                let name = b"localhost";
                assert_eq!(got[..5], [0, 0, 0, 3, name.len() as u8], "{place}");
                assert_eq!(&got[5..5 + name.len()], name);
                let port = u16::from_be_bytes([got[5 + name.len()], got[6 + name.len()]]);
                assert_eq!(format!("localhost:{port}"), place.to_string());
                assert_eq!(payload(got), Some(sent.as_slice()));
            }
        }
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
            let got = connect(&endpoint, &place, Buffer::named(64 << 10)).await;
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

    /// What the kernel holds of a lane socket is at least what the daemon named: over named
    /// sizes from a page to a megabyte, a UDP and a TCP socket built with one report, read
    /// back from the kernel (`getsockopt`), no less (Linux doubles what it is given; macOS
    /// keeps it). The oracle is the kernel.
    #[test]
    fn a_lane_socket_holds_what_the_daemon_named() {
        let mut rng = Xorshift::new(0x510E527FADE682D1);
        let held = |fd: RawFd| {
            let mut v: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: `fd` is a socket this test holds open; `v` and `len` outlive the call.
            let got = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&mut v as *mut libc::c_int).cast(),
                    &mut len,
                )
            };
            assert_eq!(got, 0);
            v as u32
        };
        for _ in 0..200 {
            let bytes = rng.range(4096, 1 << 20) as u32;
            let b = Buffer::named(bytes);
            let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            b.hold(udp.as_raw_fd()).unwrap();
            assert!(held(udp.as_raw_fd()) >= bytes, "UDP {bytes}");
            let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            b.hold(tcp.as_raw_fd()).unwrap();
            assert!(held(tcp.as_raw_fd()) >= bytes, "TCP {bytes}");
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
            payload(&[Place::protocol().header(), b"x".to_vec()].concat()),
            Some(&b"x"[..])
        );
    }
}
