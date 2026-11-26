//! The launch stage's window: what is being built before the title starts,
//! a bar, and Esc (or closing the window) to start the title now.
//!
//! The window is our own, on a thread of its own, and is gone before the
//! title makes its window. A process started minimised (a scripted run)
//! gets none: the first window of such a process takes the minimised show
//! state, and the title's window would then open in front.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetStartupInfoW, STARTF_USESHOWWINDOW, STARTUPINFOW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// The lines shown, and the bar's part (0 to 1).
static SHOWN: Mutex<(Vec<String>, f32)> = Mutex::new((Vec::new(), 0.0));
static CANCELLED: AtomicBool = AtomicBool::new(false);
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Our message: the window is to go.
const CLOSE: u32 = WM_APP + 1;
const ESC: usize = 0x1B;

/// The launch stage's window, while it lives.
pub struct Window {
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Whether the process was started minimised or hidden.
fn started_hidden() -> bool {
    let mut info = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: fills the structure of the given size.
    unsafe { GetStartupInfoW(&mut info) };
    let show = SHOW_WINDOW_CMD(i32::from(info.wShowWindow));
    (info.dwFlags & STARTF_USESHOWWINDOW).0 != 0
        && [
            SW_HIDE,
            SW_MINIMIZE,
            SW_SHOWMINIMIZED,
            SW_SHOWMINNOACTIVE,
            SW_FORCEMINIMIZE,
        ]
        .contains(&show)
}

/// Opens the window titled `title`; `None` in a process started minimised
/// or where it cannot be made.
pub fn open(title: &str) -> Option<Window> {
    if started_hidden() {
        return None;
    }
    CANCELLED.store(false, Ordering::Relaxed);
    let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("victory-stage-window".into())
        .spawn(move || {
            let made = make(&title);
            let _ = ready_tx.send(made.is_some());
            if made.is_some() {
                pump();
            }
            WINDOW.store(0, Ordering::Relaxed);
        })
        .ok()?;
    if ready_rx.recv().ok()? {
        Some(Window {
            thread: Some(thread),
        })
    } else {
        let _ = thread.join();
        None
    }
}

impl Window {
    /// Shows `lines` and a bar at `part` (0 to 1).
    pub fn show(&self, lines: Vec<String>, part: f32) {
        *SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = (lines, part.clamp(0.0, 1.0));
    }

    /// Whether Esc was pressed or the window closed.
    pub fn cancelled(&self) -> bool {
        CANCELLED.load(Ordering::Relaxed)
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let hwnd = WINDOW.load(Ordering::Relaxed);
        if hwnd != 0 {
            // SAFETY: a window of ours; a message to one gone already fails.
            let _ =
                unsafe { PostMessageW(Some(HWND(hwnd as *mut _)), CLOSE, WPARAM(0), LPARAM(0)) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn make(title: &[u16]) -> Option<HWND> {
    const CLASS: PCWSTR = w!("VIctoryLaunchStage");
    const SIZE: (i32, i32) = (640, 220);
    // SAFETY: registers our class and creates a window of it on this thread.
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            ..Default::default()
        };
        RegisterClassW(&class);
        let (x, y) = (
            (GetSystemMetrics(SM_CXSCREEN) - SIZE.0) / 2,
            (GetSystemMetrics(SM_CYSCREEN) - SIZE.1) / 2,
        );
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            x.max(0),
            y.max(0),
            SIZE.0,
            SIZE.1,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        WINDOW.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        SetTimer(Some(hwnd), 1, 250, None);
        Some(hwnd)
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

extern "system" fn procedure(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: messages to our own window, handled as the system documents.
    unsafe {
        match message {
            WM_TIMER => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == ESC => {
                CANCELLED.store(true, Ordering::Relaxed);
                LRESULT(0)
            }
            WM_CLOSE => {
                // The user closed it: the title starts now.
                CANCELLED.store(true, Ordering::Relaxed);
                LRESULT(0)
            }
            CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

/// Draws the lines and the bar.
unsafe fn paint(hwnd: HWND) {
    let (lines, part) = SHOWN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // SAFETY: painting our own window between BeginPaint and EndPaint; every
    // GDI object made here is let go before returning.
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut all = RECT::default();
        let _ = GetClientRect(hwnd, &mut all);
        let fill = |r: &RECT, color: COLORREF| {
            let brush = CreateSolidBrush(color);
            FillRect(dc, r, brush);
            let _ = DeleteObject(brush.into());
        };
        fill(&all, rgb(24, 26, 30));
        let font = CreateFontW(
            -17,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            w!("Segoe UI"),
        );
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, rgb(230, 232, 236));
        let mut y = 16;
        for line in &lines {
            let mut text: Vec<u16> = line.encode_utf16().collect();
            let mut r = RECT {
                left: 20,
                top: y,
                right: all.right - 20,
                bottom: y + 24,
            };
            DrawTextW(
                dc,
                &mut text,
                &mut r,
                DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
            y += 26;
        }
        let bar = RECT {
            left: 20,
            top: all.bottom - 44,
            right: all.right - 20,
            bottom: all.bottom - 24,
        };
        fill(&bar, rgb(52, 56, 64));
        let width = ((bar.right - bar.left) as f32 * part) as i32;
        fill(
            &RECT {
                right: bar.left + width,
                ..bar
            },
            rgb(80, 160, 110),
        );
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        let _ = EndPaint(hwnd, &ps);
    }
}
