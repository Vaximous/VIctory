//! Whether the title's CPU reads what shaders wrote and we put back into
//! title memory (`VICTORY_READ_PROBE=N`, off by default: a diagnostic).
//!
//! The fetch back of shader writes costs some 16 ms a frame (run ow19-move:
//! 42 MB a frame, half of it one 20 MB copy). What the title never reads on
//! its CPU need not come back, but nothing here says what it reads: the
//! guards see writes only. With the probe on, every `N`th frame the 4 KiB
//! pages that a landing put bytes into (of copies of at least
//! `VICTORY_READ_PROBE_MIN_KB`, 1024) are made guard pages, on top of the
//! protection they have, at the title's address and at the other views of
//! the same pages. The first touch of such a page raises a guard fault,
//! which Windows ends by taking the guard off; the fault is counted here and
//! the access goes on as it would have. A page stays armed until it is
//! touched or the guards change its protection.
//!
//! Who touched it is told by where the faulting instruction is: in our
//! copies (`crate::safecopy`) or in this module's image, ours, and the page
//! is armed again at the next arming or line; anywhere else (the title's
//! image or a system library it calls), the title's, read or write. The
//! line names the places the title read from.
//!
//! What it cannot see: a page whose protection the guards changed before
//! the title read it (the guard is then gone): the line gives how many of
//! the pages armed are still armed at its time, against those touched. A
//! copy is unread only by a run in which its pages were armed many times
//! and the title read none.
//!
//! The guards ([`crate::watch`]) look through the guard bit: a page that
//! carries it is read-only or read-write to them as before.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

const PAGE: usize = 4096;
/// The places a copy's reads are counted at, apart.
const SITES: usize = 6;

/// Every how many frames the landings arm their pages; 0, off.
pub(crate) fn every() -> u64 {
    static EVERY: OnceLock<u64> = OnceLock::new();
    *EVERY.get_or_init(|| {
        let every = std::env::var("VICTORY_READ_PROBE").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        ON.store(every != 0, Ordering::Relaxed);
        every
    })
}

/// Whether the probe is on, without a look at the environment (for the fault
/// handler: on only after a landing asked).
static ON: AtomicBool = AtomicBool::new(false);

fn min_bytes() -> u64 {
    static MIN: OnceLock<u64> = OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("VICTORY_READ_PROBE_MIN_KB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1024) * 1024
    })
}

/// One copy's counts, from one line to the next.
struct Probed {
    start: usize,
    len: usize,
    /// Pages made guard pages.
    armed: AtomicU64,
    /// Guard faults: the title's reads, the title's writes, ours.
    reads: AtomicU64,
    writes: AtomicU64,
    ours: AtomicU64,
    /// Where the title's reads were, and how many at each.
    sites: [(AtomicUsize, AtomicU64); SITES],
    elsewhere: AtomicU64,
    /// Ever armed (kept across lines).
    ever: AtomicBool,
}

impl Probed {
    fn new(start: usize, len: usize) -> Probed {
        Probed {
            start,
            len,
            armed: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            ours: AtomicU64::new(0),
            sites: std::array::from_fn(|_| (AtomicUsize::new(0), AtomicU64::new(0))),
            elsewhere: AtomicU64::new(0),
            ever: AtomicBool::new(false),
        }
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
    fn copy(&mut self, start: usize, len: usize) -> usize {
        if let Some(i) = self.copies.iter().position(|c| c.start == start && c.len == len) {
            return i;
        }
        self.copies.push(Arc::new(Probed::new(start, len)));
        let i = self.copies.len() - 1;
        self.span(start, start + len, i);
        i
    }

    fn span(&mut self, from: usize, to: usize, copy: usize) {
        let at = self.spans.partition_point(|s| s.0 < from);
        if self.spans.get(at).is_some_and(|s| s.0 == from && s.1 == to) {
            self.spans[at].2 = copy;
            return;
        }
        self.spans.insert(at, (from, to, copy));
    }

    fn find(&self, at: usize) -> Option<&Arc<Probed>> {
        // The last span beginning at or below `at` that holds it (they may
        // nest: a view's block inside an older copy's range).
        let end = self.spans.partition_point(|s| s.0 <= at);
        self.spans[..end].iter().rev().take(8).find(|s| at < s.1).map(|s| &self.copies[s.2])
    }
}

static TABLE: RwLock<Table> = RwLock::new(Table { copies: Vec::new(), spans: Vec::new() });
/// Pages our own code touched: armed again at the next arming or line.
static AGAIN: Mutex<Vec<usize>> = Mutex::new(Vec::new());

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

/// Shaders' writes of the copy over `copy` (start, length) went into title
/// memory at `[from, to)`: in a frame the probe arms, its pages (and their
/// other views') are armed.
pub(crate) fn landed(copy: (u64, u64), from: usize, to: usize) {
    let every = every();
    if every == 0 || copy.1 < min_bytes() || from >= to || !crate::frame::current().is_multiple_of(every) {
        return;
    }
    again();
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
    let probed = {
        let mut t = TABLE.write().unwrap_or_else(|e| e.into_inner());
        let i = t.copy(copy.0 as usize, copy.1 as usize);
        for other in views {
            t.span(other, other + crate::watch::PAGE, i);
        }
        t.copies[i].clone()
    };
    let armed: usize = ranges.iter().map(|&(lo, hi)| arm(lo, hi)).sum();
    probed.armed.fetch_add(armed as u64, Ordering::Relaxed);
    probed.ever.store(true, Ordering::Relaxed);
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

/// Arms again the pages our own code touched.
fn again() {
    let pages = std::mem::take(&mut *AGAIN.lock().unwrap_or_else(|e| e.into_inner()));
    for page in pages {
        arm(page, page + PAGE);
    }
}

/// A guard fault at `at` (`write`: a write) by the instruction at `site`:
/// whether it was one of ours, counted. On the faulting thread, in the
/// handler: no allocation but the list of pages to arm again.
pub(crate) fn fault(at: usize, write: bool, site: usize) -> bool {
    if !ON.load(Ordering::Relaxed) {
        return false;
    }
    // (Whoever holds the table to arm takes no fault meanwhile.)
    let t = TABLE.read().unwrap_or_else(|e| e.into_inner());
    count(&t, at, write, site)
}

fn count(t: &Table, at: usize, write: bool, site: usize) -> bool {
    let Some(probed) = t.find(at) else {
        return false;
    };
    let (from, to) = ours();
    if crate::safecopy::failing_exit(site).is_some() || (from..to).contains(&site) {
        probed.ours.fetch_add(1, Ordering::Relaxed);
        AGAIN.lock().unwrap_or_else(|e| e.into_inner()).push(at & !(PAGE - 1));
    } else if write {
        probed.writes.fetch_add(1, Ordering::Relaxed);
    } else {
        probed.read_at(site);
    }
    true
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

/// The probe's line: for each copy armed since the last, what touched it.
pub(crate) fn line(frames: f64) {
    if !ON.load(Ordering::Relaxed) {
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

    /// A guard page is armed, its first touch is told here (and goes on),
    /// the second is not a fault, and the guards see its protection as
    /// before.
    #[test]
    fn a_touched_page_is_counted_once_and_the_guards_see_through_the_bit() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualQuery,
        };
        crate::watch::install();
        ON.store(true, Ordering::Relaxed);
        // SAFETY: fresh memory of our own.
        let at = unsafe { VirtualAlloc(std::ptr::null(), 4 * PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        let probed = {
            let mut t = TABLE.write().unwrap();
            let i = t.copy(at, 4 * PAGE);
            t.copies[i].clone()
        };
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
        // A test's reads are in our image: ours, and armed again.
        assert_eq!(probed.ours.load(Ordering::Relaxed), 2);
        assert_eq!(still_armed(at, at + 4 * PAGE), 0);
        again();
        assert_eq!(still_armed(at, at + 4 * PAGE), 2);
        // A title's read (from elsewhere) is counted at its site.
        assert!(fault(at, false, 0x1000));
        assert_eq!(probed.reads.load(Ordering::Relaxed), 1);
        assert!(!fault(at + 4 * PAGE, false, 0x1000), "not a probed copy's");
        // SAFETY: as above; the page is left plain.
        unsafe { std::ptr::read_volatile(at as *const u64) };
        unsafe { std::ptr::read_volatile((at + PAGE) as *const u64) };
        // SAFETY: ours.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
        *TABLE.write().unwrap() = Table::default();
        AGAIN.lock().unwrap().clear();
        ON.store(false, Ordering::Relaxed);
    }
}
