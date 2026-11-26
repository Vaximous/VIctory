//! The mouse as part of the gamepad the title reads (config `[input]`, off
//! by default). The title asks GameInput only for keyboards and gamepads,
//! so raw mouse input (`WM_INPUT`, unaccelerated counts) drives the right
//! stick and the mouse buttons press pad buttons and triggers; the result is
//! merged into the pad the title already reads, with a real pad or the
//! keyboard's stand-in (WASD still walks).
//!
//! Movement turns the title's camera directly where a camera hook is
//! configured and found (`camera`); elsewhere it is the right stick.
//!
//! Stick: the stick shows the mouse's average speed since the title last
//! read the pad (it reads once a frame), so each frame turns by about what
//! the mouse moved in it, whatever the frame time; once the title has read
//! it and the mouse stands still the stick is back at zero by the next poll.
//! Speed `counts/s * sensitivity / FULL_SPEED` is raised to `curve`, then
//! lifted by `deadzone_boost` (a moving mouse never sits inside the title's
//! stick dead zone) and limited to `cap`.
//!
//! While mouse mode is on and one of our windows has focus the cursor is
//! hidden and kept inside the window's client area; the release key (F8)
//! frees it, and so does losing focus. Pressing the key again, or clicking
//! into the window, takes it back (that click presses nothing).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// Mouse speed (counts per second) that is a full stick at sensitivity 1.
/// About 1.25 inches per second at 1600 counts per inch.
pub const FULL_SPEED: f64 = 2000.0;
/// Shortest span a speed is taken over (a poll is 8 ms).
const MIN_WINDOW: f64 = 0.008;
/// Longest: movement older than this, unread by the title, is dropped.
const MAX_WINDOW: f64 = 0.25;
/// Wheel notches queued per direction at most.
const MOST_NOTCHES: u8 = 3;

/// What a mouse button or wheel notch presses on the pad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Nothing,
    /// Pad button bits.
    Button(u32),
    LeftTrigger,
    RightTrigger,
}

impl Target {
    /// `a b x y up down left right lb rb ls rs menu view lt rt none`.
    pub fn parse(name: &str) -> Option<Target> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Target::Nothing,
            "a" => Target::Button(A),
            "b" => Target::Button(B),
            "x" => Target::Button(X),
            "y" => Target::Button(Y),
            "up" => Target::Button(UP),
            "down" => Target::Button(DOWN),
            "left" => Target::Button(LEFT),
            "right" => Target::Button(RIGHT),
            "lb" => Target::Button(LEFT_SHOULDER),
            "rb" => Target::Button(RIGHT_SHOULDER),
            "ls" => Target::Button(LEFT_STICK),
            "rs" => Target::Button(RIGHT_STICK),
            "menu" => Target::Button(MENU),
            "view" => Target::Button(VIEW),
            "lt" => Target::LeftTrigger,
            "rt" => Target::RightTrigger,
            _ => return None,
        })
    }

    fn press(self, pad: &mut PadState) {
        match self {
            Target::Nothing => {}
            Target::Button(bits) => pad.buttons |= bits,
            Target::LeftTrigger => pad.left_trigger = 1.0,
            Target::RightTrigger => pad.right_trigger = 1.0,
        }
    }
}

/// A virtual key by name: `F1`..`F24`, a letter or digit, or `0x..`.
pub fn key(name: &str) -> Option<u8> {
    let name = name.trim();
    let upper = name.to_ascii_uppercase();
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&n).then(|| 0x6F + n);
    }
    if let Some(hex) = upper.strip_prefix("0X") {
        return u8::from_str_radix(hex, 16).ok().filter(|&vk| vk >= 0x08);
    }
    match upper.as_bytes() {
        [c] if c.is_ascii_alphanumeric() => Some(*c),
        _ => None,
    }
}

/// `[input]`, resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub sensitivity: f32,
    /// Exponent on the normalised speed: 1 linear, above 1 finer slow aim.
    pub curve: f32,
    /// Smallest stick a moving mouse gives (0 to 1).
    pub deadzone_boost: f32,
    /// Largest stick (0 to 1).
    pub cap: f32,
    pub invert_y: bool,
    /// Shortest press of a button, and of each wheel notch (and the gap
    /// between notches), so a frame sees it.
    pub hold_ms: u32,
    /// Frees and takes back the cursor; never reaches the title.
    pub release_key: u8,
    /// Left, right, middle, back (X1), forward (X2).
    pub buttons: [Target; 5],
    /// Wheel up, wheel down.
    pub wheel: [Target; 2],
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            enabled: false,
            sensitivity: 1.0,
            curve: 1.0,
            deadzone_boost: 0.15,
            cap: 1.0,
            invert_y: false,
            hold_ms: 100,
            release_key: 0x77, // F8
            buttons: [
                Target::RightTrigger,
                Target::LeftTrigger,
                Target::Button(RIGHT_STICK),
                Target::Button(LEFT_SHOULDER),
                Target::Button(RIGHT_SHOULDER),
            ],
            wheel: [Target::Button(RIGHT), Target::Button(LEFT)],
        }
    }
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Sets the mouse settings, once, before the title first asks for input.
pub fn configure(settings: Settings) {
    if SETTINGS.set(settings).is_err() {
        tracing::warn!("mouse settings were already in use");
    }
}

pub(super) fn settings() -> &'static Settings {
    SETTINGS.get_or_init(Settings::default)
}

/// The title read a gamepad since the last poll (`current_reading`).
pub(super) static PAD_READ: AtomicBool = AtomicBool::new(false);
/// The cursor is hidden over our windows (`lifecycle`'s window procedure).
static HIDDEN: AtomicBool = AtomicBool::new(false);

/// Whether the cursor is to be hidden over the title's window.
pub fn hidden() -> bool {
    HIDDEN.load(Ordering::Relaxed)
}

/// Mouse speed to stick: `dx`, `dy` counts over `secs` (mouse y grows
/// downwards, stick y upwards).
pub(super) fn stick(dx: f64, dy: f64, secs: f64, s: &Settings) -> (f32, f32) {
    if secs <= 0.0 || (dx == 0.0 && dy == 0.0) {
        return (0.0, 0.0);
    }
    let k = f64::from(s.sensitivity) / (FULL_SPEED * secs);
    let x = dx * k;
    let y = if s.invert_y { dy * k } else { -dy * k };
    let m = x.hypot(y);
    if m <= 0.0 || !m.is_finite() {
        return (0.0, 0.0);
    }
    let boost = f64::from(s.deadzone_boost).clamp(0.0, 1.0);
    let shaped = m.powf(f64::from(s.curve).max(0.05));
    let out = (boost + (1.0 - boost) * shaped).min(f64::from(s.cap).clamp(0.0, 1.0));
    let scale = out / m;
    (
        (x * scale).clamp(-1.0, 1.0) as f32,
        (y * scale).clamp(-1.0, 1.0) as f32,
    )
}

/// Movement since the title last read the pad.
#[derive(Default, Debug)]
pub(super) struct Window {
    dx: i64,
    dy: i64,
    start: f64,
    last: f64,
}

impl Window {
    /// Adds the counts since the last step, at `now` seconds; `read`: the
    /// title read the pad since then, so it saw all up to the last step.
    /// Gives the window's counts and length.
    pub(super) fn step(&mut self, dx: i32, dy: i32, now: f64, read: bool) -> (f64, f64, f64) {
        if read || self.last == 0.0 || now - self.start > MAX_WINDOW {
            self.dx = 0;
            self.dy = 0;
            self.start = if self.last == 0.0 { now } else { self.last };
        }
        self.dx += i64::from(dx);
        self.dy += i64::from(dy);
        self.last = now;
        (
            self.dx as f64,
            self.dy as f64,
            (now - self.start).max(MIN_WINDOW),
        )
    }
}

/// Buttons held at least `hold` after each press.
#[derive(Default, Debug)]
pub(super) struct Held {
    until: [f64; 5],
}

impl Held {
    /// `held`: down now; `pressed`: went down since the last step (maybe
    /// up again already). Bit `i` is button `i`.
    pub(super) fn step(&mut self, held: u32, pressed: u32, now: f64, hold: f64) -> u32 {
        let mut on = 0;
        for (i, until) in self.until.iter_mut().enumerate() {
            if pressed >> i & 1 != 0 {
                *until = now + hold;
            }
            if held >> i & 1 != 0 || now < *until {
                on |= 1 << i;
            }
        }
        on
    }
}

/// Wheel notches as presses: each held `hold`, then `hold` up.
#[derive(Default, Debug)]
pub(super) struct Wheel {
    /// Notches waiting: up, down.
    pending: [u8; 2],
    active: Option<usize>,
    until: f64,
}

impl Wheel {
    /// `notches`: up positive. Gives the direction pressed now, if any.
    pub(super) fn step(&mut self, notches: i32, now: f64, hold: f64) -> Option<usize> {
        if notches != 0 {
            let d = usize::from(notches < 0);
            self.pending[1 - d] = 0;
            let more = notches.unsigned_abs().min(u32::from(MOST_NOTCHES)) as u8;
            self.pending[d] = (self.pending[d] + more).min(MOST_NOTCHES);
        }
        if let Some(d) = self.active {
            if now < self.until {
                return Some(d);
            }
            self.active = None;
            self.until += hold;
        }
        if now < self.until {
            return None;
        }
        let d = (0..2).find(|&d| self.pending[d] > 0)?;
        self.pending[d] -= 1;
        self.active = Some(d);
        self.until = now + hold;
        Some(d)
    }
}

/// The pad from the mouse: what the poll thread turns host mouse state into.
#[derive(Default, Debug)]
pub(super) struct Mapper {
    window: Window,
    held: Held,
    wheel: Wheel,
    /// Wheel units short of a notch.
    wheel_rest: i32,
}

impl Mapper {
    /// One poll: counts moved, wheel units (120 a notch, up positive),
    /// buttons down and pressed since the last poll, whether the title read
    /// the pad since then, at `now` seconds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn step(
        &mut self,
        s: &Settings,
        (dx, dy): (i32, i32),
        wheel: i32,
        held: u32,
        pressed: u32,
        read: bool,
        now: f64,
    ) -> PadState {
        let hold = f64::from(s.hold_ms) / 1000.0;
        let (dx, dy, secs) = self.window.step(dx, dy, now, read);
        let (right_x, right_y) = stick(dx, dy, secs, s);
        let mut pad = PadState {
            right_x,
            right_y,
            ..Default::default()
        };
        let on = self.held.step(held, pressed, now, hold);
        for (i, target) in s.buttons.iter().enumerate() {
            if on >> i & 1 != 0 {
                target.press(&mut pad);
            }
        }
        self.wheel_rest = self.wheel_rest.saturating_add(wheel);
        let notches = self.wheel_rest / 120;
        self.wheel_rest -= notches * 120;
        if let Some(d) = self.wheel.step(notches, now, hold) {
            s.wheel[d].press(&mut pad);
        }
        pad
    }
}

/// Raw input, the cursor, and the release key.
#[cfg(windows)]
pub(super) mod host {
    use std::sync::Once;
    use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
        RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ClipCursor, CreateWindowExW, DispatchMessageW, GetClipCursor, GetCursorPos,
        GetForegroundWindow, GetMessageW, GetWindowInfo, GetWindowThreadProcessId, HTCLIENT,
        HWND_MESSAGE, IsIconic, MSG, PostMessageW, RI_MOUSE_WHEEL, WINDOWINFO, WM_INPUT,
        WM_MOUSEMOVE, WM_SETCURSOR,
    };
    use windows_sys::w;

    use super::*;

    /// Counts moved, wheel units, buttons down and pressed since taken.
    static DX: AtomicI32 = AtomicI32::new(0);
    static DY: AtomicI32 = AtomicI32::new(0);
    static WHEEL: AtomicI32 = AtomicI32::new(0);
    static HELD: AtomicU32 = AtomicU32::new(0);
    static PRESSED: AtomicU32 = AtomicU32::new(0);

    /// Starts the raw input thread, once.
    fn listen() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let spawned = std::thread::Builder::new()
                .name("victory-mouse".into())
                .spawn(raw_input);
            if let Err(e) = spawned {
                tracing::warn!("no mouse input: {e}");
            }
        });
    }

    /// A message-only window that gets the mouse's raw input wherever the
    /// focus is (the poll thread drops what comes without it).
    fn raw_input() {
        // SAFETY: a window of the system's message-only class, owned and
        // pumped by this thread; raw input read into room of its own size.
        unsafe {
            let hwnd = CreateWindowExW(
                0,
                w!("Message"),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                tracing::warn!("no mouse input: no window");
                return;
            }
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            if RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                tracing::warn!("no mouse input: raw input not registered");
                return;
            }
            tracing::info!("mouse input on");
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                if message.message == WM_INPUT {
                    let mut raw = RAWINPUT::default();
                    let mut size = size_of::<RAWINPUT>() as u32;
                    let got = GetRawInputData(
                        message.lParam as HRAWINPUT,
                        RID_INPUT,
                        (&raw mut raw).cast(),
                        &mut size,
                        size_of::<RAWINPUTHEADER>() as u32,
                    );
                    if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE {
                        take(&raw.data.mouse);
                    }
                }
                DispatchMessageW(&message);
            }
        }
    }

    fn take(mouse: &windows_sys::Win32::UI::Input::RAWMOUSE) {
        // Absolute devices (pens, remote desktop) give positions, not counts.
        if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 && (mouse.lLastX != 0 || mouse.lLastY != 0) {
            DX.fetch_add(mouse.lLastX, Ordering::Relaxed);
            DY.fetch_add(mouse.lLastY, Ordering::Relaxed);
        }
        // SAFETY: the flags half of the union, which mice fill.
        let (flags, data) = unsafe {
            (
                u32::from(mouse.Anonymous.Anonymous.usButtonFlags),
                mouse.Anonymous.Anonymous.usButtonData,
            )
        };
        if flags == 0 {
            return;
        }
        // Button i: down 1 << 2i, up 1 << 2i + 1.
        let (mut down, mut up) = (0, 0);
        for i in 0..5 {
            down |= (flags >> (2 * i) & 1) << i;
            up |= (flags >> (2 * i + 1) & 1) << i;
        }
        if down != 0 {
            HELD.fetch_or(down, Ordering::Relaxed);
            PRESSED.fetch_or(down, Ordering::Relaxed);
        }
        if up != 0 {
            HELD.fetch_and(!up, Ordering::Relaxed);
        }
        if flags & RI_MOUSE_WHEEL != 0 {
            WHEEL.fetch_add(i32::from(data as i16), Ordering::Relaxed);
        }
    }

    /// The client area of `hwnd` on the screen, `None` when it has none.
    fn client(hwnd: HWND) -> Option<RECT> {
        let mut info = WINDOWINFO {
            cbSize: size_of::<WINDOWINFO>() as u32,
            // SAFETY: plain data.
            ..unsafe { std::mem::zeroed() }
        };
        // SAFETY: plain queries.
        let ok = unsafe { IsIconic(hwnd) == 0 && GetWindowInfo(hwnd, &mut info) != 0 };
        let r = info.rcClient;
        (ok && r.right > r.left && r.bottom > r.top).then_some(r)
    }

    /// Our window in front, if it is.
    fn ours() -> Option<HWND> {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            let hwnd = GetForegroundWindow();
            GetWindowThreadProcessId(hwnd, &mut process);
            (!hwnd.is_null() && process == GetCurrentProcessId()).then_some(hwnd)
        }
    }

    /// Has the window ask for its cursor again (it is set on mouse moves).
    fn refresh_cursor(hwnd: HWND) {
        let lparam = (HTCLIENT | WM_MOUSEMOVE << 16) as isize;
        // SAFETY: a posted message; a window gone just fails.
        unsafe { PostMessageW(hwnd, WM_SETCURSOR, hwnd as usize, lparam) };
    }

    /// The mouse on the poll thread.
    #[derive(Default)]
    pub(in super::super) struct Mouse {
        mapper: Mapper,
        /// The cursor is captured in this window.
        captured: Option<usize>,
        /// Freed by the release key.
        released: bool,
        key_was: bool,
        /// Buttons down when the capture began, until let go.
        ignore: u32,
    }

    impl Mouse {
        pub(in super::super) fn poll(&mut self, s: &Settings) -> PadState {
            listen();
            let front = ours();
            // SAFETY: plain query.
            let key = front.is_some() && unsafe { GetAsyncKeyState(i32::from(s.release_key)) } < 0;
            if key && !self.key_was {
                self.released = !self.released;
            }
            self.key_was = key;
            let (dx, dy) = (DX.swap(0, Ordering::Relaxed), DY.swap(0, Ordering::Relaxed));
            let wheel = WHEEL.swap(0, Ordering::Relaxed);
            let pressed = PRESSED.swap(0, Ordering::Relaxed);
            let held = HELD.load(Ordering::Relaxed);
            let area = front.and_then(client);
            // A click into the window takes the cursor back.
            if self.released
                && pressed & 1 != 0
                && let Some(r) = area
            {
                let mut at = POINT { x: 0, y: 0 };
                // SAFETY: plain query.
                let inside = unsafe { GetCursorPos(&mut at) } != 0
                    && (r.left..r.right).contains(&at.x)
                    && (r.top..r.bottom).contains(&at.y);
                if inside {
                    self.released = false;
                }
            }
            let target = if self.released { None } else { front.zip(area) };
            match target {
                Some((hwnd, rect)) => {
                    if self.captured != Some(hwnd as usize) {
                        self.ignore = held;
                        self.mapper = Mapper::default();
                        HIDDEN.store(true, Ordering::Relaxed);
                        refresh_cursor(hwnd);
                        self.captured = Some(hwnd as usize);
                        tracing::debug!("mouse captured");
                    }
                    let mut now = RECT::default();
                    // SAFETY: plain query and setting.
                    unsafe {
                        if GetClipCursor(&mut now) == 0
                            || (now.left, now.top, now.right, now.bottom)
                                != (rect.left, rect.top, rect.right, rect.bottom)
                        {
                            ClipCursor(&rect);
                        }
                    }
                }
                None => {
                    if let Some(hwnd) = self.captured.take() {
                        // SAFETY: plain setting.
                        unsafe { ClipCursor(std::ptr::null()) };
                        HIDDEN.store(false, Ordering::Relaxed);
                        refresh_cursor(hwnd as HWND);
                        tracing::debug!("mouse released");
                    }
                    PAD_READ.store(false, Ordering::Relaxed);
                    super::super::camera::clear();
                    return PadState::default();
                }
            }
            self.ignore &= held;
            let read = PAD_READ.swap(false, Ordering::Relaxed);
            let now = START.elapsed().as_secs_f64();
            // Movement turns the camera where one is hooked, else the stick.
            let stick = if super::super::camera::live() {
                super::super::camera::add(dx, dy);
                (0, 0)
            } else {
                (dx, dy)
            };
            self.mapper.step(
                s,
                stick,
                wheel,
                held & !self.ignore,
                pressed & !self.ignore,
                read,
                now,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn speed_maps_to_the_stick_linearly_with_no_boost() {
        let s = Settings {
            deadzone_boost: 0.0,
            ..Settings::default()
        };
        // Half of full speed to the right for 0.1 s.
        let (x, y) = stick(FULL_SPEED * 0.05, 0.0, 0.1, &s);
        assert!(close(x, 0.5) && close(y, 0.0), "{x} {y}");
        // Mouse down is stick down; inverted, up.
        let (_, y) = stick(0.0, FULL_SPEED * 0.05, 0.1, &s);
        assert!(close(y, -0.5));
        let inverted = Settings {
            invert_y: true,
            ..s.clone()
        };
        let (_, y) = stick(0.0, FULL_SPEED * 0.05, 0.1, &inverted);
        assert!(close(y, 0.5));
        // Sensitivity scales.
        let fast = Settings {
            sensitivity: 2.0,
            ..s
        };
        let (x, _) = stick(FULL_SPEED * 0.025, 0.0, 0.1, &fast);
        assert!(close(x, 0.5));
    }

    #[test]
    fn the_curve_shapes_and_the_boost_lifts_small_moves() {
        let s = Settings {
            curve: 2.0,
            deadzone_boost: 0.0,
            ..Settings::default()
        };
        let (x, _) = stick(FULL_SPEED * 0.05, 0.0, 0.1, &s);
        assert!(close(x, 0.25), "{x}");
        let boosted = Settings {
            deadzone_boost: 0.2,
            ..s
        };
        // A single count still leaves the dead zone.
        let (x, _) = stick(1.0, 0.0, 0.1, &boosted);
        assert!((0.2..0.21).contains(&x), "{x}");
        let (x, _) = stick(FULL_SPEED * 0.05, 0.0, 0.1, &boosted);
        assert!(close(x, 0.2 + 0.8 * 0.25), "{x}");
        // Nothing moved, nothing pushed, boost or not.
        assert_eq!(stick(0.0, 0.0, 0.1, &boosted), (0.0, 0.0));
    }

    #[test]
    fn the_stick_is_capped_and_keeps_its_direction() {
        let s = Settings {
            cap: 0.8,
            ..Settings::default()
        };
        let (x, y) = stick(FULL_SPEED * 10.0, -FULL_SPEED * 10.0, 0.1, &s);
        assert!(close(x.hypot(y), 0.8), "{x} {y}");
        // Right and up (mouse y up is negative).
        assert!(close(x, y) && x > 0.0);
        let uncapped = Settings::default();
        let (x, y) = stick(FULL_SPEED * 10.0, 0.0, 0.1, &uncapped);
        assert!(close(x, 1.0) && close(y, 0.0));
        let (x, y) = stick(-FULL_SPEED * 10.0, -FULL_SPEED * 10.0, 0.1, &uncapped);
        assert!((-1.0..=1.0).contains(&x) && (-1.0..=1.0).contains(&y));
    }

    #[test]
    fn the_window_spans_since_the_title_last_read() {
        let mut w = Window::default();
        assert_eq!(w.step(10, 0, 1.0, false), (10.0, 0.0, MIN_WINDOW));
        let (dx, _, secs) = w.step(10, 0, 1.008, false);
        assert_eq!(dx, 20.0);
        assert!((secs - 0.008).abs() < 1e-9);
        let (dx, _, secs) = w.step(5, 0, 1.016, false);
        assert_eq!(dx, 25.0);
        assert!((secs - 0.016).abs() < 1e-9);
        // Read: what came since the last step starts a new window.
        let (dx, _, secs) = w.step(3, 0, 1.024, true);
        assert_eq!(dx, 3.0);
        assert!((secs - 0.008).abs() < 1e-9);
        // Old unread movement is dropped.
        let (dx, _, _) = w.step(1, 0, 2.0, false);
        assert_eq!(dx, 1.0);
    }

    #[test]
    fn the_stick_returns_to_zero_once_read_when_the_mouse_stops() {
        let s = Settings::default();
        let mut m = Mapper::default();
        let pad = m.step(&s, (40, 0), 0, 0, 0, false, 1.0);
        assert!(pad.right_x > 0.0);
        // Not read yet: the title still gets the movement.
        let pad = m.step(&s, (0, 0), 0, 0, 0, false, 1.008);
        assert!(pad.right_x > 0.0);
        // Read, and the mouse stands still: zero at the next poll.
        let pad = m.step(&s, (0, 0), 0, 0, 0, true, 1.016);
        assert_eq!((pad.right_x, pad.right_y), (0.0, 0.0));
    }

    #[test]
    fn buttons_press_triggers_and_last_long_enough() {
        let s = Settings::default();
        let mut m = Mapper::default();
        // Left and right down: right and left trigger; middle: stick click.
        let pad = m.step(&s, (0, 0), 0, 0b111, 0b111, false, 1.0);
        assert_eq!((pad.left_trigger, pad.right_trigger), (1.0, 1.0));
        assert_eq!(pad.buttons, RIGHT_STICK);
        // A click already let go still shows for `hold_ms`.
        let pad = m.step(&s, (0, 0), 0, 0, 0b1000, false, 2.0);
        assert_eq!(pad.buttons, LEFT_SHOULDER);
        let pad = m.step(&s, (0, 0), 0, 0, 0, false, 2.05);
        assert_eq!(pad.buttons, LEFT_SHOULDER);
        let pad = m.step(&s, (0, 0), 0, 0, 0, false, 2.11);
        assert_eq!(pad, PadState::default());
    }

    #[test]
    fn wheel_notches_become_separate_presses() {
        let s = Settings::default();
        let mut m = Mapper::default();
        // Two notches up in one poll: press, gap, press, gap.
        let mut seen = Vec::new();
        let mut t = 1.0;
        let mut first = true;
        while t < 1.5 {
            let wheel = if first { 240 } else { 0 };
            first = false;
            seen.push(m.step(&s, (0, 0), wheel, 0, 0, false, t).buttons);
            t += 0.01;
        }
        let presses = seen
            .windows(2)
            .filter(|w| w[0] == 0 && w[1] == RIGHT)
            .count()
            + usize::from(seen[0] == RIGHT);
        assert_eq!(presses, 2, "{seen:?}");
        assert!(seen.iter().all(|&b| b == 0 || b == RIGHT));
        // Half notches add up; down cancels queued ups.
        let mut m = Mapper::default();
        assert_eq!(m.step(&s, (0, 0), 60, 0, 0, false, 1.0).buttons, 0);
        assert_eq!(m.step(&s, (0, 0), 60, 0, 0, false, 1.01).buttons, RIGHT);
        let mut m = Mapper::default();
        assert_eq!(m.step(&s, (0, 0), 360, 0, 0, false, 1.0).buttons, RIGHT);
        assert_eq!(m.step(&s, (0, 0), -120, 0, 0, false, 1.01).buttons, RIGHT);
        assert_eq!(m.step(&s, (0, 0), 0, 0, 0, false, 1.15).buttons, 0);
        assert_eq!(m.step(&s, (0, 0), 0, 0, 0, false, 1.21).buttons, LEFT);
        assert_eq!(m.step(&s, (0, 0), 0, 0, 0, false, 1.42).buttons, 0);
    }

    #[test]
    fn names_parse() {
        assert_eq!(Target::parse("RT"), Some(Target::RightTrigger));
        assert_eq!(Target::parse("lb"), Some(Target::Button(LEFT_SHOULDER)));
        assert_eq!(Target::parse("none"), Some(Target::Nothing));
        assert_eq!(Target::parse("jump"), None);
        assert_eq!(key("F8"), Some(0x77));
        assert_eq!(key("f1"), Some(0x70));
        assert_eq!(key("p"), Some(b'P'));
        assert_eq!(key("0x2D"), Some(0x2D));
        assert_eq!(key("F25"), None);
        assert_eq!(key("Alt"), None);
    }
}
