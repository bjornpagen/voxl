//! The session's properties, each against what the relay's side of the sockets sees: a
//! still desktop sends nothing; a lane attaches only with its session's capability, and
//! nothing else from a lane counts; a closed session's lanes get no byte more; and the
//! capability goes nowhere but the control stream.

use super::control::{connection, Daemon};
use super::lane::{datagrams, stream_lane};
use super::query::Opening;
use crate::hub::Hub;
use crate::lanes::wire::{self, Attach, Capability, Down, Framer, Generation, Stamp, Up};
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
    Buffer,
    Lost(Generation),
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
            framer: Framer::control(),
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

    /// The capability, the lane's receive buffer and the hello, as a session opens.
    async fn greeted(&mut self) -> (Capability, Vec<u8>) {
        let (Said::Capability(raw), Some(c)) = self.next().await else {
            panic!("the first message is no capability");
        };
        assert_eq!(
            self.next().await.0,
            Said::Buffer,
            "the second message is no buffer"
        );
        let (Said::Text(hello), None) = self.next().await else {
            panic!("the third message is no hello");
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
            Down::Attached { .. } => (Said::Attached, None),
            Down::Buffer { .. } => (Said::Buffer, None),
            Down::Lost { generation } => (Said::Lost(generation), None),
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

/// A lane's attach, with a round trip of 100 ms measured.
fn attach(c: &Capability, generation: Generation) -> [u8; Attach::LEN] {
    let now = Instant::now();
    Attach::encode(
        c,
        generation,
        Stamp::since(now, now),
        Duration::from_millis(100),
    )
}

/// A lane's attach as a stream lane writes it: its length, then it.
fn attach_framed(c: &Capability, generation: Generation) -> [u8; Attach::STREAM_LEN] {
    let now = Instant::now();
    Attach::encode_stream(
        c,
        generation,
        Stamp::since(now, now),
        Duration::from_millis(100),
    )
}

/// A stream lane opened to `hub` with `bytes` first: the relay's end.
async fn stream(hub: &Arc<Hub>, bytes: &[u8]) -> UnixStream {
    let (mut relay, ours) = UnixStream::pair().unwrap();
    tokio::spawn(stream_lane(ours, hub.clone()));
    relay.write_all(bytes).await.unwrap();
    relay
}

/// Reads a stream lane until it ends: what it carried.
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
    let lane = stream(&hub, &attach_framed(&c, Generation::first())).await;
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
        let mut a = attach(&c, Generation::first());
        a[1..33].copy_from_slice(&changed);
        wrong.push(Attach::decode(&a).unwrap().capability);
    }
    for w in &wrong {
        let mut lane = stream(&hub, &attach_framed(w, Generation::first())).await;
        assert!(
            to_the_end(&mut lane).await.is_empty(),
            "a refused lane was answered"
        );
    }
    // Neither a lane of a few bytes, nor one of another message, is an attach.
    for first in [&[0u8, 3, 1][..], &[0, 42], b"GET / HTTP/1.1\r\n\r\n"] {
        let mut lane = stream(&hub, first).await;
        drop(lane.shutdown().await);
        assert!(to_the_end(&mut lane).await.is_empty());
    }
    let _lane = stream(&hub, &attach_framed(&c, Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    assert!(relay.silent().await, "a refused lane was confirmed");
}

/// The datagram socket: an attach with a wrong capability is dropped with no reply; one
/// with the right capability attaches its sender; anything that sender sends after (an
/// attach for another session among it) is dropped unread, and that other session hears of
/// no lane until one from another sender attaches to it. Datagrams are taken in order, so
/// a confirmation is a mark that everything before it was taken.
#[tokio::test]
async fn a_datagram_lane_is_its_sender() {
    let dir = std::env::temp_dir().join(format!("voxl-lanes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lane-datagram");
    let _ = std::fs::remove_file(&path);
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let socket = super::listen::datagram(&path).unwrap();
    tokio::spawn(datagrams(socket, hub.clone()));
    let mut a = Relay::open(&d).await;
    let (ca, _) = a.greeted().await;
    let mut b = Relay::open(&d).await;
    let (cb, _) = b.greeted().await;
    let peer = |name: &str| {
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        std::os::unix::net::UnixDatagram::bind(&p).unwrap()
    };
    let attach = |c: &Capability| attach(c, Generation::first());
    let p = peer("p");
    p.set_nonblocking(true).unwrap();
    p.send_to(&attach(&Capability::mint().unwrap()), &path)
        .unwrap();
    p.send_to(&attach(&ca), &path).unwrap();
    assert_eq!(a.next().await.0, Said::Attached);
    let mut buf = [0u8; 64];
    assert!(p.recv(&mut buf).is_err(), "a datagram lane was answered");
    // From the attached sender: an attach for b, and bytes that are nothing.
    p.send_to(&attach(&cb), &path).unwrap();
    p.send_to(b"nothing at all", &path).unwrap();
    let q = peer("q");
    q.send_to(&attach(&cb), &path).unwrap();
    assert_eq!(b.next().await.0, Said::Attached);
    assert!(b.silent().await, "b heard of a lane from a's sender");
    assert!(a.silent().await);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Once its control connection closes, a session's lanes get no byte more: frames sent
/// before the close reach the lane as it is read, in order, the lane then ends, and frames
/// sent after it make nothing; another lane attaching with its capability is refused.
#[tokio::test]
async fn after_the_close_no_lane_gets_a_byte() {
    use crate::lanes::wire::{Lane, Number};
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    let mut lane = stream(&hub, &attach_framed(&c, Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let client = hub.clients.lock()[0].clone();
    let frame = bytes::Bytes::from(vec![7u8; 3000]);
    for _ in 0..5 {
        client.lanes(|s| s.video(Number::first(), &frame, Instant::now()));
    }
    let mut framer = Framer::stream();
    let mut buf = [0u8; 4096];
    let mut carried = 0;
    while carried < 5 {
        let n = lane.read(&mut buf).await.unwrap();
        assert!(n > 0, "the lane ended before the close");
        framer.push(&buf[..n]);
        while let Some(m) = framer.frame().unwrap() {
            let Ok(Lane::Frame { frame, .. }) = Lane::decode(m) else {
                panic!("a stream lane carries frames");
            };
            assert_eq!(frame.get(), carried, "in order");
            carried += 1;
        }
    }
    drop(relay);
    while !hub.clients.lock().is_empty() {
        tokio::task::yield_now().await;
    }
    for _ in 0..5 {
        client.lanes(|s| s.video(Number::first(), &frame, Instant::now()));
        client.lanes(|s| s.audio(&frame[..200], 0, Instant::now()));
    }
    assert!(
        to_the_end(&mut lane).await.is_empty(),
        "a byte after the close"
    );
    // Its capability is forgotten: a lane presenting it is refused.
    let mut a = attach(&c, Generation::first());
    a[1..33].copy_from_slice(&raw);
    let again = Attach::decode(&a).unwrap().capability;
    let mut late = stream(&hub, &attach_framed(&again, Generation::first())).await;
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
    let _lane = stream(&hub, &attach_framed(&c, Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let mut refused = stream(
        &hub,
        &attach_framed(&Capability::mint().unwrap(), Generation::first()),
    )
    .await;
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
        let mut refused = stream(
            &hub,
            &attach_framed(&Capability::mint().unwrap(), Generation::first()),
        )
        .await;
        assert!(to_the_end(&mut refused).await.is_empty(), "{q:?}");
    }
    let stats = query(&d, Query::Stats).await;
    assert_eq!(stats[0], 0);
    let stats: serde_json::Value = serde_json::from_slice(&stats[1..]).unwrap();
    let clients = stats["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 1, "{stats}");
    assert_eq!(clients[0]["way"]["way"], "unattached");
    let _lane = stream(&hub, &attach_framed(&c, Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let stats = query(&d, Query::Stats).await;
    let stats: serde_json::Value = serde_json::from_slice(&stats[1..]).unwrap();
    assert_eq!(stats["clients"][0]["way"]["way"], "stream", "{stats}");
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
/// where the proxy needs it: the lane sockets are the group's too (0660), the control
/// socket and the keymap scratch file the user's alone (0600), whatever the umask, and
/// whatever mode a keymap a crashed voxl left had.
#[tokio::test]
async fn only_the_lanes_are_the_groups() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("voxl-entries-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o710)).unwrap();
    let runtime = super::Runtime::open(dir.clone()).unwrap();
    let sockets = runtime.sockets();
    let _control = super::listen::control(&sockets.control).unwrap();
    let _stream = super::listen::stream(&sockets.stream).unwrap();
    let _datagram = super::listen::datagram(&sockets.datagram).unwrap();
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
            ("lane-datagram".into(), 0o660),
            ("lane-stream".into(), 0o660),
        ]
    );
    assert_eq!(std::fs::read(runtime.keymap()).unwrap(), b"92 109");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A datagram lane's queue as its socket takes it: each message out, stamped at `now`, decoded
/// with its bytes.
#[cfg(test)]
fn accepted(queue: &super::sender::Queue, now: Instant) -> Vec<(Vec<u8>, wire::Head, Stamp)> {
    let mut out = Vec::new();
    while let Some((m, _)) = queue.take(now) {
        if let Ok(wire::Lane::Piece(s) | wire::Lane::Repair(s)) = wire::Lane::decode(&m) {
            let (head, sent) = (s.head, s.sent);
            out.push((m, head, sent));
        }
    }
    out
}

/// The room never waits on a report: over a datagram lane whose socket takes what it holds now
/// and then, fed generated video sizes at generated times, the room is the same before and
/// after any report comes back, decoded or missed. A room that counted unreported bytes (the
/// in-flight cap that wedged a session when its reports stopped) fails this.
#[test]
fn the_room_never_waits_on_a_report() {
    use crate::lanes::wire::{Arrivals, Fate, Late, Number, Outcome, Report};
    use crate::session::sender::Sender;
    let mut rng = crate::xorshift::Xorshift::new(0x7a3d_1c0e_9b42_f5a1);
    let mut now = Instant::now();
    let (mut s, queue, _control) = Sender::with_lane(now);
    let mut number = Number::first();
    let mut unreported: std::collections::VecDeque<(wire::Head, Stamp)> = Default::default();
    let mut checked = 0;
    for _ in 0..2_000 {
        now += Duration::from_micros(rng.range(0, 40_000));
        if rng.below(2) == 0 {
            let frame = bytes::Bytes::from(vec![0u8; rng.range(1, 30_000) as usize]);
            s.video(number, &frame, now);
            number = number.next();
        }
        if rng.below(3) == 0 {
            for (_, head, sent) in accepted(&queue, now) {
                if unreported.back().is_none_or(|(h, _)| h.block != head.block) {
                    unreported.push_back((head, sent));
                }
            }
        }
        while rng.below(2) == 0 {
            let Some((head, sent)) = unreported.pop_front() else {
                break;
            };
            let n = head.n();
            let bits = vec![0xffu8; n.div_ceil(8)];
            let mut bits = bits;
            if n % 8 != 0 {
                *bits.last_mut().expect("a byte") = (1u8 << (n % 8)) - 1;
            }
            let delays = vec![0u8; 4 * n];
            let outcome = match rng.below(4) {
                0 => Outcome::Missed,
                _ => Outcome::Decoded,
            };
            let report = Report {
                block: head.block,
                sent,
                fate: Fate::Seen {
                    outcome,
                    arrivals: Arrivals::new(n as u16, &bits, &delays).expect("arrivals"),
                },
                late: Late::new(&[], head.block).expect("none late"),
            };
            let before = s.room();
            s.reported(&report, now).expect("its own block, in order");
            assert_eq!(s.room(), before, "a report moved the room");
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked} reports");
}

/// A datagram block's deadline is the moment the socket took its first piece, the block's own
/// pieces behind it drained at the measured rate, and the base delay and the spread: with no
/// report yet and no drain measured, exactly the first piece's stamp and the relay's round trip
/// (100 ms here), the same in every piece of the block, each piece stamped as the socket took
/// it. A deadline that waited a stall, a regime or a pacing schedule fails this.
#[test]
fn a_deadline_is_the_acceptance_and_the_wait() {
    use crate::lanes::wire::{tenths, Number};
    use crate::session::sender::Sender;
    let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_fb41_bd6b);
    let start = Instant::now();
    let mut now = start;
    let (mut s, queue, _control) = Sender::with_lane(now);
    let mut number = Number::first();
    for _ in 0..300 {
        now += Duration::from_millis(rng.range(1, 100));
        let frame = bytes::Bytes::from(vec![0u8; rng.range(1, 30_000) as usize]);
        s.video(number, &frame, now);
        number = number.next();
        let taken = accepted(&queue, now);
        let stamp = Stamp::since(start, now);
        let (_, first, _) = taken.first().expect("a block");
        for (_, head, sent) in &taken {
            assert_eq!(*sent, stamp, "stamped as the socket took it");
            assert_eq!(head.due, first.due, "one deadline a block");
            assert_eq!(
                head.due,
                stamp.plus(tenths(Duration::from_millis(100))),
                "the acceptance and the wait"
            );
        }
    }
}

/// A stream lane has no report path and no code: every frame sent is one message, in order,
/// whole, with its track; it is delivered whole; and a report is refused, since no block was
/// ever sent on it.
#[test]
fn a_stream_lane_has_no_reports() {
    use crate::lanes::wire::{Fate, Generation, Lane, Late, Number, Report, Seq, Track};
    use crate::session::sender::{Broken, Delivery, Kind, Queue, Sender};
    let mut rng = crate::xorshift::Xorshift::new(0x5be0_cd19_137e_2179);
    let now = Instant::now();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut s = Sender::new(Capability::mint().expect("random"), tx, now).expect("a socket");
    let queue = Queue::new(Kind::Stream, now, Arc::new(|| {}));
    assert!(s.attach(
        Generation::first(),
        queue.clone(),
        Stamp::since(now, now),
        Duration::from_millis(100),
        now
    ));
    assert_eq!(s.delivery(), Some(Delivery::Whole));
    let mut sent = Vec::new();
    let mut number = Number::first();
    for _ in 0..500 {
        match rng.below(2) {
            0 => {
                let frame = bytes::Bytes::from(vec![7u8; rng.range(1, 100_000) as usize]);
                s.video(number, &frame, now);
                sent.push((Track::Video { number }, frame.len()));
                number = number.next();
            }
            _ => {
                let packet = vec![3u8; rng.range(1, 400) as usize];
                s.audio(&packet, 0, now);
                sent.push((Track::Audio, packet.len()));
            }
        }
    }
    let mut got = Vec::new();
    let mut next = Seq::first();
    let mut framer = Framer::stream();
    while let Some((m, block)) = queue.take(now) {
        assert_eq!(block, None, "a stream lane's message is of no block");
        framer.push(&m);
        while let Some(f) = framer.frame().expect("framed") {
            let Ok(Lane::Frame {
                frame,
                track,
                bytes,
            }) = Lane::decode(f)
            else {
                panic!("a stream lane carries frames");
            };
            assert_eq!(frame, next, "in order");
            next = next.next();
            got.push((track, bytes.len()));
        }
    }
    assert_eq!(got, sent, "every frame, whole");
    let report = Report {
        block: Seq::first(),
        sent: Stamp::since(now, now),
        fate: Fate::Unseen,
        late: Late::new(&[], Seq::first()).expect("none late"),
    };
    assert_eq!(s.reported(&report, now), Err(Broken::Unsent(Seq::first())));
}

/// The drain is the transport's rate: behind the socket a transport queue of `room` bytes
/// drains at `rate` bytes a second, and bursts of equal messages arrive after generated quiet
/// spells. A message the socket refuses is taken the moment the queue has room for it; each
/// such message but a burst's first was refused right after the one before it filled the queue,
/// so its time is exactly its own drain, and the first's is at most that. Over any window of
/// [`RATE_MEMORY`] refused messages, then, the drain is at least the rate and above it by at
/// most the window's bursts' first messages. A drain that counted the messages the queue took
/// at once (a burst after a quiet spell fills it at no cost) reads the rate several times over,
/// and fails this.
#[test]
fn the_drain_is_the_transports_rate() {
    use crate::lanes::path::RATE_MEMORY;
    use crate::session::sender::Drain;
    let mut rng = crate::xorshift::Xorshift::new(0x2f6b_5d0a_83c1_e947);
    let start = Instant::now();
    let at = |t: f64| start + Duration::from_secs_f64(t);
    for _ in 0..200 {
        let len = rng.range(100, 1201) as usize;
        let rate = rng.range(100_000, 10_000_001) as f64;
        let held = rng.range(1, 33) as usize;
        let room = held * len;
        let burst = held + rng.range(10, 61) as usize;
        let firsts = RATE_MEMORY.div_ceil(burst - held) + 1;
        let most = rate * RATE_MEMORY as f64 / (RATE_MEMORY - firsts) as f64;
        let mut drain = Drain::new();
        // The transport queue's bytes as of `then`.
        let (mut holding, mut then) = (0.0f64, 0.0f64);
        let mut now = 0.0f64;
        let mut checked = 0;
        for _ in 0..50 {
            now += rng.range(0, 2 * (burst * len) as u64) as f64 / rate;
            let queued = now;
            for _ in 0..burst {
                let left = (holding - rate * (now - then)).max(0.0);
                let over = left + len as f64 - room as f64;
                if over > 0.0 {
                    drain.refused();
                    now += over / rate;
                }
                holding = (holding - rate * (now - then)).max(0.0) + len as f64;
                then = now;
                drain.accepted(len, at(queued), at(now));
                if let Some(read) = drain.rate() {
                    assert!(
                        read >= rate * (1.0 - 1e-3) && read <= most * (1.0 + 1e-3),
                        "drain {read:.0} of a transport at {rate:.0} (at most {most:.0}): \
                         {len} B messages, bursts of {burst} into {room} B"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0);
    }
}

/// Deadlines follow the blocks' order, whatever the wait does between them. The relay decides
/// a block nothing of which came once a later block's deadline has passed, and with it every
/// frame before that later block's; that is sound only while no earlier block is still due
/// after it. Over generated waits that shrink between two blocks by more than the time between
/// them (three blocks: the first of which nothing comes, the second part of which comes on
/// time, the third of which a piece comes just past its own deadline), stamped by the daemon's
/// queue and taken by the relay's decoder: no block comes out whole once a decision has
/// released its frames. Without the daemon keeping each deadline at or after the one before,
/// the third block's deadline falls before the second's, the first is decided with the
/// second's frame, and the second then comes out whole.
#[test]
fn no_frame_is_released_before_an_earlier_one_is_decided() {
    use crate::lanes::code::{Decoder, Took};
    use crate::lanes::wire::{Lane, Number, SYMBOL};
    use crate::session::sender::Sender;
    let mut rng = crate::xorshift::Xorshift::new(0x510e_527f_ade6_82d1);
    for _ in 0..300 {
        let start = Instant::now();
        let mut now = start;
        let (mut s, queue, _control) = Sender::with_lane(now);
        let mut number = Number::first();
        let first_wait = rng.range(1_000, 3_000) as i64;
        let mut blocks = Vec::new();
        for i in 0..3 {
            // The wait, in tenths: the third block's shrinks by more than the time since the
            // second's (under 50).
            queue.lock().after = match i {
                2 => rng.range(0, (first_wait - 60) as u64) as i64,
                _ => first_wait,
            };
            let bytes = rng.range(2 * SYMBOL as u64, 20_000) as usize;
            s.video(number, &bytes::Bytes::from(vec![1u8; bytes]), now);
            number = number.next();
            blocks.push(accepted(&queue, now));
            now += Duration::from_micros(100 * rng.range(1, 50));
        }
        let [_, second, third] = &blocks[..] else {
            panic!("three blocks")
        };
        let mut dec = Decoder::new();
        let on_time = second[0].2.plus(1);
        let took = dec
            .take(Lane::decode(&second[0].0).unwrap(), on_time)
            .unwrap();
        assert_eq!(took, Took::Held, "a block of two sources or more, one come");
        // Just past the third block's own deadline.
        let past = third[0].1.due.plus(1);
        assert_eq!(
            dec.take(Lane::decode(&third[0].0).unwrap(), past).unwrap(),
            Took::Late
        );
        let mut released = None;
        while let Some(d) = dec.decide(past) {
            released = Some(d.last);
        }
        for (m, _, _) in &second[1..] {
            if let Took::Whole(head, _) = dec.take(Lane::decode(m).unwrap(), past).unwrap() {
                assert!(
                    released.is_none_or(|r| head.first > r),
                    "block {:?} whole after its frame {:?} was released",
                    head.block,
                    head.first
                );
            }
        }
    }
}

/// The capture gate is open while no lane is live and closed only by a live lane's backlog.
/// Over generated sequences on a sender (lanes of one kind attaching, generation after
/// generation; video updates of generated sizes; the socket taking some of what waits; the
/// socket breaking), after every step: the room is unbounded exactly when no lane is live or
/// the live lane's queue is empty, the sender counts as attached exactly while a lane is live,
/// and each live lane that breaks is told exactly once, with its generation. A gate that read
/// a lost lane's backlog froze its session until a reload.
#[test]
fn only_a_live_lanes_backlog_gates_the_capture() {
    use crate::hub::Out;
    use crate::lanes::wire::Number;
    use crate::session::sender::{Kind, Queue, Room, Sender};
    let mut rng = crate::xorshift::Xorshift::new(0x9b05_688c_2b3e_6c1f);
    for case in 0..200 {
        let now = Instant::now();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = Sender::new(Capability::mint().unwrap(), tx, now).unwrap();
        let kind = match case % 2 {
            0 => Kind::Datagram,
            _ => Kind::Stream,
        };
        let (mut queue, mut live): (Option<(Arc<Queue>, Generation)>, bool) = (None, false);
        let (mut generation, mut number) = (Generation::first(), Number::first());
        let mut lost = Vec::new();
        for _ in 0..rng.range(1, 60) {
            match rng.below(4) {
                0 => {
                    let q = Queue::new(kind, now, Arc::new(|| {}));
                    let rtt = Duration::from_millis(100);
                    assert!(s.attach(generation, q.clone(), Stamp::since(now, now), rtt, now));
                    (queue, live) = (Some((q, generation)), true);
                    generation = generation.next();
                }
                1 => {
                    let frame = bytes::Bytes::from(vec![3u8; rng.range(1, 30_000) as usize]);
                    s.video(number, &frame, now);
                    number = number.next();
                }
                2 if live => {
                    if let Some((q, _)) = &queue {
                        for _ in 0..rng.below(20) {
                            q.take(now);
                        }
                    }
                }
                _ => {
                    if let Some((q, g)) = &queue {
                        q.lose();
                        if live {
                            lost.push(*g);
                        }
                        live = false;
                    }
                }
            }
            let waits = queue.as_ref().map_or(0, |(q, _)| q.backlog_bytes());
            assert_eq!(
                s.room() == Room::Unbounded,
                !live || waits == 0,
                "live {live}, {waits} B waiting: {:?}",
                s.room()
            );
            assert_eq!(s.attached(), live);
        }
        let mut told = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let Out::Lost { generation } = m {
                told.push(generation);
            }
        }
        assert_eq!(told, lost, "each lost lane told once");
    }
}

/// A lost lane is told, and the next carries what waited. Over generated sequences on a
/// session with stream lanes (video updates of generated sizes; the relay reading what has
/// come; the relay's end of the lane vanishing at any point: idle, mid-frame, or just after
/// its attach): the daemon says `Lost` exactly once for each lane whose socket broke, with
/// its generation; the relay attaches the next generation; and the frames read whole across
/// the lanes are every frame sent, each once, in order. The relay reads all that has come
/// before its end vanishes, so what the lost socket held is read; a frame it held only part
/// of goes again whole on the next lane.
#[tokio::test]
async fn a_lost_lane_is_told_and_the_next_carries_what_waited() {
    use crate::lanes::wire::{Lane, Number};
    let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_5be0_cd19);
    // Lanes lost with nothing waiting, with frames waiting, and with a frame read in part.
    let (mut idle, mut waiting, mut partial) = (0, 0, 0);
    for _ in 0..30 {
        let hub = Hub::new().unwrap();
        let d = daemon(&hub);
        let mut relay = Relay::open(&d).await;
        let (c, _) = relay.greeted().await;
        let mut generation = Generation::first();
        let mut lane = stream(&hub, &attach_framed(&c, generation)).await;
        assert_eq!(relay.next().await.0, Said::Attached);
        let client = hub.clients.lock()[0].clone();
        let mut framer = Framer::stream();
        let (mut sent, mut got) = (0u64, Vec::new());
        // The current lane's bytes read, and those of the frames read whole.
        let mut counts = (0usize, 0usize);
        // Everything that has come on the lane, read without waiting.
        let read = |lane: &mut UnixStream,
                    framer: &mut Framer,
                    got: &mut Vec<u64>,
                    counts: &mut (usize, usize)| {
            let mut buf = [0u8; 8192];
            while let Ok(n @ 1..) = lane.try_read(&mut buf) {
                framer.push(&buf[..n]);
                counts.0 += n;
            }
            while let Some(m) = framer.frame().unwrap() {
                counts.1 += wire::STREAM_PREFIX + m.len();
                let Ok(Lane::Frame { frame, .. }) = Lane::decode(m) else {
                    panic!("a stream lane carries frames");
                };
                got.push(frame.get());
            }
        };
        let send = |sent: &mut u64, rng: &mut crate::xorshift::Xorshift| {
            let frame = bytes::Bytes::from(vec![5u8; rng.range(1, 6000) as usize]);
            client.lanes(|s| s.video(Number::first(), &frame, Instant::now()));
            *sent += 1;
        };
        for _ in 0..rng.range(1, 30) {
            match rng.below(4) {
                0 | 1 => send(&mut sent, &mut rng),
                2 => {
                    settle().await;
                    read(&mut lane, &mut framer, &mut got, &mut counts);
                }
                _ => {
                    settle().await;
                    read(&mut lane, &mut framer, &mut got, &mut counts);
                    match client.lanes(|s| s.stats()) {
                        crate::session::sender::Way::Stream {
                            backlog_bytes: 0, ..
                        } => idle += 1,
                        _ => waiting += 1,
                    }
                    partial += usize::from(counts.0 > counts.1);
                    drop(lane);
                    // The next frame finds the socket broken, if what waited did not.
                    send(&mut sent, &mut rng);
                    assert_eq!(relay.said().await, Some(Said::Lost(generation)));
                    generation = generation.next();
                    lane = stream(&hub, &attach_framed(&c, generation)).await;
                    assert_eq!(relay.next().await.0, Said::Attached);
                    (framer, counts) = (Framer::stream(), (0, 0));
                }
            }
        }
        while got.len() < sent as usize {
            settle().await;
            read(&mut lane, &mut framer, &mut got, &mut counts);
        }
        assert_eq!(
            got,
            (0..sent).collect::<Vec<_>>(),
            "every frame, once, in order"
        );
        assert!(relay.silent().await, "a lane told lost twice");
    }
    assert!(
        idle > 0 && waiting > 0 && partial > 0,
        "lost idle {idle}, waiting {waiting}, mid-frame {partial}"
    );
}
