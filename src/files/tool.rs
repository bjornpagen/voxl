//! `voxl files list|get|put`: file transfer, one request a run. The relay runs each over
//! the ssh master (`ssh <host> voxl files …`) and turns it into the page's HTTP request,
//! so a transfer never queues behind, or in front of, the stream. Everything is confined
//! to one root folder: the daemon's `$HOME`, which its hello names, handed back here
//! as `--root`.
//!
//! What a run writes on stdout: one line of JSON first, then for `get` the file's bytes.
//! The line is either the answer (`list`: the folder, `get`: what follows, `put`: what was
//! saved) or a refusal, `{"error": …, "status": …}`, with the HTTP status the page is to
//! see, and then the run exits 1. A `get` that fails once its bytes have begun exits 1
//! with nothing more: the relay ends the download as failed.

use super::ENTRIES_LISTED;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

/// What a run is asked, parsed once from its arguments.
#[derive(Debug, PartialEq)]
enum Request {
    /// `list --root R <path>`: a folder's entries.
    List { root: PathBuf, path: String },
    /// `get --root R <path>`: a file's bytes, or a folder zipped.
    Get { root: PathBuf, path: String },
    /// `put --root R --bytes N <folder> <name> [<sub>]`: the `N` bytes on
    /// stdin saved as `name` in `folder`, in `sub` under it for a folder's upload.
    Put {
        root: PathBuf,
        /// The upload's length, as the page's request said it: a stdin that ends before it
        /// or runs past it is no whole file.
        len: u64,
        folder: String,
        name: String,
        sub: String,
    },
}

/// Why a request was refused, with the HTTP status the page sees for it.
#[derive(Debug, PartialEq)]
struct Refused {
    status: u16,
    message: String,
}

impl Refused {
    fn new(status: u16, message: impl Into<String>) -> Refused {
        Refused {
            status,
            message: message.into(),
        }
    }
}

#[cfg(target_os = "linux")]
/// Runs `voxl files <args>` and exits with its status.
pub fn run(args: &[String]) -> ! {
    let mut out = std::io::stdout().lock();
    let status = match parse(args) {
        Ok(request) => answer(request, &mut std::io::stdin().lock(), &mut out),
        Err(usage) => {
            eprintln!("voxl files: {usage}\n{USAGE}");
            2
        }
    };
    let _ = out.flush();
    std::process::exit(status)
}

#[cfg(target_os = "linux")]
const USAGE: &str = "usage: voxl files list --root DIR PATH
       voxl files get --root DIR PATH
       voxl files put --root DIR --bytes N FOLDER NAME [SUB]";

/// What a run is asked to do: the first of its words that is no flag.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Verb {
    List,
    Get,
    Put,
}

/// Each verb by its word.
const VERBS: [(&str, Verb); 3] = [("list", Verb::List), ("get", Verb::Get), ("put", Verb::Put)];

/// A flag a run takes: each is followed by its value.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flag {
    /// `--root`: the shared folder.
    Root,
    /// `--bytes`: an upload's length.
    Bytes,
}

/// Each flag by its word.
const FLAGS: [(&str, Flag); 2] = [("--root", Flag::Root), ("--bytes", Flag::Bytes)];

/// The request `args` (after `files`) make, or what is wrong with them. A word that is
/// none of [`FLAGS`] is one of the request's own words, its verb first.
fn parse(args: &[String]) -> Result<Request, String> {
    let mut root = None;
    let mut len = None;
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match FLAGS.iter().find(|(word, _)| word == a) {
            Some((_, Flag::Root)) => root = Some(it.next().ok_or("--root needs a folder")?),
            Some((_, Flag::Bytes)) => len = Some(it.next().ok_or("--bytes needs a number")?),
            None => words.push(a.as_str()),
        }
    }
    let root = root.ok_or("--root is required")?;
    // The root as the daemon canonicalised it; canonical again here, so every path under
    // it compares with `starts_with`.
    let root = std::fs::canonicalize(root).map_err(|e| format!("--root {root}: {e}"))?;
    let (verb, words) = words.split_first().ok_or("not a request")?;
    let (_, verb) = VERBS
        .iter()
        .find(|(word, _)| word == verb)
        .ok_or_else(|| format!("{verb}: not list, get or put"))?;
    let bytes = |said: &str| {
        said.parse()
            .map_err(|_| format!("--bytes {said}: not a whole number"))
    };
    match (verb, words, len) {
        (Verb::List, [path], None) => Ok(Request::List {
            root,
            path: path.to_string(),
        }),
        (Verb::Get, [path], None) => Ok(Request::Get {
            root,
            path: path.to_string(),
        }),
        // No `sub` is the folder itself: the empty path under it.
        (Verb::Put, [folder, name], Some(said)) => Ok(Request::Put {
            root,
            len: bytes(said)?,
            folder: folder.to_string(),
            name: name.to_string(),
            sub: String::new(),
        }),
        (Verb::Put, [folder, name, sub], Some(said)) => Ok(Request::Put {
            root,
            len: bytes(said)?,
            folder: folder.to_string(),
            name: name.to_string(),
            sub: sub.to_string(),
        }),
        (Verb::List | Verb::Get, [] | [_, _, ..], None | Some(_))
        | (Verb::List | Verb::Get, [_], Some(_))
        | (Verb::Put, [] | [_] | [_, _, _, _, ..], None | Some(_))
        | (Verb::Put, [_, _] | [_, _, _], None) => Err("not a request".into()),
    }
}

/// Answers `request` on `out`, reading an upload from `input`: the exit status.
fn answer(request: Request, input: &mut impl Read, out: &mut impl Write) -> i32 {
    let result = match request {
        Request::List { root, path } => list(&root, &path).and_then(|v| line(out, &v)),
        Request::Get { root, path } => get(&root, &path, out),
        Request::Put {
            root,
            len,
            folder,
            name,
            sub,
        } => put(&root, len, &folder, &name, &sub, input).and_then(|v| line(out, &v)),
    };
    match result {
        Ok(()) => 0,
        Err(Fail::Refused(r)) => {
            let _ = line(
                out,
                &serde_json::json!({ "error": r.message, "status": r.status }),
            );
            1
        }
        Err(Fail::Broken(e)) => {
            eprintln!("voxl files: {e}");
            1
        }
    }
}

/// How a run fails: refused before its answer began, or broken while it was written.
#[derive(Debug)]
enum Fail {
    Refused(Refused),
    Broken(std::io::Error),
}

impl From<Refused> for Fail {
    fn from(r: Refused) -> Fail {
        Fail::Refused(r)
    }
}

/// One line of JSON.
fn line(out: &mut impl Write, v: &serde_json::Value) -> Result<(), Fail> {
    writeln!(out, "{v}").map_err(Fail::Broken)
}

/// A path under the root, as its components below it: the root itself is the empty path.
/// Only [`resolve`] and [`put`] make one, of a path they found under the root, so a path
/// outside it is none.
struct Inside(PathBuf);

impl Inside {
    /// The canonical path `full`, below `root`; one that is not below it is refused.
    fn of(root: &Path, full: &Path) -> Result<Inside, Refused> {
        match full.strip_prefix(root) {
            Ok(below) => Ok(Inside(below.to_path_buf())),
            Err(_) => Err(Refused::new(403, "outside the shared folder")),
        }
    }

    /// It, from the file system's root.
    fn full(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }

    /// It as the page names it: from the root, with no leading separator.
    fn page(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

/// A page's path ("Downloads/report.pdf") as the names under the root it walks: `.` parts
/// are nothing, and an absolute path, `..` or a NUL is refused.
fn relative(rel: &str) -> Result<PathBuf, Refused> {
    let bad = || Refused::new(400, "invalid path");
    if rel.contains('\0') {
        return Err(bad());
    }
    let mut below = PathBuf::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => below.push(s),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Err(bad()),
        }
    }
    Ok(below)
}

/// Resolves a page's path under `root` ([`relative`]); symlinks are followed only if they
/// stay inside the root. `Missing::Allowed`: a path that does not exist yet is taken as it
/// is.
fn resolve(root: &Path, rel: &str, missing: Missing) -> Result<Inside, Refused> {
    let below = relative(rel)?;
    match (root.join(&below).canonicalize(), missing) {
        (Ok(c), Missing::Allowed | Missing::Refused) => Inside::of(root, &c),
        (Err(_), Missing::Allowed) => Ok(Inside(below)),
        (Err(_), Missing::Refused) => Err(Refused::new(404, "not found")),
    }
}

/// Whether a path may be missing: the upload folder is made by the first upload.
#[derive(Clone, Copy)]
enum Missing {
    Allowed,
    Refused,
}

/// A folder's entries, hidden ones left out; a folder that does not exist is listed
/// empty, as the upload folder is until something is uploaded.
fn list(root: &Path, path: &str) -> Result<serde_json::Value, Fail> {
    let dir = resolve(root, path, Missing::Allowed)?;
    let mut entries = Vec::new();
    match std::fs::read_dir(dir.full(root)) {
        Ok(rd) => {
            for e in rd.filter_map(Result::ok) {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                // Followed for type and size, as a file manager would, but links that lead
                // outside the shared folder are hidden.
                let Ok(md) = std::fs::metadata(e.path()) else {
                    continue;
                };
                let link = e.file_type().is_ok_and(|t| t.is_symlink());
                if link && !std::fs::canonicalize(e.path()).is_ok_and(|c| c.starts_with(root)) {
                    continue;
                }
                if !md.is_dir() && !md.is_file() {
                    continue;
                }
                // Linux keeps every file's modification time: one it cannot say is an entry
                // it cannot read.
                let Ok(modified) = md.modified() else {
                    continue;
                };
                entries.push(serde_json::json!({
                    "name": name, "dir": md.is_dir(), "size": md.len(), "mtime": millis(modified)
                }));
                if entries.len() == ENTRIES_LISTED {
                    break;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(Refused::new(404, "not a folder").into()),
    }
    Ok(serde_json::json!({ "path": dir.page(), "entries": entries }))
}

/// A time as the page's `Date` takes it: milliseconds since 1970 began, before it
/// negative. Beyond the 292 million years either way an `i64` holds, it is held to the
/// end.
fn millis(t: std::time::SystemTime) -> i64 {
    let held = |ms: u128| i64::try_from(ms).unwrap_or(i64::MAX);
    match t.duration_since(UNIX_EPOCH) {
        Ok(after) => held(after.as_millis()),
        Err(before) => -held(before.duration().as_millis()),
    }
}

/// A file's bytes after the line that names them, or a folder zipped as it is read.
fn get(root: &Path, path: &str, out: &mut impl Write) -> Result<(), Fail> {
    let inside = resolve(root, path, Missing::Refused)?;
    // The root has no name of its own: the page calls it Home.
    let name = match inside.0.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => "Home".into(),
    };
    let path = inside.full(root);
    if path.is_dir() {
        line(
            out,
            &serde_json::json!({ "name": format!("{name}.zip"), "type": "application/zip" }),
        )?;
        return write_zip(&path, &name, out).map_err(|e| Fail::Broken(std::io::Error::other(e)));
    }
    let mut file = std::fs::File::open(&path).map_err(|_| Refused::new(404, "not found"))?;
    let md = file
        .metadata()
        .map_err(|_| Refused::new(404, "not found"))?;
    if !md.is_file() {
        return Err(Refused::new(400, "not a file").into());
    }
    line(
        out,
        &serde_json::json!({ "name": name, "type": "application/octet-stream", "size": md.len() }),
    )?;
    std::io::copy(&mut file, out).map_err(Fail::Broken)?;
    Ok(())
}

/// Formats that are already compressed: stored as they are rather than deflated again.
const COMPRESSED: &[&str] = &[
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "br", "jar", "whl", "apk", "dmg", "iso",
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "mp3", "m4a", "aac", "ogg", "opus",
    "flac", "mp4", "m4v", "mov", "mkv", "webm", "avi", "woff", "woff2", "pdf", "docx", "xlsx",
    "pptx",
];

/// Zip time (local fields, no zone) for a modification time, read as UTC.
fn zip_time(t: std::time::SystemTime) -> Option<zip::DateTime> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    zip::DateTime::from_date_and_time(
        year,
        month,
        day,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
    .ok()
}

/// A folder as a zip, written as it is read: nothing is staged on disk. Symlinks are left
/// out (they could lead outside the shared folder, or loop); unreadable folders are left
/// empty rather than failing the whole download.
fn write_zip(dir: &Path, top: &str, out: &mut impl Write) -> zip::result::ZipResult<()> {
    use zip::write::SimpleFileOptions;
    let mut z = zip::ZipWriter::new_stream(out);
    let base = SimpleFileOptions::default();
    let mut todo = vec![(dir.to_path_buf(), top.to_string())];
    while let Some((path, rel)) = todo.pop() {
        z.add_directory(format!("{rel}/"), base)?;
        let mut entries: Vec<_> = match std::fs::read_dir(&path) {
            Ok(rd) => rd.filter_map(Result::ok).collect(),
            Err(_) => continue,
        };
        entries.sort_by_key(|e| e.file_name());
        // Pushed in reverse so folders come out in name order.
        let mut dirs = Vec::new();
        for e in entries {
            let Ok(md) = e.path().symlink_metadata() else {
                continue;
            };
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = format!("{rel}/{name}");
            if md.is_dir() {
                dirs.push((e.path(), rel));
                continue;
            }
            if !md.is_file() {
                continue;
            }
            let Ok(mut file) = std::fs::File::open(e.path()) else {
                continue;
            };
            let stored = Path::new(&name)
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| COMPRESSED.contains(&x.to_ascii_lowercase().as_str()));
            let mut opts = base.large_file(md.len() >= u64::from(u32::MAX));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                opts = opts.unix_permissions(md.permissions().mode() & 0o777);
            }
            opts = if stored {
                opts.compression_method(zip::CompressionMethod::Stored)
            } else {
                // The fastest level: the machine's CPU is busy streaming the desktop.
                opts.compression_method(zip::CompressionMethod::Deflated)
                    .compression_level(Some(1))
            };
            // An entry whose time zip cannot hold, or that has none, goes without one.
            if let Ok(Some(t)) = md.modified().map(zip_time) {
                opts = opts.last_modified_time(t);
            }
            z.start_file(rel, opts)?;
            std::io::copy(&mut file, &mut z)?;
        }
        todo.extend(dirs.into_iter().rev());
    }
    z.finish()?;
    Ok(())
}

/// A plain file name: text, with no separators, no `.`/`..`, no control characters, at
/// most 255 bytes (the longest name Linux's and macOS's file systems take).
fn clean_name(name: &std::ffi::OsStr) -> Option<&str> {
    let name = name.to_str()?.trim();
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    ok.then_some(name)
}

/// `name`, or `name (1).ext`, `name (2).ext`, ... if taken in `dir`: the name chosen.
fn unique(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.into();
    }
    // A name's extension is after its last dot, unless that dot begins it (`.bashrc`).
    let (stem, ext) = match name.rfind('.') {
        Some(i @ 1..) => (&name[..i], &name[i..]),
        Some(0) | None => (name, ""),
    };
    let mut n = 0u64;
    loop {
        n += 1;
        let chosen = format!("{stem} ({n}){ext}");
        if !dir.join(&chosen).exists() {
            return chosen;
        }
    }
}

/// Saves the `len` bytes of `input` as `name` in `folder`, in `sub` under it for a
/// folder's upload: written to a hidden file first and renamed only once exactly `len`
/// bytes came, so nothing on the desktop sees (or opens) a half-uploaded file, and an
/// upload cut short leaves nothing.
fn put(
    root: &Path,
    len: u64,
    folder: &str,
    name: &str,
    sub: &str,
    input: &mut impl Read,
) -> Result<serde_json::Value, Fail> {
    let name = clean_name(name.as_ref()).ok_or_else(|| Refused::new(400, "invalid file name"))?;
    // The folder (the upload folder is made by the first upload) and `sub` inside it
    // ("Photos/2024") are made as needed, one level at a time from the root, each step
    // checked: a symlink on the way could lead outside the shared folder, and nothing may
    // be made out there. Each is a relative path, as `resolve` takes one.
    let mut inside = Inside(PathBuf::new());
    for part in relative(folder)?.join(relative(sub)?).components() {
        let p =
            clean_name(part.as_os_str()).ok_or_else(|| Refused::new(400, "invalid folder name"))?;
        let next = inside.full(root).join(p);
        if let Err(e) = std::fs::create_dir(&next) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(Refused::new(500, format!("cannot create the folder: {e}")).into());
            }
        }
        inside = match std::fs::canonicalize(&next) {
            Ok(c) => Inside::of(root, &c)?,
            Err(_) => return Err(Refused::new(403, "outside the shared folder").into()),
        };
        if !inside.full(root).is_dir() {
            return Err(Refused::new(409, "a file is in the way of the folder").into());
        }
    }
    let dir = inside.full(root);
    let part = dir.join(format!(".{name}.{}.part", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(|e| Refused::new(500, format!("cannot write: {e}")))?;
    let written = match copy_exactly(input, &mut file, len) {
        Ok(()) => len,
        Err(r) => {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(r.into());
        }
    };
    drop(file);
    let chosen = unique(&dir, name);
    if let Err(e) = std::fs::rename(&part, dir.join(&chosen)) {
        let _ = std::fs::remove_file(&part);
        return Err(Refused::new(500, format!("cannot save: {e}")).into());
    }
    let saved = Inside(inside.0.join(&chosen));
    Ok(serde_json::json!({ "path": saved.page(), "size": written }))
}

/// Copies exactly `len` bytes of `input` to `file`: an input that ends before them, or
/// has more after them, is refused.
fn copy_exactly(input: &mut impl Read, file: &mut impl Write, len: u64) -> Result<(), Refused> {
    let interrupted = || Refused::new(400, "upload interrupted");
    let mut buf = [0u8; 64 << 10];
    let mut left = len;
    while left > 0 {
        // More left than a `usize` holds is more than the buffer.
        let want = match usize::try_from(left) {
            Ok(left) => left.min(buf.len()),
            Err(_) => buf.len(),
        };
        let n = match input.read(&mut buf[..want]) {
            Ok(0) => return Err(interrupted()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(interrupted()),
        };
        left -= n as u64;
        file.write_all(&buf[..n])
            .map_err(|e| Refused::new(507, format!("cannot write: {e}")))?;
    }
    // One more byte is a longer upload than the request said.
    loop {
        match input.read(&mut buf[..1]) {
            Ok(0) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(_) | Err(_) => return Err(interrupted()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch root of its own under the system's temporary folder, removed at the end.
    struct Root(PathBuf);

    impl Root {
        fn new(tag: &str) -> Root {
            let p = std::env::temp_dir().join(format!("voxl-files-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Root(std::fs::canonicalize(p).unwrap())
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run(root: &Root, args: &[&str], input: &[u8]) -> (i32, Vec<u8>) {
        let mut all = vec!["--root".to_string(), root.0.to_string_lossy().into_owned()];
        all.extend(args.iter().map(|s| s.to_string()));
        let request = parse(&all).unwrap();
        let mut out = Vec::new();
        let status = answer(request, &mut &input[..], &mut out);
        (status, out)
    }

    fn first_line(out: &[u8]) -> serde_json::Value {
        let end = out.iter().position(|&b| b == b'\n').unwrap();
        serde_json::from_slice(&out[..end]).unwrap()
    }

    /// What is put is got back byte for byte, listed by its name and size, and a second
    /// put of the same name lands beside it: over uploads of every size from nothing to
    /// past a copy buffer. A stdin shorter or longer than `--bytes` says is refused, and leaves no file and no
    /// part of one.
    #[test]
    fn what_is_put_is_got_and_listed() {
        let root = Root::new("put");
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut draw = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for i in 0..40 {
            let len = draw(200_000) as usize;
            let body: Vec<u8> = (0..len).map(|_| draw(256) as u8).collect();
            let name = format!("f{i}.bin");
            let put = |said: usize, body: &[u8]| {
                let said = said.to_string();
                run(&root, &["put", "--bytes", &said, "up", &name], body)
            };
            // Cut short, or run past what it said: refused, and nothing of it is left.
            let before = std::fs::read_dir(root.0.join("up")).map_or(0, |d| d.count());
            for said in [
                len + 1 + draw(1000) as usize,
                len.saturating_sub(1 + draw(1000) as usize),
            ] {
                if said == len {
                    continue;
                }
                let (status, out) = put(said, &body);
                let refused = first_line(&out)["status"].as_u64();
                assert_eq!((status, refused), (1, Some(400)));
                let after = std::fs::read_dir(root.0.join("up")).map_or(0, |d| d.count());
                assert_eq!(after, before, "a refused upload left a file");
            }
            let (status, out) = put(len, &body);
            assert_eq!(status, 0, "{}", String::from_utf8_lossy(&out));
            assert_eq!(first_line(&out)["size"].as_u64(), Some(len as u64));
            let (status, out) = run(&root, &["get", &format!("up/{name}")], &[]);
            assert_eq!(status, 0);
            let end = out.iter().position(|&b| b == b'\n').unwrap();
            assert_eq!(first_line(&out)["size"].as_u64(), Some(len as u64));
            assert_eq!(&out[end + 1..], &body[..]);
            let (_, out) = run(&root, &["list", "up"], &[]);
            let listed = first_line(&out);
            let entry = listed["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["name"] == name.as_str())
                .cloned()
                .unwrap();
            assert_eq!(entry["size"].as_u64(), Some(len as u64));
        }
        let args = ["put", "--bytes", "1", "up", "f0.bin"];
        let (status, out) = run(&root, &args, b"x");
        assert_eq!(status, 0);
        assert_eq!(first_line(&out)["path"], "up/f0 (1).bin");
    }

    /// No path a page sends reaches outside the root: `..`, absolute paths and a symlink
    /// out are refused, for every request, and as an upload's folder or its subfolder.
    #[test]
    fn nothing_reaches_outside_the_root() {
        let root = Root::new("out");
        let outside = Root::new("outside");
        std::fs::write(outside.0.join("secret"), b"s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside.0, root.0.join("link")).unwrap();
        let tries = [
            "../voxl-files-outside",
            "/etc",
            "link",
            "link/secret",
            "a/../../x",
        ];
        for path in tries {
            for args in [vec!["list", path], vec!["get", path]] {
                let (status, out) = run(&root, &args, &[]);
                assert_eq!(status, 1, "{args:?}: {}", String::from_utf8_lossy(&out));
                let code = first_line(&out)["status"].as_u64().unwrap();
                assert!([400, 403, 404].contains(&code), "{args:?}: {code}");
            }
            let args = ["put", "--bytes", "1", path, "n"];
            let (status, _) = run(&root, &args, b"x");
            assert_eq!(status, 1, "put into {path}");
            let args = ["put", "--bytes", "1", ".", "n", path];
            let (status, _) = run(&root, &args, b"x");
            assert_eq!(status, 1, "put into the root's {path}");
        }
        assert_eq!(std::fs::read_dir(&outside.0).unwrap().count(), 1);
    }

    /// A time is the milliseconds since 1970 began, before it negative, and an entry is
    /// listed with its own: over generated times either side of 1970, read back as they
    /// were made, and a file set to one of them listed with it.
    #[test]
    fn a_time_is_its_milliseconds_since_1970_either_side() {
        let root = Root::new("time");
        let mut rng = crate::xorshift::Xorshift::new(0x510e_527f_ade6_82d1);
        for i in 0..2_000u32 {
            // Whole milliseconds, up to some 35 years either way.
            let ms = rng.below(1 << 40);
            let d = std::time::Duration::from_millis(ms);
            let signed = i64::try_from(ms).unwrap();
            assert_eq!(millis(UNIX_EPOCH + d), signed);
            assert_eq!(millis(UNIX_EPOCH - d), -signed);
            if i % 100 == 0 {
                let name = format!("t{i}");
                let file = std::fs::File::create(root.0.join(&name)).unwrap();
                let (at, want) = match i % 200 {
                    0 => (UNIX_EPOCH - d, -signed),
                    1.. => (UNIX_EPOCH + d, signed),
                };
                file.set_modified(at).unwrap();
                let listed = list(&root.0, "").unwrap();
                let entry = listed["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|e| e["name"] == name.as_str())
                    .cloned()
                    .unwrap();
                assert_eq!(entry["mtime"], want, "{name}");
            }
        }
    }

    /// The arguments are a request exactly: `--root` and a verb's own fields, `list` and
    /// `get` a path, `put` a folder, a name and at most one subfolder with `--bytes` a
    /// whole number, and nothing else. Over generated command lines (the root given or
    /// not, `--bytes` of any word or none, a verb of four, zero to four words, the flags
    /// anywhere among them), against that grammar written apart; a request is read with
    /// the fields as given.
    #[test]
    fn a_request_is_exactly_its_fields() {
        let root = Root::new("args");
        let r = root.0.to_string_lossy().into_owned();
        let mut rng = crate::xorshift::Xorshift::new(0xbb67_ae85_84ca_a73b);
        let lengths = ["3", "0", "x", "-1", "18446744073709551616"];
        let verbs = ["list", "get", "put", "rm"];
        let words = ["a", "b c", "..", "--max-bytes", "1"];
        for _ in 0..20_000 {
            let verb = verbs[rng.below(4) as usize];
            let fields: Vec<&str> = (0..rng.below(5))
                .map(|_| words[rng.below(words.len() as u64) as usize])
                .collect();
            let with_root = rng.below(4) > 0;
            let bytes = (rng.below(2) == 0).then(|| lengths[rng.below(5) as usize]);
            // Each word one unit, each flag with its value one: a flag goes between units.
            let mut units: Vec<Vec<String>> = std::iter::once(verb)
                .chain(fields.iter().copied())
                .map(|w| vec![w.to_string()])
                .collect();
            let mut put_flag = |flag: &str, value: &str| {
                let at = rng.below(units.len() as u64 + 1) as usize;
                units.insert(at, vec![flag.to_string(), value.to_string()]);
            };
            if with_root {
                put_flag("--root", &r);
            }
            if let Some(n) = bytes {
                put_flag("--bytes", n);
            }
            let args: Vec<String> = units.concat();
            let len = bytes.map(|n| n.parse::<u64>());
            let ok = with_root
                && matches!(
                    (verb, fields.len(), len),
                    ("list" | "get", 1, None) | ("put", 2 | 3, Some(Ok(_)))
                );
            match (parse(&args), ok) {
                (Ok(Request::List { path, .. }), true) => {
                    assert_eq!((verb, path.as_str()), ("list", fields[0]))
                }
                (Ok(Request::Get { path, .. }), true) => {
                    assert_eq!((verb, path.as_str()), ("get", fields[0]))
                }
                (
                    Ok(Request::Put {
                        len,
                        folder,
                        name,
                        sub,
                        ..
                    }),
                    true,
                ) => {
                    assert_eq!(verb, "put");
                    assert_eq!(Some(Ok(len)), bytes.map(str::parse));
                    assert_eq!((folder.as_str(), name.as_str()), (fields[0], fields[1]));
                    let given = match fields[..] {
                        [_, _, sub] => sub,
                        [] | [_] | [_, _] | [_, _, _, _, ..] => "",
                    };
                    assert_eq!(sub, given);
                }
                (Err(_), false) => {}
                (got @ Ok(_), false) | (got @ Err(_), true) => panic!("{args:?}: {got:?}"),
            }
        }
    }

    /// A folder zipped reads back, with the zip crate's own reader, as exactly the same
    /// tree: every folder (empty ones too) and every file under the top name, with its
    /// bytes. Over generated trees (names of ASCII and of other scripts, nesting, empty
    /// files, files of one repeated byte, of noise, and of either under a name that is
    /// stored whole), against the tree as it was generated.
    #[test]
    fn a_folder_zipped_reads_back_as_the_same_tree() {
        use std::collections::BTreeMap;
        use std::io::Read;
        const PARTS: [&str; 8] = ["a", "Z9", "é", "日本", "ж", "ü-ß", "😀", " x y"];
        const EXTS: [&str; 4] = ["", ".txt", ".zip", ".GZ"];
        let mut rng = crate::xorshift::Xorshift::new(0x3c6e_f372_fe94_f82b);
        for case in 0..25 {
            let root = Root::new(&format!("zip{case}"));
            let top = format!("{}-top", PARTS[rng.below(8) as usize]);
            let base = root.0.join("src");
            std::fs::create_dir_all(&base).unwrap();
            // The expected tree: a folder is `None`, a file its bytes.
            let mut want: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
            want.insert(format!("{top}/"), None);
            let mut dirs = vec![(base.clone(), format!("{top}/"))];
            let mut n = 0;
            for _ in 0..rng.range(1, 40) {
                let (at, rel) = dirs[rng.below(dirs.len() as u64) as usize].clone();
                // The index keeps names apart, whatever the file system folds together.
                n += 1;
                let name = format!(
                    "{}{}-{n}{}",
                    PARTS[rng.below(8) as usize],
                    PARTS[rng.below(8) as usize],
                    EXTS[rng.below(4) as usize]
                );
                if rng.below(4) == 0 {
                    std::fs::create_dir(at.join(&name)).unwrap();
                    want.insert(format!("{rel}{name}/"), None);
                    dirs.push((at.join(&name), format!("{rel}{name}/")));
                    continue;
                }
                let len = match rng.below(4) {
                    0 => 0,
                    1 => rng.below(100),
                    2.. => rng.below(150_000),
                } as usize;
                let body: Vec<u8> = if rng.below(2) == 0 {
                    let byte = rng.below(256) as u8;
                    vec![byte; len]
                } else {
                    (0..len).map(|_| rng.below(256) as u8).collect()
                };
                std::fs::write(at.join(&name), &body).unwrap();
                want.insert(format!("{rel}{name}"), Some(body));
            }
            let mut zipped = Vec::new();
            write_zip(&base, &top, &mut zipped).unwrap();
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
            let mut got: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).unwrap();
                let name = entry.name().unwrap().into_owned();
                let body = if entry.is_dir() {
                    None
                } else {
                    let mut b = Vec::new();
                    entry.read_to_end(&mut b).unwrap();
                    Some(b)
                };
                assert!(got.insert(name, body).is_none(), "an entry twice");
            }
            assert!(got == want, "case {case}: the tree read back differs");
        }
    }
}
