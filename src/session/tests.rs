//! The session's properties, each against what the relay's side of the sockets sees: a
//! still desktop sends nothing; a lane attaches only with its session's capability, and
//! nothing else from a lane counts; a closed session's lanes get no byte more; and the
//! capability goes nowhere but the control stream.

use super::control::{connection, Daemon};
use super::lane::{datagrams, stream_lane};
use super::query::Opening;
use crate::hub::Hub;
use crate::lanes::model::Lanes;
use crate::lanes::wire::{
    self, Attach, Capability, Down, Framer, Generation, LaneIndex, Stamp, Up,
};
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
    Lanes,
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
            if let Some(m) = self.framer.frame().unwrap() {
                let raw = m.to_vec();
                return match Down::decode(&raw).unwrap() {
                    Down::Capability(c) => (Said::Capability(raw[1..].to_vec()), Some(c.keep())),
                    Down::Attached { .. } => (Said::Attached, None),
                    Down::Lanes(_) => (Said::Lanes, None),
                    Down::Text(t) => (Said::Text(t.into()), None),
                    Down::Binary(b) => (Said::Binary(b.into()), None),
                };
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

fn first_lane() -> LaneIndex {
    LaneIndex::all(Lanes::ONE).next().unwrap()
}

/// A lane's attach as a stream lane writes it: its length, then it.
fn attach_framed(c: &Capability, lane: LaneIndex, generation: Generation) -> Vec<u8> {
    let now = Instant::now();
    let a = Attach::encode(c, lane, generation, Stamp::since(now, now));
    [&wire::lane_prefix(a.len())[..], &a].concat()
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
    let lane = stream(&hub, &attach_framed(&c, first_lane(), Generation::first())).await;
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
        let mut a = Attach::encode(
            &c,
            first_lane(),
            Generation::first(),
            Stamp::since(Instant::now(), Instant::now()),
        );
        a[1..33].copy_from_slice(&changed);
        wrong.push(Attach::decode(&a).unwrap().capability);
    }
    for w in &wrong {
        let mut lane = stream(&hub, &attach_framed(w, first_lane(), Generation::first())).await;
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
    let _lane = stream(&hub, &attach_framed(&c, first_lane(), Generation::first())).await;
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
    let attach = |c: &Capability| {
        let now = Instant::now();
        Attach::encode(c, first_lane(), Generation::first(), Stamp::since(now, now))
    };
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
/// before the close reach the lane, the lane then ends, and frames sent after it make
/// nothing; another lane attaching with its capability is refused.
#[tokio::test]
async fn after_the_close_no_lane_gets_a_byte() {
    use crate::lanes::wire::{Epoch, Lane, Number};
    let hub = Hub::new().unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    let mut lane = stream(&hub, &attach_framed(&c, first_lane(), Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let client = hub.clients.lock()[0].clone();
    let frame = bytes::Bytes::from(vec![7u8; 3000]);
    for _ in 0..5 {
        client.lanes(|s| s.video((Number::first(), Epoch::Start), &frame, Instant::now()));
    }
    drop(relay);
    while !hub.clients.lock().is_empty() {
        tokio::task::yield_now().await;
    }
    for _ in 0..5 {
        client.lanes(|s| s.video((Number::first(), Epoch::Start), &frame, Instant::now()));
        client.lanes(|s| s.audio(&frame[..200], Instant::now()));
    }
    let carried = to_the_end(&mut lane).await;
    // Every message the lane carried is of the five frames sent before the close.
    let mut framer = Framer::lane();
    framer.push(&carried);
    let mut messages = 0;
    while let Some(m) = framer.frame().unwrap() {
        let l = Lane::decode(m).unwrap();
        assert!(
            l.symbol().head.block.get() < 5,
            "a frame sent after the close"
        );
        messages += 1;
    }
    // The frames before the close were carried, so the lane was open until it.
    assert!(messages > 0, "the lane carried nothing at all");
    // Its capability is forgotten: a lane presenting it is refused.
    let mut a = Attach::encode(
        &c,
        first_lane(),
        Generation::first(),
        Stamp::since(Instant::now(), Instant::now()),
    );
    a[1..33].copy_from_slice(&raw);
    let again = Attach::decode(&a).unwrap().capability;
    let mut late = stream(
        &hub,
        &attach_framed(&again, first_lane(), Generation::first()),
    )
    .await;
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
    let _lane = stream(&hub, &attach_framed(&c, first_lane(), Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let mut refused = stream(
        &hub,
        &attach_framed(
            &Capability::mint().unwrap(),
            first_lane(),
            Generation::first(),
        ),
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
            &attach_framed(
                &Capability::mint().unwrap(),
                first_lane(),
                Generation::first(),
            ),
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
    let _lane = stream(&hub, &attach_framed(&c, first_lane(), Generation::first())).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let stats = query(&d, Query::Stats).await;
    let stats: serde_json::Value = serde_json::from_slice(&stats[1..]).unwrap();
    assert_eq!(stats["clients"][0]["way"]["way"], "lanes", "{stats}");
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

/// A sharpening pass never takes room the next capture needs: over a sender's lane
/// carried by a model link (a bottleneck of a generated rate, a fixed round trip, each
/// block reported decoded, missed or unseen once its last byte is through and half the
/// round trip has passed), fed generated video sizes at generated times, the room a pass
/// may take is the room the lanes have less what the frames' share of the pacing sends in
/// one frame interval, and none when that is all the room there is; both read from the
/// sender's own `voxl stats`. Every input is drawn from a seeded generator and every time
/// is the test's own, counted from one reading of the clock, so a run is the same run.
#[test]
fn a_pass_never_takes_room_the_next_capture_needs() {
    use crate::lanes::wire::{Epoch, Number, Outcome};
    use crate::session::sender::{Room, Sender, Way};
    let mut rng = crate::xorshift::Xorshift::new(0x5be0_cd19_137e_2179);
    let start = Instant::now();
    let mut now = start;
    let (mut s, queue, _control) = Sender::with_lane(now);
    let mut number = Number::first();
    // The model link: bytes a second through its bottleneck, and its round trip.
    let rate = rng.range(200_000, 5_000_000) as f64;
    let rtt = Duration::from_millis(rng.range(10, 120));
    let mut busy_until = start;
    let mut in_flight = std::collections::VecDeque::new();
    let (mut checked, mut spared) = (0, 0);
    for _ in 0..1_000 {
        now += Duration::from_micros(rng.range(0, 40_000));
        if rng.below(2) == 0 {
            let frame = bytes::Bytes::from(vec![0u8; rng.range(1, 20_000) as usize]);
            s.video((number, Epoch::Start), &frame, now);
            number = number.next();
        }
        for block in queue.frames() {
            let bytes: usize = block.members.iter().map(|(_, b)| b.len()).sum();
            busy_until = busy_until.max(now) + Duration::from_secs_f64(bytes as f64 / rate);
            in_flight.push_back((busy_until + rtt / 2, block));
        }
        while in_flight.front().is_some_and(|(at, _)| *at <= now) {
            let (at, block) = in_flight.pop_front().expect("one is due");
            // One block in 25 unseen, and one missed.
            let outcome = match rng.below(25) {
                0 => None,
                1 => Some(Outcome::Missed),
                _ => Some(Outcome::Decoded),
            };
            block
                .report(outcome, |r| s.reported(r, at))
                .expect("its own block");
        }
        let frame = Duration::from_micros(rng.range(1_000, 100_000));
        let Way::Lanes {
            data_bytes_per_s, ..
        } = s.stats()
        else {
            panic!("a lane is attached");
        };
        let need = (data_bytes_per_s * frame.as_secs_f64()) as usize;
        match (s.room(), s.spare(frame)) {
            (Room::Open(room), Room::Open(spare)) => {
                assert_eq!(room - spare, need, "room {room}, spare {spare}");
                spared += 1;
            }
            (Room::Open(room), Room::Full) => assert!(room <= need, "{room} <= {need}"),
            (Room::Full, Room::Full) => {}
            (room, spare) => panic!("room {room:?}, spare {spare:?}"),
        }
        checked += 1;
    }
    assert!(
        spared > 20 && checked - spared > 20,
        "{spared} of {checked}"
    );
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
