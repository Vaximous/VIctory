//! What the title's CPU reads of what shaders wrote, by guard pages: a
//! diagnostic count (`VICTORY_READ_PROBE=N`, off by default), and copies
//! the title is found not to read left on the card (`VICTORY_LEAVE_UNREAD`,
//! on unless `=0`).
//!
//! The fetch back of shader writes cost some 16 ms a frame (run ow19-move:
//! 42 MB a frame, half of it one 20 MB copy). What the title never reads on
//! its CPU need not come back, but the guards see writes only.
//!
//! A page is armed by making it a guard page on top of the protection it
//! has, at the title's address and at the other views of the same pages.
//! The first touch of it raises a guard fault, which Windows ends by taking
//! the guard off; the fault is handled here ([`fault`]) and the access goes
//! on. Who touched it is told by where the faulting instruction is: in our
//! copies (`crate::safecopy`) or our image, ours; anywhere else (the title's
//! image or a system library it calls), the title's.
//!
//! # The diagnostic (run ow21-probe)
//! Every `N`th frame the pages a landing put bytes into (of copies of at
//! least `VICTORY_READ_PROBE_MIN_KB`, 1024) are armed; the line gives per
//! copy the title's reads (by place) and writes, ours, and the pages still
//! armed. The 182.5 MB heap was read at one place, 240-560 pages a line; the
//! 21 MB upscaler buffer (fetched whole every frame, never sent up) was
//! never read.
//!
//! # Leaving unread copies on the card
//! A copy in the card's memory of at least 1 MiB whose shader writes are
//! fetched but which has sent nothing up for a while is a candidate. The
//! candidates take turns of `VICTORY_LEAVE_UNREAD_EVERY` frames (4), one at a
//! time; in its turn a candidate's landed pages are armed. A candidate whose
//! pages were armed in `VICTORY_LEAVE_UNREAD_FRAMES` frames (30) with no
//! touch of the title's is left on the card: its whole range is armed, and
//! from the next submission on its shader writes are neither declared nor
//! fetched (decided under the submission lock, so a submission does one or
//! the other for all of it), and its refreshes are skipped (the title wrote
//! nothing: a write would fault).
//!
//! The title's first touch of a copy left on the card (read or write)
//! waits in the fault until the copy is fetched back whole and landed, by a
//! thread of ours under the submission lock (no submission is half
//! prepared; every one before is on the queue ahead of the fetch). Then it
//! is read for good: fetched as before. A touch of ours (another host copy of
//! the same bytes refreshing, say) has it fetched the same way without
//! waiting. A landing's own writes (of flights that declared it before it
//! was left) are no touch: their pages are armed again after.
//!
//! What it does not see, and risks: reads by the kernel (a `ReadFile` or
//! `WriteFile` of the title's on such pages fails instead of faulting); a
//! page whose protection the guards changed (they look through the guard
//! bit, but a protection they set drops it: the copies left are armed again
//! once a frame); another host copy of the same bytes made on the card's
//! side from title memory before the fetch (the bytes come up again once the
//! fetch lands, which notes them as written).

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock, Weak};

use crate::shadow::Shadow;

const PAGE: usize = 4096;
/// The places a copy's reads are counted at, apart.
const SITES: usize = 6;

/// Every how many frames the landings arm their pages (the diagnostic); 0,
/// off.
pub(crate) fn every() -> u64 {
    static EVERY: OnceLock<u64> = OnceLock::new();
    *EVERY.get_or_init(|| {
        let every = std::env::var("VICTORY_READ_PROBE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        if every != 0 {
            ON.store(true, Ordering::Relaxed);
        }
        every
    })
}

/// Whether copies the title does not read stay on the card
/// (`VICTORY_LEAVE_UNREAD`, on unless `=0`).
pub(crate) fn leave_on() -> bool {
    static LEAVE: OnceLock<bool> = OnceLock::new();
    *LEAVE.get_or_init(|| {
        let on = std::env::var_os("VICTORY_LEAVE_UNREAD").is_none_or(|v| v != "0");
        if on {
            ON.store(true, Ordering::Relaxed);
        }
        on
    })
}

fn leave_every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| env_u64("VICTORY_LEAVE_UNREAD_EVERY", 4).max(1))
}

fn leave_window() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| env_u64("VICTORY_LEAVE_UNREAD_FRAMES", 30).max(1))
}

/// Frames a candidate must have sent nothing up for.
const QUIET_FRAMES: u64 = 120;
/// The smallest copy left on the card.
const LEAVE_MIN: u64 = 1 << 20;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Whether the probe is on, without a look at the environment (for the fault
/// handler).
static ON: AtomicBool = AtomicBool::new(false);

fn min_bytes() -> u64 {
    static MIN: OnceLock<u64> = OnceLock::new();
    *MIN.get_or_init(|| env_u64("VICTORY_READ_PROBE_MIN_KB", 1024) * 1024)
}

/// Not judged yet, or not a candidate (counted by the diagnostic only).
const NEVER: u8 = 0;
const LEARNING: u8 = 1;
/// On the card; its shader writes are not fetched.
const LEFT: u8 = 2;
/// Touched while on the card: being fetched.
const FETCHING: u8 = 3;
/// Read (or not to be left): fetched as before, for good.
const READ: u8 = 4;

/// One copy's state and counts.
pub(crate) struct Probed {
    start: usize,
    len: usize,
    state: AtomicU8,
    judged: AtomicBool,
    /// Live: cleared when its title memory is freed.
    alive: AtomicBool,
    /// The copy, and held while it is on the card (a copy dropped then is
    /// fetched first).
    shadow: Mutex<Weak<Shadow>>,
    held: Mutex<Option<Arc<Shadow>>>,
    /// Frames its pages were armed in a turn, and the last of them (+1).
    armings: AtomicU64,
    last_armed: AtomicU64,
    /// The frame (+1) it last sent something up.
    sent: AtomicU64,
    /// Touched by the title while learning.
    touched: AtomicBool,
    /// Since the last line: pages armed, guard faults (the title's reads and
    /// writes, ours), where the title's reads were.
    armed: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
    ours: AtomicU64,
    sites: [(AtomicUsize, AtomicU64); SITES],
    elsewhere: AtomicU64,
    /// Armed by the diagnostic (kept across lines).
    ever: AtomicBool,
}

impl Probed {
    fn new(start: usize, len: usize) -> Probed {
        Probed {
            start,
            len,
            state: AtomicU8::new(NEVER),
            judged: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            shadow: Mutex::new(Weak::new()),
            held: Mutex::new(None),
            armings: AtomicU64::new(0),
            last_armed: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            touched: AtomicBool::new(false),
            armed: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            ours: AtomicU64::new(0),
            sites: std::array::from_fn(|_| (AtomicUsize::new(0), AtomicU64::new(0))),
            elsewhere: AtomicU64::new(0),
            ever: AtomicBool::new(false),
        }
    }

    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    /// Whether its shader writes stay on the card now.
    pub(crate) fn left(&self) -> bool {
        matches!(self.state(), LEFT | FETCHING)
    }

    fn read_at(&self, site: usize) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        for (at, n) in &self.sites {
            match at.compare_exchange(0, site, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    n.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(there) if there == site => {
                    n.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {}
            }
        }
        self.elsewhere.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Table {
    copies: Vec<Arc<Probed>>,
    /// Address ranges armed pages lie in (a copy's own, and those of other
    /// views of its pages), sorted by start, each with its copy (an index of
    /// `copies`).
    spans: Vec<(usize, usize, usize)>,
}

impl Table {
    fn add(&mut self, probed: Arc<Probed>) -> usize {
        let (start, len) = (probed.start, probed.len);
        self.copies.push(probed);
        let i = self.copies.len() - 1;
        self.span(start, start + len, i);
        i
    }

    fn index(&self, probed: &Arc<Probed>) -> Option<usize> {
        self.copies.iter().position(|c| Arc::ptr_eq(c, probed))
    }

    fn span(&mut self, from: usize, to: usize, copy: usize) {
        let at = self.spans.partition_point(|s| s.0 < from);
        if let Some(s) = self.spans[at..].iter_mut().take_while(|s| s.0 == from).find(|s| s.1 == to) {
            // (A copy made again over the same memory: the live one.)
            s.2 = copy;
            return;
        }
        self.spans.insert(at, (from, to, copy));
    }

    fn find(&self, at: usize) -> Option<&Arc<Probed>> {
        // The last live span beginning at or below `at` that holds it (they
        // may nest: a view's block inside an older copy's range). The 16
        // nearest first; then all of them, which a fault that found none
        // there must not miss.
        let end = self.spans.partition_point(|s| s.0 <= at);
        let live = |s: &&(usize, usize, usize)| at < s.1 && self.copies[s.2].alive.load(Ordering::Relaxed);
        let near = self.spans[..end].iter().rev().take(16).find(live);
        near.or_else(|| self.spans[..end].iter().rev().skip(16).find(live)).map(|s| &self.copies[s.2])
    }

    /// Whether any span, live or not, holds `at`: a page armed for a copy
    /// since let go of.
    fn armed_once(&self, at: usize) -> bool {
        let end = self.spans.partition_point(|s| s.0 <= at);
        self.spans[..end].iter().any(|s| at < s.1)
    }
}

static TABLE: RwLock<Table> = RwLock::new(Table { copies: Vec::new(), spans: Vec::new() });
/// Pages our own code touched while counted: armed again at the next arming
/// or line.
static AGAIN: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Candidates and copies on the card.
static CANDIDATES: Mutex<Vec<Arc<Probed>>> = Mutex::new(Vec::new());
/// The candidate whose turn it is (its address) and the frame the turn ends.
static TURN: AtomicUsize = AtomicUsize::new(0);
static TURN_END: AtomicU64 = AtomicU64::new(0);
static TURN_NEXT: AtomicUsize = AtomicUsize::new(0);
/// The frame (+1) the copies on the card were last armed again.
static REARMED: AtomicU64 = AtomicU64::new(0);
/// Copies to fetch, for the fetching thread; and its word that one is done.
static TO_FETCH: Mutex<Vec<Arc<Probed>>> = Mutex::new(Vec::new());
static FETCH_ASKED: Condvar = Condvar::new();
static FETCHED: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());
/// Copies to let go of, away from where they were found gone.
static DROPPED: Mutex<Vec<Arc<Shadow>>> = Mutex::new(Vec::new());
/// Since the last line: copies left on the card, fetched on a touch of the
/// title's, of ours, found read while learning.
static LEAVE: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

thread_local! {
    /// A landing of ours is writing title memory on this thread.
    static LANDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` as a landing: guard faults of copies on the card are no touch.
pub(crate) fn as_landing<T>(f: impl FnOnce() -> T) -> T {
    let was = LANDING.with(|l| l.replace(true));
    let out = f();
    LANDING.with(|l| l.set(was));
    out
}

/// The image our code is in: its start and end.
fn ours() -> (usize, usize) {
    static OURS: OnceLock<(usize, usize)> = OnceLock::new();
    *OURS.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
        };
        let mut module = std::ptr::null_mut();
        // SAFETY: an address in our image; the out pointer is valid.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                ours as *const () as *const u16,
                &mut module,
            )
        } != 0;
        if !found || module.is_null() {
            return (0, 0);
        }
        let base = module as usize;
        // SAFETY: a loaded image's headers: the offset of the NT headers at
        // 0x3c, and the image's size in their optional header (PE32+).
        let size = unsafe {
            let nt = base + std::ptr::read_unaligned((base + 0x3c) as *const u32) as usize;
            std::ptr::read_unaligned((nt + 0x18 + 0x38) as *const u32) as usize
        };
        (base, base + size)
    })
}

/// The copy's state, made the first time.
fn of(shadow: &Shadow) -> &Arc<Probed> {
    shadow.probe.get_or_init(|| {
        let (start, len) = shadow.span();
        let probed = Arc::new(Probed::new(start as usize, len as usize));
        TABLE.write().unwrap_or_else(|e| e.into_inner()).add(probed.clone());
        probed
    })
}

/// Under the submission lock, for a copy shaders write: whether its writes
/// stay on the card (neither declared nor fetched). Judges it a candidate
/// the first time.
pub(crate) fn left(shadow: &Arc<Shadow>) -> bool {
    if !leave_on() {
        return false;
    }
    let p = of(shadow);
    if !p.judged.swap(true, Ordering::Relaxed) && shadow.carded() && p.len as u64 >= LEAVE_MIN {
        *p.shadow.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(shadow);
        let _ = p.state.compare_exchange(NEVER, LEARNING, Ordering::AcqRel, Ordering::Relaxed);
        CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).push(p.clone());
    }
    p.left()
}

/// Whether a copy's shader writes stay on the card now (no refresh is made
/// of it then).
pub(crate) fn left_now(shadow: &Shadow) -> bool {
    shadow.probe.get().is_some_and(|p| p.left())
}

/// Something of the copy went up to the card.
pub(crate) fn sent_up(shadow: &Shadow) {
    if leave_on() {
        of(shadow).sent.store(crate::frame::current() + 1, Ordering::Relaxed);
    }
}

/// Under the submission lock, as a submission begins: candidates learned
/// are left on the card, copies on the card that were let go of are
/// fetched, and the next candidate's turn comes.
pub(crate) fn submission_begins() {
    if !leave_on() {
        return;
    }
    let _ = ours();
    let frame = crate::frame::current();
    let candidates: Vec<Arc<Probed>> = CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if candidates.is_empty() {
        return;
    }
    let rearm = REARMED.swap(frame + 1, Ordering::Relaxed) != frame + 1;
    let mut done = false;
    for p in &candidates {
        match p.state() {
            LEARNING if !p.alive.load(Ordering::Relaxed) || p.touched.load(Ordering::Relaxed) => {
                p.state.store(READ, Ordering::Release);
                LEAVE[3].fetch_add(u64::from(p.touched.load(Ordering::Relaxed)), Ordering::Relaxed);
                done = true;
            }
            LEARNING => {
                let sent = p.sent.load(Ordering::Relaxed);
                let quiet = sent == 0 || frame + 1 >= sent + QUIET_FRAMES;
                if !quiet {
                    // Sent up: what it learned is not of the copy as it is.
                    p.armings.store(0, Ordering::Relaxed);
                } else if p.armings.load(Ordering::Relaxed) >= leave_window() {
                    let Some(shadow) = p.shadow.lock().unwrap_or_else(|e| e.into_inner()).upgrade() else {
                        p.state.store(READ, Ordering::Release);
                        done = true;
                        continue;
                    };
                    *p.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(shadow);
                    p.state.store(LEFT, Ordering::Release);
                    arm_views(p, p.start, p.start + p.len);
                    LEAVE[0].fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        start = format_args!("{:#x}", p.start),
                        len = format_args!("{:#x}", p.len),
                        "a copy the title's CPU did not read in {} frames of looking: what shaders write in it stays on the \
                         card (VICTORY_LEAVE_UNREAD=0: fetched back)",
                        leave_window()
                    );
                }
            }
            LEFT => {
                let gone = p.held.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|s| Arc::strong_count(s) == 1);
                if gone || !p.alive.load(Ordering::Relaxed) {
                    // Let go of by the copies' list: fetched now, under the
                    // lock this thread holds.
                    if p.alive.load(Ordering::Relaxed) && p.state.compare_exchange(LEFT, FETCHING, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                        fetch(p, true);
                    }
                    finish(p);
                    done = true;
                } else if rearm {
                    arm_views(p, p.start, p.start + p.len);
                }
            }
            READ | NEVER => done = true,
            _ => {}
        }
    }
    if done {
        CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| matches!(p.state(), LEARNING | LEFT | FETCHING));
    }
    DROPPED.lock().unwrap_or_else(|e| e.into_inner()).clear();
    if frame >= TURN_END.load(Ordering::Relaxed) {
        let learning: Vec<&Arc<Probed>> = candidates.iter().filter(|p| p.state() == LEARNING).collect();
        let next = TURN_NEXT.fetch_add(1, Ordering::Relaxed);
        let turn = learning.get(next % learning.len().max(1)).map_or(0, |p| Arc::as_ptr(p) as usize);
        TURN.store(turn, Ordering::Relaxed);
        TURN_END.store(frame + leave_every(), Ordering::Relaxed);
    }
}

/// Arms `[from, to)` of a copy and the same pages' other views.
fn arm_views(p: &Arc<Probed>, from: usize, to: usize) -> usize {
    let (from, to) = (from & !(PAGE - 1), to.next_multiple_of(PAGE));
    let mut ranges = vec![(from, to)];
    let mut views = Vec::new();
    for (block, others) in crate::inplace::shown_elsewhere(from, to) {
        let (lo, hi) = (from.max(block), to.min(block + crate::watch::PAGE));
        for other in others {
            ranges.push((lo - block + other, hi - block + other));
            views.push(other);
        }
    }
    if !views.is_empty() {
        let mut t = TABLE.write().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = t.index(p) {
            for other in views {
                t.span(other, other + crate::watch::PAGE, i);
            }
        }
    }
    ranges.iter().map(|&(lo, hi)| arm(lo, hi)).sum()
}

/// Shaders' writes of `copy` went into title memory at `[from, to)` (a
/// landing of ours, on this thread).
pub(crate) fn landed(copy: &Shadow, from: usize, to: usize) {
    if !ON.load(Ordering::Relaxed) || from >= to {
        return;
    }
    let frame = crate::frame::current();
    let every = every();
    if every != 0 && copy.span().1 >= min_bytes() && frame.is_multiple_of(every) {
        again();
        let p = of(copy);
        let armed = arm_views(p, from, to);
        p.armed.fetch_add(armed as u64, Ordering::Relaxed);
        p.ever.store(true, Ordering::Relaxed);
        return;
    }
    let Some(p) = copy.probe.get() else {
        return;
    };
    match p.state() {
        LEARNING if TURN.load(Ordering::Relaxed) == Arc::as_ptr(p) as usize => {
            again();
            let armed = arm_views(p, from, to);
            p.armed.fetch_add(armed as u64, Ordering::Relaxed);
            if armed > 0 && p.last_armed.swap(frame + 1, Ordering::Relaxed) != frame + 1 {
                p.armings.fetch_add(1, Ordering::Relaxed);
            }
        }
        // A flight declared before it was left landed: armed again.
        LEFT => {
            arm_views(p, from, to);
        }
        _ => {}
    }
}

/// Makes the pages of `[from, to)` that are read-only or read-write guard
/// pages too; how many.
fn arm(from: usize, to: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, VirtualProtect, VirtualQuery,
    };
    // (Known before any page is armed: not looked up in a fault.)
    let _ = ours();
    let mut at = from;
    let mut armed = 0;
    while at < to {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let end = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(to).max(at + PAGE);
        if info.State == MEM_COMMIT
            && info.Protect & PAGE_GUARD == 0
            && matches!(info.Protect, PAGE_READONLY | PAGE_READWRITE)
        {
            let mut old = 0;
            // SAFETY: title memory; only its protection changes, by a bit
            // the guards look through.
            let done = unsafe { VirtualProtect(at as *const _, end - at, info.Protect | PAGE_GUARD, &mut old) } != 0;
            if done && old != info.Protect {
                // The guards changed it between the look and ours: as they
                // left it.
                // SAFETY: as above.
                unsafe { VirtualProtect(at as *const _, end - at, old, &mut old) };
            } else if done {
                armed += (end - at) / PAGE;
            }
        }
        at = end;
    }
    armed
}

/// Takes the guard bit off the pages of `[from, to)` that have it.
fn disarm(from: usize, to: usize) {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_GUARD, VirtualProtect, VirtualQuery};
    let mut at = from;
    while at < to {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let end = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(to).max(at + PAGE);
        if info.Protect & PAGE_GUARD != 0 {
            let mut old = 0;
            // SAFETY: title memory; only the bit of ours goes.
            unsafe { VirtualProtect(at as *const _, end - at, info.Protect & !PAGE_GUARD, &mut old) };
        }
        at = end;
    }
}

/// Arms again the pages our own code touched.
fn again() {
    let pages = std::mem::take(&mut *AGAIN.lock().unwrap_or_else(|e| e.into_inner()));
    for page in pages {
        arm(page, page + PAGE);
    }
}

/// A guard fault at `at` (`write`: a write) by the instruction at `site`:
/// whether it was one of ours (and handled). On the faulting thread, in the
/// handler.
pub(crate) fn fault(at: usize, write: bool, site: usize) -> bool {
    if !ON.load(Ordering::Relaxed) {
        return false;
    }
    let probed = {
        // (Whoever holds the table to change it takes no fault meanwhile.)
        let t = TABLE.read().unwrap_or_else(|e| e.into_inner());
        let Some(p) = t.find(at) else {
            // Armed for a copy let go of since: the guard is off now, and
            // the access goes on (else nobody takes the fault and the
            // process ends: run ow54-port, in a memcpy).
            return t.armed_once(at);
        };
        p.clone()
    };
    let (from, to) = ours();
    let ours = crate::safecopy::failing_exit(site).is_some() || (from..to).contains(&site);
    if ours {
        probed.ours.fetch_add(1, Ordering::Relaxed);
    } else if write {
        probed.writes.fetch_add(1, Ordering::Relaxed);
    } else {
        probed.read_at(site);
    }
    match probed.state() {
        LEFT | FETCHING => {
            if LANDING.with(|l| l.get()) {
                // A landing of ours: armed again after it.
                return true;
            }
            if probed.state.compare_exchange(LEFT, FETCHING, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                LEAVE[if ours { 2 } else { 1 }].fetch_add(1, Ordering::Relaxed);
                ask_fetch(probed.clone());
            }
            if !ours {
                // The title reads (or writes) what shaders wrote: not before
                // it is in title memory.
                let (lock, done) = &FETCHED;
                let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
                while probed.state() == FETCHING {
                    held = done.wait_timeout(held, std::time::Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        }
        LEARNING => {
            if ours {
                AGAIN.lock().unwrap_or_else(|e| e.into_inner()).push(at & !(PAGE - 1));
            } else {
                probed.touched.store(true, Ordering::Relaxed);
            }
        }
        _ => {
            if ours && every() != 0 {
                AGAIN.lock().unwrap_or_else(|e| e.into_inner()).push(at & !(PAGE - 1));
            }
        }
    }
    true
}

/// Has the fetching thread fetch `p` (begun once).
fn ask_fetch(p: Arc<Probed>) {
    static THREAD: std::sync::Once = std::sync::Once::new();
    THREAD.call_once(|| {
        let spawned = std::thread::Builder::new().name("victory copies left: fetch".into()).spawn(|| loop {
            let next = {
                let mut asked = TO_FETCH.lock().unwrap_or_else(|e| e.into_inner());
                while asked.is_empty() {
                    asked = FETCH_ASKED.wait(asked).unwrap_or_else(|e| e.into_inner());
                }
                asked.remove(0)
            };
            fetch(&next, false);
            finish(&next);
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no thread to fetch copies left on the card");
        }
    });
    TO_FETCH.lock().unwrap_or_else(|e| e.into_inner()).push(p);
    FETCH_ASKED.notify_one();
}

/// Fetches the copy whole and lands it (`locked`: the caller holds the
/// submission lock).
fn fetch(p: &Arc<Probed>, locked: bool) {
    let shadow = p.held.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let (Some(shadow), Some(host)) = (shadow, crate::host::host()) {
        as_landing(|| host.fetch_left(&shadow, locked));
    }
}

/// After the fetch (or none): read for good, unguarded, and the waiters go on.
fn finish(p: &Arc<Probed>) {
    p.state.store(READ, Ordering::Release);
    disarm(p.start, p.start + p.len);
    for (block, others) in crate::inplace::shown_elsewhere(p.start, p.start + p.len) {
        let (lo, hi) = (p.start.max(block), (p.start + p.len).min(block + crate::watch::PAGE));
        for other in others {
            disarm(lo - block + other, hi - block + other);
        }
    }
    if let Some(shadow) = p.held.lock().unwrap_or_else(|e| e.into_inner()).take() {
        DROPPED.lock().unwrap_or_else(|e| e.into_inner()).push(shadow);
    }
    let (lock, done) = &FETCHED;
    drop(lock.lock().unwrap_or_else(|e| e.into_inner()));
    done.notify_all();
}

/// Title memory `[at, at + len)` was freed: its copies are gone.
pub(crate) fn freed(at: usize, len: usize) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    let end = if len == 0 { at + 1 } else { at.saturating_add(len) };
    let t = TABLE.read().unwrap_or_else(|e| e.into_inner());
    for p in t.copies.iter().filter(|p| p.start < end && p.start + p.len > at) {
        p.alive.store(false, Ordering::Relaxed);
    }
}

/// The pages of `[from, to)` still armed.
fn still_armed(from: usize, to: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_GUARD, VirtualQuery};
    let mut at = from;
    let mut n = 0;
    while at < to {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let end = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(to).max(at + PAGE);
        if info.Protect & PAGE_GUARD != 0 {
            n += (end - at) / PAGE;
        }
        at = end;
    }
    n
}

/// `site` as a module's name and an offset in it.
fn named(site: usize) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
        GetModuleHandleExW,
    };
    let mut module = std::ptr::null_mut();
    // SAFETY: the out pointer is valid; an address in no image is refused.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            site as *const u16,
            &mut module,
        )
    } != 0;
    if !found || module.is_null() {
        return format!("{site:#x}");
    }
    let mut name = [0u16; 260];
    // SAFETY: a loaded module; the buffer's length.
    let n = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
    let name = String::from_utf16_lossy(&name[..n.min(name.len())]);
    let name = name.rsplit(['\\', '/']).next().unwrap_or("?").to_owned();
    format!("{name}+{:#x}", site - module as usize)
}

/// The probe's lines: the copies left on the card, and (the diagnostic) for
/// each copy armed since the last line what touched it.
pub(crate) fn line(frames: f64) {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    if leave_on() {
        let [left, title, ours, read] = [0, 1, 2, 3].map(|i| LEAVE[i].swap(0, Ordering::Relaxed));
        let candidates = CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let on_card: Vec<&Arc<Probed>> = candidates.iter().filter(|p| p.left()).collect();
        let learning = candidates.iter().filter(|p| p.state() == LEARNING).count();
        let mb = on_card.iter().map(|p| p.len).sum::<usize>() as f64 / 1e6;
        if !candidates.is_empty() || left + title + ours + read > 0 {
            tracing::info!(
                "copies the title's CPU does not read, left on the card (VICTORY_LEAVE_UNREAD=0: fetched back): {} now, {mb:.1} \
                 MB; {learning} being looked at; since the last line {left} left, {title} fetched at the title's touch, {ours} \
                 at ours, {read} found read while looked at",
                on_card.len()
            );
        }
    }
    if every() == 0 {
        return;
    }
    again();
    let copies: Vec<Arc<Probed>> = TABLE.read().unwrap_or_else(|e| e.into_inner()).copies.clone();
    let mut rows: Vec<(u64, String)> = Vec::new();
    let (mut unread, mut read) = (0, 0);
    for c in copies.iter().filter(|c| c.ever.load(Ordering::Relaxed)) {
        let armed = c.armed.swap(0, Ordering::Relaxed);
        let reads = c.reads.swap(0, Ordering::Relaxed);
        let writes = c.writes.swap(0, Ordering::Relaxed);
        let ours = c.ours.swap(0, Ordering::Relaxed);
        let elsewhere = c.elsewhere.swap(0, Ordering::Relaxed);
        let sites: Vec<String> = c
            .sites
            .iter()
            .filter_map(|(at, n)| {
                let (at, n) = (at.swap(0, Ordering::Relaxed), n.swap(0, Ordering::Relaxed));
                (at != 0 && n != 0).then(|| (n, named(at)))
            })
            .map(|(n, at)| format!("{n} at {at}"))
            .collect();
        if armed == 0 && reads + writes + ours == 0 {
            continue;
        }
        if reads == 0 {
            unread += 1;
        } else {
            read += 1;
        }
        let still = still_armed(c.start, c.start + c.len);
        rows.push((
            reads,
            format!(
                "{:#x} ({:.1} MB): {:.0} pages armed a frame, the title read {reads} ({}{}), wrote {writes}; ours {ours}; {still} \
                 armed now",
                c.start,
                c.len as f64 / 1e6,
                armed as f64 / frames,
                if sites.is_empty() { "-".to_owned() } else { sites.join(", ") },
                if elsewhere > 0 { format!(", {elsewhere} elsewhere") } else { String::new() },
            ),
        ));
    }
    if rows.is_empty() {
        return;
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    tracing::info!(
        "read probe (VICTORY_READ_PROBE={}): of {} copies whose landed pages were armed, the title's CPU read {read} and not \
         {unread}: {}",
        every(),
        read + unread,
        rows.into_iter().map(|r| r.1).collect::<Vec<_>>().join("; ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh memory of our own, `pages` of 4 KiB, registered as a copy's.
    fn memory(pages: usize) -> (usize, Arc<Probed>) {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        crate::watch::install();
        ON.store(true, Ordering::Relaxed);
        // SAFETY: fresh memory of our own.
        let at = unsafe { VirtualAlloc(std::ptr::null(), pages * PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        let probed = Arc::new(Probed::new(at, pages * PAGE));
        TABLE.write().unwrap().add(probed.clone());
        (at, probed)
    }

    fn release(at: usize, probed: &Probed) {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        probed.alive.store(false, Ordering::Relaxed);
        AGAIN.lock().unwrap().clear();
        // SAFETY: ours.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// A guard page is armed, its first touch is told here (and goes on),
    /// the second is not a fault, and the guards see its protection as
    /// before.
    #[test]
    fn a_touched_page_is_counted_once_and_the_guards_see_through_the_bit() {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, VirtualQuery};
        let (at, probed) = memory(4);
        probed.state.store(LEARNING, Ordering::Relaxed);
        assert_eq!(arm(at, at + 2 * PAGE), 2);
        assert_eq!(still_armed(at, at + 4 * PAGE), 2);
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer.
        unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        assert_eq!(info.Protect, PAGE_READWRITE | PAGE_GUARD);
        // SAFETY: our memory, committed: a read, twice, then a write of the
        // other page.
        let first = unsafe { std::ptr::read_volatile((at + 8) as *const u64) };
        let second = unsafe { std::ptr::read_volatile((at + 16) as *const u64) };
        unsafe { std::ptr::write_volatile((at + PAGE) as *mut u64, 7) };
        assert_eq!((first, second), (0, 0));
        // A test's touches are in our image: ours, and armed again.
        assert_eq!(probed.ours.load(Ordering::Relaxed), 2);
        assert!(!probed.touched.load(Ordering::Relaxed));
        assert_eq!(still_armed(at, at + 4 * PAGE), 0);
        again();
        assert_eq!(still_armed(at, at + 4 * PAGE), 2);
        // A title's read (from elsewhere) is counted at its site, and the
        // copy is found read.
        assert!(fault(at, false, 0x1000));
        assert_eq!(probed.reads.load(Ordering::Relaxed), 1);
        assert!(probed.touched.load(Ordering::Relaxed));
        assert!(!fault(at + 4 * PAGE + (1 << 30), false, 0x1000), "not a probed copy's");
        disarm(at, at + 4 * PAGE);
        assert_eq!(still_armed(at, at + 4 * PAGE), 0);
        release(at, &probed);
    }

    /// A copy on the card: a landing's writes are no touch; a touch of ours
    /// asks for the fetch and goes on; the copy is then read for good and
    /// unguarded, and a title's touch after that does not wait.
    #[test]
    fn a_copy_left_on_the_card_is_fetched_at_a_touch() {
        let (at, probed) = memory(3);
        probed.state.store(LEFT, Ordering::Relaxed);
        assert_eq!(arm(at, at + 3 * PAGE), 3);
        // SAFETY: our memory, committed.
        as_landing(|| unsafe { std::ptr::write_volatile(at as *mut u64, 1) });
        assert_eq!(probed.state(), LEFT, "a landing is no touch");
        // No host here: the fetching thread finds no copy to fetch, and
        // finishes it as read.
        // SAFETY: as above.
        unsafe { std::ptr::read_volatile((at + PAGE) as *const u64) };
        let began = std::time::Instant::now();
        while probed.state() != READ {
            assert!(began.elapsed() < std::time::Duration::from_secs(5), "never fetched");
            std::thread::yield_now();
        }
        assert_eq!(still_armed(at, at + 3 * PAGE), 0, "unguarded once fetched");
        // The title's touch of a copy read waits for nothing.
        assert!(fault(at + 2 * PAGE, false, 0x1000));
        assert_eq!(probed.state(), READ);
        release(at, &probed);
    }
}
