//! Input macros: replaying a click across every account, pasting a copied
//! `/travel` command into the first account's chat, and inviting every
//! other account into the first one's group.
//!
//! Unlike the rest of the app these *do* send input to Dofus (SendInput).
//! They run on a dedicated thread: a LL hook callback that blocks for more
//! than a few hundred ms gets silently uninstalled by Windows, and a macro
//! sleeps between every step.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Live account HWNDs in cycle order. The click starts on whichever of
    /// them is in the foreground and walks the list from there.
    ClickAll(Vec<isize>),
    /// Paste the clipboard into this account's chat and send it.
    Travel(isize),
    /// Type `/invite <name>` in `leader`'s chat for each name.
    InviteAll { leader: isize, names: Vec<String> },
}

/// Delays between macro steps, in milliseconds. Hand-editable in
/// `config.json` (`macro_timing`); there is no UI for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MacroTiming {
    /// Wait after a window comes to the foreground, before touching it.
    pub switch_ms: u32,
    /// Wait after moving the cursor and after each click.
    pub click_ms: u32,
    /// Wait after opening the chat and after pasting.
    pub chat_ms: u32,
    /// Wait between two `/invite` commands, so Dofus' anti-flood doesn't
    /// swallow the later ones.
    pub invite_ms: u32,
}

impl Default for MacroTiming {
    fn default() -> Self {
        Self {
            switch_ms: 120,
            click_ms: 60,
            chat_ms: 120,
            invite_ms: 400,
        }
    }
}

static SENDER: OnceLock<Sender<Job>> = OnceLock::new();
/// Set from `submit` until the worker finishes the job. Doubles as the
/// "ignore further triggers" flag read by the hooks.
static BUSY: AtomicBool = AtomicBool::new(false);
static AUTO_TRAVEL: AtomicBool = AtomicBool::new(true);
static SWITCH_MS: AtomicU32 = AtomicU32::new(120);
static CLICK_MS: AtomicU32 = AtomicU32::new(60);
static CHAT_MS: AtomicU32 = AtomicU32::new(120);
static INVITE_MS: AtomicU32 = AtomicU32::new(400);

pub fn is_busy() -> bool {
    BUSY.load(Ordering::Relaxed)
}

pub fn auto_travel() -> bool {
    AUTO_TRAVEL.load(Ordering::Relaxed)
}

pub fn set_auto_travel(on: bool) {
    AUTO_TRAVEL.store(on, Ordering::Relaxed);
}

pub fn set_timing(t: MacroTiming) {
    SWITCH_MS.store(t.switch_ms, Ordering::Relaxed);
    CLICK_MS.store(t.click_ms, Ordering::Relaxed);
    CHAT_MS.store(t.chat_ms, Ordering::Relaxed);
    INVITE_MS.store(t.invite_ms, Ordering::Relaxed);
}

/// Queues a job unless one is already playing — a second press during a
/// click-all is dropped, never queued behind it.
pub fn submit(job: Job) {
    let Some(tx) = SENDER.get() else { return };
    if BUSY
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    if tx.send(job).is_err() {
        BUSY.store(false, Ordering::Release);
    }
}

pub fn spawn() {
    let (tx, rx) = channel::<Job>();
    if SENDER.set(tx).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("rorg-macros".into())
        .spawn(move || {
            for job in rx {
                match job {
                    Job::ClickAll(hwnds) => imp::click_all(&hwnds),
                    Job::Travel(hwnd) => imp::travel(hwnd),
                    Job::InviteAll { leader, names } => imp::invite_all(leader, &names),
                }
                BUSY.store(false, Ordering::Release);
            }
        })
        .expect("macros thread");
}

fn sleep_ms(ms: &AtomicU32) {
    std::thread::sleep(Duration::from_millis(ms.load(Ordering::Relaxed) as u64));
}

/// Cursor position as a fraction of a client rect, or `None` when it lies
/// outside. Fractions rather than pixels so a click lands on the same UI
/// element in windows of different sizes (Dofus scales its UI).
pub(crate) fn to_relative(cursor: (i32, i32), rect: (i32, i32, i32, i32)) -> Option<(f64, f64)> {
    let (left, top, right, bottom) = rect;
    let (w, h) = (right - left, bottom - top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let (x, y) = (cursor.0 - left, cursor.1 - top);
    if x < 0 || y < 0 || x >= w || y >= h {
        return None;
    }
    Some((x as f64 / w as f64, y as f64 / h as f64))
}

pub(crate) fn from_relative(rel: (f64, f64), rect: (i32, i32, i32, i32)) -> (i32, i32) {
    let (left, top, right, bottom) = rect;
    (
        left + (rel.0 * (right - left) as f64).round() as i32,
        top + (rel.1 * (bottom - top) as f64).round() as i32,
    )
}

/// Recognises what Dofus map sites put on the clipboard: `/travel -20,34`,
/// `/travel -20 34`, `/travel [-20,34]`. Anything else is ignored, so the
/// auto paste never sends arbitrary clipboard text into the game chat.
pub(crate) fn is_travel_command(text: &str) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^/travel\s+\[?\s*-?\d+\s*[,;\s]\s*-?\d+\s*\]?$")
            .expect("travel regex")
    });
    re.is_match(text.trim())
}

/// The chat command inviting `name`. Same syntax in Dofus and Rétro.
pub(crate) fn invite_command(name: &str) -> String {
    format!("/invite {}", name.trim())
}

#[cfg(windows)]
mod imp {
    use super::*;

    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEINPUT, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_RETURN, VK_SPACE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetWindowInfo, SetCursorPos, WINDOWINFO,
    };

    const VK_A: VIRTUAL_KEY = VIRTUAL_KEY(0x41);
    const VK_V: VIRTUAL_KEY = VIRTUAL_KEY(0x56);
    const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);

    pub fn click_all(hwnds: &[isize]) {
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        let origin = foreground();
        let Some(start) = hwnds.iter().position(|&h| h == origin) else {
            return;
        };
        let Some(rel) =
            client_rect(origin).and_then(|r| to_relative((cursor.x, cursor.y), r))
        else {
            return;
        };

        for k in 0..hwnds.len() {
            let hwnd = hwnds[(start + k) % hwnds.len()];
            if k > 0 && !activate(hwnd) {
                continue;
            }
            let Some(rect) = client_rect(hwnd) else { continue };
            let (x, y) = from_relative(rel, rect);
            let _ = unsafe { SetCursorPos(x, y) };
            sleep_ms(&CLICK_MS);
            left_click();
            sleep_ms(&CLICK_MS);
        }

        activate(origin);
        let _ = unsafe { SetCursorPos(cursor.x, cursor.y) };
    }

    pub fn travel(hwnd: isize) {
        if !activate(hwnd) {
            return;
        }
        // Space opens the chat. Ctrl+A then selects whatever was already in
        // the input — including that Space, if the chat was already open —
        // so the paste replaces it instead of producing " /travel …".
        tap(VK_SPACE);
        sleep_ms(&CHAT_MS);
        chord(VK_CONTROL, VK_A);
        chord(VK_CONTROL, VK_V);
        sleep_ms(&CHAT_MS);
        tap(VK_RETURN);
    }

    /// Types the commands rather than pasting them: the user's clipboard
    /// stays untouched, and the clipboard watcher isn't woken up.
    pub fn invite_all(leader: isize, names: &[String]) {
        if !activate(leader) {
            return;
        }
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                sleep_ms(&INVITE_MS);
                // The user may have switched away meanwhile; never type
                // into another window.
                if foreground() != leader {
                    return;
                }
            }
            // Same open-chat / select-all dance as `travel`.
            tap(VK_SPACE);
            sleep_ms(&CHAT_MS);
            chord(VK_CONTROL, VK_A);
            type_text(&super::invite_command(name));
            sleep_ms(&CHAT_MS);
            tap(VK_RETURN);
        }
    }

    fn foreground() -> isize {
        unsafe { GetForegroundWindow() }.0 as isize
    }

    /// Client area in screen coordinates, as (left, top, right, bottom).
    fn client_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
        let mut info = WINDOWINFO {
            cbSize: std::mem::size_of::<WINDOWINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetWindowInfo(HWND(hwnd as *mut _), &mut info) }.ok()?;
        let r = info.rcClient;
        Some((r.left, r.top, r.right, r.bottom))
    }

    /// Focuses `hwnd` and waits until Windows reports it in the foreground.
    ///
    /// If the foreground lock-out refuses the switch, a synthetic Alt tap
    /// makes this process "the last one to send input", which lifts it, and
    /// we try once more.
    fn activate(hwnd: isize) -> bool {
        for attempt in 0..2 {
            if attempt == 1 {
                tap(VK_MENU);
            }
            crate::win::focus_window(hwnd);
            let deadline = Instant::now() + FOCUS_TIMEOUT / 2;
            while Instant::now() < deadline {
                if foreground() == hwnd {
                    sleep_ms(&SWITCH_MS);
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        false
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) {
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    }

    fn tap(vk: VIRTUAL_KEY) {
        send(&[key(vk, false), key(vk, true)]);
    }

    /// Sends `text` as Unicode characters (`KEYEVENTF_UNICODE`), so it is
    /// independent of the keyboard layout — accents in names included.
    fn type_text(text: &str) {
        for unit in text.encode_utf16() {
            let ch = |flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            send(&[ch(KEYEVENTF_UNICODE), ch(KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn chord(modifier: VIRTUAL_KEY, vk: VIRTUAL_KEY) {
        send(&[
            key(modifier, false),
            key(vk, false),
            key(vk, true),
            key(modifier, true),
        ]);
        std::thread::sleep(Duration::from_millis(30));
    }

    fn left_click() {
        let mouse = |flags| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        send(&[mouse(MOUSEEVENTF_LEFTDOWN)]);
        std::thread::sleep(Duration::from_millis(30));
        send(&[mouse(MOUSEEVENTF_LEFTUP)]);
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn click_all(_: &[isize]) {}
    pub fn travel(_: isize) {}
    pub fn invite_all(_: isize, _: &[String]) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_round_trips_on_same_rect() {
        let rect = (100, 50, 1380, 770);
        let rel = to_relative((740, 410), rect).unwrap();
        assert_eq!(from_relative(rel, rect), (740, 410));
    }

    #[test]
    fn relative_scales_to_a_differently_sized_window() {
        // Centre of a 1280×720 client maps to the centre of a 640×360 one.
        let rel = to_relative((640, 360), (0, 0, 1280, 720)).unwrap();
        assert_eq!(from_relative(rel, (2000, 100, 2640, 460)), (2320, 280));
    }

    #[test]
    fn cursor_outside_client_area_is_rejected() {
        let rect = (100, 100, 200, 200);
        assert!(to_relative((99, 150), rect).is_none());
        assert!(to_relative((150, 200), rect).is_none());
        assert!(to_relative((150, 150), (0, 0, 0, 0)).is_none());
    }

    #[test]
    fn travel_command_formats_are_recognised() {
        for ok in [
            "/travel -20,34",
            "/travel 5,-12",
            "/travel -20 34",
            "/travel [-20,34]",
            "/travel -20, 34\r\n",
            "/TRAVEL 1,2",
        ] {
            assert!(is_travel_command(ok), "{ok:?}");
        }
    }

    #[test]
    fn other_clipboard_text_is_rejected() {
        for bad in [
            "",
            "/travel",
            "travel -20,34",
            "/travel -20,34 hello",
            "/w friend hi",
            "/travel -20,34\n/w friend hi",
        ] {
            assert!(!is_travel_command(bad), "{bad:?}");
        }
    }

    #[test]
    fn invite_command_format() {
        assert_eq!(invite_command("Bob-Iop"), "/invite Bob-Iop");
        assert_eq!(invite_command(" Élodie "), "/invite Élodie");
    }

    #[test]
    fn submit_without_worker_is_a_no_op() {
        // No spawn() in tests: submit must neither panic nor latch BUSY.
        submit(Job::Travel(1));
        assert!(!is_busy());
    }
}
