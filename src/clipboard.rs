//! What the clipboard sync exchanges with clients, and which ways it runs: the same on X,
//! macOS and Windows. Each platform's `desktop` syncs it its own way and names these as
//! its own.

/// Clipboard contents exchanged with clients.
pub enum ClipContent {
    Text(String),
    /// PNG bytes.
    Image(Vec<u8>),
}

/// Which ways the clipboard syncs (`--enable-clipboard`): a client's into the desktop's
/// (`true` or `in`), and the desktop's out to clients (`true` or `out`).
pub struct ClipboardMode {
    pub to_server: bool,
    pub to_client: bool,
}
