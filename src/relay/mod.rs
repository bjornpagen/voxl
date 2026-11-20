//! `voxl relay`: the viewer's side of a session, run on the viewer's machine. It serves the
//! page on 127.0.0.1 and, for each page that connects, runs a session: `ssh <host> voxl
//! attach` for the control stream (to the daemon of `--runtime`'s folder, when it is given),
//! and one lane, through a proxy, to `--place` on the server (the protocol's `localhost:6901`
//! by default), for video and audio.
//!
//! **The lane.** One stream through `--proxy`, an HTTP CONNECT proxy, asked for as the
//! session begins: the transport delivers every byte, in order, and the relay gives
//! the page each frame as it is, one at a time, reading the next only once the page has said
//! it took the one before; each update the page took it tells the daemon on the control
//! stream. A lane that ends after the daemon confirmed it (`Down::Attached`) opens again,
//! saying the last update the page took (`wire::Holds`): the daemon goes on from it with
//! the next frame if it is still the stream's newest, and starts its viewer over on a
//! keyframe if not; one that ends before is the daemon or the transport refusing it, and
//! ends the session.

mod access;
mod files;
mod proxy;
mod session;
mod ssh;

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::net::Ipv4Addr;
use std::sync::Arc;

/// What `voxl relay` is told: each flag given once, every one but `--runtime` and `--place`
/// required.
#[derive(Debug, PartialEq)]
struct Flags {
    /// `--proxy`: the HTTP proxy the lane goes through.
    proxy: proxy::Endpoint,
    /// `--ssh`: where `voxl attach` and `voxl files` run.
    ssh: ssh::Destination,
    /// `--port`: the page's port on 127.0.0.1.
    port: u16,
    /// `--runtime`: the folder of the daemon `voxl attach` reaches, on the server; not
    /// given, the daemon in the server's default folder.
    daemon: ssh::Daemon,
    /// `--place`: where the lane goes, as the server's proxy is asked for it; when not
    /// given, the protocol's.
    place: proxy::Place,
}

/// The relay's flags, every one it reads.
const FLAGS: [&str; 5] = ["--proxy", "--ssh", "--port", "--runtime", "--place"];

const USAGE: &str = "usage: voxl relay --proxy HOST:PORT --ssh DESTINATION --port PORT [--runtime DIR] [--place localhost:PORT]";

impl Flags {
    /// `--name value` or `--name=value`, each at most once, the three required ones
    /// exactly once, and nothing else.
    fn parse(args: &[String]) -> Result<Flags, String> {
        // What each flag was given, in `FLAGS`' order.
        let mut given = FLAGS.map(|_| None);
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
            let Some(flag) = FLAGS.iter().position(|flag| *flag == name) else {
                return Err(format!("{name}: not a flag of voxl relay"));
            };
            if given[flag].replace(value).is_some() {
                return Err(format!("{name} is given twice"));
            }
        }
        let [proxy, ssh, port, runtime, place] = given;
        let need = |v: Option<String>, name: &str| v.ok_or_else(|| format!("{name} is required"));
        let port = need(port, "--port")?;
        Ok(Flags {
            proxy: proxy::Endpoint::parse(&need(proxy, "--proxy")?)?,
            ssh: ssh::Destination::parse(&need(ssh, "--ssh")?)?,
            port: port
                .parse::<std::num::NonZeroU16>()
                .map_err(|_| format!("--port {port}: not a port from 1 to 65535"))?
                .get(),
            daemon: match runtime {
                Some(r) => ssh::Daemon::In(ssh::Runtime::parse(&r)?),
                None => ssh::Daemon::Default,
            },
            place: match place {
                Some(p) => proxy::Place::parse(&p)?,
                None => proxy::Place::protocol(),
            },
        })
    }
}

/// Runs the relay with these flags (after `relay`) until it is stopped.
pub fn run(args: &[String]) -> ! {
    let flags = match Flags::parse(args) {
        Ok(flags) => flags,
        Err(e) => {
            eprintln!("voxl relay: {e}\n{USAGE}");
            std::process::exit(2)
        }
    };
    let env = crate::env::Env::read(|name| std::env::var_os(name)).unwrap_or_else(|e| {
        eprintln!("voxl relay: {e}");
        std::process::exit(2)
    });
    crate::logging::init(env.log.logger());
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
        proxy: flags.proxy,
        ssh: flags.ssh,
        daemon: flags.daemon,
        place: flags.place,
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

    /// Over drawn command lines (each of the five flags given zero, one or two times, in
    /// either spelling, with a value or without one at the end, ports good and bad,
    /// proxies, destinations, runtime folders and places good and bad, an unknown flag now and then,
    /// in any order): a line is read exactly when each required flag is given once,
    /// `--runtime` and `--place` at most once, each with a value it takes, and nothing else
    /// is; and it is read as the values given, a runtime folder not given read as none and a
    /// place not given as the protocol's. The oracle is the drawing's
    /// record of what it put in the line.
    #[test]
    fn the_relay_takes_exactly_its_flags() {
        let mut rng = Xorshift::new(0xC6A4A7935BD1E995);
        let ports = ["6901", "1", "65535", "0", "65536", "-1", "x", ""];
        let proxies = [
            "127.0.0.1:1080",
            "[::1]:443",
            "proxy.example:8443",
            "127.0.0.1",
            "a:0",
            "",
        ];
        let dests = ["desk", "a.example", "-oProxyCommand=x", "a b", ""];
        let runtimes = ["/run/voxl", "/run/voxl-bench/3", "a b", "--x", "x=y", ""];
        let places = [
            "localhost:6901",
            "localhost:6913",
            "localhost:22",
            "localhost:69130",
            "127.0.0.1:6901",
            "",
        ];
        let mut read = 0;
        for _ in 0..40_000 {
            // Per flag: the values given, in order.
            let mut given: [Vec<String>; 5] = Default::default();
            let mut words: Vec<Vec<String>> = Vec::new();
            for (f, name) in ["--proxy", "--ssh", "--port", "--runtime", "--place"]
                .into_iter()
                .enumerate()
            {
                for _ in 0..[0, 1, 1, 1, 2][rng.below(5) as usize] {
                    let v = match f {
                        0 => proxies[rng.below(proxies.len() as u64) as usize].to_string(),
                        1 => dests[rng.below(dests.len() as u64) as usize].to_string(),
                        3 => runtimes[rng.below(runtimes.len() as u64) as usize].to_string(),
                        4 => places[rng.below(places.len() as u64) as usize].to_string(),
                        2 | 5.. => ports[rng.below(ports.len() as u64) as usize].to_string(),
                    };
                    given[f].push(v.clone());
                    words.push(match rng.below(2) {
                        0 => vec![format!("{name}={v}")],
                        1.. => vec![name.to_string(), v],
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
            let once =
                given[..3].iter().all(|g| g.len() == 1) && given[3..].iter().all(|g| g.len() <= 1);
            let valid = once
                && !unknown
                && !cut
                && proxy::Endpoint::parse(&given[0][0]).is_ok()
                && ssh::Destination::parse(&given[1][0]).is_ok()
                && given[2][0].parse::<u16>().is_ok_and(|p| p != 0)
                && given[3].iter().all(|r| !r.is_empty())
                && given[4].iter().all(|p| proxy::Place::parse(p).is_ok());
            match Flags::parse(&args) {
                Ok(flags) => {
                    assert!(valid, "{args:?} read");
                    assert_eq!(flags.proxy, proxy::Endpoint::parse(&given[0][0]).unwrap());
                    assert_eq!(flags.ssh, ssh::Destination::parse(&given[1][0]).unwrap());
                    assert_eq!(flags.port.to_string(), given[2][0]);
                    assert_eq!(
                        flags.daemon,
                        given[3]
                            .first()
                            .map_or(ssh::Daemon::Default, |r| ssh::Daemon::In(
                                ssh::Runtime::parse(r).unwrap()
                            ))
                    );
                    assert_eq!(
                        flags.place,
                        given[4]
                            .first()
                            .map_or_else(proxy::Place::protocol, |p| proxy::Place::parse(p)
                                .unwrap())
                    );
                    read += 1;
                }
                Err(_) => assert!(!valid, "{args:?} refused"),
            }
        }
        assert!(read > 100, "{read}");
    }
}
