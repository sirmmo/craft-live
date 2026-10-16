//! The data folder: room states, sessions, the proof keys and the server secret.
//!
//! ```text
//! <data_dir>/proof-key.pem, proof-key-old.pem   the WOPI proof keys (owner-only)
//! <data_dir>/sessions.json                      live sessions (owner-only; holds WOPI tokens)
//! <data_dir>/rooms/<hash>/meta.json             a room: its file, version, saved/dirty state
//! <data_dir>/rooms/<hash>/state.yjs             the room's whole Yjs state (one update)
//! <data_dir>/rooms/<hash>/orphan-<ms>.yjs       a state set aside when the file changed outside
//! ```
//!
//! Every write goes through a temporary file renamed over the old one, so a crash never leaves
//! half a file.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Largest room state read back (the protocol's 16 MiB update, with room to grow by merging).
pub const MAX_STATE: u64 = 256 << 20;

/// Writes a file through a temporary one.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), String> {
    write_with(path, data, false)
}

/// Writes a file only its owner may read (Unix), through a temporary one.
pub fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    write_with(path, data, true)
}

fn write_with(path: &Path, data: &[u8], private: bool) -> Result<(), String> {
    let dir = path.parent().ok_or_else(|| format!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.{}.tmp", crate::ids::token(6)));
    let result = (|| -> Result<(), String> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut f = opts.open(&tmp).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
        f.write_all(data).map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
        f.sync_all().map_err(|e| format!("can't write {}: {e}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| format!("can't write {}: {e}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Reads a file up to `limit` bytes; `None` when it's missing.
pub fn read_limited(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    match std::fs::File::open(path) {
        Ok(f) => {
            let mut out = Vec::new();
            f.take(limit.saturating_add(1)).read_to_end(&mut out).map_err(|e| format!("{}: {e}", path.display()))?;
            if out.len() as u64 > limit {
                return Err(format!("{} is larger than {limit} bytes", path.display()));
            }
            Ok(Some(out))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// What craft-live remembers about a room between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RoomMeta {
    pub room: String,
    /// The WOPISrc the room was last opened with (empty for a room only desktop clients opened).
    pub wopi_src: String,
    /// The file's name (`Budget.xlsx`).
    pub name: String,
    /// The file's `Version` (CheckFileInfo) when the room was seeded or last saved.
    pub version: String,
    /// The file's WebDAV etag the room is based on, for saves through a desktop client's own
    /// WebDAV access (empty: unknown, after a WOPI save).
    pub etag: String,
    /// The room holds a document (it was seeded).
    pub seeded: bool,
    /// Edits since the last save reached the file.
    pub dirty: bool,
    pub last_saved_ms: i64,
    pub seeded_ms: i64,
}

/// The folder of one room: a hash of its id, so any room id is a safe folder name.
pub fn room_dir(data_dir: &Path, room: &str) -> PathBuf {
    let hash = Sha256::digest(room.as_bytes());
    let hex: String = hash.iter().take(16).map(|b| format!("{b:02x}")).collect();
    data_dir.join("rooms").join(hex)
}

/// A room's stored state: `(meta, Yjs state)`; `None` when there's none.
pub fn load_room(data_dir: &Path, room: &str) -> Result<Option<(RoomMeta, Vec<u8>)>, String> {
    let dir = room_dir(data_dir, room);
    let Some(meta) = read_limited(&dir.join("meta.json"), 1 << 20)? else { return Ok(None) };
    let meta: RoomMeta = serde_json::from_slice(&meta).map_err(|e| format!("{}: {e}", dir.join("meta.json").display()))?;
    if meta.room != room {
        return Ok(None);
    }
    let state = read_limited(&dir.join("state.yjs"), MAX_STATE)?.unwrap_or_default();
    Ok(Some((meta, state)))
}

/// Stores a room's state and meta (state first, so meta never points at a missing state).
pub fn save_room(data_dir: &Path, meta: &RoomMeta, state: Option<&[u8]>) -> Result<(), String> {
    let dir = room_dir(data_dir, &meta.room);
    if let Some(s) = state {
        write_private(&dir.join("state.yjs"), s)?;
    }
    let text = serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?;
    write_private(&dir.join("meta.json"), &text)
}

/// Sets a room's state aside (the file changed outside while the room had unsaved edits).
pub fn orphan_room(data_dir: &Path, room: &str, state: &[u8], now_ms: i64) -> Result<PathBuf, String> {
    let path = room_dir(data_dir, room).join(format!("orphan-{now_ms}.yjs"));
    write_private(&path, state)?;
    Ok(path)
}

/// Every stored room's meta (for `craft-live rooms`).
pub fn list_rooms(data_dir: &Path) -> Vec<RoomMeta> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("rooms")) else { return Vec::new() };
    let mut out: Vec<RoomMeta> = entries
        .filter_map(Result::ok)
        .filter_map(|e| read_limited(&e.path().join("meta.json"), 1 << 20).ok().flatten())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect();
    out.sort_by(|a, b| a.room.cmp(&b.room));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooms_round_trip() {
        let dir = std::env::temp_dir().join(format!("craft-live-store-{}", crate::ids::token(8)));
        assert_eq!(load_room(&dir, "nc:x:1").unwrap(), None);
        let meta = RoomMeta { room: "nc:x:1".into(), seeded: true, dirty: true, version: "7".into(), ..RoomMeta::default() };
        save_room(&dir, &meta, Some(&[1, 2, 3])).unwrap();
        assert_eq!(load_room(&dir, "nc:x:1").unwrap(), Some((meta.clone(), vec![1, 2, 3])));
        assert_eq!(list_rooms(&dir), vec![meta]);
        assert_ne!(room_dir(&dir, "a"), room_dir(&dir, "b"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
