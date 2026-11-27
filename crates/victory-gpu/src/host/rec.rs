//! A host list's D3D12 list, with its barrier calls joined
//! (`VICTORY_JOIN_BARRIERS`, on by default; `0`: each call at once, as
//! before; A/B-able with `VICTORY_AB=<s>,VICTORY_JOIN_BARRIERS`).
//!
//! The state changes the host asks for (`crate::list::ensure_state`, the
//! aliasing before a texture's first upload) and the title's own barriers
//! and pipeline syncs wait here instead of going to the list at once; they
//! go, in the order they were asked for, in one call, at the next use of the
//! list for anything else. Every use of the list goes through [`Rec`]'s
//! `Deref`, so nothing is recorded ahead of a barrier asked for before it:
//! draws, dispatches, copies, clears, resolves, indirect executions, queries
//! and the list's `Close` all find what was held recorded first. (Code that
//! takes the list out as a plain `&ID3D12GraphicsCommandList`, or casts it
//! to a later interface, gets it flushed at that moment; none of it asks for
//! a held barrier while it records through what it took.)
//!
//! Joined calls are what the card would have done anyway: barriers with no
//! work between them wait for the same work. Only a resource named twice in
//! one call is not allowed, so a barrier on a resource already held sends
//! what is held first. Ported from VIctory-Belobug-AMD d3a6c2c3da (GPL-3.0),
//! where in a replay joining the calls that had no work between them saved
//! 2.4 ms of 79.5 (2,490 calls joined, 1,619 of them a texture's state
//! change after its upload followed by the next texture's aliasing).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Whether barriers are held now (`VICTORY_JOIN_BARRIERS`, or its phase of
/// `VICTORY_AB`).
pub(crate) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_JOIN_BARRIERS") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOIN_BARRIERS").is_none_or(|v| v != "0"))
}

/// Barriers held, and a reference to each resource they name: the title's
/// barriers name host copies by borrowed pointers, and a copy the title
/// releases (or the host replaces) between the barrier and the next use of
/// the list must still be alive when the call is made.
#[derive(Default)]
struct Held {
    barriers: Vec<D3D12_RESOURCE_BARRIER>,
    alive: Vec<ID3D12Resource>,
}

// SAFETY: descriptions with borrowed interface pointers of free-threaded
// objects, which `alive` holds.
unsafe impl Send for Held {}

/// Since the last frame-rate line: barrier calls asked for while held, and
/// calls made of them.
pub(crate) static STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

pub(crate) struct Rec {
    list: ID3D12GraphicsCommandList,
    /// Whether anything is held (read at every use of the list, so not the
    /// lock).
    any: AtomicBool,
    held: Mutex<Held>,
    /// Whether the list may have written memory since its last wait for every
    /// shader write ([`Rec::title_wait`]): [`Rec::writes`] sets it. A list starts so (an earlier one of the submission may have written).
    dirty: AtomicBool,
    /// A wait of the title's held: nothing that could write was recorded since the
    /// last wait, so none is needed yet; it is made before the next thing that may.
    waiting: AtomicBool,
    /// Unused since only writers mark the list ([`Rec::writes`]); kept for
    /// [`Rec::loud`]'s callers.
    quiet: AtomicBool,
    /// Uses of the list so far ([`Rec::uses`]).
    uses: AtomicU64,
}

/// `VICTORY_HOLD_WAITS` (on by default; `0` off): the title's waits for every shader
/// write (its barrier groups, pipeline syncs) are left out where nothing since the
/// last one could have written memory, and made only before the next thing that may
/// (which would otherwise overtake what was read before the wait). In the open world
/// some 800 such waits a frame each drained the card, most between draws that store
/// nothing. Render targets, depth and copies are kept in order by their state
/// changes, which stay.
pub(crate) fn hold_waits() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_HOLD_WAITS") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HOLD_WAITS").is_none_or(|v| v != "0"))
}

/// Since the last frame-rate line: the title's waits made at once, held, and made
/// later of those held.
pub(crate) static WAITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// `VICTORY_CLAMP_WAITS` (on by default; `0` off): the argument clamps
/// (`crate::indirect`) wait for every earlier shader write only where the list
/// may have written since its last such wait ([`Rec::clamp_wait`]), and make no
/// second wait after their pass: the records they write are ordered before their
/// executions by the scratch buffer's move to indirect arguments, and the budget
/// is only ever touched by atomics. Off, each clamp made both waits, some 190
/// pairs a frame in the open world, each a drain of the card.
pub(crate) fn clamp_waits() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_CLAMP_WAITS") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLAMP_WAITS").is_none_or(|v| v != "0"))
}

/// Since the last frame-rate line: the clamps' first waits made, and left out.
pub(crate) static CLAMP_WAITS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A wait for every shader write: an unordered-access barrier naming nothing.
fn wait_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 { UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }) },
    }
}

impl Rec {
    pub(crate) fn new(list: ID3D12GraphicsCommandList) -> Self {
        Self {
            list,
            any: AtomicBool::new(false),
            held: Mutex::new(Held::default()),
            dirty: AtomicBool::new(true),
            waiting: AtomicBool::new(false),
            quiet: AtomicBool::new(false),
            uses: AtomicU64::new(0),
        }
    }

    /// How many times the list was used to record (any call through it):
    /// what changes it, something went into the list.
    pub(crate) fn uses(&self) -> u64 {
        self.uses.load(Relaxed)
    }

    /// The title waits for every earlier shader write: the wait is made, or held
    /// where nothing could have written since the last one ([`hold_waits`]).
    ///
    /// # Safety
    /// An open list.
    pub(crate) unsafe fn title_wait(&self) {
        self.quiet.store(false, Relaxed);
        if hold_waits() && !self.dirty.load(Relaxed) {
            self.waiting.store(true, Relaxed);
            WAITS[1].fetch_add(1, Relaxed);
            return;
        }
        self.waiting.store(false, Relaxed);
        // SAFETY: the caller's; it names no resource.
        unsafe { self.barriers(&[wait_barrier()]) };
        self.dirty.store(false, Relaxed);
        WAITS[0].fetch_add(1, Relaxed);
    }

    /// A draw or dispatch is being recorded: if its shaders may write (`writes`),
    /// as [`Rec::writes`]; one that writes nothing needs no wait held before it.
    pub(crate) fn quiet(&self, writes: bool) {
        if writes {
            self.writes();
        }
    }

    /// Something that may write memory a shader reads is about to be recorded (a
    /// draw or dispatch whose shaders store, a copy, clear or resolve of the
    /// title's): the title's held wait is made first, and the list may have
    /// written from here. Only these count: state the title sets, and the host's
    /// own uploads and state changes (ordered by their transitions), do not.
    pub(crate) fn writes(&self) {
        if self.waiting.swap(false, Relaxed) {
            // SAFETY: an open list (it is being recorded); the barrier names nothing.
            unsafe { self.barriers(&[wait_barrier()]) };
            WAITS[2].fetch_add(1, Relaxed);
        }
        self.dirty.store(true, Relaxed);
    }

    /// An argument clamp is about to read what earlier shaders may have written:
    /// whether it must wait for every shader write first. With [`clamp_waits`]
    /// only where the list may have written since its last wait; the caller then
    /// records that wait, which also stands for a held one of the title's.
    pub(crate) fn clamp_wait(&self) -> bool {
        if clamp_waits() && !self.dirty.load(Relaxed) {
            CLAMP_WAITS[1].fetch_add(1, Relaxed);
            return false;
        }
        self.dirty.store(false, Relaxed);
        self.waiting.store(false, Relaxed);
        CLAMP_WAITS[0].fetch_add(1, Relaxed);
        true
    }

    /// The end of a quiet span (a title call begins), or with `closing` the end
    /// of the list: a held wait is made then (the next list may write what this
    /// one read).
    pub(crate) fn loud(&self) {
        self.quiet.store(false, Relaxed);
    }

    /// The list is about to close: a held wait is made.
    pub(crate) fn closing(&self) {
        self.writes();
    }

    /// Records `barriers` now, or holds them for the next use of the list.
    ///
    /// # Safety
    /// As `ResourceBarrier`'s: an open list, live resources.
    pub(crate) unsafe fn barriers(&self, barriers: &[D3D12_RESOURCE_BARRIER]) {
        if barriers.is_empty() {
            return;
        }
        if !on() {
            // (What was held while it was on goes first.)
            self.flush();
            // SAFETY: the caller's.
            unsafe { self.list.ResourceBarrier(barriers) };
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        // A resource named again: what is held goes first, so that no call
        // names one resource twice.
        let again = barriers
            .iter()
            .flat_map(named)
            .flatten()
            .any(|r| held.barriers.iter().flat_map(named).flatten().any(|h| h.as_raw() == r.as_raw()));
        if again {
            self.record(&mut held);
        }
        for b in barriers {
            // (One wait for every shader write is enough per call.)
            if every_write(b) && held.barriers.iter().any(every_write) {
                continue;
            }
            held.alive.extend(named(b).into_iter().flatten().cloned());
            // SAFETY: a plain copy of the description; nothing is released
            // through it (its pointers are `ManuallyDrop`), and `alive`
            // holds what it names.
            held.barriers.push(unsafe { std::ptr::read(b) });
        }
        STATS[0].fetch_add(1, Relaxed);
        self.any.store(true, Relaxed);
    }

    /// What is held goes to the list, in one call.
    fn flush(&self) {
        if !self.any.load(Relaxed) {
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        self.record(&mut held);
    }

    fn record(&self, held: &mut Held) {
        self.any.store(false, Relaxed);
        if held.barriers.is_empty() {
            return;
        }
        // SAFETY: the barriers were asked for on this open list, in order,
        // with nothing recorded since; `alive` holds their resources.
        unsafe { self.list.ResourceBarrier(&held.barriers) };
        held.barriers.clear();
        held.alive.clear();
        STATS[1].fetch_add(1, Relaxed);
    }

    /// Before the list is reset: what is still held is dropped unrecorded
    /// (a closed list had it recorded at its `Close`; nothing else should
    /// be left).
    pub(crate) fn forget(&self) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        self.dirty.store(true, Relaxed);
        self.waiting.store(false, Relaxed);
        self.quiet.store(false, Relaxed);
        self.any.store(false, Relaxed);
        held.barriers.clear();
        held.alive.clear();
    }
}

/// The resources a barrier names: a transition's, both of an aliasing,
/// an unordered-access barrier's (`None` for one on every resource).
fn named(b: &D3D12_RESOURCE_BARRIER) -> [Option<&ID3D12Resource>; 2] {
    // SAFETY: the union's member is the one `Type` says.
    unsafe {
        match b.Type {
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => [b.Anonymous.Transition.pResource.as_ref(), None],
            D3D12_RESOURCE_BARRIER_TYPE_ALIASING => {
                [b.Anonymous.Aliasing.pResourceBefore.as_ref(), b.Anonymous.Aliasing.pResourceAfter.as_ref()]
            }
            D3D12_RESOURCE_BARRIER_TYPE_UAV => [b.Anonymous.UAV.pResource.as_ref(), None],
            _ => [None, None],
        }
    }
}

/// Whether `b` waits for every shader write (an unordered-access barrier
/// naming no resource).
fn every_write(b: &D3D12_RESOURCE_BARRIER) -> bool {
    // SAFETY: the union's member is the one `Type` says.
    b.Type == D3D12_RESOURCE_BARRIER_TYPE_UAV && unsafe { b.Anonymous.UAV.pResource.is_none() }
}

impl std::ops::Deref for Rec {
    type Target = ID3D12GraphicsCommandList;

    /// The list, with what is held recorded first.
    fn deref(&self) -> &ID3D12GraphicsCommandList {
        self.uses.fetch_add(1, Relaxed);
        self.flush();
        &self.list
    }
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    let [asked, made] = [0, 1].map(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    if asked > 0.0 {
        tracing::info!("barrier calls per frame (VICTORY_JOIN_BARRIERS): {asked:.0} asked for, {made:.0} made of them");
    }
    let [now, held, later] = [0, 1, 2].map(|i| WAITS[i].swap(0, Relaxed) as f64 / frames);
    if now + held > 0.0 {
        tracing::info!(
            "the title's waits for every shader write per frame (VICTORY_HOLD_WAITS): {now:.0} made, {held:.0} held, {later:.0} of them made later"
        );
    }
    let [made, left] = [0, 1].map(|i| CLAMP_WAITS[i].swap(0, Relaxed) as f64 / frames);
    if made + left > 0.0 {
        tracing::info!("argument clamps' waits per frame (VICTORY_CLAMP_WAITS): {made:.0} made, {left:.0} left out");
    }
}
