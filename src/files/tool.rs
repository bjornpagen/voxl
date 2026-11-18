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

/// The request `args` (after `files`) make, or what is wrong with them.
fn parse(args: &[String]) -> Result<Request, String> {
    let mut root = None;
    let mut len = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => root = Some(it.next().ok_or("--root needs a folder")?),
            "--bytes" => len = Some(it.next().ok_or("--bytes needs a number")?),
            _ => rest.push(a.as_str()),
        }
    }
    let root = root.ok_or("--root is required")?;
    // The root as the daemon canonicalised it; canonical again here, so every path under
    // it compares with `starts_with`.
    let root = std::fs::canonicalize(root).map_err(|e| format!("--root {root}: {e}"))?;
    match (rest.as_slice(), len) {
        (["list", path], None) => Ok(Request::List {
            root,
            path: path.to_string(),
        }),
        (["get", path], None) => Ok(Request::Get {
            root,
            path: path.to_string(),
        }),
        (["put", folder, name, sub @ ..], Some(len)) if sub.len() <= 1 => Ok(Request::Put {
            root,
            len: len
                .parse()
                .map_err(|_| format!("--bytes {len}: not a whole number"))?,
            folder: folder.to_string(),
            name: name.to_string(),
            sub: sub.first().map_or(String::new(), |s| s.to_string()),
        }),
        _ => Err("not a request".into()),
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

/// Resolves a page's path ("Downloads/report.pdf") under `root`. Absolute paths and `..`
/// are refused; symlinks are followed only if they stay inside the root. `Missing::Allowed`:
/// a path that does not exist yet is taken as it is.
fn resolve(root: &Path, rel: &str, missing: Missing) -> Result<PathBuf, Refused> {
    let bad = || Refused::new(400, "invalid path");
    if rel.contains('\0') {
        return Err(bad());
    }
    let mut p = root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => p.push(s),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Err(bad()),
        }
    }
    match (p.canonicalize(), missing) {
        (Ok(c), _) if c.starts_with(root) => Ok(c),
        (Ok(_), _) => Err(Refused::new(403, "outside the shared folder")),
        (Err(_), Missing::Allowed) => Ok(p),
        (Err(_), Missing::Refused) => Err(Refused::new(404, "not found")),
    }
}

/// Whether a path may be missing: the upload folder is made by the first upload.
#[derive(Clone, Copy)]
enum Missing {
    Allowed,
    Refused,
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A folder's entries, hidden ones left out; a folder that does not exist is listed
/// empty, as the upload folder is until something is uploaded.
fn list(root: &Path, path: &str) -> Result<serde_json::Value, Fail> {
    let dir = resolve(root, path, Missing::Allowed)?;
    let mut entries = Vec::new();
    match std::fs::read_dir(&dir) {
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
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as u64);
                entries.push(serde_json::json!({
                    "name": name, "dir": md.is_dir(), "size": md.len(), "mtime": mtime
                }));
                if entries.len() == ENTRIES_LISTED {
                    break;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(Refused::new(404, "not a folder").into()),
    }
    Ok(serde_json::json!({ "path": rel(root, &dir).trim_start_matches('/'), "entries": entries }))
}

/// A file's bytes after the line that names them, or a folder zipped as it is read.
fn get(root: &Path, path: &str, out: &mut impl Write) -> Result<(), Fail> {
    let path = resolve(root, path, Missing::Refused)?;
    let name = path
        .file_name()
        .map_or_else(|| "Home".into(), |n| n.to_string_lossy().into_owned());
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
            let mut opts = base.large_file(md.len() >= u32::MAX as u64);
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
            if let Some(t) = md.modified().ok().and_then(zip_time) {
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

/// A plain file name: no separators, no `.`/`..`, no control characters, at most 255
/// bytes (the longest name Linux's and macOS's file systems take).
fn clean_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    ok.then_some(name)
}

/// `name`, or `name (1).ext`, `name (2).ext`, ... if taken.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut n = 0u64;
    loop {
        n += 1;
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if !p.exists() {
            return p;
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
    let name = clean_name(name).ok_or_else(|| Refused::new(400, "invalid file name"))?;
    // The folder (the upload folder is made by the first upload) and `sub` inside it
    // ("Photos/2024") are made as needed, one level at a time from the root, each step
    // checked: a symlink on the way could lead outside the shared folder, and nothing may
    // be made out there.
    let mut dir = root.to_path_buf();
    // A folder is a relative path, as `resolve` takes one: an absolute one or `..` is none.
    if Path::new(folder)
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(Refused::new(400, "invalid path").into());
    }
    for part in folder
        .split('/')
        .chain(sub.split('/'))
        .filter(|p| !p.is_empty() && *p != ".")
    {
        let p = clean_name(part).ok_or_else(|| Refused::new(400, "invalid folder name"))?;
        let next = dir.join(p);
        if let Err(e) = std::fs::create_dir(&next) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(Refused::new(500, format!("cannot create the folder: {e}")).into());
            }
        }
        dir = match std::fs::canonicalize(&next) {
            Ok(c) if c.starts_with(root) && c.is_dir() => c,
            Ok(c) if c.starts_with(root) => {
                return Err(Refused::new(409, "a file is in the way of the folder").into())
            }
            _ => return Err(Refused::new(403, "outside the shared folder").into()),
        };
    }
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
    let dest = unique(&dir, name);
    if let Err(e) = std::fs::rename(&part, &dest) {
        let _ = std::fs::remove_file(&part);
        return Err(Refused::new(500, format!("cannot save: {e}")).into());
    }
    Ok(serde_json::json!({ "path": rel(root, &dest).trim_start_matches('/'), "size": written }))
}

/// Copies exactly `len` bytes of `input` to `file`: an input that ends before them, or
/// has more after them, is refused.
fn copy_exactly(input: &mut impl Read, file: &mut impl Write, len: u64) -> Result<(), Refused> {
    let interrupted = || Refused::new(400, "upload interrupted");
    let mut buf = [0u8; 64 << 10];
    let mut left = len;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
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
    /// out are refused, for every request.
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
        }
        assert_eq!(std::fs::read_dir(&outside.0).unwrap().count(), 1);
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
                    assert_eq!(Some(len), bytes.and_then(|n| n.parse().ok()));
                    assert_eq!((folder.as_str(), name.as_str()), (fields[0], fields[1]));
                    assert_eq!(sub, fields.get(2).copied().unwrap_or_default());
                }
                (Err(_), false) => {}
                (got, _) => panic!("{args:?}: {got:?}"),
            }
        }
    }
}
