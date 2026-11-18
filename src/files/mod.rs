//! `voxl files`: the shared folder's listing, downloads and uploads, run over ssh by the
//! relay on the daemon's machine. The relay reads its bound on a listing from here.

#[cfg(any(target_os = "linux", test))]
mod tool;
#[cfg(target_os = "linux")]
pub use tool::run;

/// The most entries a listing has: a folder holds more only in a tool's output, and the
/// page draws every entry it is sent. Provisional: upstream's bound (the first files
/// panel), unmeasured; what would measure it is the page's time to draw a listing of this
/// many. The panel shows the first ones and says nothing of the rest.
pub(crate) const ENTRIES_LISTED: usize = 5000;
