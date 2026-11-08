use std::ffi::{c_char, c_int, CStr};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use probe_api::{DownloadStats, Probe, Progress, Result};

use crate::jlink::{c_path, call_error, dll, JLink};

type Callback = *const (dyn Fn(Progress) + 'static);

struct Slot(Callback);
// SAFETY: the pointer is only dereferenced during `download`, which keeps the
// referent alive and blocks until the library has finished calling back.
unsafe impl Send for Slot {}

static PROGRESS: Mutex<Option<Slot>> = Mutex::new(None);

/// Clears the callback even if the download fails.
struct ProgressGuard;

impl Drop for ProgressGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = PROGRESS.lock() {
            *slot = None;
        }
    }
}

fn text(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the library passes NUL-terminated strings valid for the call.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

unsafe extern "C" fn on_progress(action: *const c_char, item: *const c_char, percent: c_int) {
    if let Ok(slot) = PROGRESS.lock() {
        if let Some(Slot(callback)) = slot.as_ref() {
            // SAFETY: see `Slot`.
            unsafe {
                (**callback)(Progress {
                    action: text(action),
                    item: text(item),
                    percent,
                })
            };
        }
    }
}

pub(crate) fn download(
    jlink: &mut JLink,
    file: &Path,
    progress: &dyn Fn(Progress),
) -> Result<DownloadStats> {
    let path = c_path(file)?;

    // The library requires a halted core; failing here just means it was halted.
    if let Err(e) = jlink.halt() {
        tracing::debug!("halt before download: {e}");
    }
    // Only sectors whose contents differ are erased and written.
    if let Err(e) = jlink.exec("SetSkipProgOnCRCMatch = 1") {
        tracing::warn!("cannot enable skipping unchanged sectors: {e}");
    }

    // SAFETY: only the lifetime is erased; `_guard` clears the slot before
    // `progress` goes out of scope.
    let callback: Callback = unsafe { std::mem::transmute(progress as *const dyn Fn(Progress)) };
    if let Ok(mut slot) = PROGRESS.lock() {
        *slot = Some(Slot(callback));
    }
    let _guard = ProgressGuard;
    dll!(jlink, JLINK_SetFlashProgProgressCallback(Some(on_progress)));

    let started = Instant::now();
    let code = dll!(jlink, JLINK_DownloadFile(path.as_ptr(), 0));
    let elapsed = started.elapsed();
    dll!(jlink, JLINK_SetFlashProgProgressCallback(None));
    if code < 0 {
        return Err(call_error("JLINK_DownloadFile", code));
    }
    // A non-negative result only means success; on V9.80 it is not a byte count.
    Ok(DownloadStats { elapsed })
}
