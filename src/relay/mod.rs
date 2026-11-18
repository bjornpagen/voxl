//! `voxl relay`: the viewer's side of a session, run on the viewer's machine. It serves the
//! page on 127.0.0.1 and, for each page that connects, runs a session: `ssh <host> voxl
//! attach` for the control stream, and one lane, through a proxy, for video and audio. It
//! hands the page every frame the lane completes, in order.
//!
//! **The lane.** One per session, of the kind the lanes description names. A datagram lane
//! (a SOCKS5 proxy's UDP relay: Hysteria2's) carries coded blocks with repair; the relay's
//! decoder completes them, decides each block at its deadline, and reports it to the daemon.
//! A stream lane (one HTTP CONNECT stream through naive over HTTP/2 or HTTP/3) carries the
//! frames as they are, in order, whole: TCP or QUIC delivers every byte, so nothing on it has
//! a deadline and the relay reports nothing. The lane's socket holds what the daemon names
//! (`Down::Buffer`: one deadline at the drain rate). A lane that ends after the daemon
//! confirmed it (`Down::Attached`) opens again as the next generation; one that ends before
//! is a transport refusing lanes and ends the session. An unconfirmed datagram attach goes
//! again after the least round trip seen (see `session::Session::round_trip`).
//!
//! **What the page shows** ([`release`]). The page decodes the screen's one VP9 stream and
//! shows each update as soon as its decoder puts it out. Every update the relay completes is
//! decodable (the daemon's encoder refers only to pictures it knows the page has), so the
//! relay gives the page every one, in order; one that missed its deadline on a datagram lane
//! is passed over, and the next refers past it.

mod access;
mod description;
mod files;
mod proxy;
mod release;
mod session;
mod ssh;

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

/// What `voxl relay` is told, each flag required and given once.
#[derive(Debug, PartialEq)]
struct Flags {
    /// `--lanes`: the lanes description, read at every session start.
    lanes: PathBuf,
    /// `--ssh`: where `voxl attach` and `voxl files` run.
    ssh: ssh::Destination,
    /// `--port`: the page's port on 127.0.0.1.
    port: u16,
}

const USAGE: &str = "usage: voxl relay --lanes FILE --ssh DESTINATION --port PORT";

impl Flags {
    /// `--name value` or `--name=value`, each of the three exactly once, and nothing else.
    fn parse(args: &[String]) -> Result<Flags, String> {
        let (mut lanes, mut ssh, mut port) = (None, None, None);
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let (name, value) = match a.split_once('=') {
                Some((n, v)) => (n, v.to_string()),
                None => (
                    a.as_str(),
                    it.next()
                        .ok_or_else(|| format!("{a} needs a value"))?
                        .clone(),
                ),
            };
            let slot = match name {
                "--lanes" => &mut lanes,
                "--ssh" => &mut ssh,
                "--port" => &mut port,
                _ => return Err(format!("{name}: not a flag of voxl relay")),
            };
            if slot.replace(value).is_some() {
                return Err(format!("{name} is given twice"));
            }
        }
        let need = |v: Option<String>, name: &str| v.ok_or_else(|| format!("{name} is required"));
        let port = need(port, "--port")?;
        Ok(Flags {
            lanes: PathBuf::from(need(lanes, "--lanes")?),
            ssh: ssh::Destination::parse(&need(ssh, "--ssh")?)?,
            port: port
                .parse()
                .ok()
                .filter(|&p| p != 0)
                .ok_or_else(|| format!("--port {port}: not a port from 1 to 65535"))?,
        })
    }
}

/// Runs the relay with these flags (after `relay`) until it is stopped.
pub fn run(args: &[String]) -> ! {
    let flags = Flags::parse(args).unwrap_or_else(|e| {
        eprintln!("voxl relay: {e}\n{USAGE}");
        std::process::exit(2)
    });
    let env = crate::env::Env::read(|name| std::env::var_os(name));
    let mut log = env_logger::Builder::new();
    log.parse_filters(&env.log.filter);
    if let Some(style) = &env.log.style {
        log.parse_write_style(style);
    }
    crate::logging::init(log.build());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for voxl relay");
    match rt.block_on(serve(flags)) {
        Ok(()) => crate::logging::exit(0),
        Err(e) => {
            log::error!("voxl relay: {e}");
            crate::logging::exit(1)
        }
    }
}

async fn serve(flags: Flags) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, flags.port)).await?;
    log::info!(
        "voxl relay: the page is at http://127.0.0.1:{}/, the desktop through ssh {}",
        flags.port,
        flags.ssh
    );
    let files = Arc::new(files::Files {
        to: flags.ssh.clone(),
        grant: parking_lot::Mutex::new(None),
    });
    let sessions = Arc::new(session::Config {
        lanes: flags.lanes,
        ssh: flags.ssh,
        files: files.clone(),
    });
    let app = router(sessions, files);
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        if let Err(e) = tcp.set_nodelay(true) {
            log::warn!("the page's connection waits to fill packets: {e}");
        }
    });
    axum::serve(listener, app).await
}

/// A file of the page, as the build put it in the binary.
fn asset<S: Clone + Send + Sync + 'static>(
    content_type: &'static str,
    bytes: &'static [u8],
) -> axum::routing::MethodRouter<S> {
    get(move || async move {
        (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response()
    })
}

/// Scripts bundled from web/src by build.rs.
macro_rules! bundled {
    ($name:literal) => {
        asset(
            "text/javascript",
            include_bytes!(concat!(env!("OUT_DIR"), "/web/", $name)),
        )
    };
}

/// The page is cross-origin isolated (COOP `same-origin`, COEP `require-corp`), which
/// is what lets its worker share counters with it (a SharedArrayBuffer); everything it
/// loads is the relay's own.
async fn isolated(mut r: axum::response::Response) -> axum::response::Response {
    let h = r.headers_mut();
    h.insert(
        "cross-origin-opener-policy",
        axum::http::HeaderValue::from_static("same-origin"),
    );
    h.insert(
        "cross-origin-embedder-policy",
        axum::http::HeaderValue::from_static("require-corp"),
    );
    r
}

fn router(sessions: Arc<session::Config>, files: Arc<files::Files>) -> Router {
    let index = asset(
        "text/html; charset=utf-8",
        include_bytes!("../../web/index.html"),
    );
    let file_routes = Router::new()
        .route("/files/list", get(files::list))
        .route("/files/download", get(files::download))
        .route("/files/upload", axum::routing::put(files::upload))
        .with_state(files);
    Router::new()
        .route("/", index.clone())
        .route("/index.html", index)
        .route("/client.js", bundled!("client.js"))
        .route("/worker.js", bundled!("worker.js"))
        .route("/audio.js", bundled!("audio.js"))
        .route("/mic.js", bundled!("mic.js"))
        .route(
            "/favicon.ico",
            asset("image/x-icon", include_bytes!("../../web/favicon.ico")),
        )
        .route(
            "/inter.woff2",
            asset("font/woff2", include_bytes!("../../web/inter.woff2")),
        )
        .route("/ws", get(session::upgrade))
        .with_state(sessions)
        .merge(file_routes)
        .layer(axum::middleware::map_response(isolated))
        // Last, so it runs first: a request for the wrong host or from another site's
        // page is turned away before anything else.
        .layer(axum::middleware::from_fn(access::check))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xorshift::Xorshift;

    /// Over drawn command lines (each of the three flags given zero, one or two times, in
    /// either spelling, with a value or without one at the end, ports good and bad,
    /// destinations good and bad, an unknown flag now and then, in any order): a line is
    /// read exactly when each flag is given once with a value it takes and nothing else is,
    /// and it is read as the values given. The oracle is the drawing's record of what it
    /// put in the line.
    #[test]
    fn the_relay_takes_exactly_its_three_flags() {
        let mut rng = Xorshift::new(0xC6A4A7935BD1E995);
        let ports = ["6901", "1", "65535", "0", "65536", "-1", "x", ""];
        let dests = ["desk", "a.example", "-oProxyCommand=x", "a b", ""];
        let mut read = 0;
        for _ in 0..20_000 {
            // Per flag: the values given, in order.
            let mut given: [Vec<String>; 3] = Default::default();
            let mut words: Vec<Vec<String>> = Vec::new();
            for (f, name) in ["--lanes", "--ssh", "--port"].into_iter().enumerate() {
                for _ in 0..[0, 1, 1, 1, 2][rng.below(5) as usize] {
                    let v = match f {
                        0 => format!("/lanes/{}", rng.below(9)),
                        1 => dests[rng.below(dests.len() as u64) as usize].to_string(),
                        _ => ports[rng.below(ports.len() as u64) as usize].to_string(),
                    };
                    given[f].push(v.clone());
                    words.push(match rng.below(2) {
                        0 => vec![format!("{name}={v}")],
                        _ => vec![name.to_string(), v],
                    });
                }
            }
            let unknown = rng.below(10) == 0;
            if unknown {
                words.push(vec!["--title".into(), "x".into()]);
            }
            for i in (1..words.len()).rev() {
                let j = rng.below(i as u64 + 1) as usize;
                words.swap(i, j);
            }
            // Now and then the last flag loses its value.
            let cut = rng.below(10) == 0 && words.last().is_some_and(|w| w.len() == 2);
            let mut args: Vec<String> = words.concat();
            if cut {
                args.pop();
            }
            let once = given.iter().all(|g| g.len() == 1);
            let valid = once
                && !unknown
                && !cut
                && ssh::Destination::parse(&given[1][0]).is_ok()
                && given[2][0].parse::<u16>().is_ok_and(|p| p != 0);
            match Flags::parse(&args) {
                Ok(flags) => {
                    assert!(valid, "{args:?} read");
                    assert_eq!(flags.lanes, PathBuf::from(&given[0][0]));
                    assert_eq!(flags.ssh, ssh::Destination::parse(&given[1][0]).unwrap());
                    assert_eq!(flags.port.to_string(), given[2][0]);
                    read += 1;
                }
                Err(_) => assert!(!valid, "{args:?} refused"),
            }
        }
        assert!(read > 500, "{read}");
    }
}
