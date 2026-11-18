//! The lanes description: which kind of lanes this session's transport gives, and through
//! which proxy. One line, written by whoever sets the transport up and read at every
//! session start (the transport may change between sessions):
//!
//! ```text
//! datagram <socks5 host:port>
//! streams <proxy host:port> <count>
//! connections <proxy host:port> <count>
//! ```
//!
//! A line means one thing and has one spelling: fields one space apart, the line ended by
//! a newline, numbers without sign or leading zero, an IPv6 address in brackets and in its
//! shortest form. Anything else is a [`Malformed`] the page shows; nothing is assumed.

use crate::lanes::model::Lanes;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;
use std::path::Path;

/// What a session's lanes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Description {
    /// One lane of datagrams, through a SOCKS5 proxy's UDP ASSOCIATE.
    Datagram { proxy: Endpoint },
    /// CONNECT streams through one proxy endpoint, each in order on its own: as many as the
    /// daemon asks for, up to `cap` (the streams the proxy's connection takes at once).
    Streams { proxy: Endpoint, cap: Lanes },
    /// CONNECT streams through a proxy endpoint that deals each new stream to its own
    /// connection, in turn among `lanes`: all of them, opened and reopened as one set, in
    /// order, so each lands on a connection of its own.
    Connections { proxy: Endpoint, lanes: Lanes },
}

/// A proxy's address.
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

/// Why a lanes description was not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Malformed {
    /// There is no description at the path (no file, or a link to none): the proxy client
    /// it would describe has no transport chosen. A cause of its own, never a default.
    Unchosen { path: String },
    /// The file could not be read.
    Unreadable { path: String, why: String },
    /// Not one line ended by a newline.
    NotOneLine,
    /// The first field names no kind of lanes.
    Kind(String),
    /// The kind takes another number of fields.
    Fields {
        kind: &'static str,
        want: usize,
        got: usize,
    },
    /// The proxy is not `host:port`.
    Endpoint(String),
    /// The count is not a whole number from 1 to `u16::MAX` (a lane's index is two bytes on
    /// the wire).
    Count(String),
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Malformed::Unchosen { path } => write!(
                f,
                "no transport chosen for this server (no lanes description at {path}): choose one with the proxy client's own command, then reload"
            ),
            Malformed::Unreadable { path, why } => {
                write!(f, "the lanes description {path} cannot be read: {why}")
            }
            Malformed::NotOneLine => {
                write!(f, "the lanes description is not one line ended by a newline")
            }
            Malformed::Kind(kind) => write!(
                f,
                "the lanes description names no kind of lanes: {kind:?} is not datagram, streams or connections"
            ),
            Malformed::Fields { kind, want, got } => write!(
                f,
                "the lanes description's {kind} line has {got} fields, not {want}"
            ),
            Malformed::Endpoint(v) => write!(
                f,
                "the lanes description's proxy {v:?} is not host:port (an IPv6 address in brackets, in its shortest form)"
            ),
            Malformed::Count(v) => write!(
                f,
                "the lanes description's count {v:?} is not a whole number from 1 to {}",
                u16::MAX
            ),
        }
    }
}

impl Description {
    /// Reads and parses the description in the file at `path`.
    pub fn read(path: &Path) -> Result<Description, Malformed> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Malformed::Unchosen {
                path: path.display().to_string(),
            },
            _ => Malformed::Unreadable {
                path: path.display().to_string(),
                why: e.to_string(),
            },
        })?;
        Description::parse(&text)
    }

    /// Parses a file's whole text.
    pub fn parse(text: &str) -> Result<Description, Malformed> {
        let line = text
            .strip_suffix('\n')
            .filter(|l| !l.contains('\n'))
            .ok_or(Malformed::NotOneLine)?;
        let fields: Vec<&str> = line.split(' ').collect();
        let want = |kind: &'static str, want: usize| match fields.len() == want {
            true => Ok(()),
            false => Err(Malformed::Fields {
                kind,
                want,
                got: fields.len(),
            }),
        };
        match fields[0] {
            "datagram" => {
                want("datagram", 2)?;
                Ok(Description::Datagram {
                    proxy: Endpoint::parse(fields[1])?,
                })
            }
            "streams" => {
                want("streams", 3)?;
                Ok(Description::Streams {
                    proxy: Endpoint::parse(fields[1])?,
                    cap: count(fields[2])?,
                })
            }
            "connections" => {
                want("connections", 3)?;
                Ok(Description::Connections {
                    proxy: Endpoint::parse(fields[1])?,
                    lanes: count(fields[2])?,
                })
            }
            kind => Err(Malformed::Kind(kind.to_string())),
        }
    }
}

/// The line as the file holds it, newline and all.
impl fmt::Display for Description {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Description::Datagram { proxy } => writeln!(f, "datagram {proxy}"),
            Description::Streams { proxy, cap } => writeln!(f, "streams {proxy} {}", cap.get()),
            Description::Connections { proxy, lanes } => {
                writeln!(f, "connections {proxy} {}", lanes.get())
            }
        }
    }
}

/// A whole number as written without sign or leading zero.
fn whole(v: &str) -> Option<u64> {
    let digits = !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit());
    let canonical = v == "0" || !v.starts_with('0');
    // `parse` refuses a number past u64.
    (digits && canonical).then(|| v.parse().ok())?
}

/// A count of lanes.
fn count(v: &str) -> Result<Lanes, Malformed> {
    whole(v)
        .and_then(|n| u16::try_from(n).ok())
        .and_then(Lanes::new)
        .ok_or_else(|| Malformed::Count(v.to_string()))
}

impl Endpoint {
    fn parse(v: &str) -> Result<Endpoint, Malformed> {
        let bad = || Malformed::Endpoint(v.to_string());
        let (host, port) = v.rsplit_once(':').ok_or_else(bad)?;
        let port = whole(port)
            .and_then(|p| u16::try_from(p).ok())
            .and_then(NonZeroU16::new)
            .ok_or_else(bad)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// No description at the path is `Unchosen` and nothing else: no file, and a link to a
    /// file that isn't there (a link to a transport that has no description); a path that
    /// is there and can't be read as one (a folder) is `Unreadable`; a description there is
    /// read. Real files, in a folder of the test's own.
    #[cfg(unix)]
    #[test]
    fn no_description_is_unchosen_and_nothing_else_is() {
        let dir = std::env::temp_dir().join(format!("voxl-unchosen-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("lanes")).unwrap();
        let none = dir.join("none");
        let dangling = dir.join("dangling");
        let folder = dir.join("lanes");
        let there = dir.join("lanes").join("datagram");
        std::fs::write(&there, "datagram 127.0.0.1:1\n").unwrap();
        std::os::unix::fs::symlink("lanes/gone", &dangling).unwrap();
        let unchosen = |p: &Path| Malformed::Unchosen {
            path: p.display().to_string(),
        };
        assert_eq!(Description::read(&none), Err(unchosen(&none)));
        assert_eq!(Description::read(&dangling), Err(unchosen(&dangling)));
        assert!(matches!(
            Description::read(&folder),
            Err(Malformed::Unreadable { .. })
        ));
        assert!(Description::read(&there).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
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

    fn draw(rng: &mut Xorshift) -> Description {
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
        let proxy = Endpoint {
            host,
            port: NonZeroU16::new(rng.range(1, 65536) as u16).unwrap(),
        };
        let count = Lanes::new(rng.range(1, u64::from(u16::MAX) + 1) as u16).unwrap();
        match rng.below(3) {
            0 => Description::Datagram { proxy },
            1 => Description::Streams { proxy, cap: count },
            _ => Description::Connections {
                proxy,
                lanes: count,
            },
        }
    }

    /// Every description is read back from the line it is written as.
    #[test]
    fn a_description_reads_back_from_its_line() {
        let mut rng = Xorshift::new(0x5DEECE66D);
        for _ in 0..50_000 {
            let d = draw(&mut rng);
            assert_eq!(Description::parse(&d.to_string()), Ok(d.clone()), "{d}");
        }
    }

    /// A line is read only if it is the one spelling of what it reads as: over every
    /// description's line with one byte inserted, deleted or replaced (a byte of the
    /// line's own alphabet, or one around it: a tab, a second newline, a sign, a colon,
    /// a bracket), and every line cut short, whatever reads writes back as exactly the
    /// text read. So no line is read loosely, and none is read as another.
    #[test]
    fn only_a_descriptions_one_spelling_is_read() {
        const BYTES: &[u8] = b" \t\n\r:[]+-.0123456789abcdefxyzADGSTRM";
        let mut rng = Xorshift::new(0x2545F4914F6CDD1D);
        let mut refused = 0;
        for _ in 0..100_000 {
            let line = draw(&mut rng).to_string().into_bytes();
            let at = rng.below(line.len() as u64 + 1) as usize;
            let byte = BYTES[rng.below(BYTES.len() as u64) as usize];
            let mut v = line.clone();
            match rng.below(4) {
                0 => v.insert(at, byte),
                1 if at < v.len() => {
                    v.remove(at);
                }
                2 if at < v.len() => v[at] = byte,
                _ => v.truncate(at),
            }
            let text = String::from_utf8(v).unwrap();
            match Description::parse(&text) {
                Ok(d) => assert_eq!(d.to_string(), text, "{text:?} read loosely"),
                Err(_) => refused += 1,
            }
        }
        // The oracle above holds vacuously if nothing is refused.
        assert!(refused > 50_000, "{refused}");
    }

    /// Each way a line is not a description is its own error, naming what is wrong: over
    /// every drawn description's line broken in one way (not one line; a word that is no
    /// kind; a field more or fewer; a proxy that is no `host:port` in its one spelling; a
    /// count that is no whole number from 1 to 65535 in its one spelling), the error is the
    /// one for the break the drawing made.
    #[test]
    fn a_malformed_line_says_what_is_wrong() {
        let mut rng = Xorshift::new(0xB5297A4D3F84D5B5);
        let bad_proxies = [
            "127.0.0.1:0",
            "127.0.0.1:080",
            "127.0.0.1",
            "[::0]:443",
            "[::1]",
            "-a.example:1",
            "a..example:1",
            "1.2.3.256:1",
        ];
        let bad_counts = ["0", "06", "65536", "+1", "", "x", "-1"];
        for _ in 0..20_000 {
            let d = draw(&mut rng);
            let line = d.to_string();
            let mut fields: Vec<String> = line
                .trim_end_matches('\n')
                .split(' ')
                .map(String::from)
                .collect();
            let kind: &'static str = match d {
                Description::Datagram { .. } => "datagram",
                Description::Streams { .. } => "streams",
                Description::Connections { .. } => "connections",
            };
            let want_fields = fields.len();
            let pick = |rng: &mut Xorshift, of: &[&str]| {
                of[rng.below(of.len() as u64) as usize].to_string()
            };
            let (text, want) = match rng.below(6) {
                0 => match rng.below(3) {
                    0 => (
                        line.trim_end_matches('\n').to_string(),
                        Malformed::NotOneLine,
                    ),
                    1 => (format!("{line}{line}"), Malformed::NotOneLine),
                    _ => (format!("{line}\n"), Malformed::NotOneLine),
                },
                1 => {
                    let word = pick(&mut rng, &["tunnel", "Datagram", "stream", "", "lanes"]);
                    fields[0] = word.clone();
                    (format!("{}\n", fields.join(" ")), Malformed::Kind(word))
                }
                2 => {
                    match rng.below(2) {
                        0 => fields.push("x".into()),
                        _ => drop(fields.pop()),
                    }
                    let got = fields.len();
                    let want = Malformed::Fields {
                        kind,
                        want: want_fields,
                        got,
                    };
                    (format!("{}\n", fields.join(" ")), want)
                }
                3 => {
                    let proxy = pick(&mut rng, &bad_proxies);
                    fields[1] = proxy.clone();
                    (
                        format!("{}\n", fields.join(" ")),
                        Malformed::Endpoint(proxy),
                    )
                }
                _ => match d {
                    Description::Datagram { .. } => continue,
                    _ => {
                        let count = pick(&mut rng, &bad_counts);
                        fields[2] = count.clone();
                        (format!("{}\n", fields.join(" ")), Malformed::Count(count))
                    }
                },
            };
            assert_eq!(Description::parse(&text), Err(want), "{text:?}");
        }
    }
}
