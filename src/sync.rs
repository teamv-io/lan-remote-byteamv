//! Shared clipboard-sync and file-transfer helpers used by both host and viewer.
//!
//! The TCP control channel is full-duplex: each peer runs a reader (incoming) and
//! a writer (outgoing) on a cloned handle. Since send and recv are independent
//! directions, clipboard text and file chunks flow both ways without interleaving.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::Sender;
use tracing::{info, warn};

use crate::proto::ControlMsg;

/// File-transfer chunk size (bytes). Stays well under the control-frame cap.
pub const FILE_CHUNK: usize = 48 * 1024;

/// Best-effort path to the user's Downloads directory.
pub fn downloads_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    let base = home
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Downloads")
}

/// Keep only the basename, defending against path-traversal in the sent name.
fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file").trim();
    if base.is_empty() {
        "file".to_string()
    } else {
        base.to_string()
    }
}

/// Avoid clobbering an existing file by appending " (n)".
fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|s| s.to_string_lossy().into_owned());
    for i in 1..10_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let cand = dir.join(name);
        if !cand.exists() {
            return cand;
        }
    }
    path
}

/// Reassembles incoming file transfers into the Downloads directory.
#[derive(Default)]
pub struct FileReceiver {
    open: HashMap<u32, (File, String, u64, u64)>, // id -> (file, name, received, total)
}

impl FileReceiver {
    /// Feed a control message; non-file messages are ignored.
    pub fn handle(&mut self, msg: &ControlMsg) {
        match msg {
            ControlMsg::FileStart { id, name, size } => {
                let dir = downloads_dir();
                let _ = fs::create_dir_all(&dir);
                let safe = sanitize(name);
                let path = unique_path(dir.join(&safe));
                match File::create(&path) {
                    Ok(f) => {
                        info!("Receiving '{}' ({} bytes) → {}", safe, size, path.display());
                        self.open.insert(*id, (f, safe, 0, *size));
                    }
                    Err(e) => warn!("Cannot create file {}: {e}", path.display()),
                }
            }
            ControlMsg::FileChunk { id, data } => {
                if let Some((f, _, recv, _)) = self.open.get_mut(id) {
                    if let Err(e) = f.write_all(data) {
                        warn!("File write error: {e}");
                    }
                    *recv += data.len() as u64;
                }
            }
            ControlMsg::FileEnd { id } => {
                if let Some((mut f, name, recv, total)) = self.open.remove(id) {
                    let _ = f.flush();
                    info!("File '{}' complete ({}/{} bytes)", name, recv, total);
                }
            }
            _ => {}
        }
    }
}

/// Watch the local clipboard and forward text changes to the peer while enabled.
/// `last` is shared with [`apply_remote_clipboard`] to suppress echo loops.
pub fn spawn_clipboard_watch(
    enabled: Arc<AtomicBool>,
    last: Arc<Mutex<String>>,
    out_tx: Sender<ControlMsg>,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("clipboard-watch".into())
        .spawn(move || {
            let mut cb = match arboard::Clipboard::new() {
                Ok(c) => c,
                Err(e) => {
                    warn!("Clipboard unavailable: {e}");
                    return;
                }
            };
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if enabled.load(Ordering::Relaxed) {
                    if let Ok(text) = cb.get_text() {
                        if !text.is_empty() {
                            let mut guard = last.lock().unwrap();
                            if text != *guard {
                                *guard = text.clone();
                                drop(guard);
                                out_tx.try_send(ControlMsg::Clipboard { text }).ok();
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
        .ok();
}

/// Apply a clipboard value received from the peer (updates `last` to avoid echo).
pub fn apply_remote_clipboard(text: String, last: Arc<Mutex<String>>) {
    {
        let mut guard = last.lock().unwrap();
        if *guard == text {
            return;
        }
        *guard = text.clone();
    }
    if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
        warn!("Set clipboard failed: {e}");
    }
}
