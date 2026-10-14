//! Watches the clipboard for a copied `/travel` command and hands it to the
//! macro worker.
//!
//! Event-driven like the window watcher: a message-only window registered
//! with `AddClipboardFormatListener` receives `WM_CLIPBOARDUPDATE`, so the
//! thread sleeps in `GetMessageW` the rest of the time. The clipboard is
//! read only while the app is active and auto-travel is on, and the text is
//! dropped right after the `/travel` check — nothing is kept.

use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    RegisterClassW, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE,
    WNDCLASSW,
};

use crate::macros::{self, Job};

const CF_UNICODETEXT: u32 = 13;
/// A `/travel` command is ~20 characters; anything much longer is not one
/// and isn't worth copying out of the clipboard.
const MAX_CHARS: usize = 64;

pub fn spawn_watcher() {
    std::thread::Builder::new()
        .name("rorg-clipboard".into())
        .spawn(|| unsafe { run() })
        .expect("clipboard thread");
}

unsafe fn run() { unsafe {
    let Ok(module) = GetModuleHandleW(None) else { return };
    let class = w!("rorganizer_clipboard");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: module.into(),
        lpszClassName: class,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return;
    }
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!(""),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(module.into()),
        None,
    ) else {
        return;
    };
    if AddClipboardFormatListener(hwnd).is_err() {
        return;
    }

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).into() {
        DispatchMessageW(&msg);
    }
}}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT { unsafe {
    if msg == WM_CLIPBOARDUPDATE {
        on_clipboard_update(hwnd);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}}

unsafe fn on_clipboard_update(hwnd: HWND) { unsafe {
    if !macros::auto_travel() || macros::is_busy() {
        return;
    }
    let foreground = GetForegroundWindow().0 as isize;
    // Checked before reading: when the app is inactive or the copy came
    // from a Dofus window, the clipboard is never opened.
    let Some(target) = crate::hooks::travel_target(foreground) else { return };
    let Some(text) = read_text(hwnd) else { return };
    if macros::is_travel_command(&text) {
        macros::submit(Job::Travel(target));
    }
}}

/// Reads the clipboard as text, or `None` if it isn't text or is too long.
/// The copying app may still hold the clipboard open for a moment, hence
/// the few retries.
unsafe fn read_text(hwnd: HWND) -> Option<String> { unsafe {
    IsClipboardFormatAvailable(CF_UNICODETEXT).ok()?;
    let mut opened = false;
    for _ in 0..5 {
        if OpenClipboard(Some(hwnd)).is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if !opened {
        return None;
    }
    let text = (|| {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let global = HGLOBAL(handle.0);
        let ptr = GlobalLock(global) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let max = (GlobalSize(global) / 2).min(MAX_CHARS + 1);
        let units = std::slice::from_raw_parts(ptr, max);
        let len = units.iter().position(|&c| c == 0).unwrap_or(max);
        let out = (len <= MAX_CHARS).then(|| String::from_utf16_lossy(&units[..len]));
        let _ = GlobalUnlock(global);
        out
    })();
    let _ = CloseClipboard();
    text
}}
