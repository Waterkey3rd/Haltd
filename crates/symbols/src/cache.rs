//! On-disk index cache, keyed by the hash of the ELF contents.

use std::fs;
use std::path::{Path, PathBuf};

use crate::index::Index;

const MAGIC: &[u8; 8] = b"JLKDIDX\0";
/// Bump whenever [`Index`] or the way it is computed changes.
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = MAGIC.len() + 4;

pub(crate) fn default_dir() -> Option<PathBuf> {
    Some(crate::paths::cache_root()?.join("index"))
}

pub(crate) fn file_for(dir: &Path, hash: &str) -> PathBuf {
    dir.join(format!("{hash}.idx"))
}

/// Returns None for a missing, stale or corrupt file; the caller rebuilds.
pub(crate) fn load(path: &Path) -> Option<Index> {
    let bytes = fs::read(path).ok()?;
    let body = bytes
        .strip_prefix(MAGIC)?
        .strip_prefix(&FORMAT_VERSION.to_le_bytes())?;
    debug_assert_eq!(bytes.len() - body.len(), HEADER_LEN);
    match bincode::deserialize(body) {
        Ok(index) => Some(index),
        Err(error) => {
            tracing::warn!("discarding corrupt index {}: {error}", path.display());
            None
        }
    }
}

/// Write atomically, so a crash cannot leave a truncated index behind.
pub(crate) fn store(path: &Path, index: &Index) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = bincode::serialize(index).map_err(std::io::Error::other)?;
    let mut bytes = Vec::with_capacity(HEADER_LEN + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&body);

    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&temporary, &bytes)?;
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("haltd-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn sample() -> Index {
        Index {
            strings: b"abc".to_vec(),
            units: vec![1, 2],
            ..Index::default()
        }
    }

    #[test]
    fn index_round_trips() {
        let path = file_for(&scratch("round"), "h");
        store(&path, &sample()).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.strings, b"abc");
        assert_eq!(loaded.units, vec![1, 2]);
    }

    #[test]
    fn wrong_version_and_corruption_are_rejected() {
        let path = file_for(&scratch("bad"), "h");
        store(&path, &sample()).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        bytes[MAGIC.len()] = bytes[MAGIC.len()].wrapping_add(1);
        fs::write(&path, &bytes).unwrap();
        assert!(load(&path).is_none());

        fs::write(&path, b"JLKDIDX\0").unwrap();
        assert!(load(&path).is_none());

        store(&path, &sample()).unwrap();
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..HEADER_LEN + 3]).unwrap();
        assert!(load(&path).is_none());
    }

    /// Windows refuses to replace a file another process holds open.
    #[cfg(windows)]
    #[test]
    fn a_locked_target_fails_cleanly_and_keeps_the_old_index() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = file_for(&scratch("locked"), "h");
        store(&path, &sample()).unwrap();
        let lock = fs::File::options()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(store(&path, &sample()).is_err());
        drop(lock);
        assert!(load(&path).is_some());
        let leftovers = fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn missing_file_is_a_miss() {
        assert!(load(Path::new("/nonexistent/haltd.idx")).is_none());
    }
}
