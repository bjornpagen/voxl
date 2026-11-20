//! The session's properties, each against what the relay's side of the sockets sees: a
//! still desktop sends nothing; a lane attaches only with its session's capability, and
//! nothing else from a lane counts; a closed session's lane gets no byte more; a lane whose
//! socket breaks holds no capture back; what the relay says the page took reaches the
//! session's link, by each frame's name, and a took of a lane since replaced counts for
//! nothing; and the capability goes nowhere but the control stream.

use super::control::{connection, Daemon};
use super::query::Opening;
use crate::event::{BinUp, MicMsg};
use crate::hub::Hub;
use crate::session::link::Path;
use crate::session::sender::Way;
use crate::wire::Byte;
use crate::wire::{Capability, Down, Frame, Framer, Holds, Sequence, Track, Up};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn daemon(hub: &Arc<Hub>) -> Arc<Daemon> {
    // A microphone thread that has ended: these tests send it nothing they read back.
    daemon_heard(hub).0
}

/// A daemon of `hub`'s, and what its microphone's thread is sent.
fn daemon_heard(hub: &Arc<Hub>) -> (Arc<Daemon>, std::sync::mpsc::Receiver<MicMsg>) {
    let (mic, heard) = std::sync::mpsc::channel();
    let d = Arc::new(Daemon {
        hub: hub.clone(),
        audio: super::control::Audio::Sent,
        mic,
        max_record: Duration::from_secs(300),
    });
    (d, heard)
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

/// A lane's first bytes: the capability it presents, and that its page holds nothing.
fn attach(c: &Capability) -> Vec<u8> {
    attach_holding(c, Holds::Nothing)
}

/// A lane's first bytes: the capability it presents, and what its page holds.
fn attach_holding(c: &Capability, holds: Holds) -> Vec<u8> {
    let mut first = c.bytes().to_vec();
    holds.encode(&mut first);
    first
}

/// A lane opened to `hub` with `bytes` first: the relay's end.
async fn open_lane(hub: &Arc<Hub>, bytes: &[u8]) -> UnixStream {
    let (mut relay, ours) = UnixStream::pair().unwrap();
    tokio::spawn(super::lane::lane(ours, hub.clone()));
    relay.write_all(bytes).await.unwrap();
    relay
}

/// Writes `bytes` on `lane` as one message that hands over `descriptors`, as a proxy hands
/// a statement over with a stream's first bytes.
fn hand(lane: &UnixStream, bytes: &[u8], descriptors: &[std::os::fd::RawFd]) {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use std::os::fd::AsRawFd;
    let rights = [ControlMessage::ScmRights(descriptors)];
    let with: &[ControlMessage] = match descriptors {
        [] => &[],
        [_, ..] => &rights,
    };
    let iov = [std::io::IoSlice::new(bytes)];
    let sent = sendmsg::<()>(lane.as_raw_fd(), &iov, with, MsgFlags::empty(), None).unwrap();
    assert_eq!(sent, bytes.len());
}

/// The path of `client`'s lane, as `voxl stats` says it.
fn path_of(client: &crate::hub::Client) -> Path {
    match client.lane(|s| s.stats(Instant::now())) {
        Way::Lane(stats) => stats.path,
        way @ (Way::Unattached | Way::Recording | Way::Closed) => panic!("no lane: {way:?}"),
    }
}

/// A lane takes its statement or nothing. Over generated lanes, each presenting its
/// session's capability in one to three writes, any of which hands descriptors over:
/// - one that hands none over attaches, and its path is not stated;
/// - one that hands over a descriptor that is no `SOCK_SEQPACKET` socket (a pipe's writing
///   end, a datagram socket), or more than one descriptor, in one write or across them, is
///   closed unanswered, as a lane with no capability is, and the session's lane before it
///   stays; and the daemon keeps no copy of what was handed over: once the test has let go
///   of its own, each pipe reads its end of file.
#[tokio::test]
async fn a_lane_takes_its_statement_or_nothing() {
    use std::os::fd::AsRawFd;
    let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_5be0_cd19);
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, _) = relay.greeted().await;
    let client = hub.clients.lock()[0].clone();
    let (mut bare, mut refused) = (0, 0);
    for run in 0..200 {
        let capability = attach(&c);
        // Where the capability's writes are cut, and which of them hands over how many.
        let cuts = [rng.range(1, 32) as usize, rng.range(1, 32) as usize];
        let (a, b) = (cuts[0].min(cuts[1]), cuts[0].max(cuts[1]));
        let writes = [&capability[..a], &capability[a..b], &capability[b..]];
        let mut with = [0usize; 3];
        for _ in 0..[0, 1, 2, 3][rng.below(4) as usize] {
            with[rng.below(3) as usize] += 1;
        }
        // The middle write may be empty (the cuts met): it is not sent, and hands nothing.
        if a == b {
            with[1] = 0;
        }
        let handed: usize = with.iter().sum();
        let datagram = rng.below(2) == 0;
        let (lane_end, ours) = UnixStream::pair().unwrap();
        tokio::spawn(super::lane::lane(ours, hub.clone()));
        let mut pipes = Vec::new();
        for (bytes, n) in writes.into_iter().zip(with) {
            let mut descriptors = Vec::new();
            for _ in 0..n {
                let (r, w) = nix::unistd::pipe().unwrap();
                descriptors.push(w);
                pipes.push(r);
            }
            let raw: Vec<_> = match (datagram, n) {
                // One descriptor alone may be a socket that keeps no record whole.
                (true, 1) if handed == 1 => {
                    let (s, _other) = std::os::unix::net::UnixDatagram::pair().unwrap();
                    descriptors.push(s.into());
                    vec![descriptors[1].as_raw_fd()]
                }
                _ => descriptors.iter().map(|d| d.as_raw_fd()).collect(),
            };
            if !bytes.is_empty() {
                hand(&lane_end, bytes, &raw);
            }
            // The test's own copies are let go here: the daemon's are the only ones.
        }
        let mut lane_end = lane_end;
        match handed {
            0 => {
                assert_eq!(relay.next().await.0, Said::Attached, "run {run}");
                assert_eq!(path_of(&client), Path::Unstated, "run {run}");
                bare += 1;
            }
            1.. => {
                assert!(
                    to_the_end(&mut lane_end).await.is_empty(),
                    "run {run}: a refused lane was answered"
                );
                assert!(
                    relay.silent().await,
                    "run {run}: a refused lane was confirmed"
                );
                for pipe in &pipes {
                    nix::fcntl::fcntl(
                        pipe,
                        nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
                    )
                    .unwrap();
                    let mut byte = [0u8; 1];
                    assert_eq!(
                        nix::unistd::read(pipe, &mut byte),
                        Ok(0),
                        "run {run}: a copy kept"
                    );
                }
                refused += 1;
            }
        }
    }
    assert!(bare > 20 && refused > 100, "{bare} {refused}");
}

/// A lane says what its page holds, and its viewer holds exactly that. Through the real
/// lane socket, over generated lanes of one session that watches or does not (`v,1`,
/// `v,0`), each saying after its capability that its page holds nothing or the stream
/// through a generated frame: the lane attaches, and the viewer is watching or not, as it
/// was, holding what the lane said, whatever it held or was before. A lane whose byte after
/// the capability is no tag of what a page holds, with or without bytes after it, is closed
/// unanswered, and the viewer is as it was.
#[tokio::test]
async fn a_lane_says_what_its_page_holds() {
    use crate::hub::Viewer;
    let mut rng = crate::xorshift::Xorshift::new(0xbb67_ae85_84ca_a73b);
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, _) = relay.greeted().await;
    let client = hub.clients.lock()[0].clone();
    let (mut through, mut nothing, mut refused, mut watched) = (0, 0, 0, 0);
    for run in 0..300 {
        let watches = rng.below(2) == 0;
        relay
            .say(Up::Text(["v,0", "v,1"][usize::from(watches)]))
            .await;
        let is_set = |v: Viewer| matches!(v, Viewer::NotWatching(_)) != watches;
        while !is_set(client.viewer()) {
            tokio::task::yield_now().await;
        }
        let before = client.viewer();
        if rng.below(5) == 0 {
            let mut first = c.bytes().to_vec();
            first.push(rng.range(2, 256) as u8);
            first.extend((0..rng.below(6)).map(|_| rng.next() as u8));
            let mut lane = open_lane(&hub, &first).await;
            drop(lane.shutdown().await);
            assert!(to_the_end(&mut lane).await.is_empty(), "run {run}");
            assert!(
                relay.silent().await,
                "run {run}: a refused lane was confirmed"
            );
            assert_eq!(client.viewer(), before, "run {run}");
            refused += 1;
            continue;
        }
        let holds = match rng.below(3) {
            0 => Holds::Nothing,
            1.. => Holds::Through(crate::wire::Version(rng.next() as u32)),
        };
        let _lane = open_lane(&hub, &attach_holding(&c, holds)).await;
        assert_eq!(relay.next().await.0, Said::Attached, "run {run}");
        let want = match watches {
            true => Viewer::Watching(holds),
            false => Viewer::NotWatching(holds),
        };
        assert_eq!(client.viewer(), want, "run {run}");
        match holds {
            Holds::Nothing => nothing += 1,
            Holds::Through(_) => through += 1,
        }
        watched += usize::from(watches);
    }
    assert!(
        through > 100 && nothing > 40 && refused > 30 && watched > 60,
        "{through} {nothing} {refused} {watched}"
    );
}

/// A statement's two ends: a `SOCK_SEQPACKET` pair, as the proxy makes one.
#[cfg(target_os = "linux")]
fn statement() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
    socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .unwrap()
}

/// A lane's path is what its statement says, and nothing once it ends. Through the real
/// lane socket's reader and a real `SOCK_SEQPACKET` pair, over generated lanes: the
/// reading end is handed over with the first of the capability's bytes, cut at a generated
/// place, and the test keeps no copy. The lane attaches with nothing known of its path;
/// each record written (generated round trips and rates, written as the proxy writes them)
/// is then the lane's path, and wakes the capture loop, which waits on the gate with no
/// time; the statement's end (its writing end closed, a message of another length, a
/// record with a zero field) leaves nothing known, wakes the loop, and the lane still
/// carries its frames. And a lane that hands over two such reading ends is refused.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_lanes_path_is_what_its_statement_says() {
    use crate::session::link::{BytesPerSecond, Known};
    use std::os::fd::AsRawFd;
    let mut rng = crate::xorshift::Xorshift::new(0x428a_2f98_d728_ae22);
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, _) = relay.greeted().await;
    let client = hub.clients.lock()[0].clone();
    let until = |want: Path| {
        let client = client.clone();
        async move {
            while path_of(&client) != want {
                tokio::task::yield_now().await;
            }
        }
    };
    // Two statements are none: a lane that hands over two reading ends is closed unanswered.
    let (one, _a) = statement();
    let (other, _b) = statement();
    let (mut two, ours) = UnixStream::pair().unwrap();
    tokio::spawn(super::lane::lane(ours, hub.clone()));
    hand(&two, &attach(&c), &[one.as_raw_fd(), other.as_raw_fd()]);
    assert!(to_the_end(&mut two).await.is_empty());
    assert!(
        relay.silent().await,
        "a lane with two statements was confirmed"
    );
    let (mut said, mut ends) = (0, [0; 3]);
    for run in 0..60 {
        let (reading, writing) = statement();
        let capability = attach(&c);
        let cut = rng.range(1, 33) as usize;
        let (mut lane, ours) = UnixStream::pair().unwrap();
        tokio::spawn(super::lane::lane(ours, hub.clone()));
        hand(&lane, &capability[..cut], &[reading.as_raw_fd()]);
        drop(reading);
        lane.write_all(&capability[cut..]).await.unwrap();
        assert_eq!(relay.next().await.0, Said::Attached, "run {run}");
        assert_eq!(path_of(&client), Path::Unknown, "run {run}");
        let record = |least: u64, rate: u64| [least.to_be_bytes(), rate.to_be_bytes()].concat();
        let write = |message: &[u8]| {
            let sent = nix::sys::socket::send(
                writing.as_raw_fd(),
                message,
                nix::sys::socket::MsgFlags::empty(),
            );
            assert_eq!(sent, Ok(message.len()));
        };
        for _ in 0..rng.below(6) {
            let (least, rate) = (rng.range(1, 1 << 40), rng.range(1, 1 << 40));
            settle().await;
            hub.drain_wake();
            write(&record(least, rate));
            until(Path::Known(Known {
                round_trip: Duration::from_nanos(least),
                rate: BytesPerSecond(rate),
            }))
            .await;
            assert!(woken(&hub), "run {run}: a path stated woke nothing");
            said += 1;
        }
        settle().await;
        hub.drain_wake();
        let how = rng.below(3) as usize;
        match how {
            0 => drop(writing),
            1 => {
                let len = [1, 8, 15, 17, 24][rng.below(5) as usize];
                write(&vec![7u8; len]);
            }
            2.. => {
                let zero = [(0, 5), (5, 0), (0, 0)][rng.below(3) as usize];
                write(&record(zero.0, zero.1));
            }
        }
        // Nothing may have been known before the end either: the wake is what tells it.
        while !woken(&hub) {
            tokio::task::yield_now().await;
        }
        assert_eq!(path_of(&client), Path::Unknown, "run {run}");
        ends[how] += 1;
        let frame = bytes::Bytes::from(vec![run as u8; 2_000]);
        client.lane(|s| s.video(&frame, Instant::now()));
        let (_, track, bytes) = next_frame(&mut lane, &mut Framer::new()).await;
        assert_eq!((track, &bytes[..]), (Track::Video, &frame[..]), "run {run}");
    }
    assert!(said > 60 && ends.iter().all(|n| *n > 5), "{said} {ends:?}");
}

/// The next frame a lane carries, read with `framer`: its name, its track and its bytes.
async fn next_frame(lane: &mut UnixStream, framer: &mut Framer) -> (Sequence, Track, Vec<u8>) {
    loop {
        if let Some(m) = framer.frame().unwrap() {
            let f = Frame::decode(m).expect("a lane carries frames");
            return (f.sequence, f.track, f.bytes.to_vec());
        }
        let mut buf = [0u8; 8192];
        let n = lane.read(&mut buf).await.unwrap();
        assert!(n > 0, "the lane ended");
        framer.push(&buf[..n]);
    }
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
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
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
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
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
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, raw) = relay.greeted().await;
    let mut lane = open_lane(&hub, &attach(&c)).await;
    assert_eq!(relay.next().await.0, Said::Attached);
    let client = hub.clients.lock()[0].clone();
    for i in 0..5u8 {
        let frame = bytes::Bytes::from(vec![i; 3000]);
        client.lane(|s| s.video(&frame, Instant::now()));
    }
    let mut framer = Framer::new();
    for carried in 0..5 {
        let (_, track, bytes) = next_frame(&mut lane, &mut framer).await;
        assert_eq!((track, bytes[0]), (Track::Video, carried), "in order");
    }
    let frame = bytes::Bytes::from(vec![7u8; 3000]);
    drop(relay);
    while !hub.clients.lock().is_empty() {
        tokio::task::yield_now().await;
    }
    for _ in 5..10 {
        client.lane(|s| s.video(&frame, Instant::now()));
        client.lane(|s| s.audio(&frame[..200], 0, Instant::now()));
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
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
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
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
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
        let hub = Hub::new(crate::hub::tests::hello()).unwrap();
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
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
    let d = daemon(&hub);
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
        match mode & 0o002 == 0 {
            true => assert!(got.is_ok(), "{mode:o}: {got:?}"),
            false => assert_eq!(got, Err(Untrusted::OthersWrite(mode)), "{mode:o}"),
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

/// A lane whose socket breaks ends, and the next attaches in its place: over generated
/// sessions (updates of generated sizes; the relay reading what has come; the relay's end of
/// the lane vanishing at any point, idle, with updates waiting or mid-update), once the next
/// update finds the socket broken the session holds no capture back (it has no way to be
/// sent frames on), the control stream says nothing of it, and the next lane attaches and
/// carries the updates sent after it, whole and in order. What the broken lane held is dropped: its viewer starts
/// over on a keyframe on the next lane (`hub`'s), never on a stream with a hole in it.
#[tokio::test]
async fn a_broken_lane_ends_and_the_next_attaches() {
    let mut rng = crate::xorshift::Xorshift::new(0x1f83_d9ab_5be0_cd19);
    let mut waited = 0;
    for _ in 0..30 {
        let hub = Hub::new(crate::hub::tests::hello()).unwrap();
        let d = daemon(&hub);
        let mut relay = Relay::open(&d).await;
        let (c, _) = relay.greeted().await;
        let mut lane = open_lane(&hub, &attach(&c)).await;
        assert_eq!(relay.next().await.0, Said::Attached);
        let client = hub.clients.lock()[0].clone();
        let mut sent = 0u8;
        let mut send = |rng: &mut crate::xorshift::Xorshift| {
            let frame = bytes::Bytes::from(vec![sent; rng.range(1, 60_000) as usize]);
            client.lane(|s| s.video(&frame, Instant::now()));
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
            waited += usize::from(client.lane(|s| s.queued()) > 0);
            drop(lane);
            // An update finds the socket broken, if what waited did not. (On macOS a test's
            // child process may hold the relay's end a moment: a socket pair is made close-on-exec
            // only after it is made, and a fork between the two inherits it.)
            let ended = tokio::time::timeout(Duration::from_secs(5), async {
                while client.lane(|s| s.gate(Instant::now())).is_some() {
                    send(&mut rng);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });
            assert!(ended.await.is_ok(), "a broken lane is live");
            assert_eq!(client.look(), None, "a broken lane is looked at");
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

/// Every frame a lane carried, read with `framer` as far as it has come, with every task run
/// as far as it goes: each its name, its track and its length.
async fn carried(lane: &mut UnixStream, framer: &mut Framer) -> Vec<(Sequence, Track, u64)> {
    let mut frames = Vec::new();
    loop {
        settle().await;
        let mut buf = [0u8; 8192];
        let mut read = 0;
        while let Ok(n @ 1..) = lane.try_read(&mut buf) {
            framer.push(&buf[..n]);
            read += n;
        }
        while let Some(m) = framer.frame().unwrap() {
            let f = Frame::decode(m).expect("a lane carries frames");
            frames.push((f.sequence, f.track, f.bytes.len() as u64));
        }
        if read == 0 {
            return frames;
        }
    }
}

/// What the relay says the page took reaches the session's link by each frame's name, video
/// and audio alike, and a took of a lane since replaced counts for nothing. Over generated
/// sessions through the real sockets: updates of generated sizes and Opus packets go onto a
/// lane (a packet only while the gate is open); the relay reads each frame's name off the
/// lane and says some taken, each up to the one it names; the lane is replaced now and then,
/// which drops what the old one was sent; and the relay goes on saying frames of the old
/// lane taken. After every step what the link has outstanding is the model's: the current
/// lane's frames not yet named, as many and of as many bytes.
#[tokio::test]
async fn a_took_counts_for_the_lane_it_was_sent_on() {
    let mut rng = crate::xorshift::Xorshift::new(0x7137_449f_23ef_65cd);
    let (mut stale, mut counted, mut sound) = (0, 0, 0);
    for _ in 0..40 {
        let hub = Hub::new(crate::hub::tests::hello()).unwrap();
        let d = daemon(&hub);
        let mut relay = Relay::open(&d).await;
        let (c, _) = relay.greeted().await;
        let mut lane = open_lane(&hub, &attach(&c)).await;
        let mut framer = Framer::new();
        assert_eq!(relay.next().await.0, Said::Attached);
        let client = hub.clients.lock()[0].clone();
        // The model: the current lane's frames not yet taken, oldest first, each its name
        // and bytes; every name ever sent on a lane before it.
        let mut outstanding: Vec<(Sequence, u64)> = Vec::new();
        let mut gone: Vec<Sequence> = Vec::new();
        for _ in 0..rng.range(5, 40) {
            match rng.below(6) {
                0 | 1 => {
                    let frame = bytes::Bytes::from(vec![9u8; rng.range(1, 5_000) as usize]);
                    client.lane(|s| s.video(&frame, Instant::now()));
                }
                2 => {
                    let packet = vec![4u8; rng.range(1, 400) as usize];
                    client.lane(|s| s.audio(&packet, 0, Instant::now()));
                }
                3 => match outstanding.len() {
                    0 => {}
                    n @ 1.. => {
                        let upto = rng.below(n as u64) as usize;
                        let sequence = outstanding[upto].0;
                        relay.say(Up::Took { sequence }).await;
                        gone.extend(outstanding.drain(..=upto).map(|(name, _)| name));
                        counted += 1;
                    }
                },
                4 => match gone.len() {
                    0 => {}
                    n @ 1.. => {
                        let sequence = gone[rng.below(n as u64) as usize];
                        relay.say(Up::Took { sequence }).await;
                        stale += 1;
                    }
                },
                5.. => {
                    gone.extend(outstanding.drain(..).map(|(name, _)| name));
                    lane = open_lane(&hub, &attach(&c)).await;
                    framer = Framer::new();
                    assert_eq!(relay.next().await.0, Said::Attached);
                }
            }
            for (name, track, bytes) in carried(&mut lane, &mut framer).await {
                sound += usize::from(track == Track::Audio);
                outstanding.push((name, bytes));
            }
            let Way::Lane(stats) = client.lane(|s| s.stats(Instant::now())) else {
                panic!("a session with a lane says its lane");
            };
            let bytes: u64 = outstanding.iter().map(|(_, b)| b).sum();
            assert_eq!(stats.outstanding_bytes, bytes);
            assert_eq!(stats.outstanding_frames, outstanding.len());
        }
        drop(lane);
    }
    assert!(
        stale > 30 && counted > 30 && sound > 30,
        "{stale} stale, {counted} counted, {sound} packets"
    );
}

/// Whether the hub's waker is set: what the capture loop's wait would return on at once.
fn woken(hub: &Hub) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let mut wake = [PollFd::new(hub.waker.waitable(), PollFlags::POLLIN)];
    poll(&mut wake, PollTimeout::ZERO).unwrap() > 0
}

/// A took wakes the capture loop, which waits on the gate with no time limit: told nothing,
/// it would never look at the gate the took opened. Through the real control socket, over
/// generated runs of updates sent onto a lane: with updates on the way the gate is held
/// and nothing has woken the loop; once the relay says the last of them taken, the loop is
/// woken and the gate it then reads is open.
#[tokio::test]
async fn a_took_wakes_the_capture_loop() {
    use crate::session::link::Gate;
    let mut rng = crate::xorshift::Xorshift::new(0x9b05_688c_2b3e_6c1f);
    let hub = Hub::new(crate::hub::tests::hello()).unwrap();
    let d = daemon(&hub);
    let mut relay = Relay::open(&d).await;
    let (c, _) = relay.greeted().await;
    let mut lane = open_lane(&hub, &attach(&c)).await;
    let mut framer = Framer::new();
    assert_eq!(relay.next().await.0, Said::Attached);
    let client = hub.clients.lock()[0].clone();
    for _ in 0..200 {
        let mut last = None;
        for _ in 0..rng.range(1, 6) {
            let frame = bytes::Bytes::from(vec![3u8; rng.range(1, 3_000) as usize]);
            client.lane(|s| s.video(&frame, Instant::now()));
            last = Some(next_frame(&mut lane, &mut framer).await.0);
        }
        let last = last.expect("an update was sent");
        settle().await;
        hub.drain_wake();
        assert!(!woken(&hub), "the loop was woken with nothing told");
        // A long time on: time alone opens nothing.
        let held = client.lane(|s| s.gate(Instant::now() + Duration::from_secs(3600)));
        assert!(matches!(held, Some(Gate::Full | Gate::Late)), "{held:?}");
        relay.say(Up::Took { sequence: last }).await;
        settle().await;
        assert!(woken(&hub), "a took woke nothing");
        let gate = client.lane(|s| s.gate(Instant::now()));
        assert_eq!(gate, Some(Gate::Open));
    }
}

/// What a microphone's thread is told, as a test compares it: the kind, the session and the
/// audio.
fn heard(m: MicMsg) -> (&'static str, u64, Vec<u8>) {
    match m {
        MicMsg::Start(id) => ("start", id, Vec::new()),
        MicMsg::Opus(id, packet) => ("opus", id, packet.to_vec()),
        MicMsg::Stop(id) => ("stop", id, Vec::new()),
    }
}

/// A viewer's microphone reaches the microphone's thread as its session's, and nothing
/// else does. Over generated sessions of `mi,1`, `mi,0`, and binary messages of the types
/// the page sends and of any byte, each carrying 0 to 3 bytes, through the real control
/// path: the thread is told the session starts and stops as each command says, and each
/// Opus packet that carries anything, with the session's id, in order; nothing of a pasted
/// image or a type the page does not send; and, when the session's connection closes,
/// that it stops.
#[tokio::test]
async fn a_viewers_microphone_reaches_the_microphones_thread() {
    let mut rng = crate::xorshift::Xorshift::new(0x510e_527f_ade6_82d1);
    for _ in 0..100 {
        let hub = Hub::new(crate::hub::tests::hello()).unwrap();
        let (d, mic) = daemon_heard(&hub);
        let mut relay = Relay::open(&d).await;
        relay.greeted().await;
        let id = hub.clients.lock()[0].id;
        let mut want = Vec::new();
        for _ in 0..rng.below(30) {
            let data: Vec<u8> = (0..rng.below(4)).map(|_| rng.next() as u8).collect();
            match rng.below(5) {
                0 => {
                    relay.say(Up::Text("mi,1")).await;
                    want.push(("start", id, Vec::new()));
                }
                1 => {
                    relay.say(Up::Text("mi,0")).await;
                    want.push(("stop", id, Vec::new()));
                }
                2.. => {
                    // A type byte the page sends, or any byte at all.
                    let tag = match rng.below(2) {
                        0 => BinUp::ALL[rng.below(BinUp::ALL.len() as u64) as usize].byte(),
                        1.. => rng.below(256) as u8,
                    };
                    let message = [&[tag][..], &data].concat();
                    relay.say(Up::Binary(&message)).await;
                    match (BinUp::of(tag), data.is_empty()) {
                        (Some(BinUp::MicOpus), false) => want.push(("opus", id, data)),
                        (Some(BinUp::MicOpus), true)
                        | (Some(BinUp::ClipImage) | None, true | false) => {}
                    }
                }
            }
        }
        drop(relay);
        // The session ends once its connection has: its microphone stops with it.
        want.push(("stop", id, Vec::new()));
        let mut told = Vec::new();
        for _ in 0..1_000 {
            settle().await;
            told.extend(mic.try_iter().map(heard));
            if told.len() >= want.len() {
                break;
            }
        }
        settle().await;
        told.extend(mic.try_iter().map(heard));
        assert_eq!(told, want);
    }
}

/// A session is sent desktop audio exactly while it last asked for it, where the daemon
/// sends audio at all. Over generated sessions of `au,1` and `au,0` through the real control
/// path, on a daemon that sends audio and one that does not: after each, the session wants
/// audio as `voxl stats` says it exactly when its last ask was `au,1` and the daemon sends
/// audio.
#[tokio::test]
async fn a_session_wants_audio_as_it_last_asked() {
    use super::control::Audio;
    let mut rng = crate::xorshift::Xorshift::new(0x9b05_688c_2b3e_6c1f);
    for audio in [Audio::Sent, Audio::Off] {
        for _ in 0..50 {
            let hub = Hub::new(crate::hub::tests::hello()).unwrap();
            let d = Arc::new(Daemon {
                audio,
                ..Arc::into_inner(daemon(&hub)).unwrap()
            });
            let mut relay = Relay::open(&d).await;
            relay.greeted().await;
            let client = hub.clients.lock()[0].clone();
            for _ in 0..rng.range(1, 20) {
                let asked = rng.below(2) == 1;
                let word = match asked {
                    true => "au,1",
                    false => "au,0",
                };
                relay.say(Up::Text(word)).await;
                settle().await;
                let wants = client.stats()["audio"] == true;
                assert_eq!(wants, asked && audio == Audio::Sent, "{audio:?}, {word}");
            }
        }
    }
}
