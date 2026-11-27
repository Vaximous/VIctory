//! Diagnostics: the title's waits on what the card's work tells it
//! (`VICTORY_GPU_WAITS`, on unless `=0`; diagnostics builds only).
//!
//! Fences and bottom-of-pipe writes ("labels") are set by the thread that
//! lands flights ([`crate::host`]'s relay) once the card has run the work
//! ahead of them and what it wrote is back. A title thread that waits for
//! one stands for the card's work and that landing: if it does so between
//! frames, the frame is our preparing and the card's work in a row.
//!
//! Waited on, as the title can: an event it gave a fence for a value not yet
//! reached (`SetEventOnCompletion`), a fence's value or a label's address
//! (`WaitOnAddress`), or a fence's value read again and again
//! (`GetCompletedValue`, counted only). The title's `WaitForSingleObject(Ex)`,
//! `WaitForMultipleObjects(Ex)` and `WaitOnAddress` are bound to wrappers that
//! time a wait only when its object is one of those. A line a second:
//! [`line`].

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

type Handle = *mut c_void;

/// Whether the title's waits are looked at.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_WAITS").is_none_or(|v| v != "0"))
}

/// What a wait was on.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Object {
    /// A fence (its address), for an event or its value.
    Fence(usize),
    /// A label: the address of a bottom-of-pipe write.
    Label(usize),
}

impl Object {
    fn name(self) -> String {
        match self {
            Object::Fence(at) => format!("fence {at:#x}"),
            Object::Label(at) => format!("label {at:#x}"),
        }
    }
}

#[derive(Default)]
struct Known {
    /// Events given for a fence value not reached yet: the fence, the value.
    events: HashMap<usize, (usize, u64)>,
    /// The address of each fence's value: the fence.
    values: HashMap<usize, usize>,
    /// Label addresses.
    labels: std::collections::HashSet<usize>,
    /// The title's signals of a fence not set yet: when it called.
    signals: HashMap<(usize, u64), Instant>,
    /// Labels a submission writes, not written yet: when it was prepared.
    queued: HashMap<usize, Instant>,
}

static KNOWN: Mutex<Option<Known>> = Mutex::new(None);
/// Whether any event is pending: most waits look no further.
static EVENTS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static INSIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `f`, unless this thread is in it already: `None`. The wrappers of the
/// waits look up and note under locks of ours, and a lock waited for is a
/// wait on an address, wrapped too where our own imports are (a timeline):
/// without this the thread went round until its stack overflowed (a suite
/// that ran a timeline's tests before ones with many threads on one lock).
fn inside<T>(f: impl FnOnce() -> T) -> Option<T> {
    if INSIDE.get() {
        return None;
    }
    INSIDE.set(true);
    let result = f();
    INSIDE.set(false);
    Some(result)
}

fn known<T>(f: impl FnOnce(&mut Known) -> T) -> T {
    f(KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Known::default))
}

/// Since the last line: waits, microseconds, the longest; polls that found a
/// fence below a value signalled; fences and labels set, microseconds from the
/// title's signal (a label: the submission's preparing) to the set, the
/// longest.
static STATS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
const WAITS: usize = 0;
const WAIT_US: usize = 1;
const WAIT_MOST: usize = 2;
const POLLS: usize = 3;
const FENCES: usize = 4;
const FENCE_US: usize = 5;
const FENCE_MOST: usize = 6;
const LABELS: usize = 7;
const LABEL_US: usize = 8;

/// Since the last line, by object: waits, microseconds; and the longest wait's
/// object and how long after its signal it was set.
static BY: Mutex<Option<(HashMap<Object, (u64, u64)>, Option<(u64, Object)>)>> = Mutex::new(None);
/// Each fence's latest set: the value, microseconds from the signal.
static LAST_SET: Mutex<Option<HashMap<Object, u64>>> = Mutex::new(None);

/// A fence was made: its value lies at `value`.
pub(crate) fn fence_made(fence: usize, value: usize) {
    if on() {
        known(|k| k.values.insert(value, fence));
    }
}

/// The title asked `event` to be set when `fence` reaches `value`.
pub(crate) fn event_asked(event: usize, fence: usize, value: u64) {
    if on() && known(|k| k.events.insert(event, (fence, value))).is_none() {
        EVENTS.fetch_add(1, Relaxed);
    }
}

/// `event` was set by its fence.
pub(crate) fn event_set(event: usize) {
    if on() && known(|k| k.events.remove(&event)).is_some() {
        EVENTS.fetch_sub(1, Relaxed);
    }
}

/// The title signals `fence` to `value` behind its submissions.
pub(crate) fn signal_called(fence: usize, value: u64) {
    if on() {
        known(|k| {
            k.signals.entry((fence, value)).or_insert_with(Instant::now);
        });
    }
}

/// `fence` was set to `value`.
pub(crate) fn fence_set(fence: usize, value: u64) {
    if !on() {
        return;
    }
    let called = known(|k| {
        let mut first: Option<Instant> = None;
        k.signals.retain(|&(f, v), &mut at| {
            let done = f == fence && v <= value;
            if done {
                first = Some(first.map_or(at, |f| f.min(at)));
            }
            !done
        });
        first
    });
    if let Some(called) = called {
        let us = called.elapsed().as_micros() as u64;
        STATS[FENCES].fetch_add(1, Relaxed);
        STATS[FENCE_US].fetch_add(us, Relaxed);
        STATS[FENCE_MOST].fetch_max(us, Relaxed);
        LAST_SET.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(Object::Fence(fence), us);
    }
}

/// A submission prepared now writes the labels `at`.
pub(crate) fn labels_queued(at: impl Iterator<Item = usize>) {
    if !on() {
        return;
    }
    let now = Instant::now();
    known(|k| {
        for at in at {
            k.labels.insert(at);
            k.queued.entry(at).or_insert(now);
        }
    });
}

/// The label at `at` was written.
pub(crate) fn label_set(at: usize) {
    if !on() {
        return;
    }
    let queued = known(|k| {
        k.labels.insert(at);
        k.queued.remove(&at)
    });
    if let Some(queued) = queued {
        let us = queued.elapsed().as_micros() as u64;
        STATS[LABELS].fetch_add(1, Relaxed);
        STATS[LABEL_US].fetch_add(us, Relaxed);
        LAST_SET.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(Object::Label(at), us);
    }
}

/// The title read `fence` at `value` while a signal of a higher value was
/// asked for.
pub(crate) fn polled(fence: usize, value: u64) {
    if on() && known(|k| k.signals.keys().any(|&(f, v)| f == fence && v > value)) {
        STATS[POLLS].fetch_add(1, Relaxed);
    }
}

/// What the title waits on in `objects`, if it is one of ours.
pub(crate) fn event_object(objects: &[usize]) -> Option<Object> {
    if EVENTS.load(Relaxed) == 0 {
        return None;
    }
    inside(|| known(|k| objects.iter().find_map(|o| k.events.get(o).map(|&(fence, _)| Object::Fence(fence))))).flatten()
}

pub(crate) fn address_object(at: usize) -> Option<Object> {
    inside(|| {
        known(|k| {
            if let Some(&fence) = k.values.get(&at) {
                Some(Object::Fence(fence))
            } else if k.labels.contains(&at) {
                Some(Object::Label(at))
            } else {
                None
            }
        })
    })
    .flatten()
}

fn waited(object: Object, since: Instant) {
    let us = since.elapsed().as_micros() as u64;
    STATS[WAITS].fetch_add(1, Relaxed);
    STATS[WAIT_US].fetch_add(us, Relaxed);
    STATS[WAIT_MOST].fetch_max(us, Relaxed);
    let mut by = BY.lock().unwrap_or_else(|e| e.into_inner());
    let (all, longest) = by.get_or_insert_with(|| (HashMap::new(), None));
    let e = all.entry(object).or_default();
    *e = (e.0 + 1, e.1 + us);
    if longest.is_none_or(|(most, _)| us > most) {
        *longest = Some((us, object));
    }
}

/// Runs `call`, a wait of the title's, timed if it is on `object`.
pub(crate) fn timed<T>(object: Option<Object>, call: impl FnOnce() -> T) -> T {
    let Some(object) = object else {
        return call();
    };
    let since = Instant::now();
    let result = call();
    // SAFETY: plain queries of this thread's own.
    unsafe {
        use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
        let error = GetLastError();
        inside(|| waited(object, since));
        SetLastError(error);
    }
    result
}

/// The real functions, by the wrappers' order.
static REAL: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
const NAMES: [&str; 5] =
    ["WaitForSingleObject", "WaitForSingleObjectEx", "WaitForMultipleObjects", "WaitForMultipleObjectsEx", "WaitOnAddress"];

fn real<F: Copy>(i: usize) -> F {
    let at = REAL[i].load(Relaxed);
    // SAFETY: the address of the function of that name the import had, of
    // the type `F` the wrapper names.
    unsafe { std::mem::transmute_copy::<usize, F>(&at) }
}

unsafe extern "system" fn wait_one(object: Handle, ms: u32) -> u32 {
    let real: unsafe extern "system" fn(Handle, u32) -> u32 = real(0);
    // SAFETY: the title's own call.
    timed(event_object(&[object as usize]), || unsafe { real(object, ms) })
}

unsafe extern "system" fn wait_one_ex(object: Handle, ms: u32, alertable: i32) -> u32 {
    let real: unsafe extern "system" fn(Handle, u32, i32) -> u32 = real(1);
    // SAFETY: the title's own call.
    timed(event_object(&[object as usize]), || unsafe { real(object, ms, alertable) })
}

/// The handles of a `WaitForMultipleObjects` call.
pub(crate) fn handles(count: u32, objects: *const Handle) -> Vec<usize> {
    if objects.is_null() || count == 0 || EVENTS.load(Relaxed) == 0 {
        return Vec::new();
    }
    // SAFETY: the caller passes `count` handles.
    unsafe { std::slice::from_raw_parts(objects, count.min(64) as usize) }.iter().map(|&h| h as usize).collect()
}

unsafe extern "system" fn wait_many(count: u32, objects: *const Handle, all: i32, ms: u32) -> u32 {
    let real: unsafe extern "system" fn(u32, *const Handle, i32, u32) -> u32 = real(2);
    // SAFETY: the title's own call.
    timed(event_object(&handles(count, objects)), || unsafe { real(count, objects, all, ms) })
}

unsafe extern "system" fn wait_many_ex(count: u32, objects: *const Handle, all: i32, ms: u32, alertable: i32) -> u32 {
    let real: unsafe extern "system" fn(u32, *const Handle, i32, u32, i32) -> u32 = real(3);
    // SAFETY: the title's own call.
    timed(event_object(&handles(count, objects)), || unsafe { real(count, objects, all, ms, alertable) })
}

unsafe extern "system" fn wait_on_address(at: Handle, compare: Handle, size: usize, ms: u32) -> i32 {
    let real: unsafe extern "system" fn(Handle, Handle, usize, u32) -> i32 = real(4);
    // SAFETY: the title's own call.
    timed(address_object(at as usize), || unsafe { real(at, compare, size, ms) })
}

/// What the title's import `name`, found at `address`, is bound to: the
/// address itself, or our wrapper of it.
pub fn import(name: &str, address: usize) -> usize {
    if !on() {
        return address;
    }
    let Some(i) = NAMES.iter().position(|n| *n == name) else {
        return address;
    };
    if REAL[i].load(Relaxed) == 0 {
        REAL[i].store(address, Relaxed);
    }
    if REAL[i].load(Relaxed) != address {
        // Another module's function of that name: left as it is.
        return address;
    }
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Relaxed) {
        tracing::info!("the title's waits on what the card's work sets are timed (VICTORY_GPU_WAITS=0: not)");
    }
    match i {
        0 => wait_one as *const () as usize,
        1 => wait_one_ex as *const () as usize,
        2 => wait_many as *const () as usize,
        3 => wait_many_ex as *const () as usize,
        _ => wait_on_address as *const () as usize,
    }
}

/// The line of a second: `frames` the frames in it.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|s| s.swap(0, Relaxed)).collect();
    let (by, longest) = BY.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let f = frames.max(1.0);
    let ms = |us: u64| us as f64 / 1e3;
    let mut top: Vec<(Object, (u64, u64))> = by.into_iter().collect();
    top.sort_unstable_by_key(|t| std::cmp::Reverse(t.1.1));
    let top: Vec<String> = top
        .iter()
        .take(3)
        .map(|(o, (n, us))| format!("{} {:.1} ms in {n}", o.name(), ms(*us)))
        .collect();
    let last = LAST_SET.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
    let longest = longest.map_or("none".to_string(), |(us, o)| {
        let set = last.get(&o).map_or("not seen set".to_string(), |s| format!("set {:.1} ms after its signal", ms(*s)));
        format!("{:.1} ms on {} ({set})", ms(us), o.name())
    });
    tracing::info!(
        "title waited {:.1} ms a frame on GPU signals in {:.1} waits (VICTORY_GPU_WAITS), longest {longest}; most: {}; \
         {:.1} polls a frame found a fence below a value signalled; fences set {:.1} ms after the title's signal on average \
         ({:.1} a frame, the longest {:.1} ms); labels written {:.1} ms after their submission was prepared on average ({:.1} a frame)",
        ms(s[WAIT_US]) / f,
        s[WAITS] as f64 / f,
        if top.is_empty() { "-".to_string() } else { top.join(", ") },
        s[POLLS] as f64 / f,
        ms(s[FENCE_US]) / s[FENCES].max(1) as f64,
        s[FENCES] as f64 / f,
        ms(s[FENCE_MOST]),
        ms(s[LABEL_US]) / s[LABELS].max(1) as f64,
        s[LABELS] as f64 / f,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wait inside a wrapper's own looking up (a lock of ours waited for)
    /// goes straight to the real wait: nothing is looked up again.
    #[test]
    fn a_wait_inside_the_looking_up_is_not_looked_up() {
        let at = 0x5EED_0000usize;
        known(|k| {
            k.labels.insert(at);
        });
        assert!(address_object(at).is_some());
        assert_eq!(inside(|| address_object(at).is_none()), Some(true));
        assert!(address_object(at).is_some());
        known(|k| {
            k.labels.remove(&at);
        });
    }

    /// A title thread waits on an event given for a fence value; another
    /// thread sets the fence: the wait is timed against that fence, and the
    /// set against the title's signal.
    #[test]
    fn a_wait_on_a_fence_event_is_timed_and_the_set_is_after_the_signal() {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};
        // SAFETY: a module every process has; a NUL-terminated name.
        let real = unsafe { GetProcAddress(GetModuleHandleA(c"kernel32.dll".as_ptr().cast()), c"WaitForSingleObject".as_ptr().cast()) }
            .expect("WaitForSingleObject") as usize;
        let wrapper = import("WaitForSingleObject", real);
        assert_ne!(wrapper, real, "wrapped");
        let wait: unsafe extern "system" fn(Handle, u32) -> u32 = unsafe { std::mem::transmute(wrapper) };
        // SAFETY: a plain auto-reset event of our own.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) } as usize;
        let fence = 0x5eed_0000usize;
        let before = STATS[WAITS].load(Relaxed);
        signal_called(fence, 7);
        event_asked(event, fence, 7);
        let setter = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            fence_set(fence, 7);
            event_set(event);
            // SAFETY: our event.
            unsafe { SetEvent(event as _) };
        });
        // SAFETY: our event; the wrapper calls the real function.
        assert_eq!(unsafe { wait(event as _, 5000) }, 0);
        setter.join().unwrap();
        assert!(STATS[WAITS].load(Relaxed) > before, "the wait was timed");
        assert!(STATS[FENCES].load(Relaxed) > 0, "the set was timed against the signal");
        // Not one of ours any more: not timed.
        let waits = STATS[WAITS].load(Relaxed);
        // SAFETY: our event, set again.
        unsafe { SetEvent(event as _) };
        assert_eq!(unsafe { wait(event as _, 0) }, 0);
        assert_eq!(STATS[WAITS].load(Relaxed), waits);
    }
}
