//! rustyfiles fork: lets the application choose the drop effect shown to the user while files
//! are dragged over its window (upstream always reports "copy").
//!
//! The effect returned from `Drop` stays `DROPEFFECT_COPY` on purpose: the application performs
//! the copy/move itself, and returning MOVE would make some sources delete the originals.

use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS};

/// MK_ALT (0x20) is not exported by the windows crate.
const MK_ALT_BIT: u32 = 0x20;

pub const NONE: u32 = 0;
pub const COPY: u32 = 1;
pub const MOVE: u32 = 2;
pub const LINK: u32 = 4;

static HINT: AtomicU32 = AtomicU32::new(COPY);

/// Effect to show when no modifier key is held (e.g. MOVE for same-volume drops).
pub fn set(effect: u32) {
    HINT.store(effect, Ordering::Relaxed);
}

pub(crate) fn effect_for(keys: MODIFIERKEYS_FLAGS) -> DROPEFFECT {
    let k = keys.0;
    let ctrl = k & MK_CONTROL.0 != 0;
    let shift = k & MK_SHIFT.0 != 0;
    let alt = k & MK_ALT_BIT != 0;
    if (ctrl && shift) || alt {
        DROPEFFECT_LINK
    } else if ctrl {
        DROPEFFECT_COPY
    } else if shift {
        DROPEFFECT_MOVE
    } else {
        DROPEFFECT(HINT.load(Ordering::Relaxed))
    }
}
