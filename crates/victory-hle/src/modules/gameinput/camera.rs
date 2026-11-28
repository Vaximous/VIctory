//! Mouse look written straight into the title's camera, as mouse injectors
//! for console emulators do: where the user's camera file
//! (`[input] camera`, env `VICTORY_MOUSE_CAMERA`; nothing of the title is in
//! this repository) names the camera's yaw and pitch, each mouse count adds
//! a fixed angle to them, 1:1 with no stick curve, acceleration or aim
//! assist, and the mouse leaves the right stick alone. Where none of the
//! named cameras is found the mouse moves the stick (`mouse`).
//!
//! A field is found by a pointer chain from the title image's base:
//! `[rva, o1, o2, ...]` is `base + rva`, then for each further offset the
//! pointer there plus the offset. Every read and write goes through the
//! kernel (`ReadProcessMemory` on ourselves), so a chain that leads nowhere
//! fails instead of faulting. The angles are written when the title asks
//! for its gamepad reading, on its own thread, before it updates its
//! camera from the (then still) stick.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
#[cfg(feature = "diagnostics")]
use std::sync::atomic::{AtomicI64, AtomicU64};

/// One camera's fields and how a mouse count turns them.
#[derive(Clone, Debug, PartialEq)]
pub struct Camera {
    pub name: String,
    pub yaw: Vec<u64>,
    pub pitch: Vec<u64>,
    /// Angle per mouse count at sensitivity 1 (its sign is the direction).
    pub yaw_per_count: f32,
    pub pitch_per_count: f32,
    pub pitch_min: f32,
    pub pitch_max: f32,
    /// The yaw's period (2π for radians, 360 for degrees); 0 does not wrap.
    pub wrap: f32,
    /// A chain to the camera object, and the image-relative qword it must
    /// start with to be this camera (its type); none: not checked.
    pub object: Vec<u64>,
    pub kind: Option<u64>,
    /// Copies the title carries the angles through from frame to frame;
    /// each moves by the same amount as `yaw` / `pitch`.
    pub yaw_copies: Vec<Vec<u64>>,
    pub pitch_copies: Vec<Vec<u64>>,
    /// A chain to three floats of position (the camera's or the player's),
    /// logged with the heading once a route second; none: not logged.
    pub position: Vec<u64>,
}

struct Cameras {
    base: u64,
    list: Vec<Camera>,
    /// Mouse sensitivity and vertical inversion.
    scale: f32,
    invert_y: bool,
}

static CAMERAS: OnceLock<Cameras> = OnceLock::new();
/// Counts moved since the title last read its pad.
static DX: AtomicI32 = AtomicI32::new(0);
static DY: AtomicI32 = AtomicI32::new(0);
/// A camera was found when the title last read its pad.
static LIVE: AtomicBool = AtomicBool::new(false);
/// Counts turned into a camera and counts dropped for want of one
/// (horizontal, vertical), since the start.
#[cfg(feature = "diagnostics")]
static TURNED: [AtomicI64; 2] = [const { AtomicI64::new(0) }; 2];
#[cfg(feature = "diagnostics")]
static DROPPED: [AtomicI64; 2] = [const { AtomicI64::new(0) }; 2];

/// With a route played back (`VICTORY_INPUT_REPLAY`; diagnostics builds), once each second of
/// the route: the camera's yaw and pitch and the counts so far, to compare
/// where replays of one route looked at the same moment of it.
#[cfg(feature = "diagnostics")]
fn note_heading(found: impl FnOnce() -> Option<(f32, f32)>) {
    static LAST_S: AtomicU64 = AtomicU64::new(u64::MAX);
    let Some(us) = super::route::route_now_us() else { return };
    let s = us / 1_000_000;
    if LAST_S.swap(s, Ordering::Relaxed) == s {
        return;
    }
    let found = found();
    let turned = [0, 1].map(|i| TURNED[i].load(Ordering::Relaxed));
    let dropped = [0, 1].map(|i| DROPPED[i].load(Ordering::Relaxed));
    match found {
        Some((yaw, pitch)) => tracing::info!(route_s = s, yaw, pitch, ?turned, ?dropped, "route camera"),
        None => tracing::info!(route_s = s, ?turned, ?dropped, "route camera: none found"),
    }
    #[cfg(windows)]
    if let Some(cameras) = CAMERAS.get() {
        note_position(&host::Own, cameras.base, &cameras.list, s);
    }
}

/// Once a route second (diagnostics builds): the first found camera's
/// position, where the camera file names a chain to it; and with
/// `VICTORY_ROUTE_CAMERA_DUMP=1` its object's first [`DUMP_BYTES`] as the
/// floats among them that could be coordinates (finite, 1 to 100,000 in
/// size), `offset=value`, to find which of them move as the player walks.
#[cfg(feature = "diagnostics")]
fn note_position(memory: &impl Memory, base: u64, list: &[Camera], s: u64) {
    let Some(camera) = list.iter().find(|c| present(memory, base, c)) else { return };
    if !camera.position.is_empty()
        && let Some(at) = resolve(memory, base, &camera.position)
    {
        let mut b = [0u8; 12];
        if memory.read(at, &mut b) {
            let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
            tracing::info!(route_s = s, x = f(0), y = f(4), z = f(8), "route position");
        }
    }
    static DUMP: OnceLock<bool> = OnceLock::new();
    if !*DUMP.get_or_init(|| std::env::var_os("VICTORY_ROUTE_CAMERA_DUMP").is_some_and(|v| v == "1")) || camera.object.is_empty() {
        return;
    }
    let Some(at) = resolve(memory, base, &camera.object) else { return };
    let mut b = vec![0u8; DUMP_BYTES];
    if !memory.read(at, &mut b) {
        return;
    }
    let mut out = String::new();
    for (i, w) in b.chunks_exact(4).enumerate() {
        let v = f32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        if v.is_finite() && (1.0..100_000.0).contains(&v.abs()) {
            use std::fmt::Write;
            let _ = write!(out, " {:x}={}", i * 4, v);
        }
    }
    tracing::info!(route_s = s, floats = %out, "route camera dump");
}

/// The bytes of the camera object [`note_position`] dumps.
#[cfg(feature = "diagnostics")]
const DUMP_BYTES: usize = 0x2000;

/// The first found camera's yaw and pitch now.
fn heading(memory: &impl Memory, base: u64, list: &[Camera]) -> Option<(f32, f32)> {
    list.iter().filter(|c| present(memory, base, c)).find_map(|c| {
        let y = read_f32(memory, resolve(memory, base, &c.yaw)?)?;
        let p = read_f32(memory, resolve(memory, base, &c.pitch)?)?;
        Some((y, p))
    })
}

/// The cameras of the user's file, with the title image's base.
pub fn configure(base: u64, list: Vec<Camera>, sensitivity: f32, invert_y: bool) {
    let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
    tracing::info!(cameras = ?names, "mouse camera hooks");
    let _ = CAMERAS.set(Cameras {
        base,
        list,
        scale: sensitivity,
        invert_y,
    });
}

/// Whether mouse movement goes to a camera rather than the stick.
pub(super) fn live() -> bool {
    LIVE.load(Ordering::Relaxed)
}

/// Counts for the camera, applied at the title's next pad read.
pub(super) fn add(dx: i32, dy: i32) {
    DX.fetch_add(dx, Ordering::Relaxed);
    DY.fetch_add(dy, Ordering::Relaxed);
}

/// Drops counts not yet applied (the cursor was let go).
pub(super) fn clear() {
    DX.store(0, Ordering::Relaxed);
    DY.store(0, Ordering::Relaxed);
}

/// The new yaw and pitch for `counts`.
pub(super) fn turned(
    camera: &Camera,
    (yaw, pitch): (f32, f32),
    (dx, dy): (i32, i32),
    scale: f32,
    invert_y: bool,
) -> (f32, f32) {
    let dy = if invert_y { -dy } else { dy };
    let mut yaw = yaw + dx as f32 * camera.yaw_per_count * scale;
    if camera.wrap > 0.0 {
        let half = camera.wrap / 2.0;
        yaw = (yaw + half).rem_euclid(camera.wrap) - half;
    }
    let pitch = pitch + dy as f32 * camera.pitch_per_count * scale;
    let (lo, hi) = (
        camera.pitch_min.min(camera.pitch_max),
        camera.pitch_max.max(camera.pitch_min),
    );
    (yaw, pitch.clamp(lo, hi))
}

/// Memory of our own process, read and written through the kernel.
pub(super) trait Memory {
    fn read(&self, at: u64, into: &mut [u8]) -> bool;
    fn write(&self, at: u64, from: &[u8]) -> bool;
}

fn read_u64(memory: &impl Memory, at: u64) -> Option<u64> {
    let mut b = [0; 8];
    memory.read(at, &mut b).then(|| u64::from_le_bytes(b))
}

fn read_f32(memory: &impl Memory, at: u64) -> Option<f32> {
    let mut b = [0; 4];
    memory
        .read(at, &mut b)
        .then(|| f32::from_le_bytes(b))
        .filter(|v| v.is_finite())
}

/// Where a chain leads, if it leads anywhere.
pub(super) fn resolve(memory: &impl Memory, base: u64, chain: &[u64]) -> Option<u64> {
    let (first, rest) = chain.split_first()?;
    let mut at = base.checked_add(*first)?;
    for offset in rest {
        let pointer = read_u64(memory, at)?;
        if pointer < 0x1_0000 {
            return None;
        }
        at = pointer.checked_add(*offset)?;
    }
    Some(at)
}

/// Whether the camera's object is there and of its kind (when one is named).
fn present(memory: &impl Memory, base: u64, camera: &Camera) -> bool {
    let Some(kind) = camera.kind else {
        return true;
    };
    resolve(memory, base, &camera.object)
        .and_then(|at| read_u64(memory, at))
        .is_some_and(|first| Some(first) == base.checked_add(kind))
}

/// Moves the float at each copy by `by`, wrapped into `wrap` (0: not).
fn shift(memory: &impl Memory, base: u64, copies: &[Vec<u64>], by: f32, wrap: f32) {
    if by == 0.0 {
        return;
    }
    for chain in copies {
        let Some(at) = resolve(memory, base, chain) else {
            continue;
        };
        let Some(value) = read_f32(memory, at) else {
            continue;
        };
        let mut value = value + by;
        if wrap > 0.0 {
            let half = wrap / 2.0;
            value = (value + half).rem_euclid(wrap) - half;
        }
        memory.write(at, &value.to_le_bytes());
    }
}

/// Turns every camera found by the counts; whether one was.
pub(super) fn turn(
    memory: &impl Memory,
    base: u64,
    list: &[Camera],
    counts: (i32, i32),
    scale: f32,
    invert_y: bool,
) -> bool {
    let mut found = false;
    for camera in list {
        if !present(memory, base, camera) {
            continue;
        }
        let (Some(y), Some(p)) = (
            resolve(memory, base, &camera.yaw),
            resolve(memory, base, &camera.pitch),
        ) else {
            continue;
        };
        let (Some(yaw), Some(pitch)) = (read_f32(memory, y), read_f32(memory, p)) else {
            continue;
        };
        found = true;
        if counts == (0, 0) {
            continue;
        }
        let (new_yaw, new_pitch) = turned(camera, (yaw, pitch), counts, scale, invert_y);
        memory.write(y, &new_yaw.to_le_bytes());
        memory.write(p, &new_pitch.to_le_bytes());
        // The copies move by the unwrapped turn and the clamped tilt.
        let by_yaw = counts.0 as f32 * camera.yaw_per_count * scale;
        shift(memory, base, &camera.yaw_copies, by_yaw, camera.wrap);
        shift(memory, base, &camera.pitch_copies, new_pitch - pitch, 0.0);
    }
    found
}

/// Whether a route played back (`VICTORY_INPUT_REPLAY`) turns the camera to
/// an absolute heading rather than by counts added to whatever it has
/// (`VICTORY_ROUTE_CAMERA_ABS=1`; off by default): the heading the camera
/// had when it was found, plus every count of the route since, written at
/// each pad read. What the title turns by itself in between (a follow
/// camera that swings behind a walking player, at its own frame pace) is
/// undone.
fn absolute() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| super::route::replaying() && std::env::var_os("VICTORY_ROUTE_CAMERA_ABS").is_some_and(|v| v == "1"))
}

/// For [`absolute`]: the heading the camera had when found, and the counts
/// since.
static TARGET: std::sync::Mutex<Option<((f32, f32), (i64, i64))>> = std::sync::Mutex::new(None);

/// Turns the first found camera to its absolute target ([`absolute`]);
/// whether one was found.
fn turn_to(memory: &impl Memory, base: u64, list: &[Camera], counts: (i32, i32), scale: f32, invert_y: bool) -> bool {
    let mut target = TARGET.lock().unwrap_or_else(|e| e.into_inner());
    for camera in list {
        if !present(memory, base, camera) {
            continue;
        }
        let (Some(y), Some(p)) = (resolve(memory, base, &camera.yaw), resolve(memory, base, &camera.pitch)) else {
            continue;
        };
        let (Some(yaw), Some(pitch)) = (read_f32(memory, y), read_f32(memory, p)) else {
            continue;
        };
        // Found anew (another camera object, or after none): its heading now
        // is the base.
        if !LIVE.load(Ordering::Relaxed) || target.is_none() {
            *target = Some(((yaw, pitch), (0, 0)));
        }
        let Some((from, sum)) = target.as_mut() else { continue };
        sum.0 += i64::from(counts.0);
        sum.1 += i64::from(counts.1);
        let clamp = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let (new_yaw, new_pitch) = turned(camera, *from, (clamp(sum.0), clamp(sum.1)), scale, invert_y);
        if (new_yaw, new_pitch) != (yaw, pitch) {
            memory.write(y, &new_yaw.to_le_bytes());
            memory.write(p, &new_pitch.to_le_bytes());
            let mut by_yaw = new_yaw - yaw;
            if camera.wrap > 0.0 {
                let half = camera.wrap / 2.0;
                by_yaw = (by_yaw + half).rem_euclid(camera.wrap) - half;
            }
            shift(memory, base, &camera.yaw_copies, by_yaw, camera.wrap);
            shift(memory, base, &camera.pitch_copies, new_pitch - pitch, 0.0);
        }
        return true;
    }
    false
}

/// A camera trace for a route played back (`VICTORY_ROUTE_CAMERA_TRACE=<file>`;
/// off when unset): lines of `<route seconds> <yaw> <pitch>`, the camera's
/// heading at those moments of a replay that went where the route goes
/// (the "route camera" lines of a diagnostics build). The title's follow
/// camera turns by itself as the player walks, by as much as 1.1 rad in a
/// few seconds with no mouse input, and by how much depends on the frame
/// pace: the same counts left the camera facing elsewhere at the door.
/// From the first checkpoint until the last one (and route 80 s at most),
/// while a camera is found, it is written at each pad read to the heading
/// interpolated between the checkpoints around the route's moment; the
/// route's mouse counts are in the trace already and are not added again.
fn trace() -> Option<&'static [(f64, f32, f32)]> {
    static TRACE: OnceLock<Option<Vec<(f64, f32, f32)>>> = OnceLock::new();
    TRACE
        .get_or_init(|| {
            if !super::route::replaying() {
                return None;
            }
            let path = std::env::var_os("VICTORY_ROUTE_CAMERA_TRACE")?;
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(%error, "the camera trace could not be read (VICTORY_ROUTE_CAMERA_TRACE)");
                    return None;
                }
            };
            let mut points: Vec<(f64, f32, f32)> = text
                .lines()
                .filter_map(|line| {
                    let mut w = line.split_whitespace().map(str::parse::<f64>);
                    let (Some(Ok(t)), Some(Ok(y)), Some(Ok(p))) = (w.next(), w.next(), w.next()) else { return None };
                    (t.is_finite() && y.is_finite() && p.is_finite()).then_some((t, y as f32, p as f32))
                })
                .filter(|&(t, _, _)| t <= TRACE_UNTIL_S)
                .collect();
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
            tracing::info!(
                path = %std::path::Path::new(&path).display(),
                points = points.len(),
                from_s = points.first().map(|p| p.0),
                to_s = points.last().map(|p| p.0),
                "route camera trace (VICTORY_ROUTE_CAMERA_TRACE)"
            );
            (!points.is_empty()).then_some(points)
        })
        .as_deref()
}

/// With a camera trace: when the found camera's yaw first read other than
/// zero (the title's gameplay camera came up; before it, from the world's
/// first frame on, it reads exactly 0). The route is held at the trace's
/// first checkpoint until then (`route::clock`).
static READY: OnceLock<std::time::Instant> = OnceLock::new();

/// When the gameplay camera came up ([`READY`]).
pub(super) fn ready_at() -> Option<std::time::Instant> {
    READY.get().copied()
}

/// The trace's first checkpoint, in route microseconds.
pub(super) fn trace_from_us() -> Option<u64> {
    trace().and_then(|p| p.first()).map(|p| (p.0 * 1e6) as u64)
}

/// The trace is followed no further than this route second.
const TRACE_UNTIL_S: f64 = 80.0;

/// The trace's heading at route second `t`, if `t` is within it: yaw along
/// the shorter way round (`wrap`), pitch straight.
fn traced(points: &[(f64, f32, f32)], t: f64, wrap: f32) -> Option<(f32, f32)> {
    let first = points.first()?;
    let last = points.last()?;
    if t < first.0 || t > last.0 {
        return None;
    }
    let i = points.partition_point(|p| p.0 <= t);
    let a = points[i.saturating_sub(1)];
    let Some(&b) = points.get(i) else { return Some((a.1, a.2)) };
    let k = if b.0 > a.0 { ((t - a.0) / (b.0 - a.0)) as f32 } else { 0.0 };
    let mut dy = b.1 - a.1;
    if wrap > 0.0 {
        let half = wrap / 2.0;
        dy = (dy + half).rem_euclid(wrap) - half;
    }
    let mut yaw = a.1 + dy * k;
    if wrap > 0.0 {
        let half = wrap / 2.0;
        yaw = (yaw + half).rem_euclid(wrap) - half;
    }
    Some((yaw, a.2 + (b.2 - a.2) * k))
}

/// A track for a route played back with a camera trace
/// (`VICTORY_ROUTE_STEER=<file>`; off when unset): lines of
/// `<route seconds> <x> <y> <z>`, where the position the camera file names
/// (`position`) was at those moments of a replay that went where the route
/// goes. Between route [`STEER_FROM_S`] and [`STEER_UNTIL_S`], when the
/// position is more than [`STEER_OFF_M`] from the track's, the traced
/// heading is turned toward the track a second ahead, by as much as the
/// walk's bearing is off ([`STEER_MOST`] at most): the player walks where
/// the camera faces, so the walk turns with it. Which way a heading turns
/// the walk is taken from the track and the trace themselves (`sign`).
struct Steer {
    points: Vec<(f64, [f32; 3])>,
    /// +1 or -1: the walk's bearing (atan2 of y, x) moves with the yaw so.
    sign: f32,
    /// The walk's bearing less `sign` times the yaw, on average: the yaw
    /// that walks toward a bearing ([`reach`]).
    offset: f32,
}

const STEER_FROM_S: f64 = 46.0;
const STEER_UNTIL_S: f64 = 74.0;
const STEER_OFF_M: f32 = 0.5;
const STEER_MOST: f32 = 0.5;
/// The position wraps at a cell of this many metres (x and y): run ow93-out's
/// track went from y -31.7 to -1.4 in a second of walking straight on.
const STEER_CELL: f32 = 32.0;

/// A difference of positions across the cell's edge, the short way.
fn across(d: f32) -> f32 {
    d - STEER_CELL * (d / STEER_CELL).round()
}

fn wrapped(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    (a + PI).rem_euclid(TAU) - PI
}

fn steer() -> Option<&'static Steer> {
    static STEER: OnceLock<Option<Steer>> = OnceLock::new();
    STEER
        .get_or_init(|| {
            let trace = trace()?;
            let path = std::env::var_os("VICTORY_ROUTE_STEER")?;
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(%error, "the route track could not be read (VICTORY_ROUTE_STEER)");
                    return None;
                }
            };
            let mut points: Vec<(f64, [f32; 3])> = text
                .lines()
                .filter_map(|line| {
                    let v: Vec<f64> = line.split_whitespace().filter_map(|w| w.parse().ok()).collect();
                    (v.len() >= 4 && v.iter().all(|x| x.is_finite())).then(|| (v[0], [v[1] as f32, v[2] as f32, v[3] as f32]))
                })
                .collect();
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
            // The bearing of each second's step of the walk, against the
            // traced yaw then: with the camera behind the player the two move
            // together (sign +1) or against each other (-1); the sign whose
            // differences stay the most alike.
            let mut pairs = Vec::new();
            for w in points.windows(2) {
                let (a, b) = (w[0], w[1]);
                let (dx, dy) = (across(b.1[0] - a.1[0]), across(b.1[1] - a.1[1]));
                if (dx * dx + dy * dy).sqrt() < 0.3 || a.0 < STEER_FROM_S || b.0 > STEER_UNTIL_S {
                    continue;
                }
                if let Some((yaw, _)) = traced(trace, (a.0 + b.0) / 2.0, std::f32::consts::TAU) {
                    pairs.push((dy.atan2(dx), yaw));
                }
            }
            let spread = |sign: f32| {
                let (c, s) = pairs.iter().fold((0.0f32, 0.0f32), |(c, s), &(b, y)| {
                    let d = wrapped(b - sign * y);
                    (c + d.cos(), s + d.sin())
                });
                1.0 - (c * c + s * s).sqrt() / pairs.len().max(1) as f32
            };
            let (plus, minus) = (spread(1.0), spread(-1.0));
            let sign = if plus <= minus { 1.0 } else { -1.0 };
            let (c, s) = pairs.iter().fold((0.0f32, 0.0f32), |(c, s), &(b, y)| {
                let d = wrapped(b - sign * y);
                (c + d.cos(), s + d.sin())
            });
            let offset = s.atan2(c);
            tracing::info!(
                path = %std::path::Path::new(&path).display(),
                points = points.len(),
                steps = pairs.len(),
                spread_plus = plus,
                spread_minus = minus,
                sign,
                "route track (VICTORY_ROUTE_STEER)"
            );
            (points.len() >= 2 && !pairs.is_empty()).then_some(Steer { points, sign, offset })
        })
        .as_ref()
}

/// The track's position at route second `t` ([`Steer`]), straight between
/// its points.
fn tracked(points: &[(f64, [f32; 3])], t: f64) -> Option<[f32; 3]> {
    let i = points.partition_point(|p| p.0 <= t);
    let a = points.get(i.checked_sub(1)?)?;
    let Some(b) = points.get(i) else { return Some(a.1) };
    let k = if b.0 > a.0 { ((t - a.0) / (b.0 - a.0)) as f32 } else { 0.0 };
    Some(std::array::from_fn(|n| a.1[n] + if n < 2 { across(b.1[n] - a.1[n]) } else { b.1[n] - a.1[n] } * k))
}

/// The turn [`Steer`] adds to the traced yaw at route second `t`: 0 when not
/// steering, on the track, or with no position.
fn steered(memory: &impl Memory, base: u64, camera: &Camera, _trace: &[(f64, f32, f32)], t: f64) -> f32 {
    if !(STEER_FROM_S..=STEER_UNTIL_S).contains(&t) || camera.position.is_empty() {
        return 0.0;
    }
    let Some(steer) = steer() else { return 0.0 };
    let Some(at) = resolve(memory, base, &camera.position) else { return 0.0 };
    let mut b = [0u8; 12];
    if !memory.read(at, &mut b) {
        return 0.0;
    }
    let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let now = [f(0), f(4), f(8)];
    let (Some(here), Some(ahead)) = (tracked(&steer.points, t), tracked(&steer.points, t + 1.0)) else { return 0.0 };
    let off = (across(now[0] - here[0]).powi(2) + across(now[1] - here[1]).powi(2)).sqrt();
    let (rx, ry) = (across(ahead[0] - here[0]), across(ahead[1] - here[1]));
    let turn = if off > STEER_OFF_M && (rx * rx + ry * ry).sqrt() >= 0.2 {
        let wanted = across(ahead[1] - now[1]).atan2(across(ahead[0] - now[0]));
        let walked = ry.atan2(rx);
        steer.sign * wrapped(wanted - walked).clamp(-STEER_MOST, STEER_MOST)
    } else {
        0.0
    };
    let turn = match reach(t, now, steer, yaw_now(t)) {
        Some(to) => to,
        None => turn,
    };
    let second = LAST_S_PEEK.load(Ordering::Relaxed) != t as u64;
    if nudge_on() {
        nudge(t, now, here, (rx, ry), off, turn, second);
    }
    static LAST_S: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    LAST_S_PEEK.store(t as u64, Ordering::Relaxed);
    if LAST_S.swap(t as u64, Ordering::Relaxed) != t as u64 {
        let (key, back) = route_nudge();
        tracing::info!(route_s = t, off, turn, x = now[0], y = now[1], z = now[2], nudge = key.unwrap_or(0), back, "route steer");
    }
    turn
}

/// The traced yaw at route second `t` (0 if the trace has none).
fn yaw_now(t: f64) -> f32 {
    trace().and_then(|points| traced(points, t, std::f32::consts::TAU)).map_or(0.0, |(yaw, _)| yaw)
}

/// The route second of a key press the walk must reach the track for
/// (`VICTORY_ROUTE_REACH=<route seconds>`; off when unset): the outdoor
/// route presses the key that gets into the pickup at 66.1 s, at the foot
/// of the stairs, and the title walks the player to the door from where
/// they stand. Replayed, the player came off the stairs a second late,
/// 1.6 m short, and the walk to the door stuck at the stairs' rail: 7-9 m
/// off the track from 66 s on, every run that stayed indoors (ow96-base,
/// ow102-base). From that second on, until the player is within
/// [`REACH_NEAR_M`] of where the track was then, the camera faces that
/// spot and forward is held, the key left out; once there the key is
/// pressed ([`REACH_PRESS_S`]), and the route goes on as recorded.
fn reach_at() -> Option<f64> {
    static AT: OnceLock<Option<f64>> = OnceLock::new();
    *AT.get_or_init(|| std::env::var("VICTORY_ROUTE_REACH").ok().and_then(|v| v.trim().parse().ok()))
}

const REACH_NEAR_M: f32 = 0.8;
const REACH_PRESS_S: f64 = 0.2;
/// How long after the press the walk is helped at most.
const REACH_MOST_S: f64 = 8.0;
/// The key the route presses there (`Y`).
const REACH_KEY: u8 = 0x59;

/// [`reach_at`]'s state: 0 not begun, 1 walking there, 2 pressing (until
/// the route second in the second field), 3 done.
static REACH: std::sync::Mutex<(u8, f64)> = std::sync::Mutex::new((0, 0.0));

/// [`reach_at`]: the camera's turn from the traced yaw `yaw` at route second
/// `t`, the player at `now`, if the walk is being helped.
fn reach(t: f64, now: [f32; 3], steer: &Steer, yaw: f32) -> Option<f32> {
    let at = reach_at()?;
    if t < at || t > at + REACH_MOST_S {
        return None;
    }
    let target = tracked(&steer.points, at)?;
    let (dx, dy) = (across(target[0] - now[0]), across(target[1] - now[1]));
    let far = (dx * dx + dy * dy).sqrt();
    let mut state = REACH.lock().unwrap_or_else(|e| e.into_inner());
    match state.0 {
        0 if far <= REACH_NEAR_M => {
            // There in time: the route's own press does it.
            state.0 = 3;
            tracing::info!(route_s = t, far, "route reach: there in time");
            None
        }
        0 | 1 if far > REACH_NEAR_M => {
            if state.0 == 0 {
                tracing::info!(route_s = t, far, "route reach: walking there");
            }
            state.0 = 1;
            // The yaw that walks toward the spot.
            let wanted = steer.sign * wrapped(dy.atan2(dx) - steer.offset);
            Some(wrapped(wanted - yaw))
        }
        1 => {
            tracing::info!(route_s = t, far, "route reach: there, the key pressed");
            *state = (2, t + REACH_PRESS_S);
            None
        }
        2 if t >= state.1 => {
            state.0 = 3;
            None
        }
        _ => None,
    }
}

/// [`reach_at`]'s keys: forward held and the route's key left out while
/// walking there, the key held while pressing.
pub(super) fn route_reach_keys(down: &mut Vec<u8>) {
    if reach_at().is_none() {
        return;
    }
    match REACH.lock().unwrap_or_else(|e| e.into_inner()).0 {
        1 => {
            down.retain(|&k| k != REACH_KEY);
            if !down.contains(&0x57) {
                down.push(0x57);
            }
        }
        2 => {
            if !down.contains(&REACH_KEY) {
                down.push(REACH_KEY);
            }
        }
        _ => {}
    }
}

/// The route second [`steered`] last saw.
static LAST_S_PEEK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

/// Whether the steered walk is also nudged toward the track by the strafe
/// keys, and backed off when it is blocked (`VICTORY_ROUTE_NUDGE=1`; off by
/// default; with `VICTORY_ROUTE_STEER`).
///
/// Turning the heading alone did not bring the walk back where it was
/// blocked: at route 65-71 s of the outdoor route the player stood at the
/// stairs, 7 m off the track and behind it, the bearing to the track ahead
/// nearly the walk's own, so the turns stayed tiny (run ow96-base).
fn nudge_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ROUTE_NUDGE").is_some_and(|v| v == "1"))
}

/// Off the track by more than this sideways (metres), the strafe key toward it is held.
const NUDGE_SIDE_M: f32 = 0.6;
/// The distance grew by more than this in a second with the turns small: blocked.
const NUDGE_GREW_M: f32 = 0.8;
/// How long the walk backs off when blocked (route seconds).
const NUDGE_BACK_S: f64 = 0.6;

/// The strafe key to hold (0: none), whether to back off, and the state
/// that decides them, from [`nudge`].
static NUDGE_KEY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static NUDGE_BACK: AtomicBool = AtomicBool::new(false);
/// Of the second before: the distance off and sideways (f32 bits), whether
/// a strafe key was held, and which way strafing left goes (+1 / -1: the
/// track's left as the walk's bearing turns, until it is seen otherwise).
static NUDGE_STATE: std::sync::Mutex<(f32, f32, bool, f32, f64)> = std::sync::Mutex::new((0.0, 0.0, false, 1.0, f64::MIN));

/// [`nudge_on`]: decides the keys for route second `t` from where the
/// player is (`now`), where the track is (`here`) and goes (`dir`), how far
/// off that is, and the turn the heading got; `second`: the first call of
/// a route second, where what the second before did is looked at.
fn nudge(t: f64, now: [f32; 3], here: [f32; 3], dir: (f32, f32), off: f32, turn: f32, second: bool) {
    let len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt();
    let mut state = NUDGE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let (dx, dy) = (across(here[0] - now[0]), across(here[1] - now[1]));
    // The track to the walk's left (> 0) or right, and how far behind it the
    // player is (> 0: behind).
    let (side, behind) = if len >= 0.2 { ((dir.0 * dy - dir.1 * dx) / len, (dir.0 * dx + dir.1 * dy) / len) } else { (0.0, 0.0) };
    if second {
        let (off_was, side_was, strafed, _, _) = *state;
        // Strafing made it worse: the keys go the other way round.
        if strafed && side.abs() > side_was.abs() + 0.3 {
            state.3 = -state.3;
            tracing::info!(route_s = t, side, side_was, "route nudge: strafing went away from the track, the other way round now");
        }
        // Blocked: the distance grew with the turns small, behind the track.
        if off > 1.5 && off - off_was > NUDGE_GREW_M && turn.abs() < 0.15 && behind > 0.5 {
            state.4 = t + NUDGE_BACK_S;
        }
        state.0 = off;
        state.1 = side;
    }
    let key = if off > 1.0 && side.abs() > NUDGE_SIDE_M {
        // A (0x41) to the left, D (0x44) to the right.
        if side * state.3 > 0.0 { 0x41 } else { 0x44 }
    } else {
        0
    };
    if second {
        state.2 = key != 0;
    }
    NUDGE_KEY.store(key, Ordering::Relaxed);
    NUDGE_BACK.store(t < state.4, Ordering::Relaxed);
}

/// While a route is replayed with [`nudge_on`]: the strafe key to hold
/// (`A` or `D`), and whether the walk backs off (forward let go, `S` held).
pub(super) fn route_nudge() -> (Option<u8>, bool) {
    if following() {
        return (None, FOLLOW_BACK.load(Ordering::Relaxed));
    }
    if !nudge_on() {
        return (None, false);
    }
    let t = super::route::route_now_us().map_or(0.0, |us| us as f64 / 1e6);
    if !(STEER_FROM_S..=STEER_UNTIL_S).contains(&t) {
        return (None, false);
    }
    let key = NUDGE_KEY.load(Ordering::Relaxed);
    ((key != 0).then_some(key), NUDGE_BACK.load(Ordering::Relaxed))
}

/// When the player first had control: the first pad read at which the
/// gameplay camera was found with its yaw other than zero (from the world's
/// first frame until the gameplay camera comes up it reads exactly 0). Asked
/// for only while the input is recorded or recorded tracks are followed.
static CONTROL: OnceLock<std::time::Instant> = OnceLock::new();

/// When the player first had control ([`CONTROL`]).
pub(super) fn control_at() -> Option<std::time::Instant> {
    CONTROL.get().copied()
}

/// How often the recorded tracks get a line (seconds).
const TRACK_EVERY_S: f64 = 0.25;

/// The tracks written beside a recording: `<name>.cam` lines of
/// `<seconds since control> <yaw> <pitch>`, `<name>.pos` lines of
/// `<seconds since control> <x> <y> <z>`, each flushed as written (a crash
/// loses nothing said before it).
struct TrackOut {
    cam: std::io::BufWriter<std::fs::File>,
    pos: Option<std::io::BufWriter<std::fs::File>>,
    last: f64,
}

/// `None`: not begun; `Some(None)`: the files could not be made.
static TRACK_OUT: std::sync::Mutex<Option<Option<TrackOut>>> = std::sync::Mutex::new(None);

/// The position the camera file names, read now.
fn position(memory: &impl Memory, base: u64, camera: &Camera) -> Option<[f32; 3]> {
    if camera.position.is_empty() {
        return None;
    }
    let at = resolve(memory, base, &camera.position)?;
    let mut b = [0u8; 12];
    if !memory.read(at, &mut b) {
        return None;
    }
    let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let p = [f(0), f(4), f(8)];
    p.iter().all(|v| v.is_finite()).then_some(p)
}

/// The first found camera, with its heading.
fn found_camera<'a>(memory: &impl Memory, base: u64, list: &'a [Camera]) -> Option<(&'a Camera, f32, f32)> {
    list.iter().filter(|c| present(memory, base, c)).find_map(|c| {
        let y = read_f32(memory, resolve(memory, base, &c.yaw)?)?;
        let p = read_f32(memory, resolve(memory, base, &c.pitch)?)?;
        Some((c, y, p))
    })
}

/// Recording (`VICTORY_INPUT_RECORD`): from the player's first control on
/// ([`CONTROL`]), a line of each track every [`TRACK_EVERY_S`].
fn record_tracks(memory: &impl Memory, base: u64, list: &[Camera]) {
    let Some(&got) = CONTROL.get() else { return };
    let t = got.elapsed().as_secs_f64();
    let mut out = TRACK_OUT.lock().unwrap_or_else(|e| e.into_inner());
    if out.is_none() {
        let opened = super::route::record_path().and_then(|rec| {
            let open = |ext: &str| {
                let path = rec.with_extension(ext);
                match std::fs::File::create(&path) {
                    Ok(f) => Some(std::io::BufWriter::new(f)),
                    Err(error) => {
                        tracing::warn!(%error, path = %path.display(), "a recorded track could not be made");
                        None
                    }
                }
            };
            let cam = open("cam")?;
            let pos = if list.iter().any(|c| !c.position.is_empty()) { open("pos") } else { None };
            tracing::info!(
                position = pos.is_some(),
                "route: the camera's heading and the player's position are recorded beside the route, in seconds since the player's control"
            );
            Some(TrackOut { cam, pos, last: f64::MIN })
        });
        *out = Some(opened);
    }
    let Some(Some(track)) = out.as_mut() else { return };
    if t - track.last < TRACK_EVERY_S {
        return;
    }
    let Some((camera, yaw, pitch)) = found_camera(memory, base, list) else { return };
    track.last = t;
    let _ = writeln!(track.cam, "{t:.3} {yaw} {pitch}");
    let _ = track.cam.flush();
    if let (Some(pos), Some([x, y, z])) = (track.pos.as_mut(), position(memory, base, camera)) {
        let _ = writeln!(pos, "{t:.3} {x} {y} {z}");
        let _ = pos.flush();
    }
}

/// Recorded tracks a replay follows: the heading and position lines beside
/// the route (`<name>.cam`, `<name>.pos`, as [`record_tracks`] writes them),
/// when the route says when its player got control (`C`). Off with
/// `VICTORY_ROUTE_TRACKS=0`. From the replay's own player's control on, the
/// camera is written to the recorded heading at each pad read (the route's
/// mouse counts are in it already and are not added), turned toward the
/// recorded position a second ahead when the player is off it
/// ([`follow_turn`]); a player that stops while the recorded walk goes on
/// backs off and walks on turned aside.
struct Tracks {
    cam: Vec<(f64, f32, f32)>,
    pos: Vec<(f64, [f32; 3])>,
    /// As [`Steer`]'s: how the walk's bearing moves with the yaw.
    sign: f32,
}

/// Lines of a time and `N` numbers, sorted by time; others are left out.
fn read_lines<const N: usize>(path: &std::path::Path) -> Option<Vec<(f64, [f32; N])>> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut points: Vec<(f64, [f32; N])> = text
        .lines()
        .filter_map(|line| {
            let v: Vec<f64> = line.split_whitespace().map_while(|w| w.parse().ok()).collect();
            (v.len() > N && v.iter().all(|x| x.is_finite())).then(|| (v[0], std::array::from_fn(|i| v[i + 1] as f32)))
        })
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    Some(points)
}

/// Whether the walk's bearing moves with the yaw (+1) or against it (-1),
/// from the recorded steps of 0.3 m or more against the heading then; with
/// how alike each sign leaves the differences (0: all alike) and the steps.
fn walk_sign(pos: &[(f64, [f32; 3])], cam: &[(f64, f32, f32)]) -> (f32, f32, f32, usize) {
    let mut pairs = Vec::new();
    for w in pos.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (dx, dy) = (across(b.1[0] - a.1[0]), across(b.1[1] - a.1[1]));
        if (dx * dx + dy * dy).sqrt() < 0.3 {
            continue;
        }
        if let Some((yaw, _)) = traced(cam, (a.0 + b.0) / 2.0, std::f32::consts::TAU) {
            pairs.push((dy.atan2(dx), yaw));
        }
    }
    let spread = |sign: f32| {
        let (c, s) = pairs.iter().fold((0.0f32, 0.0f32), |(c, s), &(b, y)| {
            let d = wrapped(b - sign * y);
            (c + d.cos(), s + d.sin())
        });
        1.0 - (c * c + s * s).sqrt() / pairs.len().max(1) as f32
    };
    let (plus, minus) = (spread(1.0), spread(-1.0));
    (if plus <= minus { 1.0 } else { -1.0 }, plus, minus, pairs.len())
}

fn tracks() -> Option<&'static Tracks> {
    static TRACKS: OnceLock<Option<Tracks>> = OnceLock::new();
    TRACKS
        .get_or_init(|| {
            super::route::route_control_us()?;
            if std::env::var_os("VICTORY_ROUTE_TRACKS").is_some_and(|v| v == "0") {
                return None;
            }
            let rec = super::route::replay_path()?;
            let cam: Vec<(f64, f32, f32)> =
                read_lines::<2>(&rec.with_extension("cam"))?.into_iter().map(|(t, [y, p])| (t, y, p)).collect();
            let pos = read_lines::<3>(&rec.with_extension("pos")).unwrap_or_default();
            if cam.len() < 2 {
                return None;
            }
            let (sign, plus, minus, steps) = walk_sign(&pos, &cam);
            tracing::info!(
                cam = cam.len(),
                pos = pos.len(),
                until_s = cam.last().map(|c| c.0),
                steps,
                spread_plus = plus,
                spread_minus = minus,
                sign,
                "route: the recorded tracks beside it are followed (VICTORY_ROUTE_TRACKS=0: not)"
            );
            Some(Tracks { cam, pos, sign })
        })
        .as_ref()
}

/// Whether a replay follows recorded tracks ([`Tracks`]).
pub(super) fn following() -> bool {
    tracks().is_some()
}

/// Off the track by more than this (metres), the heading is turned toward it.
const FOLLOW_OFF_M: f32 = 0.5;
/// The most the heading is turned toward the track (radians), and the most
/// when farther off than [`FOLLOW_FAR_M`].
const FOLLOW_MOST: f32 = 0.6;
const FOLLOW_FAR_M: f32 = 1.5;
const FOLLOW_MOST_FAR: f32 = 0.9;
/// How far along the track from its point nearest the player the heading is
/// taken and the walk aimed (seconds of the recording).
const HEAD_LEAD_S: f64 = 0.3;
const AIM_LEAD_S: f64 = 0.6;
/// The nearest point is looked for from this much before the last one found
/// to this much after the replay's moment (seconds): the walk goes on along
/// the track, never to a later pass of the same place.
const NEAR_BACK_S: f64 = 0.5;
const NEAR_AHEAD_S: f64 = 2.0;
/// Moved less than [`STALL_M`] in [`STALL_S`] while the recorded player moved
/// [`STALL_RECORDED_M`] or more in the same second: stuck.
const STALL_M: f32 = 0.25;
const STALL_RECORDED_M: f32 = 0.7;
const STALL_S: f64 = 1.0;
/// Stuck: the walk backs off this long, then walks on turned aside for
/// [`ASIDE_S`] by [`ASIDE`] (the side swaps at each stall).
const BACK_S: f64 = 0.7;
const ASIDE_S: f64 = 1.0;
const ASIDE: f32 = 0.7;

/// [`follow_turn`]'s state: the last check (time, position), backing off
/// and turning aside until (seconds since control), the side, the track's
/// moment nearest the player last found, the second last logged and the
/// stalls so far.
struct Follow {
    check: Option<(f64, [f32; 3])>,
    back_until: f64,
    aside_until: f64,
    side: f32,
    near: f64,
    second: i64,
    stalls: u32,
}

const FOLLOW_START: Follow =
    Follow { check: None, back_until: f64::MIN, aside_until: f64::MIN, side: 1.0, near: 0.0, second: i64::MIN, stalls: 0 };
static FOLLOW: std::sync::Mutex<Follow> = std::sync::Mutex::new(FOLLOW_START);
/// The walk backs off now (forward let go, back held: [`route_nudge`]).
static FOLLOW_BACK: AtomicBool = AtomicBool::new(false);

/// The flat distance between two positions (across the cell's edge).
fn apart(a: [f32; 3], b: [f32; 3]) -> f32 {
    (across(a[0] - b[0]).powi(2) + across(a[1] - b[1]).powi(2)).sqrt()
}

/// The recorded moment in `from..=to` whose position is nearest `now`
/// (to the recorded points, then between the two around it), and how far.
fn nearest(points: &[(f64, [f32; 3])], from: f64, to: f64, now: [f32; 3]) -> Option<(f64, f32)> {
    let lo = points.partition_point(|p| p.0 < from);
    let hi = points.partition_point(|p| p.0 <= to);
    let (i, _) = points[lo..hi].iter().enumerate().min_by(|a, b| apart(a.1.1, now).total_cmp(&apart(b.1.1, now)))?;
    let i = lo + i;
    // On the segments either side, the nearest spot.
    let mut best = (points[i].0, apart(points[i].1, now));
    for j in [i.checked_sub(1), Some(i)].into_iter().flatten() {
        let (Some(a), Some(b)) = (points.get(j), points.get(j + 1)) else { continue };
        let (dx, dy) = (across(b.1[0] - a.1[0]), across(b.1[1] - a.1[1]));
        let len = dx * dx + dy * dy;
        if len < 1e-6 {
            continue;
        }
        let k = ((across(now[0] - a.1[0]) * dx + across(now[1] - a.1[1]) * dy) / len).clamp(0.0, 1.0);
        let at = a.0 + (b.0 - a.0) * f64::from(k);
        let Some(p) = tracked(points, at) else { continue };
        let d = apart(p, now);
        if d < best.1 && (from..=to).contains(&at) {
            best = (at, d);
        }
    }
    Some(best)
}

/// What [`follow_turn`] decides at `t` (seconds since control), the player
/// at `now`: the recorded moment whose heading the camera takes (a little
/// past the track's point nearest the player, so a player behind the
/// recording turns where the recording turned, not before), the turn added
/// to it, and whether to back off. Pure, for the tests; the state is the
/// caller's.
fn follow_step(f: &mut Follow, tracks: &Tracks, t: f64, now: [f32; 3]) -> (f64, f32, bool) {
    let Some((near, off)) = nearest(&tracks.pos, (f.near - NEAR_BACK_S).max(0.0), t + NEAR_AHEAD_S, now) else {
        return (t, 0.0, false);
    };
    f.near = f.near.max(near);
    let head = near + HEAD_LEAD_S;
    let (Some(from), Some(to), Some(aim)) =
        (tracked(&tracks.pos, near), tracked(&tracks.pos, near + 1.0), tracked(&tracks.pos, near + AIM_LEAD_S))
    else {
        return (head, 0.0, false);
    };
    let (rx, ry) = (across(to[0] - from[0]), across(to[1] - from[1]));
    let walking = (rx * rx + ry * ry).sqrt() >= 0.5;
    // Stuck: the recorded player walked on in the last second, ours not.
    match f.check {
        Some((at, was)) if t - at >= STALL_S => {
            let moved = apart(now, was);
            let recorded = match (tracked(&tracks.pos, at), tracked(&tracks.pos, t)) {
                (Some(a), Some(b)) => apart(a, b),
                _ => 0.0,
            };
            if moved < STALL_M && recorded >= STALL_RECORDED_M && t > f.aside_until {
                f.back_until = t + BACK_S;
                f.aside_until = t + BACK_S + ASIDE_S;
                f.side = -f.side;
                f.stalls += 1;
                tracing::info!(route_s = t, moved, recorded, off, side = f.side, stalls = f.stalls, "route follow: stuck, backing off and turning aside");
            }
            f.check = Some((t, now));
        }
        None => f.check = Some((t, now)),
        _ => {}
    }
    let back = t < f.back_until;
    let turn = if t < f.aside_until {
        f.side * ASIDE
    } else if off > FOLLOW_OFF_M && walking {
        let most = if off > FOLLOW_FAR_M { FOLLOW_MOST_FAR } else { FOLLOW_MOST };
        let wanted = across(aim[1] - now[1]).atan2(across(aim[0] - now[0]));
        let walked = ry.atan2(rx);
        tracks.sign * wrapped(wanted - walked).clamp(-most, most)
    } else {
        0.0
    };
    if f.second != t as i64 {
        f.second = t as i64;
        tracing::info!(
            route_s = t,
            off,
            behind_s = t - near,
            turn,
            back,
            stalls = f.stalls,
            x = now[0],
            y = now[1],
            z = now[2],
            "route follow (off: metres from the track; behind_s: how far the player is behind the recording)"
        );
    }
    (head, turn, back)
}

/// [`follow_step`] with its state, the backing off set for the keys: the
/// recorded moment whose heading to take and the turn added to it.
fn follow_turn(tracks: &Tracks, t: f64, now: Option<[f32; 3]>) -> (f64, f32) {
    let Some(now) = now else {
        FOLLOW_BACK.store(false, Ordering::Relaxed);
        return (t, 0.0);
    };
    let mut f = FOLLOW.lock().unwrap_or_else(|e| e.into_inner());
    let (head, turn, back) = follow_step(&mut f, tracks, t, now);
    FOLLOW_BACK.store(back, Ordering::Relaxed);
    (head, turn)
}

/// Writes the first found camera to the recorded heading now, turned toward
/// the recorded positions ([`follow_turn`]); `None` when no tracks are
/// followed or this moment is outside them, else whether a camera was found.
fn turn_to_tracks(memory: &impl Memory, base: u64, list: &[Camera]) -> Option<bool> {
    let tracks = tracks()?;
    let got = CONTROL.get()?;
    let t = got.elapsed().as_secs_f64();
    if t > tracks.cam.last()?.0 {
        FOLLOW_BACK.store(false, Ordering::Relaxed);
        return None;
    }
    let (camera, yaw, pitch) = found_camera(memory, base, list)?;
    let (Some(y), Some(p)) = (resolve(memory, base, &camera.yaw), resolve(memory, base, &camera.pitch)) else { return Some(false) };
    let (at, turn) = follow_turn(tracks, t, position(memory, base, camera));
    let (new_yaw, new_pitch) = traced(&tracks.cam, at.min(tracks.cam.last()?.0), camera.wrap)?;
    let new_yaw = if turn == 0.0 { new_yaw } else { wrapped(new_yaw + turn) };
    let (lo, hi) = (camera.pitch_min.min(camera.pitch_max), camera.pitch_max.max(camera.pitch_min));
    let new_pitch = new_pitch.clamp(lo, hi);
    if (new_yaw, new_pitch) != (yaw, pitch) {
        memory.write(y, &new_yaw.to_le_bytes());
        memory.write(p, &new_pitch.to_le_bytes());
        let mut by_yaw = new_yaw - yaw;
        if camera.wrap > 0.0 {
            let half = camera.wrap / 2.0;
            by_yaw = (by_yaw + half).rem_euclid(camera.wrap) - half;
        }
        shift(memory, base, &camera.yaw_copies, by_yaw, camera.wrap);
        shift(memory, base, &camera.pitch_copies, new_pitch - pitch, 0.0);
    }
    Some(true)
}

/// Writes the first found camera to the trace's heading now; `None` when
/// the trace does not hold this moment, else whether a camera was found.
fn turn_to_trace(memory: &impl Memory, base: u64, list: &[Camera]) -> Option<bool> {
    let points = trace()?;
    let t = super::route::route_now_us()? as f64 / 1e6;
    for camera in list {
        if !present(memory, base, camera) {
            continue;
        }
        let (Some(y), Some(p)) = (resolve(memory, base, &camera.yaw), resolve(memory, base, &camera.pitch)) else {
            continue;
        };
        let (Some(yaw), Some(pitch)) = (read_f32(memory, y), read_f32(memory, p)) else {
            continue;
        };
        let (new_yaw, new_pitch) = traced(points, t, camera.wrap)?;
        let turn = steered(memory, base, camera, points, t);
        let new_yaw = if turn == 0.0 { new_yaw } else { wrapped(new_yaw + turn) };
        let (lo, hi) = (camera.pitch_min.min(camera.pitch_max), camera.pitch_max.max(camera.pitch_min));
        let new_pitch = new_pitch.clamp(lo, hi);
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::info!(route_s = t, yaw, new_yaw, "route camera trace: followed from here"));
        if (new_yaw, new_pitch) != (yaw, pitch) {
            memory.write(y, &new_yaw.to_le_bytes());
            memory.write(p, &new_pitch.to_le_bytes());
            let mut by_yaw = new_yaw - yaw;
            if camera.wrap > 0.0 {
                let half = camera.wrap / 2.0;
                by_yaw = (by_yaw + half).rem_euclid(camera.wrap) - half;
            }
            shift(memory, base, &camera.yaw_copies, by_yaw, camera.wrap);
            shift(memory, base, &camera.pitch_copies, new_pitch - pitch, 0.0);
        }
        return Some(true);
    }
    None
}

/// At the title's pad read: the counts since the last one go to the cameras.
pub(super) fn apply() {
    let Some(cameras) = CAMERAS.get() else {
        return;
    };
    if cameras.list.is_empty() {
        return;
    }
    let counts = (DX.swap(0, Ordering::Relaxed), DY.swap(0, Ordering::Relaxed));
    #[cfg(windows)]
    if READY.get().is_none()
        && trace().is_some()
        && heading(&host::Own, cameras.base, &cameras.list).is_some_and(|(yaw, _)| yaw != 0.0)
        && READY.set(std::time::Instant::now()).is_ok()
    {
        tracing::info!(route_s = super::route::route_now_us().map(|us| us as f64 / 1e6), "route camera trace: the gameplay camera came up (its yaw read other than 0)");
    }
    // The player's first control, where it is asked for (recording, or a
    // replay that follows recorded tracks): nothing more is read otherwise.
    #[cfg(windows)]
    if CONTROL.get().is_none()
        && (super::route::recording() || following())
        && heading(&host::Own, cameras.base, &cameras.list).is_some_and(|(yaw, _)| yaw != 0.0)
    {
        let now = std::time::Instant::now();
        if CONTROL.set(now).is_ok() {
            tracing::info!("route: the player has control (the gameplay camera came up)");
            super::route::record_control(now);
        }
    }
    #[cfg(windows)]
    if super::route::recording() {
        record_tracks(&host::Own, cameras.base, &cameras.list);
    }
    #[cfg(windows)]
    let found = if let Some(found) = turn_to_tracks(&host::Own, cameras.base, &cameras.list) {
        found
    } else if let Some(found) = turn_to_trace(&host::Own, cameras.base, &cameras.list) {
        found
    } else if absolute() {
        turn_to(&host::Own, cameras.base, &cameras.list, counts, cameras.scale, cameras.invert_y)
    } else {
        turn(&host::Own, cameras.base, &cameras.list, counts, cameras.scale, cameras.invert_y)
    };
    #[cfg(not(windows))]
    let found = false;
    #[cfg(feature = "diagnostics")]
    {
        let into = if found { &TURNED } else { &DROPPED };
        into[0].fetch_add(i64::from(counts.0), Ordering::Relaxed);
        into[1].fetch_add(i64::from(counts.1), Ordering::Relaxed);
    }
    if LIVE.swap(found, Ordering::Relaxed) != found {
        tracing::info!(found, "mouse camera");
    }
    #[cfg(all(windows, feature = "diagnostics"))]
    note_heading(|| heading(&host::Own, cameras.base, &cameras.list));
}

#[cfg(windows)]
mod host {
    use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) struct Own;

    impl super::Memory for Own {
        fn read(&self, at: u64, into: &mut [u8]) -> bool {
            let mut done = 0;
            // SAFETY: the kernel checks the source; `into` is ours.
            let ok = unsafe {
                ReadProcessMemory(
                    GetCurrentProcess(),
                    at as *const _,
                    into.as_mut_ptr().cast(),
                    into.len(),
                    &mut done,
                )
            };
            ok != 0 && done == into.len()
        }
        fn write(&self, at: u64, from: &[u8]) -> bool {
            let mut done = 0;
            // SAFETY: the kernel checks the target, which a chain found.
            let ok = unsafe {
                WriteProcessMemory(
                    GetCurrentProcess(),
                    at as *const _,
                    from.as_ptr().cast(),
                    from.len(),
                    &mut done,
                )
            };
            ok != 0 && done == from.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use super::*;

    /// Bytes at addresses; everything else unreadable.
    #[derive(Default)]
    struct Fake(RefCell<BTreeMap<u64, u8>>);

    impl Fake {
        fn put(&self, at: u64, bytes: &[u8]) {
            for (i, b) in bytes.iter().enumerate() {
                self.0.borrow_mut().insert(at + i as u64, *b);
            }
        }
        fn f32(&self, at: u64) -> f32 {
            read_f32(self, at).unwrap()
        }
    }

    impl Memory for Fake {
        fn read(&self, at: u64, into: &mut [u8]) -> bool {
            let m = self.0.borrow();
            for (i, b) in into.iter_mut().enumerate() {
                match m.get(&(at + i as u64)) {
                    Some(v) => *b = *v,
                    None => return false,
                }
            }
            true
        }
        fn write(&self, at: u64, from: &[u8]) -> bool {
            let mut b = vec![0; from.len()];
            self.read(at, &mut b) && {
                self.put(at, from);
                true
            }
        }
    }

    fn camera() -> Camera {
        Camera {
            name: "test".into(),
            yaw: vec![0x100, 0x10],
            pitch: vec![0x100, 0x14],
            yaw_per_count: 0.001,
            pitch_per_count: -0.001,
            pitch_min: -1.0,
            pitch_max: 1.0,
            wrap: std::f32::consts::TAU,
            object: Vec::new(),
            kind: None,
            yaw_copies: Vec::new(),
            pitch_copies: Vec::new(),
            position: Vec::new(),
        }
    }

    #[test]
    fn counts_turn_by_a_fixed_angle_each() {
        let c = camera();
        let (yaw, pitch) = turned(&c, (0.5, 0.2), (100, 50), 1.0, false);
        assert!((yaw - 0.6).abs() < 1e-6 && (pitch - 0.15).abs() < 1e-6);
        // Sensitivity scales; inverted, down looks up.
        let (yaw, pitch) = turned(&c, (0.5, 0.2), (100, 50), 2.0, true);
        assert!((yaw - 0.7).abs() < 1e-6 && (pitch - 0.3).abs() < 1e-6);
    }

    #[test]
    fn yaw_wraps_and_pitch_is_clamped() {
        let c = camera();
        let (yaw, pitch) = turned(&c, (3.1, 0.9), (100, -1000), 1.0, false);
        assert!((yaw - (3.2 - std::f32::consts::TAU)).abs() < 1e-5, "{yaw}");
        assert_eq!(pitch, 1.0);
        let degrees = Camera {
            wrap: 360.0,
            yaw_per_count: 1.0,
            ..c
        };
        let (yaw, _) = turned(&degrees, (170.0, 0.0), (20, 0), 1.0, false);
        assert!((yaw + 170.0).abs() < 1e-4, "{yaw}");
    }

    #[test]
    fn chains_follow_pointers_and_fail_safely() {
        let m = Fake::default();
        let base = 0x1000_0000;
        m.put(base + 0x100, &0x2000_0000_u64.to_le_bytes());
        m.put(0x2000_0010, &0.5_f32.to_le_bytes());
        m.put(0x2000_0014, &0.0_f32.to_le_bytes());
        assert_eq!(resolve(&m, base, &[0x100, 0x10]), Some(0x2000_0010));
        assert_eq!(resolve(&m, base, &[0x100]), Some(base + 0x100));
        assert_eq!(resolve(&m, base, &[0x200, 0x10]), None);
        assert!(turn(&m, base, &[camera()], (100, 100), 1.0, false));
        assert!((m.f32(0x2000_0010) - 0.6).abs() < 1e-6);
        assert!((m.f32(0x2000_0014) + 0.1).abs() < 1e-6);
        // A null pointer is no camera, and nothing is written.
        m.put(base + 0x100, &0_u64.to_le_bytes());
        assert!(!turn(&m, base, &[camera()], (100, 100), 1.0, false));
    }

    #[test]
    fn kinds_are_checked_and_copies_move_together() {
        let m = Fake::default();
        let base = 0x1000_0000;
        m.put(base + 0x100, &0x2000_0000_u64.to_le_bytes());
        m.put(0x2000_0000, &(base + 0x5000).to_le_bytes());
        for (at, v) in [(0x10, 3.1_f32), (0x14, 0.9), (0x20, 3.0), (0x24, 0.5)] {
            m.put(0x2000_0000 + at, &v.to_le_bytes());
        }
        let c = Camera {
            object: vec![0x100, 0],
            kind: Some(0x5000),
            yaw_copies: vec![vec![0x100, 0x20], vec![0x300, 0x20]],
            pitch_copies: vec![vec![0x100, 0x24]],
            ..camera()
        };
        // 100 counts: +0.1 yaw (wrapping past π), pitch up to its limit: +0.1 of the asked 0.2.
        assert!(turn(&m, base, std::slice::from_ref(&c), (100, -200), 1.0, false));
        assert!((m.f32(0x2000_0010) - (3.2 - std::f32::consts::TAU)).abs() < 1e-5);
        assert!((m.f32(0x2000_0020) - 3.1).abs() < 1e-5);
        assert!((m.f32(0x2000_0014) - 1.0).abs() < 1e-6);
        assert!((m.f32(0x2000_0024) - 0.6).abs() < 1e-5);
        // Another kind of object there: not this camera, nothing written.
        m.put(0x2000_0000, &(base + 0x6000).to_le_bytes());
        assert!(!turn(&m, base, std::slice::from_ref(&c), (100, 0), 1.0, false));
        assert!((m.f32(0x2000_0020) - 3.1).abs() < 1e-5);
    }

    /// A walk along x at 1 m/s whose bearing is the yaw (sign +1).
    fn straight() -> Tracks {
        Tracks {
            cam: (0..20).map(|i| (f64::from(i), 0.0, 0.0)).collect(),
            pos: (0..20).map(|i| (f64::from(i), [i as f32 * 0.9, 0.0, 0.0])).collect(),
            sign: 1.0,
        }
    }

    fn follow() -> Follow {
        FOLLOW_START
    }

    #[test]
    fn walk_sign_is_read_from_the_tracks() {
        let t = straight();
        let (sign, plus, _, steps) = walk_sign(&t.pos, &t.cam);
        assert_eq!((sign, steps), (1.0, 19));
        assert!(plus < 1e-3, "{plus}");
        // The bearing against the yaw: -1.
        let cam: Vec<(f64, f32, f32)> = (0..10).map(|i| (f64::from(i), 0.2 * i as f32, 0.0)).collect();
        let mut p = [0.0f32; 3];
        let mut pos = vec![(0.0, p)];
        for i in 0..9 {
            let b = -0.2 * (i as f32 + 0.5);
            p = [p[0] + b.cos(), p[1] + b.sin(), 0.0];
            pos.push((f64::from(i + 1), p));
        }
        assert_eq!(walk_sign(&pos, &cam).0, -1.0);
    }

    #[test]
    fn off_the_track_turns_toward_it() {
        let t = straight();
        let mut f = follow();
        // On the track: no turn; the heading a little past the nearest point.
        let (head, turn, back) = follow_step(&mut f, &t, 2.0, [1.8, 0.0, 0.0]);
        assert!((head - (2.0 + HEAD_LEAD_S)).abs() < 1e-3 && turn == 0.0 && !back, "{head} {turn}");
        // 2 m to its right (y -2): turned left (toward +y), at most FOLLOW_MOST_FAR.
        let mut f = follow();
        let (_, turn, back) = follow_step(&mut f, &t, 2.0, [1.8, -2.0, 0.0]);
        assert!(turn > 0.6 && turn <= FOLLOW_MOST_FAR && !back, "{turn}");
    }

    #[test]
    fn a_player_behind_takes_the_heading_where_it_is() {
        let t = straight();
        let mut f = follow();
        // At the recording's 3 s spot while the replay is at 4.5 s.
        let (head, _, _) = follow_step(&mut f, &t, 4.5, [2.7, 0.0, 0.0]);
        assert!((head - (3.0 + HEAD_LEAD_S)).abs() < 1e-3, "{head}");
    }

    #[test]
    fn a_stop_backs_off_and_turns_aside_only_when_the_recording_walked() {
        let t = straight();
        let mut f = follow();
        let at = [3.0, 0.0, 0.0];
        assert!(!follow_step(&mut f, &t, 3.0, at).2);
        // A second later the player has not moved while the recording walked 0.9 m.
        let (_, turn, back) = follow_step(&mut f, &t, 4.05, at);
        assert!(back && turn.abs() == ASIDE, "{turn} {back}");
        let (_, turn, back) = follow_step(&mut f, &t, 4.05 + BACK_S + 0.1, at);
        assert!(!back && turn.abs() == ASIDE);
        assert_eq!(f.stalls, 1);
        // The recording standing still too (its first seconds): no stall.
        let still = Tracks {
            cam: t.cam.clone(),
            pos: (0..20).map(|i| (f64::from(i), [if i < 5 { 0.0 } else { (i - 5) as f32 }, 0.0, 0.0])).collect(),
            sign: 1.0,
        };
        let mut f = follow();
        follow_step(&mut f, &still, 3.0, [0.0, 0.0, 0.0]);
        assert!(!follow_step(&mut f, &still, 4.1, [0.0, 0.0, 0.0]).2);
        assert_eq!(f.stalls, 0);
    }

    #[test]
    fn track_lines_read_back() {
        let dir = std::env::temp_dir().join(format!("victory-track-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.cam");
        std::fs::write(&path, "# x\n1.250 0.5 -0.1\n0.000 0.4 0\nbad\n").unwrap();
        let lines = read_lines::<2>(&path).unwrap();
        assert_eq!(lines, vec![(0.0, [0.4, 0.0]), (1.25, [0.5, -0.1])]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn our_own_memory_is_read_and_written_through_the_kernel() {
        #[cfg(windows)]
        {
            let mut value = 1.5_f32;
            let at = &raw mut value as u64;
            let own = host::Own;
            assert_eq!(read_f32(&own, at), Some(1.5));
            assert!(own.write(at, &2.5_f32.to_le_bytes()));
            // SAFETY: written through the kernel just now.
            assert_eq!(unsafe { std::ptr::read_volatile(&raw const value) }, 2.5);
            assert_eq!(read_f32(&own, 8), None);
        }
    }
}
