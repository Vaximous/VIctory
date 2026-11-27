//! Where the write watch's work falls, by the title's allocations (builds
//! with diagnostics): per frame, for each protection the title asked its
//! memory for, the title's write faults, the blocks made read-only (taken,
//! or guarded as another view of a page taken), the blocks we opened
//! ourselves, and the kernel's askings (KiB walked, pages found written);
//! and, for each allocation with GPU access bits (0x800 and up), whether the
//! title's CPU has ever written it since it was made.
//!
//! What it is for: memory only the GPU touches needs no write watch, no
//! protection changes and no hashing. Whether such memory carries any of
//! that work is what this tells, before a policy is built on it.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::RwLock;

/// Kinds of protection told apart; the last slot is everything else.
const CLASSES: usize = 16;
/// Allocations with GPU access bits followed one by one.
const GPU_SLOTS: usize = 48;
/// The console's GPU access bits.
const GPU_ACCESS: u32 = 0x7_f800;

/// What is counted: title write faults, blocks made read-only, blocks we
/// opened, KiB the kernel was asked about, pages it found written.
const WHAT: usize = 5;
pub(crate) const FAULT: usize = 0;
pub(crate) const TAKEN: usize = 1;
pub(crate) const OURS: usize = 2;
pub(crate) const ASKED_KIB: usize = 3;
pub(crate) const FOUND: usize = 4;

#[derive(Default)]
struct Registry {
    /// `[from, to)`, class, GPU slot (`u8::MAX`: none); sorted by `from`.
    allocations: Vec<(u64, u64, u8, u8)>,
    /// Each class's protection and whether it is of 1 GiB or more.
    classes: Vec<(u32, bool)>,
    /// The GPU slots' allocations: `[from, to)`, protection.
    gpu: Vec<(u64, u64, u32)>,
}

static REGISTRY: RwLock<Registry> =
    RwLock::new(Registry { allocations: Vec::new(), classes: Vec::new(), gpu: Vec::new() });
static COUNTS: [[AtomicU64; WHAT]; CLASSES] = [const { [const { AtomicU64::new(0) }; WHAT] }; CLASSES];
/// Per GPU slot, since it was made: title write faults, pages the kernel
/// found written, blocks made read-only.
static GPU_EVER: [[AtomicU64; 3]; GPU_SLOTS] = [const { [const { AtomicU64::new(0) }; 3] }; GPU_SLOTS];

/// The title was given `[at, at + len)` with the console's protection
/// `protect`.
pub(crate) fn allocated(at: usize, len: usize, protect: u32) {
    if len == 0 {
        return;
    }
    let (from, to) = (at as u64, at.saturating_add(len) as u64);
    let mut r = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    let key = (protect, len >= 1 << 30);
    let class = match r.classes.iter().position(|&c| c == key) {
        Some(i) => i,
        None if r.classes.len() < CLASSES - 1 => {
            r.classes.push(key);
            r.classes.len() - 1
        }
        None => CLASSES - 1,
    } as u8;
    let mut slot = u8::MAX;
    if protect & GPU_ACCESS != 0 {
        let free = r.gpu.iter().position(|g| g.1 == 0);
        let i = match free {
            Some(i) => Some(i),
            None if r.gpu.len() < GPU_SLOTS => {
                r.gpu.push((0, 0, 0));
                Some(r.gpu.len() - 1)
            }
            None => None,
        };
        if let Some(i) = i {
            r.gpu[i] = (from, to, protect);
            GPU_EVER[i].iter().for_each(|c| c.store(0, Relaxed));
            slot = i as u8;
        }
    }
    let at = r.allocations.partition_point(|a| a.0 < from);
    r.allocations.insert(at, (from, to, class, slot));
}

/// The title let go of `[at, at + len)` (`len` 0: of the allocation at
/// `at`).
pub(crate) fn freed(at: usize, len: usize) {
    let (at, end) = (at as u64, at.saturating_add(len) as u64);
    let mut r = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    let gone = |&(from, to, _, _): &(u64, u64, u8, u8)| if len == 0 { from == at } else { from >= at && to <= end };
    let slots: Vec<u8> = r.allocations.iter().filter(|a| gone(a)).map(|a| a.3).filter(|&s| s != u8::MAX).collect();
    for s in slots {
        r.gpu[s as usize] = (0, 0, 0);
    }
    r.allocations.retain(|a| !gone(a));
}

/// The innermost allocation holding `address`: its class and GPU slot.
fn find(r: &Registry, address: u64) -> (usize, u8) {
    let end = r.allocations.partition_point(|a| a.0 <= address);
    r.allocations[..end]
        .iter()
        .rev()
        .take(32)
        .filter(|a| address < a.1)
        .min_by_key(|a| a.1 - a.0)
        .map_or((CLASSES - 1, u8::MAX), |a| (a.2 as usize, a.3))
}

/// `n` of `what` at title address `address`. Never waits (a fault may
/// count): what finds the registry being changed counts as other memory.
pub(crate) fn count(what: usize, address: usize, n: u64) {
    if n == 0 {
        return;
    }
    let (class, slot) = match REGISTRY.try_read() {
        Ok(r) => find(&r, address as u64),
        Err(_) => (CLASSES - 1, u8::MAX),
    };
    COUNTS[class][what].fetch_add(n, Relaxed);
    if slot != u8::MAX {
        let ever = match what {
            FAULT => 0,
            FOUND => 1,
            TAKEN => 2,
            _ => return,
        };
        GPU_EVER[slot as usize][ever].fetch_add(n, Relaxed);
    }
}

/// For the log, over `frames` frames.
pub(crate) fn line(frames: f64) {
    let frames = frames.max(1.0);
    let taken: Vec<[u64; WHAT]> = COUNTS.iter().map(|c| std::array::from_fn(|w| c[w].swap(0, Relaxed))).collect();
    let r = REGISTRY.read().unwrap_or_else(|e| e.into_inner());
    let name = |i: usize| match r.classes.get(i) {
        Some(&(p, big)) if i < CLASSES - 1 => format!("{p:#x}{}", if big { " (1 GiB+)" } else { "" }),
        _ => "other".to_string(),
    };
    let per = |n: u64| n as f64 / frames;
    let classes: Vec<String> = taken
        .iter()
        .enumerate()
        .filter(|(_, c)| c.iter().any(|&n| n != 0))
        .map(|(i, c)| {
            format!(
                "{} {:.1} faults, {:.1} blocks made read-only, {:.1} opened by us, {:.0} MB asked of the kernel with {:.1} pages \
                 found written",
                name(i),
                per(c[FAULT]),
                per(c[TAKEN]),
                per(c[OURS]),
                per(c[ASKED_KIB]) / 1024.0,
                per(c[FOUND]),
            )
        })
        .collect();
    tracing::info!(
        "the write watch's work per frame by the protection the title asked its memory for: {}",
        if classes.is_empty() { "none".to_string() } else { classes.join("; ") }
    );
    let gpu: Vec<String> = r
        .gpu
        .iter()
        .enumerate()
        .filter(|(_, g)| g.1 != 0)
        .map(|(i, &(from, to, p))| {
            let [faults, found, taken] = std::array::from_fn::<u64, 3, _>(|w| GPU_EVER[i][w].load(Relaxed));
            format!(
                "{from:#x}+{:#x} ({p:#x}) {}",
                to - from,
                if faults + found == 0 {
                    format!("never written by the CPU ({taken} blocks made read-only)")
                } else {
                    format!("{faults} faults, {found} pages found written, {taken} blocks made read-only")
                }
            )
        })
        .collect();
    if !gpu.is_empty() {
        tracing::info!("allocations with GPU access, since they were made: {}", gpu.join("; "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_is_counted_by_the_innermost_allocation() {
        // Far from any address other tests use.
        let base = 0x7e_0000_0000usize;
        allocated(base, 1 << 30, 0x801);
        allocated(base + 0x10000, 0x20000, 0x2004);
        let r = REGISTRY.read().unwrap();
        let (outer, none) = find(&r, base as u64 + 0x1000);
        let (inner, slot) = find(&r, base as u64 + 0x18000);
        assert_ne!(outer, inner);
        assert_eq!(r.classes[outer], (0x801, true));
        assert_eq!(r.classes[inner], (0x2004, false));
        assert_ne!(none, u8::MAX);
        assert_ne!(slot, u8::MAX);
        drop(r);
        count(FAULT, base + 0x18000, 3);
        assert_eq!(GPU_EVER[slot as usize][0].load(Relaxed), 3);
        freed(base + 0x10000, 0);
        let r = REGISTRY.read().unwrap();
        assert_eq!(find(&r, base as u64 + 0x18000).0, outer);
        drop(r);
        freed(base, 0);
    }
}
