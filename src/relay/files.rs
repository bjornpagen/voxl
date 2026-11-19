//! The page's files panel, served by `voxl files …` on the server over ssh, one ssh
//! command per request and streamed both ways (src/files.rs has the server's side):
//!
//! - `GET /files/list?path=P`: `voxl files list --root R P`.
//! - `GET /files/download?path=P`: `voxl files get --root R P`.
//! - `PUT /files/upload?path=D&name=N[&sub=S]`: `voxl files put --root R --bytes L D N
//!   [S]`, the body's L bytes on its stdin, saved only if all L arrive.
//!
//! A run writes one line of JSON first: its answer (a listing, what a download is, where an
//! upload went), or a refusal, `{"error": …, "status": …}`, after which it exits 1. A
//! download's bytes follow its line. `R` is the shared folder the daemon's hello names.

use super::ssh;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout};

/// What the files routes reach the server with.
pub struct Files {
    pub to: ssh::Destination,
    /// The shared folder as the newest session's hello named it; none before the first
    /// hello.
    pub grant: Mutex<Option<Grant>>,
}

/// The shared folder, as the daemon's hello names it.
#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub root: String,
}

type Q = Query<HashMap<String, String>>;

/// Uploads must carry this header. Browsers only send custom headers cross-origin after a
/// CORS preflight, which the relay never answers, so other sites can't upload.
const UPLOAD_HEADER: &str = "x-voxl-upload";

/// The longest file name, in bytes: `NAME_MAX` in Linux's `<linux/limits.h>`, and the most
/// macOS's file systems take. Published.
const NAME_MAX: usize = 255;

/// The longest path, in bytes: `PATH_MAX` in Linux's `<linux/limits.h>`. Published.
const PATH_MAX: usize = 4096;

/// The most bytes JSON writes for one byte of a string: a control byte as `\u00XX`
/// (RFC 8259 §7). Published.
const JSON_ESCAPED: usize = 6;

/// The digits of the largest `u64`. Derived: counted here.
const U64_DIGITS: usize = {
    let (mut n, mut digits) = (u64::MAX, 0);
    while n > 0 {
        n /= 10;
        digits += 1;
    }
    digits
};

/// The longest first line read from `voxl files`: a listing, the longest, of at most
/// `crate::files::ENTRIES_LISTED` entries, each its fields around a name of at most
/// `NAME_MAX` bytes escaped and two `u64`s, inside the listing's fields around a path of at
/// most `PATH_MAX` bytes escaped, and the newline. Derived.
const LINE_MOST: u64 = {
    let entry = r#"{"dir":false,"mtime":,"name":"","size":},"#.len()
        + NAME_MAX * JSON_ESCAPED
        + 2 * U64_DIGITS;
    let listing = r#"{"entries":[],"path":""}"#.len() + PATH_MAX * JSON_ESCAPED + 1;
    (crate::files::ENTRIES_LISTED * entry + listing) as u64
};

/// The bytes read from `voxl files`'s stdout at a time, and the most of its stderr kept as
/// its reason: a pipe's capacity, 65,536 bytes on Linux (pipe(7): "the pipe capacity is 16
/// pages") and on macOS, the most one read gives and the most a process writes without a
/// reader before it waits. Published.
const CHUNK: usize = 64 << 10;

fn failed(status: StatusCode, why: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": why }).to_string(),
    )
        .into_response()
}

/// Why `child` failed when it said nothing on stdout: its stderr's first line, or how it
/// ended.
async fn reason(mut child: Child) -> String {
    let mut err = Vec::new();
    let read = match child.stderr.take() {
        Some(e) => e.take(CHUNK as u64).read_to_end(&mut err).await.map(drop),
        None => Err(std::io::Error::other("ssh gave no stderr")),
    };
    let status = child.wait().await;
    let line = String::from_utf8_lossy(&err);
    match line.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some(said) => said.to_string(),
        None => match (read, status) {
            (Err(e), Ok(_) | Err(_)) => format!("the server's voxl files said why, unread: {e}"),
            (Ok(()), Ok(s)) => format!("the server's voxl files ended: {s}"),
            (Ok(()), Err(e)) => format!("the server's voxl files: {e}"),
        },
    }
}

/// A run's first line.
enum First {
    /// Its answer: the line, and the run with what follows it.
    Answer(Vec<u8>, BufReader<ChildStdout>, Child),
    /// Its refusal, or its failure before it said anything, as the page is answered.
    Refused(Response),
}

/// A run's refusal: what it says, and the HTTP status the page is to see.
#[derive(serde::Deserialize)]
struct Refusal {
    error: String,
    status: u16,
}

/// Runs `voxl files <args>` with `stdin` fed to it as it comes, and reads its first line.
async fn first(files: &Files, args: &[&str], stdin: Option<Body>) -> First {
    let mut child = match ssh::files(&files.to, args) {
        Ok(c) => c,
        Err(e) => return First::Refused(failed(StatusCode::BAD_GATEWAY, &format!("ssh: {e}"))),
    };
    let (Some(mut pipe), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return First::Refused(failed(
            StatusCode::BAD_GATEWAY,
            "ssh gave no stdin or stdout",
        ));
    };
    match stdin {
        // The body goes to the server while it answers. One that breaks off, or a server
        // that stops reading, ends the server's stdin short of the length it was told, which
        // it refuses, and its refusal is the answer.
        Some(body) => {
            tokio::spawn(async move {
                let mut chunks = body.into_data_stream();
                while let Some(Ok(chunk)) = chunks.next().await {
                    if pipe.write_all(&chunk).await.is_err() {
                        return;
                    }
                }
            });
        }
        // Nothing to send: the server's stdin ends at once.
        None => drop(pipe),
    }
    let mut out = BufReader::new(stdout);
    let mut line = Vec::new();
    let read = (&mut out)
        .take(LINE_MOST)
        .read_until(b'\n', &mut line)
        .await;
    match read {
        Ok(_) if line.ends_with(b"\n") => {}
        Ok(0) | Err(_) => {
            return First::Refused(failed(StatusCode::BAD_GATEWAY, &reason(child).await))
        }
        Ok(_) => {
            return First::Refused(failed(
                StatusCode::BAD_GATEWAY,
                "the server's voxl files said more than a line",
            ))
        }
    }
    // An answer never has an `error`: a line that reads as a refusal is one.
    match serde_json::from_slice::<Refusal>(&line) {
        Ok(r) => First::Refused(match StatusCode::from_u16(r.status) {
            Ok(status) => failed(status, &r.error),
            Err(_) => failed(
                StatusCode::BAD_GATEWAY,
                &format!(
                    "{} (the server gave status {}, which HTTP has not)",
                    r.error, r.status
                ),
            ),
        }),
        Err(_) => First::Answer(line, out, child),
    }
}

/// The run's whole output once it has ended well: one line of JSON.
async fn whole(files: &Files, args: &[&str], stdin: Option<Body>) -> Response {
    let (line, mut child) = match first(files, args, stdin).await {
        First::Answer(line, _, child) => (line, child),
        First::Refused(r) => return r,
    };
    match child.wait().await {
        Ok(s) if s.success() => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            line,
        )
            .into_response(),
        Ok(_) | Err(_) => failed(StatusCode::BAD_GATEWAY, &reason(child).await),
    }
}

/// The shared folder of the newest session; none before the first hello.
fn grant(files: &Files) -> Option<Grant> {
    files.grant.lock().clone()
}

/// The page's answer while no session has named the shared folder.
fn no_grant() -> Response {
    failed(
        StatusCode::SERVICE_UNAVAILABLE,
        "no session has said where the shared folder is yet",
    )
}

pub async fn list(State(files): State<Arc<Files>>, Query(q): Q) -> Response {
    let Some(g) = grant(&files) else {
        return no_grant();
    };
    let Some(path) = q.get("path") else {
        return failed(StatusCode::BAD_REQUEST, "a listing names its folder");
    };
    whole(&files, &["list", "--root", &g.root, path], None).await
}

pub async fn upload(
    State(files): State<Arc<Files>>,
    Query(q): Q,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !headers.contains_key(UPLOAD_HEADER) {
        return failed(StatusCode::FORBIDDEN, "missing upload header");
    }
    let (Some(dir), Some(name)) = (q.get("path"), q.get("name")) else {
        return failed(
            StatusCode::BAD_REQUEST,
            "an upload names its folder and its file",
        );
    };
    // Its length, so the server saves it only if all of it arrived: a body cut short ends
    // the server's stdin as a whole one would.
    let Some(said) = headers.get(header::CONTENT_LENGTH) else {
        return failed(StatusCode::LENGTH_REQUIRED, "an upload says its length");
    };
    // Parsed once here, and written for the server as the number it is.
    let length: u64 = match said.to_str().map(str::parse) {
        Ok(Ok(length)) => length,
        Ok(Err(_)) | Err(_) => {
            return failed(
                StatusCode::BAD_REQUEST,
                "an upload's length is not a whole number of bytes",
            )
        }
    };
    let length = length.to_string();
    let Some(g) = grant(&files) else {
        return no_grant();
    };
    let mut args = vec!["put", "--root", &g.root, "--bytes", &length, dir, name];
    if let Some(sub) = q.get("sub") {
        args.push(sub);
    }
    whole(&files, &args, Some(body)).await
}

/// What a download's line says it is.
#[derive(serde::Deserialize)]
struct Download {
    name: String,
    #[serde(rename = "type")]
    content_type: String,
}

/// Streams the server's file, or folder zipped, to the page as it comes, saved under the
/// name the server gives. A download that fails once its bytes have begun ends its body
/// with an error, so the browser marks it failed.
pub async fn download(State(files): State<Arc<Files>>, Query(q): Q) -> Response {
    let Some(g) = grant(&files) else {
        return no_grant();
    };
    let Some(path) = q.get("path") else {
        return failed(StatusCode::BAD_REQUEST, "a download names its file");
    };
    let (line, out, child) = match first(&files, &["get", "--root", &g.root, path], None).await {
        First::Answer(line, out, child) => (line, out, child),
        First::Refused(r) => return r,
    };
    let Ok(what) = serde_json::from_slice::<Download>(&line) else {
        return failed(
            StatusCode::BAD_GATEWAY,
            "the server's voxl files named no download",
        );
    };
    let rest = futures_util::stream::unfold(Some((out, child)), |state| async move {
        let (mut out, mut child) = state?;
        let mut buf = vec![0u8; CHUNK];
        match out.read(&mut buf).await {
            Ok(0) => match child.wait().await {
                Ok(s) if s.success() => None,
                Ok(_) | Err(_) => Some((Err(std::io::Error::other(reason(child).await)), None)),
            },
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), Some((out, child))))
            }
            Err(e) => Some((Err(e), None)),
        }
    });
    let (Ok(content_type), Ok(saved_as)) = (
        HeaderValue::from_str(&what.content_type),
        HeaderValue::from_str(&disposition(&what.name)),
    ) else {
        return failed(
            StatusCode::BAD_GATEWAY,
            "the server's voxl files named a download no HTTP header can carry",
        );
    };
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, content_type);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::CONTENT_DISPOSITION, saved_as);
    (h, Body::from_stream(rest)).into_response()
}

/// `Content-Disposition` for a download saved as `name` (RFC 6266): an ASCII fallback and
/// the name itself, percent-encoded (RFC 8187).
fn disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(
            |c| match c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                true => c,
                false => '_',
            },
        )
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        match b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            true => encoded.push(char::from(b)),
            false => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}
