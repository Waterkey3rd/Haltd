//! Where files live and how source paths compare.
//!
//! Paths are canonicalized where they enter (DWARF, editor requests), so the
//! rest of the code only ever sees `/` separators. All of this is a no-op on
//! Linux.

use std::borrow::Cow;
use std::path::PathBuf;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Root of everything jlinkd writes by itself: index cache, logs, recordings.
/// `$JLINKD_CACHE_DIR`, or else the platform default.
pub fn cache_root() -> Option<PathBuf> {
    env_path("JLINKD_CACHE_DIR").or_else(default_root)
}

#[cfg(not(windows))]
fn default_root() -> Option<PathBuf> {
    env_path("XDG_CACHE_HOME")
        .map(|base| base.join("jlinkd"))
        .or_else(|| env_path("HOME").map(|home| home.join(".cache").join("jlinkd")))
}

/// `jlinkd` in the user's Documents folder, wherever that has been moved to.
#[cfg(windows)]
fn default_root() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath};

    let mut raw = std::ptr::null_mut();
    // SAFETY: the GUID and out pointer are valid. On success `raw` is a
    // NUL-terminated string; it is freed below, which is also required on failure.
    let documents = unsafe {
        let status = SHGetKnownFolderPath(&FOLDERID_Documents, 0, std::ptr::null_mut(), &mut raw);
        let path = (status == 0).then(|| {
            let length = (0..).take_while(|&i| *raw.add(i) != 0).count();
            std::ffi::OsString::from_wide(std::slice::from_raw_parts(raw, length))
        });
        CoTaskMemFree(raw.cast());
        path
    };
    Some(PathBuf::from(documents?).join("jlinkd"))
}

/// Canonical spelling: `/` separators and a lower-case drive letter.
#[cfg(windows)]
pub fn normalize(path: &str) -> Cow<'_, str> {
    let mut out = path.replace('\\', "/");
    if out.as_bytes().get(1) == Some(&b':') {
        out[..1].make_ascii_lowercase();
    }
    Cow::Owned(out)
}

#[cfg(not(windows))]
pub fn normalize(path: &str) -> Cow<'_, str> {
    Cow::Borrowed(path)
}

/// One value for every spelling of a path, for use as a map key.
#[cfg(windows)]
pub fn key(path: &str) -> Cow<'_, str> {
    Cow::Owned(normalize(path).to_ascii_lowercase())
}

#[cfg(not(windows))]
pub fn key(path: &str) -> Cow<'_, str> {
    Cow::Borrowed(path)
}

/// Whether two canonical paths, or components of them, name the same thing.
pub(crate) fn same(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Whether `path` ends with the whole components of `suffix`.
pub(crate) fn ends_with_components(path: &str, suffix: &str) -> bool {
    let Some(split) = path.len().checked_sub(suffix.len()) else {
        return false;
    };
    let (head, tail) = path.as_bytes().split_at(split);
    let tail_matches = if cfg!(windows) {
        tail.eq_ignore_ascii_case(suffix.as_bytes())
    } else {
        tail == suffix.as_bytes()
    };
    tail_matches && (head.is_empty() || head.ends_with(b"/"))
}

/// A first component such as `c:`, which `..` cannot climb above.
pub(crate) fn is_drive(component: &str) -> bool {
    cfg!(windows) && component.len() == 2 && component.as_bytes()[1] == b':'
}

pub(crate) fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || path.split('/').next().is_some_and(is_drive)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::index::join_path;

    #[test]
    fn default_cache_root_is_inside_documents() {
        let root = default_root().unwrap();
        assert!(root.is_absolute(), "{root:?}");
        assert!(root.ends_with("jlinkd"), "{root:?}");
    }

    #[test]
    fn every_spelling_of_a_path_normalizes_alike() {
        for spelling in [r"C:\a\b.cpp", "C:/a/b.cpp", r"c:\a\b.cpp", r"c:/a\b.cpp"] {
            assert_eq!(normalize(spelling), "c:/a/b.cpp");
        }
        assert_eq!(normalize("/home/me/a.c"), "/home/me/a.c");
        assert_eq!(normalize(r"中文\a.c"), "中文/a.c");
        assert_eq!(key(r"E:\A\B.c"), key("e:/a/b.C"));
    }

    #[test]
    fn drive_letters_are_absolute_and_stop_parent_components() {
        assert_eq!(
            join_path(Some(r"E:\w\build"), Some("..\\src"), "a.c"),
            "e:/w/src/a.c"
        );
        assert_eq!(
            join_path(Some("/ignored"), Some(r"D:\inc"), "a.h"),
            "d:/inc/a.h"
        );
        assert_eq!(join_path(Some(r"E:\w"), None, r"C:/x/a.c"), "c:/x/a.c");
        assert_eq!(join_path(Some("C:/"), Some("../.."), "a.c"), "c:/a.c");
        assert_eq!(join_path(Some("/w/build"), None, "a.c"), "/w/build/a.c");
    }

    #[test]
    fn comparison_ignores_case() {
        assert!(same("Core/Main.c", "core/main.C"));
        assert!(ends_with_components("e:/w/Core/Main.c", "core/main.c"));
        assert!(!ends_with_components("e:/w/xcore/main.c", "core/main.c"));
        // Must not slice inside a multi-byte character.
        assert!(!ends_with_components("e:/中文.c", "x.c"));
    }
}
