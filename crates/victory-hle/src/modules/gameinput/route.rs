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
//! `<us> R` the mouse let go (not captured);
//! `<us> W` the first frame of the world was shown
//! (`victory_gpu::frame::world_began`);
//! `<us> S` the world settled after it (`victory_gpu::frame::world_settled`).
//!
//! From version 2 on each time is `<us>:<poll>`, with the count of the title's
//! keyboard polls by then. Played back, the events before the world are given
//! by that count (a menu takes its key at its own poll, however long the
//! loading before it took), and those after it by the time since the replay's
//! own first frame of the world: a load that takes longer here no longer moves
//! every step of the walk to another place. A file of version 1 with a `W`
//! line added by hand plays the part before it by time, held short of the
//! world until the replay's own is shown. How far each phase drifted is said
//! in the log.
//!
//! The first frame of the world is the title's flyover while it still loads,
//! and the walk begins only after it, which takes longer in one build than
//! another. With an `S` line (or `VICTORY_ROUTE_SETTLE_S`, route seconds) the
//! events after it are given by the time since the replay's own world
//! settled, the ones between the world and it held short of it as before.
//!
//! After the settling the route is played by the title's frames, not by the
//! time: each frame the title begins moves it on by `VICTORY_ROUTE_FRAME_MS`
//! (52 ms; `0`: by the time, as before). The walk is the same number of the
//! title's frames in every build: one that runs it at 43 ms a frame walked
//! the same seconds in some 370 frames against 320 at 51 ms, and went past
//! the door the route goes out by (half of the runs of one route ended in
//! the house, at 43 to 45 ms a frame; those at 50 to 56 ms went out).
//! A world that settles more than `VICTORY_ROUTE_SETTLE_MOST_S` (3) later
//! after its first frame than the route's did is not waited for: the walk
//! begins from there.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// The title's keyboard polls so far.
static POLLS: AtomicU64 = AtomicU64::new(0);

/// The title polls its keyboard: counted, and when recording, the first
/// frame of the world and its settling are noted once they happened.
pub(super) fn polled() {
    POLLS.fetch_add(1, Ordering::Relaxed);
    static NOTED: AtomicBool = AtomicBool::new(false);
    static SETTLE_NOTED: AtomicBool = AtomicBool::new(false);
    if SETTLE_NOTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(rec) = recorder() else { return };
    if !NOTED.load(Ordering::Relaxed) {
        let Some(world) = victory_gpu::frame::world_began() else { return };
        if NOTED.swap(true, Ordering::Relaxed) {
            return;
        }
        let us = world.saturating_duration_since(zero()).as_micros() as u64;
        let mut rec = rec.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(rec.out, "{us}:{} W", POLLS.load(Ordering::Relaxed));
        let _ = rec.out.flush();
    }
    let Some(settled) = victory_gpu::frame::world_settled() else { return };
    if SETTLE_NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    // Without a poll count: an older reader takes it as a time alone.
    let us = settled.saturating_duration_since(zero()).as_micros() as u64;
    let mut rec = rec.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(rec.out, "{us} S");
    let _ = rec.out.flush();
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
        let _ = writeln!(self.out, "{}:{} {text}", now_us(), POLLS.load(Ordering::Relaxed));
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
                let _ = writeln!(out, "# victory input route 2");
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
    /// When the world's first frame was shown, if the file says.
    world: Option<u64>,
    /// When the world settled after it, if the file (or
    /// `VICTORY_ROUTE_SETTLE_S`) says.
    settle: Option<u64>,
    /// Of events before the world that have a poll count: the count and the
    /// time, in order.
    polls: Vec<(u64, u64)>,
}

fn parse(text: &str) -> Route {
    let mut route = Route {
        keys: Vec::new(),
        pads: vec![Vec::new(); 8],
        mouse: Vec::new(),
        mouse_at: Mutex::new(0),
        end: 0,
        world: None,
        settle: None,
        polls: Vec::new(),
    };
    let mut polls = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(at) = words.next() else { continue };
        let (us, poll) = match at.split_once(':') {
            Some((us, poll)) => (us.parse::<u64>().ok(), poll.parse::<u64>().ok()),
            None => (at.parse::<u64>().ok(), None),
        };
        let Some(us) = us else { continue };
        route.end = route.end.max(us);
        let kind = words.next();
        if let Some(poll) = poll
            && kind != Some("W")
            && kind != Some("S")
        {
            polls.push((poll, us));
        }
        match kind {
            Some("W") => route.world = route.world.or(Some(us)),
            Some("S") => route.settle = route.settle.or(Some(us)),
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
    if let Some(world) = route.world {
        route.polls = polls.into_iter().filter(|&(_, us)| us < world).collect();
        route.polls.sort_unstable();
    }
    route
}

/// Where in the route the replay is now, in the route's microseconds: before
/// the world by the title's polls (or by time, for a file without them),
/// held short of the world until the replay's own world is shown; then the
/// time since that. A file that does not say when its world began is played
/// by time from the first poll throughout, as before.
fn clock(route: &Route) -> u64 {
    let now = now_us();
    let Some(world) = route.world else { return now };
    if let Some(began) = victory_gpu::frame::world_began() {
        let here = began.saturating_duration_since(zero()).as_micros() as u64;
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::info!(
                route_ms = world / 1000,
                replay_ms = here / 1000,
                drift_ms = (here as i64 - world as i64) / 1000,
                polls = POLLS.load(Ordering::Relaxed),
                "route: the world's first frame; from here the route plays by the time since it"
            );
        });
        let by_world = world + now.saturating_sub(here);
        let Some(settle) = route.settle.filter(|&s| s > world) else { return by_world };
        // The settling, or at the latest the route's own distance from the
        // world to it and `settle_most` more: the replay's anchor, its time
        // and frame, taken once.
        static ANCHOR: OnceLock<(u64, u64, bool)> = OnceLock::new();
        let latest = settle_most().map(|most| here + (settle - world) + most);
        let anchor = match ANCHOR.get() {
            Some(&anchor) => anchor,
            None => {
                let settled = victory_gpu::frame::world_settled()
                    .map(|s| s.saturating_duration_since(zero()).as_micros() as u64)
                    .zip(victory_gpu::frame::world_settled_frame());
                let found = match (settled, latest) {
                    (Some((there, _)), Some(latest)) if there > latest => Some((latest, victory_gpu::frame::frames(), false)),
                    (Some((there, frame)), _) => Some((there, frame, true)),
                    (None, Some(latest)) if now >= latest => Some((latest, victory_gpu::frame::frames(), false)),
                    (None, _) => None,
                };
                let Some(found) = found else { return by_world.min(settle - 1) };
                *ANCHOR.get_or_init(|| found)
            }
        };
        let (there, first, settled) = anchor;
        static SHIFT: std::sync::Once = std::sync::Once::new();
        SHIFT.call_once(|| {
            let (route_after, replay_after) = (settle - world, there.saturating_sub(here));
            tracing::info!(
                route_after_world_ms = route_after / 1000,
                replay_after_world_ms = replay_after / 1000,
                shift_ms = (replay_after as i64 - route_after as i64) / 1000,
                settled,
                "route: the world settled (or settled late: anchored at the latest, VICTORY_ROUTE_SETTLE_MOST_S); from here the route plays on from it, its walk moved by the shift"
            );
        });
        if let Some(per_frame) = frame_us() {
            let frames = victory_gpu::frame::frames().saturating_sub(first);
            return settle + frames * per_frame;
        }
        return settle + now.saturating_sub(there);
    }
    let at = if route.polls.is_empty() {
        now
    } else {
        let polls = POLLS.load(Ordering::Relaxed);
        let i = route.polls.partition_point(|&(p, _)| p <= polls);
        let at = i.checked_sub(1).map_or(0, |i| route.polls[i].1);
        // Each event before the world as it is reached: how far from its time.
        static LAST: AtomicU64 = AtomicU64::new(0);
        if LAST.fetch_max(at, Ordering::Relaxed) < at {
            tracing::info!(
                route_ms = at / 1000,
                replay_ms = now / 1000,
                drift_ms = (now as i64 - at as i64) / 1000,
                polls,
                "route: an event before the world, given at its poll"
            );
        }
        at
    };
    at.min(world.saturating_sub(1))
}

fn replay() -> Option<&'static Route> {
    static ROUTE: OnceLock<Option<Route>> = OnceLock::new();
    ROUTE
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_INPUT_REPLAY")?;
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let mut route = parse(&text);
                    if let Some(s) = std::env::var("VICTORY_ROUTE_SETTLE_S").ok().and_then(|v| v.trim().parse::<f64>().ok()) {
                        route.settle = Some((s * 1e6) as u64);
                    }
                    tracing::info!(
                        path = %std::path::Path::new(&path).display(),
                        keys = route.keys.len(),
                        mouse = route.mouse.len(),
                        seconds = route.end as f64 / 1e6,
                        world_s = route.world.map(|w| w as f64 / 1e6),
                        settle_s = route.settle.map(|s| s as f64 / 1e6),
                        by_polls = route.polls.len(),
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

/// Where the route stops being played (`VICTORY_ROUTE_UNTIL_S`, route
/// seconds): from there on it gives no keys, empty pads and a still mouse,
/// and the scripted keys (`VICTORY_KEYS`) walk on alone.
fn until() -> u64 {
    static UNTIL: OnceLock<u64> = OnceLock::new();
    *UNTIL.get_or_init(|| {
        std::env::var("VICTORY_ROUTE_UNTIL_S").ok().and_then(|v| v.trim().parse::<f64>().ok()).map_or(u64::MAX, |s| (s * 1e6) as u64)
    })
}

/// How much later after the world than the route's own the replay's world
/// may settle before the route goes on without it, in microseconds
/// (`VICTORY_ROUTE_SETTLE_MOST_S`, 3 if unset; `0`: it waits for the
/// settling however late). A world that settled 13 and 25 s later than the
/// route's kept the walk waiting in the bedroom long after the player had
/// it, and it never went out.
fn settle_most() -> Option<u64> {
    static US: OnceLock<Option<u64>> = OnceLock::new();
    *US.get_or_init(|| {
        let s = std::env::var("VICTORY_ROUTE_SETTLE_MOST_S").ok().map_or(Some(3.0), |v| v.trim().parse::<f64>().ok());
        s.filter(|&s| s > 0.0).map(|s| (s * 1e6) as u64)
    })
}

/// After the world settled, how far a frame of the title moves the route on,
/// in microseconds (`VICTORY_ROUTE_FRAME_MS`, 52 if unset; `0`: none, the
/// route plays by the time).
fn frame_us() -> Option<u64> {
    static US: OnceLock<Option<u64>> = OnceLock::new();
    *US.get_or_init(|| {
        let ms = std::env::var("VICTORY_ROUTE_FRAME_MS").ok().map_or(Some(52.0), |v| v.trim().parse::<f64>().ok());
        let us = ms.filter(|&ms| ms > 0.0).map(|ms| (ms * 1000.0) as u64);
        tracing::info!(frame_ms = us.map(|us| us as f64 / 1000.0), "route: after the world settled it plays by the title's frames (VICTORY_ROUTE_FRAME_MS)");
        us
    })
}

/// The route's clock ([`clock`]), if it is still played.
fn playing(route: &Route) -> Option<u64> {
    let at = clock(route);
    if at < until() {
        return Some(at);
    }
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        let after_settle_s = victory_gpu::frame::world_settled().map(|s| s.elapsed().as_secs_f64());
        let frames = victory_gpu::frame::world_settled_frame().map(|f| victory_gpu::frame::frames().saturating_sub(f));
        tracing::info!(route_s = at as f64 / 1e6, after_settle_s, frames, "route: played up to VICTORY_ROUTE_UNTIL_S; the scripted keys go on")
    });
    None
}

/// The keys the route holds now.
pub(super) fn replay_keys() -> Vec<u8> {
    replay().and_then(|r| latest(&r.keys, playing(r)?)).unwrap_or_default()
}

/// Pad slot `slot` as the route has it now (empty before its first word).
pub(super) fn replay_pad(slot: usize) -> Option<PadState> {
    replay().and_then(|r| r.pads.get(slot).and_then(|p| latest(p, playing(r)?))).flatten()
}

/// The mouse at this poll: what was moved and pressed since the poll before,
/// summed, with the buttons held now; `Released` if the route let it go.
pub(super) fn replay_mouse() -> MouseAt {
    let Some(route) = replay() else { return MouseAt::Released };
    let Some(now) = playing(route) else { return MouseAt::Moved(0, 0, 0, 0, 0) };
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
        assert_eq!(r.world, None);
    }

    #[test]
    fn version_two_reads_polls_and_the_world() {
        let r = parse("# x\n10:1 K 0d\n20:5 K -\n500:40 W\n600:48 K 57\n");
        assert_eq!(r.world, Some(500));
        assert_eq!(r.polls, vec![(1, 10), (5, 20)]);
        assert_eq!(r.keys.len(), 3);
    }
}
