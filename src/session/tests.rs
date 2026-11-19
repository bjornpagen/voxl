//! The session's properties, each against what the relay's side of the sockets sees: a
//! still desktop sends nothing; a lane attaches only with its session's capability, and
//! nothing else from a lane counts; a closed session's lane gets no byte more; a lane whose
//! socket breaks gates nothing; the capture gate is open exactly while nothing waits for the
//! socket, and captures follow the socket's rate; and the capability goes nowhere but the
//! control stream.

use super::control::{connection, Daemon};
use super::query::Opening;
use crate::hub::Hub;
use crate::wire::{Capability, Down, Frame, Framer, Track, Up};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn daemon(hub: &Arc<Hub>) -> Arc<Daemon> {
    Arc::new(Daemon {
        hub: hub.clone(),
        audio: super::control::Audio::Sent,
        // A microphone thread that has ended: the tests send it nothing they read back.
        mic: std::sync::mpsc::channel().0,
        resize: super::control::Resizing::Fixed,
        max_record: Duration::from_secs(300),
    })
}

/// The relay's end of a control connection: the messages it has read so far.
struct Relay {
    socket: UnixStream,
    framer: Framer,
}

/// A control message as the relay reads it, owned.
#[derive(Debug, PartialEq)]
enum Said {
    Capability(Vec<u8>),
    Attached,
    Text(String),
    Binary(Vec<u8>),
}

impl Relay {
    /// A session of `d`'s, opened.
    async fn open(d: &Arc<Daemon>) -> Relay {
        let (mut relay, ours) = UnixStream::pair().unwrap();
        tokio::spawn(connection(ours, d.clone()));
        relay.write_all(&Opening::Session.bytes()).await.unwrap();
        Relay {
            socket: relay,
            framer: Framer::new(),
        }
    }

    /// The next control message, waiting for it; with a capability, the capability too.
    async fn next(&mut self) -> (Said, Option<Capability>) {
        loop {
            if let Some(said) = self.read() {
                return said;
            }
            let mut buf = [0u8; 4096];
            let n = self.socket.read(&mut buf).await.unwrap();
            assert!(n > 0, "the control stream closed");
            self.framer.push(&buf[..n]);
        }
    }

    /// The capability and the hello, as a session opens.
    async fn greeted(&mut self) -> (Capability, Vec<u8>) {
        let (Said::Capability(raw), Some(c)) = self.next().await else {
            panic!("the first message is no capability");
        };
        let (Said::Text(hello), None) = self.next().await else {
            panic!("the second message is no hello");
        };
        assert!(hello.starts_with("{\"t\":\"hello\""), "{hello}");
        (c, raw)
    }

    async fn say(&mut self, up: Up<'_>) {
        let mut out = Vec::new();
        up.encode(&mut out);
        self.socket.write_all(&out).await.unwrap();
    }

    /// The next control message, if one has come with every task run as far as it goes.
    async fn said(&mut self) -> Option<Said> {
        settle().await;
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = self.socket.try_read(&mut buf) {
            self.framer.push(&buf[..n]);
        }
        self.read().map(|(said, _)| said)
    }

    /// The next whole control message read, if any.
    fn read(&mut self) -> Option<(Said, Option<Capability>)> {
        let raw = self.framer.frame().unwrap()?.to_vec();
        Some(match Down::decode(&raw).unwrap() {
            Down::Capability(c) => (Said::Capability(raw[1..].to_vec()), Some(c.keep())),
            Down::Attached => (Said::Attached, None),
            Down::Text(t) => (Said::Text(t.into()), None),
            Down::Binary(b) => (Said::Binary(b.into()), None),
        })
    }

    /// Whether the daemon has sent nothing more, with every task run as far as it goes.
    async fn silent(&mut self) -> bool {
        settle().await;
        let mut buf = [0u8; 1];
        matches!(self.socket.try_read(&mut buf), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            && self.framer.frame().unwrap().is_none()
    }
}

/// Lets every task run as far as it can.
async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

/// A lane's first bytes: the capability it presents.
fn attach(c: &Capability) -> [u8; crate::wire::CAPABILITY] {
    *c.bytes()
}

/// A lane opened to `hub` with `bytes` first: the relay's end.
async fn open_lane(hub: &Arc<Hub>, bytes: &[u8]) -> UnixStream {
    let (mut relay, ours) = UnixStream::pair().unwrap();
    tokio::spawn(super::lane::lane(ours, hub.clone()));
    relay.write_all(bytes).await.unwrap();
    relay
}

/// Reads a lane until it ends: what it carried.
async fn to_the_end(lane: &mut UnixStream) -> Vec<u8> {
    let mut all = Vec::new();
    lane.read_to_end(&mut all).await.unwrap();
    all
}

/// Ten minutes of a still desktop, on tokio's paused clock, send nothing after the hello:
/// not on the control stream, not on a lane attached, with the viewer watching and
/// listening. The daemon sends nothing it was not asked for, and no time wakes it.
#[tokio::test(start_paused = true)]
async fn a_still_desktop_sends_nothing_after_the_hello() {
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, _) = relay.greeted().await;
    let lane = open_lane(&hub, &attach(&c)).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    relay.say(Up::Text("v,1")).await;
    relay.say(Up::Text("au,1")).await;
    assert!(relay.silent().await);
    let start = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(600)).await;
    assert_eq!(start.elapsed(), Duration::from_secs(600));
    assert!(relay.silent().await, "the control stream said something");
    let mut buf = [0u8; 1];
    let lane_said = lane.try_read(&mut buf);
    assert!(
        matches!(&lane_said, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "the lane carried {lane_said:?}"
    );
}

/// A lane attaches only with the capability of an open session: one with any other (a
/// made-up one, one of each byte changed, one of a session closed) is closed with no byte
/// back, and no session hears of it; one with the right capability is confirmed on its
/// session's control stream, once.
#[tokio::test]
async fn a_wrong_or_stale_capability_is_refused() {
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    // A session that closes: its capability is stale.
    let mut gone = Relay::open(&d).await;
    let (stale, _) = gone.greeted().await;
    drop(gone);
    while hub.clients.lock().len() > 1 {
        tokio::task::yield_now().await;
    }
    let mut wrong = vec![Capability::mint().unwrap(), stale];
    for i in 0..raw.len() {
        let mut changed = raw.clone();
        changed[i] ^= 1 << (i % 8);
        wrong.push(Capability::presented(&changed.try_into().unwrap()));
    }
    for w in &wrong {
        let mut lane = open_lane(&hub, &attach(w)).await;
        assert!(
            to_the_end(&mut lane).await.is_empty(),
            "a refused lane was answered"
        );
    }
    // Neither a lane of a few bytes, nor one of another message, is an attach.
    for first in [&[0u8, 3, 1][..], &[0, 42], b"GET / HTTP/1.1\r\n\r\n"] {
        let mut lane = open_lane(&hub, first).await;
        drop(lane.shutdown().await);
        assert!(to_the_end(&mut lane).await.is_empty());
    }
    let _lane = open_lane(&hub, &attach(&c)).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    assert!(relay.silent().await, "a refused lane was confirmed");
}

/// Once its control connection closes, a session's lane gets no byte more: frames sent
/// before the close reach the lane as it is read, in order, the lane then ends, and frames
/// sent after it make nothing; another lane attaching with its capability is refused.
#[tokio::test]
async fn after_the_close_no_lane_gets_a_byte() {
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    let mut lane = open_lane(&hub, &attach(&c)).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let client = hub.clients.lock()[0].clone();
    for i in 0..5u8 {
        let frame = bytes::Bytes::from(vec![i; 3000]);
        client.lane(|s| s.video(&frame));
    }
    let mut framer = Framer::new();
    let mut buf = [0u8; 4096];
    let mut carried = 0;
    while carried < 5 {
        let n = lane.read(&mut buf).await.unwrap();
        assert!(n > 0, "the lane ended before the close");
        framer.push(&buf[..n]);
        while let Some(m) = framer.frame().unwrap() {
            let Ok(Frame {
                track: Track::Video,
                bytes,
            }) = Frame::decode(m)
            else {
                panic!("a lane carries the updates");
            };
            assert_eq!(bytes[0], carried, "in order");
            carried += 1;
        }
    }
    let frame = bytes::Bytes::from(vec![7u8; 3000]);
    drop(relay);
    while !hub.clients.lock().is_empty() {
        tokio::task::yield_now().await;
    }
    for _ in 0..5 {
        client.lane(|s| s.video(&frame));
        client.lane(|s| s.audio(&frame[..200], 0));
    }
    assert!(
        to_the_end(&mut lane).await.is_empty(),
        "a byte after the close"
    );
    // Its capability is forgotten: a lane presenting it is refused.
    let again = Capability::presented(&raw.clone().try_into().unwrap());
    let mut late = open_lane(&hub, &attach(&again)).await;
    assert!(to_the_end(&mut late).await.is_empty());
}

/// Every line logged, for the test below.
static LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

struct Lines;

impl log::Log for Lines {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        LINES
            .lock()
            .unwrap()
            .push(format!("{} {}", r.target(), r.args()));
    }
    fn flush(&self) {}
}

/// The capability goes nowhere but its control stream: not into a log line (every line of
/// a session's whole life, opened, attached, refused, closed, read as text, in hex either
/// case, and as a list of numbers), not into the environment or the arguments, and it has
/// no `Debug` or `Display` to print it with (this compiles only while it has neither). The
/// log is the process's one logger, which another test may want too: so this runs in a
/// process of its own.
#[test]
fn the_capability_reaches_no_log_env_or_argv() {
    if !in_a_process_of_its_own("session::tests::the_capability_reaches_no_log_env_or_argv") {
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(the_capability_reaches_nothing());
}

/// `name` runs in a process of its own, the test binary started again with only that test
/// selected. True in that process; in the first, starts it and checks that it ran the test
/// and the test passed.
fn in_a_process_of_its_own(name: &str) -> bool {
    const ALONE: &str = "VOXL_TEST_IN_A_PROCESS_OF_ITS_OWN";
    if std::env::var_os(ALONE).is_some() {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--test-threads=1"])
        .env(ALONE, "1")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && said.contains("1 passed"),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    false
}

async fn the_capability_reaches_nothing() {
    trait AmbiguousIfDebug<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfDebug<()> for T {}
    impl<T: ?Sized + std::fmt::Debug> AmbiguousIfDebug<u8> for T {}
    trait AmbiguousIfDisplay<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfDisplay<()> for T {}
    impl<T: ?Sized + std::fmt::Display> AmbiguousIfDisplay<u8> for T {}
    <Capability as AmbiguousIfDebug<_>>::check();
    <Capability as AmbiguousIfDisplay<_>>::check();

    log::set_boxed_logger(Box::new(Lines)).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    let _lane = open_lane(&hub, &attach(&c)).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let mut refused = open_lane(&hub, &attach(&Capability::mint().unwrap())).await;
    to_the_end(&mut refused).await;
    drop(relay);
    while !hub.clients.lock().is_empty() {
        tokio::task::yield_now().await;
    }
    let forms = [
        raw.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        raw.iter().map(|b| format!("{b:02X}")).collect::<String>(),
        format!("{raw:?}"),
        String::from_utf8_lossy(&raw).into_owned(),
    ];
    let lines = LINES.lock().unwrap().clone();
    assert!(
        lines.iter().any(|l| l.contains("session")),
        "no session was logged"
    );
    for l in &lines {
        for f in &forms {
            assert!(
                !l.contains(f.as_str()),
                "a log line holds the capability: {l}"
            );
        }
    }
    for (k, v) in std::env::vars_os() {
        let (k, v) = (k.to_string_lossy(), v.to_string_lossy());
        assert!(forms
            .iter()
            .all(|f| !k.contains(f.as_str()) && !v.contains(f.as_str())));
    }
    for a in std::env::args_os() {
        let a = a.to_string_lossy();
        assert!(forms.iter().all(|f| !a.contains(f.as_str())));
    }
}

/// A query's answer: the tag byte and what follows it, read to the connection's end.
async fn query(d: &Arc<Daemon>, q: super::query::Query) -> Vec<u8> {
    let (mut asker, ours) = UnixStream::pair().unwrap();
    tokio::spawn(connection(ours, d.clone()));
    asker.write_all(&Opening::Query(q).bytes()).await.unwrap();
    asker.shutdown().await.unwrap();
    to_the_end(&mut asker).await
}

/// Plays the capture thread for screenshots: each asked for (the asker wakes the hub) is
/// answered with these pixels. The thread waits on the hub's waker, and outlives the test
/// blocked there.
fn capture(hub: &Arc<Hub>, w: usize, h: usize, bgrx: Vec<u8>) {
    let hub = hub.clone();
    std::thread::spawn(move || loop {
        hub.drain_wake();
        let snaps = std::mem::take(&mut *hub.snapshots.lock());
        for tx in snaps {
            let shot = crate::record::Snapshot {
                width: w,
                height: h,
                bgrx: bgrx.clone(),
            };
            let _ = tx.send(Arc::new(shot));
        }
        let mut wake = [nix::poll::PollFd::new(
            hub.waker.waitable(),
            nix::poll::PollFlags::POLLIN,
        )];
        nix::poll::poll(&mut wake, nix::poll::PollTimeout::NONE).unwrap();
    });
}

/// A query is answered once and is no session: its answer is a tag and what was asked,
/// never a capability; while it runs, a lane presenting any capability is refused; and
/// the stats it asks list the sessions as they are. Over every query (a recording of
/// one second, a recording until the asker's stream ends, a screenshot, the stats).
#[tokio::test]
async fn a_query_never_mints_a_capability_or_reaches_a_lane() {
    use super::query::{Query, Recording};
    let hub = Hub::new().unwrap();
    capture(&hub, 4, 2, vec![9; 4 * 2 * 4]);
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    let one = std::num::NonZeroU32::new(1).unwrap();
    for q in [
        Query::Stats,
        Query::Screenshot,
        Query::Record(Recording::UntilClosed),
        Query::Record(Recording::Seconds(one)),
    ] {
        let answer = query(&d, q).await;
        assert!(
            matches!(answer.first(), Some(0 | 1)),
            "{q:?}: {:?}",
            answer.first()
        );
        assert!(
            !answer.windows(raw.len()).any(|w| w == raw.as_slice()),
            "{q:?}: the session's capability in a query's answer"
        );
        // Only the session attaches; nothing a query left behind does.
        let mut refused = open_lane(&hub, &attach(&Capability::mint().unwrap())).await;
        assert!(to_the_end(&mut refused).await.is_empty(), "{q:?}");
    }
    let stats = query(&d, Query::Stats).await;
    assert_eq!(stats[0], 0);
    let stats: serde_json::Value = serde_json::from_slice(&stats[1..]).unwrap();
    let clients = stats["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 1, "{stats}");
    assert_eq!(clients[0]["way"]["way"], "unattached");
    let _lane = open_lane(&hub, &attach(&c)).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let stats = query(&d, Query::Stats).await;
    let stats: serde_json::Value = serde_json::from_slice(&stats[1..]).unwrap();
    assert_eq!(stats["clients"][0]["way"]["way"], "lane", "{stats}");
}

/// A screenshot is a PNG of the screen as captured: its header, its size, and every
/// pixel's colour back, over generated screens.
#[tokio::test]
async fn a_screenshot_decodes_to_the_screen() {
    let mut rng = crate::xorshift::Xorshift::new(0x3c6e_f372_fe94_f82b);
    for _ in 0..8 {
        let (w, h) = (1 + rng.below(64) as usize, 1 + rng.below(48) as usize);
        let bgrx: Vec<u8> = (0..w * h * 4).map(|_| rng.below(256) as u8).collect();
        let hub = Hub::new().unwrap();
        capture(&hub, w, h, bgrx.clone());
        let d = daemon(&hub);
        let answer = query(&d, super::query::Query::Screenshot).await;
        assert_eq!(answer[0], 0, "{}", String::from_utf8_lossy(&answer[1..]));
        let png = &answer[1..];
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgb).unwrap();
        assert_eq!((info.width as usize, info.height as usize), (w, h));
        for (got, [b, g, r, _]) in rgb.as_chunks::<3>().0.iter().zip(bgrx.as_chunks::<4>().0) {
            assert_eq!(*got, [*r, *g, *b]);
        }
    }
}

/// The page asks for a size at every animation frame while a window is dragged: the
/// daemon resizes to the newest size asked, once, and never to one before it, however
/// many asks come before the capture loop looks. Over generated bursts of 1 to 200 asks of
/// any size, each followed by the capture loop's look (`rebuilds_for`, with a screen that
/// takes any size): the screen is resized once, to the burst's last ask, and a second
/// look finds nothing asked.
#[tokio::test]
async fn a_burst_of_sizes_is_one_resize_to_the_last() {
    struct Screen(Vec<(u32, u32)>);
    impl crate::video::Resize for Screen {
        fn resize(&mut self, w: u32, h: u32) -> Result<(u32, u32), String> {
            self.0.push((w, h));
            Ok((w, h))
        }
    }
    let hub = Hub::new().unwrap();
    *hub.resize_max.lock() = Some((u32::MAX, u32::MAX));
    let d = Arc::new(Daemon {
        resize: super::control::Resizing::Follows,
        ..Arc::into_inner(daemon(&hub)).unwrap()
    });
    let mut relay = Relay::open(&d).await;
    relay.greeted().await;
    let mut rng = crate::xorshift::Xorshift::new(0x6a09_e667_bb67_ae85);
    for _ in 0..200 {
        let asks: Vec<(u32, u32)> = (0..rng.range(1, 201))
            .map(|_| (rng.next() as u32, rng.next() as u32))
            .collect();
        for (w, h) in &asks {
            relay.say(Up::Text(&format!("sz,{w},{h}"))).await;
        }
        settle().await;
        let mut screen = Screen(Vec::new());
        let asked = hub.resize_req.lock().take().expect("a size was asked");
        crate::video::rebuilds_for(&mut screen, asked, (0, 0));
        assert_eq!(screen.0, [*asks.last().unwrap()]);
        assert_eq!(*hub.resize_req.lock(), None);
    }
}

/// A runtime folder is taken only if it is a folder of this user's that others may not
/// write in: over every mode bit others and the group can have, a folder is refused
/// exactly when others may write; a file, a link to a good folder, and nothing at all are
/// refused. (Another user's folder needs another user to make, which a test cannot.)
#[test]
fn a_runtime_folder_is_one_nobody_else_may_write_in() {
    use super::{Runtime, Untrusted};
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("voxl-runtime-{}", std::process::id()));
    std::fs::create_dir(&base).unwrap();
    let dir = base.join("run");
    std::fs::create_dir(&dir).unwrap();
    for bits in 0..0o100u32 {
        let mode = 0o700 | bits;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        let got = Runtime::open(dir.clone());
        match mode & 0o002 {
            0 => assert!(got.is_ok(), "{mode:o}: {got:?}"),
            _ => assert_eq!(got, Err(Untrusted::OthersWrite(mode)), "{mode:o}"),
        }
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = base.join("file");
    std::fs::write(&file, b"").unwrap();
    assert_eq!(Runtime::open(file), Err(Untrusted::NotAFolder));
    let link = base.join("link");
    std::os::unix::fs::symlink(&dir, &link).unwrap();
    assert_eq!(Runtime::open(link), Err(Untrusted::NotAFolder));
    assert!(matches!(
        Runtime::open(base.join("none")),
        Err(Untrusted::Unread(_))
    ));
    std::fs::remove_dir_all(&base).unwrap();
}

/// What the daemon makes in its runtime folder is reachable by the folder's group only
/// where the proxy needs it: the lane socket is the group's too (0660), the control
/// socket and the keymap scratch file the user's alone (0600), whatever the umask, and
/// whatever mode a keymap a crashed voxl left had.
#[tokio::test]
async fn only_the_lane_is_the_groups() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("voxl-entries-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o710)).unwrap();
    let runtime = super::Runtime::open(dir.clone()).unwrap();
    let sockets = runtime.sockets();
    let _control = super::listen::control(&sockets.control).unwrap();
    let _lane = super::listen::lane(&sockets.lane).unwrap();
    // A keymap a crashed voxl left, group- and world-readable.
    std::fs::write(runtime.keymap(), b"92").unwrap();
    std::fs::set_permissions(runtime.keymap(), std::fs::Permissions::from_mode(0o644)).unwrap();
    super::private_file(&runtime.keymap(), b"92 109").unwrap();
    let mut entries: Vec<(String, u32)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let mode = e.metadata().unwrap().permissions().mode() & 0o777;
            (e.file_name().into_string().unwrap(), mode)
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        [
            ("control".into(), 0o600),
            ("keymap".into(), 0o600),
            ("lane-stream".into(), 0o660),
        ]
    );
    assert_eq!(std::fs::read(runtime.keymap()).unwrap(), b"92 109");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The capture gate is open exactly while nothing waits for a live lane's socket. Over
/// generated sequences on a sender (lanes attaching, each replacing the one before; video
/// updates and Opus packets of generated sizes; the socket taking some of what waits; the
/// socket breaking): after every step, the gate is shut exactly when a lane is live and its
/// queue holds bytes, and the sender counts as attached exactly while a lane is live. A gate
/// that let a frame wait, or read a broken lane's bytes, fails this.
#[test]
fn the_gate_is_shut_exactly_while_bytes_wait() {
    use crate::session::sender::{Queue, Sender};
    let mut rng = crate::xorshift::Xorshift::new(0x9b05_688c_2b3e_6c1f);
    let (mut open, mut shut) = (0, 0);
    for _ in 0..500 {
        let mut s = Sender::new(Capability::mint().unwrap());
        let (mut queue, mut live): (Option<Arc<Queue>>, bool) = (None, false);
        for _ in 0..rng.range(1, 80) {
            match rng.below(6) {
                0 => {
                    let q = Queue::new(Arc::new(|| {}));
                    assert!(s.attach(q.clone()));
                    (queue, live) = (Some(q), true);
                }
                1 => s.video(&bytes::Bytes::from(vec![
                    3u8;
                    rng.range(1, 30_000) as usize
                ])),
                2 => s.audio(&vec![4u8; rng.range(1, 400) as usize], 0),
                3 | 4 => {
                    if let Some(q) = &queue {
                        for _ in 0..rng.below(3) {
                            q.take();
                        }
                    }
                }
                _ => {
                    if let Some(q) = &queue {
                        q.lose();
                        live = false;
                    }
                }
            }
            let waiting = queue.as_ref().map_or(0, |q| q.waiting_bytes());
            assert_eq!(
                s.waits(),
                live && waiting > 0,
                "live {live}, {waiting} B waiting"
            );
            match s.waits() {
                true => shut += 1,
                false => open += 1,
            }
            assert_eq!(s.attached(), live);
        }
    }
    assert!(open > 1000 && shut > 1000, "open {open}, shut {shut}");
}

/// Captures follow the socket's rate, and never more than one update waits for it. A
/// simulated session: the lane's socket takes bytes at a generated rate, a write waiting
/// until the transport has taken what came before (as a stream's does); Opus packets come
/// every 10 ms; the capture loop captures as the gate and the frame interval let it, each
/// capture coded for a generated time into an update of a generated size. Over generated
/// rates, coding times and sizes, on a virtual clock:
/// - every capture starts with nothing waiting, and no more than one update ever waits;
/// - the socket idles only while a capture is coded or the frame interval holds it: the
///   bytes it took are its rate over the time anything waited, and the captures are as
///   many as that allows.
///
/// A gate that let a capture start with bytes waiting fails this.
#[test]
fn captures_follow_the_sockets_rate() {
    use crate::session::sender::Sender;
    let mut rng = crate::xorshift::Xorshift::new(0x2f6b_5d0a_83c1_e947);
    let us = Duration::from_micros;
    let interval = us(16_667);
    let seconds = 5.0;
    for case in 0..300 {
        let rate = rng.range(20_000, 5_000_001) as f64;
        let coding = us(rng.range(1_000, 20_000));
        let most = rng.range(1, 400_000) as usize;
        // Opus packets of at most a quarter of the socket's rate: more would hold the gate
        // shut on their own.
        let audio = (rng.range(80, 400) as usize)
            .min((rate / 400.0) as usize)
            .max(1);
        let start = Instant::now();
        let (mut s, queue) = Sender::with_lane();
        // When the socket began to take the front message.
        let mut began = start;
        // The capture loop: when its last capture went, and when the one it codes is done.
        let (mut captured, mut coded): (Option<Instant>, Option<Instant>) = (None, None);
        let mut next_audio = start;
        let (mut captures, mut busy, mut taken) = (0u64, Duration::ZERO, 0usize);
        let mut now = start;
        let end = start + Duration::from_secs_f64(seconds);
        while now < end {
            // The next event: the socket taking the front message, a coded update, an Opus
            // packet, or a capture the gate and the interval let go.
            let waiting = queue.waiting_bytes() > 0;
            let accept = queue
                .front_len()
                .map(|len| began + Duration::from_secs_f64(len as f64 / rate));
            let capture = match (s.waits(), coded) {
                (false, None) => Some(captured.map_or(now, |c| (c + interval).max(now))),
                (true, _) | (false, Some(_)) => None,
            };
            let at = [accept, coded, Some(next_audio), capture]
                .into_iter()
                .flatten()
                .min()
                .unwrap();
            if waiting {
                busy += at - now;
            }
            now = at;
            if accept == Some(now) {
                taken += queue.take().unwrap().len();
                began = now;
                continue;
            }
            if coded == Some(now) {
                let bytes = rng.range(1, most as u64 + 1) as usize;
                s.video(&bytes::Bytes::from(vec![1u8; bytes]));
                assert!(
                    queue.updates_waiting() <= 1,
                    "case {case}: two updates wait"
                );
                coded = None;
            } else if next_audio == now {
                s.audio(&vec![2u8; audio], 0);
                next_audio += us(10_000);
            } else {
                assert!(!waiting, "case {case}: a capture with bytes waiting");
                captures += 1;
                captured = Some(now);
                coded = Some(now + coding);
            }
            // A message queued while nothing waited is begun at once.
            if !waiting {
                began = now;
            }
        }
        // The socket took its rate over the time anything waited, less the message it was
        // taking at the end.
        let frame = (most + 16) as f64;
        let (took, owed) = (taken as f64, rate * busy.as_secs_f64());
        assert!(
            took <= owed * (1.0 + 1e-4) + 1.0 && took >= owed * (1.0 - 1e-4) - frame,
            "case {case}: took {took:.0} in {busy:?} busy at {rate:.0}"
        );
        // Each capture cycle is at most its coding, the frame interval, and its update taken
        // at what the Opus packets leave of the rate: as many as fit.
        let most_cycle = (coding + interval).as_secs_f64() + frame / (0.75 * rate);
        let least = (seconds / most_cycle) as u64;
        assert!(
            captures + 2 >= least,
            "case {case}: {captures} captures, at least {least} fit"
        );
    }
}

/// A lane whose socket breaks ends, and the next attaches in its place: over generated
/// sessions (updates of generated sizes; the relay reading what has come; the relay's end of
/// the lane vanishing at any point, idle, with updates waiting or mid-update), once the next
/// update finds the socket broken the session's gate is open and it is not attached, the
/// control stream says nothing of it, and the next lane attaches and carries the updates
/// sent after it, whole and in order. What the broken lane held is dropped: its viewer starts
/// over on a keyframe on the next lane (`hub`'s), never on a stream with a hole in it.
#[tokio::test]
async fn a_broken_lane_ends_and_the_next_attaches() {
    let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_5be0_cd19);
    let mut waited = 0;
    for _ in 0..30 {
        let hub = Hub::new().unwrap();
        let d = daemon(&hub);
        let mut relay = Relay::open(&d).await;
        let (c, _) = relay.greeted().await;
        let mut lane = open_lane(&hub, &attach(&c)).await;
        assert_eq!(relay.next().await.0, Said::Attached);
        let client = hub.clients.lock()[0].clone();
        let mut sent = 0u8;
        let mut send = |rng: &mut crate::xorshift::Xorshift| {
            let frame = bytes::Bytes::from(vec![sent; rng.range(1, 60_000) as usize]);
            client.lane(|s| s.video(&frame));
            sent = sent.wrapping_add(1);
            sent.wrapping_sub(1)
        };
        for _ in 0..rng.range(1, 10) {
            for _ in 0..rng.below(4) {
                send(&mut rng);
            }
            settle().await;
            let mut buf = vec![0u8; rng.range(1, 70_000) as usize];
            drop(lane.try_read(&mut buf));
            waited += usize::from(client.lane(|s| s.waits()));
            drop(lane);
            // An update finds the socket broken, if what waited did not. (On macOS a test's
            // child process may hold the relay's end a moment: a socket pair is made close-on-exec
            // only after it is made, and a fork between the two inherits it.)
            let ended = tokio::time::timeout(Duration::from_secs(5), async {
                while client.lane(|s| s.attached()) {
                    send(&mut rng);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });
            assert!(ended.await.is_ok(), "a broken lane is live");
            assert!(!client.lane(|s| s.waits()), "a broken lane gates");
            assert_eq!(relay.said().await, None, "a broken lane told");
            lane = open_lane(&hub, &attach(&c)).await;
            assert_eq!(relay.next().await.0, Said::Attached);
            let first = send(&mut rng);
            let second = send(&mut rng);
            let mut framer = Framer::new();
            let mut got = Vec::new();
            let mut buf = [0u8; 8192];
            while got.len() < 2 {
                let n = lane.read(&mut buf).await.unwrap();
                assert!(n > 0, "the next lane ended");
                framer.push(&buf[..n]);
                while let Some(m) = framer.frame().unwrap() {
                    let f = Frame::decode(m).unwrap();
                    assert_eq!(f.track, Track::Video);
                    got.push(f.bytes[0]);
                }
            }
            assert_eq!(got, [first, second], "whole and in order");
        }
    }
    assert!(waited > 0, "no lane broke with updates waiting");
}
