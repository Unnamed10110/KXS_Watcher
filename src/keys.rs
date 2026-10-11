//! Two Windows keyboard quirks, fixed in a message hook that sees every message before winit:
//! - Alt codes (Alt + 1 2 4 on the numpad types `|`): Windows sends the character after Alt is
//!   released, when winit has no key event left to attach it to, so winit drops it. The hook
//!   catches it and `raw_input_hook` hands it to egui as typed text.
//! - When no window of ours has the keyboard focus (after a native dialog, for one), Windows sends
//!   plain keys as "system" keys, and their default handling beeps on every key even though the
//!   text still gets typed. The hook takes the focus back and passes them on as ordinary keys.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use crate::tasks::Lock;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_F10, VK_MENU};
use windows::Win32::UI::WindowsAndMessaging::{MSG, WM_CHAR, WM_KEYDOWN, WM_KEYFIRST, WM_KEYLAST, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP};

pub static TYPED: Mutex<String> = Mutex::new(String::new());
pub static CTX: OnceLock<egui::Context> = OnceLock::new();

thread_local! {
    /// The last keyboard message was Alt going up.
    static ALT_UP: Cell<bool> = const { Cell::new(false) };
}

/// lParam bit 29: Alt was down.
const ALT_DOWN: isize = 1 << 29;

/// Sees every message before winit; never swallows one, may turn a key into a plain one.
pub fn hook(msg: *const c_void) -> bool {
    // SAFETY: winit passes a pointer to its own `MSG`, taken from the queue and translated and
    // dispatched after this returns; it is writable (winit makes it from `&mut msg`).
    let m = unsafe { &mut *(msg as *mut MSG) };
    let (key, alt) = (m.wParam.0, m.lParam.0 & ALT_DOWN != 0);
    if matches!(m.message, WM_SYSKEYDOWN | WM_SYSKEYUP) && !alt && key != usize::from(VK_F10.0) {
        // A "system" key without Alt: nothing of ours has the keyboard focus. Take it back so
        // the next keys come normally, and pass this one on as a plain key (no beep).
        // SAFETY: `hwnd` is our window, on this thread.
        unsafe { SetFocus(Some(m.hwnd)) }.ok();
        m.message = if m.message == WM_SYSKEYDOWN { WM_KEYDOWN } else { WM_KEYUP };
    }
    match m.message {
        WM_KEYUP | WM_SYSKEYUP => ALT_UP.with(|a| a.set(key == usize::from(VK_MENU.0))),
        WM_CHAR if ALT_UP.with(|a| a.replace(false)) => {
            if let Some(c) = char::from_u32(key as u32).filter(|c| !c.is_control()) {
                TYPED.locked().push(c);
                if let Some(ctx) = CTX.get() {
                    ctx.request_repaint();
                }
            }
        }
        WM_KEYFIRST..=WM_KEYLAST => ALT_UP.with(|a| a.set(false)),
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{LPARAM, WPARAM};

    fn msg(message: u32, wparam: usize, lparam: isize) -> MSG {
        MSG { message, wParam: WPARAM(wparam), lParam: LPARAM(lparam), ..Default::default() }
    }

    #[test]
    fn system_keys_without_alt_become_plain_keys() {
        let mut m = msg(WM_SYSKEYDOWN, 0x5A, 1); // Z with no focus window: would beep
        hook(&mut m as *mut MSG as *const c_void);
        assert_eq!(m.message, WM_KEYDOWN);
        let mut m = msg(WM_SYSKEYUP, 0x5A, 1 | (3 << 30));
        hook(&mut m as *mut MSG as *const c_void);
        assert_eq!(m.message, WM_KEYUP);
        let mut m = msg(WM_SYSKEYDOWN, 0x61, 1 | ALT_DOWN); // Alt + numpad 1 (an Alt code): untouched
        hook(&mut m as *mut MSG as *const c_void);
        assert_eq!(m.message, WM_SYSKEYDOWN);
        let mut m = msg(WM_SYSKEYDOWN, usize::from(VK_F10.0), 1); // F10 is a real system key
        hook(&mut m as *mut MSG as *const c_void);
        assert_eq!(m.message, WM_SYSKEYDOWN);
    }
}
