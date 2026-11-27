//! A route played once and played back: `VICTORY_INPUT_RECORD=<file>` writes
//! what the host's keyboard, pads and mouse gave the title, and
//! `VICTORY_INPUT_REPLAY=<file>` gives the title that again with the same
//! timing, with or without focus, live input left out meanwhile. Times are
//! from the zero the scripted keys (`VICTORY_KEYS`) count from: the first
//! time the title's input is polled.
//!
//! The file is text, a line an event, written as it happens and flushed at
//! once (a crash loses nothing said before it):
//! `<us> K <vk>,..` the keys held from then on (`-`: none);
//! `<us> P <slot> -` the pad slot empty, or
//! `<us> P <slot> <buttons> <lt> <rt> <lx> <ly> <rx> <ry>` its state;
//! `<us> M <dx> <dy> <wheel> <held> <pressed>` the mouse while captured: the
//! counts moved and the wheel since the poll before, the buttons held and
//! pressed (the movement goes where the live mouse's goes: a hooked camera
//! or the stick);
//! `<us> R` the mouse let go (not captured).

use std::io::Write;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use super::PadState;

/// The zero of the scripted keys and of a route: the first call.
pub(super) fn zero() -> Instant {
    static ZERO: LazyLock<Instant> = LazyLock::new(Instant::now);
    *ZERO
}

fn now_us() -> u64 {
    zero().elapsed().as_micros() as u64
}

/// The mouse at a poll: moved, wheel, held, pressed; or let go.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum MouseAt {
    Moved(i32, i32, i32, u32, u32),
    Released,
}

struct Recorder {
    out: std::io::BufWriter<std::fs::File>,
    keys: Option<Vec<u8>>,
    pads: [Option<Option<PadState>>; 8],
    held: Option<u32>,
    released: Option<bool>,
}

impl Recorder {
    fn line(&mut self, text: std::fmt::Arguments) {
        let _ = writeln!(self.out, "{} {text}", now_us());
        let _ = self.out.flush();
    }
}

fn recorder() -> Option<&'static Mutex<Recorder>> {
    static REC: OnceLock<Option<Mutex<Recorder>>> = OnceLock::new();
    REC.get_or_init(|| {
        let path = std::env::var_os("VICTORY_INPUT_RECORD")?;
        if replay().is_some() {
            tracing::warn!("VICTORY_INPUT_RECORD left out: a route is played back");
            return None;
        }
        if let Some(dir) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::File::create(&path) {
            Ok(file) => {
                tracing::info!(path = %std::path::Path::new(&path).display(), "input recorded (VICTORY_INPUT_RECORD)");
                let mut out = std::io::BufWriter::new(file);
                let _ = writeln!(out, "# victory input route 1");
                let _ = out.flush();
                Some(Mutex::new(Recorder { out, keys: None, pads: [None; 8], held: None, released: None }))
            }
            Err(error) => {
                tracing::warn!(%error, "the input record could not be made");
                None
            }
        }
    })
    .as_ref()
}

/// Records the keys held now, if they changed.
pub(super) fn record_keys(keys: &[u8]) {
    let Some(rec) = recorder() else { return };
    let mut rec = rec.lock().unwrap_or_else(|e| e.into_inner());
    let mut sorted = keys.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if rec.keys.as_ref() == Some(&sorted) {
        return;
    }
    let text = if sorted.is_empty() { "-".to_string() } else { sorted.iter().map(|k| format!("{k:02x}")).collect::<Vec<_>>().join(",") };
    rec.line(format_args!("K {text}"));
    rec.keys = Some(sorted);
}

/// Records pad slot `slot` now, if it changed.
pub(super) fn record_pad(slot: usize, pad: Option<PadState>) {
    let Some(rec) = recorder() else { return };
    if slot >= 8 {
        return;
    }
    let mut rec = rec.lock().unwrap_or_else(|e| e.into_inner());
    if rec.pads[slot] == Some(pad) {
        return;
    }
    match pad {
        None => rec.line(format_args!("P {slot} -")),
        Some(p) => rec.line(format_args!(
            "P {slot} {} {} {} {} {} {} {}",
            p.buttons, p.left_trigger, p.right_trigger, p.left_x, p.left_y, p.right_x, p.right_y
        )),
    }
    rec.pads[slot] = Some(pad);
}

/// Records the mouse at a poll: movement and presses always, held buttons
/// and a release when they change.
pub(super) fn record_mouse(at: MouseAt) {
    let Some(rec) = recorder() else { return };
    let mut rec = rec.lock().unwrap_or_else(|e| e.into_inner());
    match at {
        MouseAt::Released => {
            if rec.released != Some(true) {
                rec.line(format_args!("R"));
                rec.released = Some(true);
                rec.held = None;
            }
        }
        MouseAt::Moved(dx, dy, wheel, held, pressed) => {
            if dx != 0 || dy != 0 || wheel != 0 || pressed != 0 || rec.held != Some(held) || rec.released != Some(false) {
                rec.line(format_args!("M {dx} {dy} {wheel} {held} {pressed}"));
                rec.held = Some(held);
                rec.released = Some(false);
            }
        }
    }
}

/// A route read back.
struct Route {
    keys: Vec<(u64, Vec<u8>)>,
    pads: Vec<Vec<(u64, Option<PadState>)>>,
    mouse: Vec<(u64, MouseAt)>,
    /// The mouse events given so far.
    mouse_at: Mutex<usize>,
    end: u64,
}

fn parse(text: &str) -> Route {
    let mut route = Route { keys: Vec::new(), pads: vec![Vec::new(); 8], mouse: Vec::new(), mouse_at: Mutex::new(0), end: 0 };
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(us) = words.next().and_then(|w| w.parse::<u64>().ok()) else { continue };
        route.end = route.end.max(us);
        match words.next() {
            Some("K") => {
                let keys = match words.next() {
                    Some("-") | None => Vec::new(),
                    Some(list) => list.split(',').filter_map(|k| u8::from_str_radix(k, 16).ok()).collect(),
                };
                route.keys.push((us, keys));
            }
            Some("P") => {
                let Some(slot) = words.next().and_then(|w| w.parse::<usize>().ok()).filter(|&s| s < 8) else { continue };
                let rest: Vec<&str> = words.collect();
                let pad = if rest.first() == Some(&"-") {
                    None
                } else {
                    let f = |i: usize| rest.get(i).and_then(|w| w.parse::<f32>().ok()).unwrap_or(0.0);
                    Some(PadState {
                        buttons: rest.first().and_then(|w| w.parse().ok()).unwrap_or(0),
                        left_trigger: f(1),
                        right_trigger: f(2),
                        left_x: f(3),
                        left_y: f(4),
                        right_x: f(5),
                        right_y: f(6),
                    })
                };
                route.pads[slot].push((us, pad));
            }
            Some("M") => {
                let n: Vec<i64> = words.filter_map(|w| w.parse().ok()).collect();
                if n.len() == 5 {
                    route.mouse.push((us, MouseAt::Moved(n[0] as i32, n[1] as i32, n[2] as i32, n[3] as u32, n[4] as u32)));
                }
            }
            Some("R") => route.mouse.push((us, MouseAt::Released)),
            _ => {}
        }
    }
    route
}

fn replay() -> Option<&'static Route> {
    static ROUTE: OnceLock<Option<Route>> = OnceLock::new();
    ROUTE
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_INPUT_REPLAY")?;
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let route = parse(&text);
                    tracing::info!(
                        path = %std::path::Path::new(&path).display(),
                        keys = route.keys.len(),
                        mouse = route.mouse.len(),
                        seconds = route.end as f64 / 1e6,
                        "a route is played back (VICTORY_INPUT_REPLAY); live input is left out"
                    );
                    Some(route)
                }
                Err(error) => {
                    tracing::warn!(%error, "the route to play back could not be read");
                    None
                }
            }
        })
        .as_ref()
}

/// Whether a route is played back: live input is left out.
pub(super) fn replaying() -> bool {
    replay().is_some()
}

/// The latest of `events` at or before `now`.
fn latest<T: Clone>(events: &[(u64, T)], now: u64) -> Option<T> {
    let i = events.partition_point(|e| e.0 <= now);
    i.checked_sub(1).map(|i| events[i].1.clone())
}

/// The keys the route holds now.
pub(super) fn replay_keys() -> Vec<u8> {
    replay().and_then(|r| latest(&r.keys, now_us())).unwrap_or_default()
}

/// Pad slot `slot` as the route has it now (empty before its first word).
pub(super) fn replay_pad(slot: usize) -> Option<PadState> {
    replay().and_then(|r| r.pads.get(slot).and_then(|p| latest(p, now_us()))).flatten()
}

/// The mouse at this poll: what was moved and pressed since the poll before,
/// summed, with the buttons held now; `Released` if the route let it go.
pub(super) fn replay_mouse() -> MouseAt {
    let Some(route) = replay() else { return MouseAt::Released };
    let now = now_us();
    let mut at = route.mouse_at.lock().unwrap_or_else(|e| e.into_inner());
    let (mut dx, mut dy, mut wheel, mut pressed) = (0, 0, 0, 0);
    let mut held = None;
    let mut released = false;
    while *at < route.mouse.len() && route.mouse[*at].0 <= now {
        match route.mouse[*at].1 {
            MouseAt::Moved(x, y, w, h, p) => {
                dx += x;
                dy += y;
                wheel += w;
                pressed |= p;
                held = Some(h);
                released = false;
            }
            MouseAt::Released => {
                released = true;
                (dx, dy, wheel, pressed) = (0, 0, 0, 0);
            }
        }
        *at += 1;
    }
    if released {
        return MouseAt::Released;
    }
    let held = held.or_else(|| {
        // Nothing new: the buttons held as the last event left them.
        match (*at).checked_sub(1).map(|i| route.mouse[i].1) {
            Some(MouseAt::Moved(_, _, _, h, _)) => Some(h),
            _ => None,
        }
    });
    match held {
        Some(held) => MouseAt::Moved(dx, dy, wheel, held, pressed),
        None => MouseAt::Released,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_read_back() {
        let r = parse("# x\n10 K 0d,57\n20 P 0 1 0 0 0.5 -0.5 0 0\n30 M 3 -4 0 1 1\n40 R\n50 K -\n");
        assert_eq!(r.keys, vec![(10, vec![0x0d, 0x57]), (50, vec![])]);
        assert_eq!(r.pads[0].len(), 1);
        assert_eq!(r.mouse, vec![(30, MouseAt::Moved(3, -4, 0, 1, 1)), (40, MouseAt::Released)]);
        assert_eq!(latest(&r.keys, 45), Some(vec![0x0d, 0x57]));
        assert_eq!(latest(&r.keys, 5), None);
    }
}
