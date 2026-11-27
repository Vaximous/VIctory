//! Diagnostics (`VICTORY_LEDGER=1`; off by default): a ring of our own
//! writes into title memory, the last 131,072 of them (some 5 seconds of
//! play), each with its address, length, the path that wrote it, the frame
//! and the thread. A crash in the title's walk of its heap's nodes (image
//! +0x4e9b578, +0x4e9b54a: the node in r15, its record in rsi or rcx) is
//! looked up in it, and what of ours wrote over those bytes is said in the
//! crash report.
//!
//! Landings merge whole pieces by the dword, which says nothing of the
//! bytes they changed; of each 64 KiB block's last 0x18 bytes (where the
//! nodes lie) the dwords a landing changed are noted one by one, with what
//! they held before and after.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// The paths that write title memory.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum Path {
    /// A landing's merge of what the card wrote (`land.rs`).
    Landing = 1,
    /// A dword of a block's end a landing changed (before and after kept).
    LandingEnd = 2,
    /// What shaders wrote, put back into title memory (`shadow.rs`).
    PutBack = 3,
    /// `copy_mapped` into title memory.
    CopyMapped = 4,
    /// A texture read-back merged in by the dword (`texback.rs`).
    Texback = 5,
    /// What shaders wrote, written whole (`host::write_mapped`).
    WriteMapped = 6,
    /// A write the kernel made for the title (a file read into its memory;
    /// `watch::kernel_wrote`): the title's, not ours, but it lands too.
    Kernel = 7,
}

const NAMES: [&str; 8] = ["?", "landing", "block end changed", "put back", "copy_mapped", "texture read-back", "write_mapped", "the title's file read (kernel)"];

const N: usize = 1 << 17;

struct Entry {
    at: AtomicU64,
    /// Length, path << 48.
    len: AtomicU64,
    /// Frame, thread id << 40.
    when: AtomicU64,
    /// For [`Path::LandingEnd`]: before << 32 | after.
    extra: AtomicU64,
}

static RING: [Entry; N] = [const { Entry { at: AtomicU64::new(0), len: AtomicU64::new(0), when: AtomicU64::new(0), extra: AtomicU64::new(0) } }; N];
static NEXT: AtomicUsize = AtomicUsize::new(0);

pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_LEDGER").is_some_and(|v| v == "1");
        if on {
            tracing::info!("our writes into title memory are kept in a ring for crash reports (VICTORY_LEDGER)");
        }
        on
    })
}

fn thread() -> u64 {
    // SAFETY: no arguments.
    u64::from(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() })
}

fn put(at: usize, len: usize, path: Path, extra: u64) {
    let e = &RING[NEXT.fetch_add(1, Relaxed) % N];
    e.at.store(0, Relaxed);
    e.len.store(len as u64 & 0xffff_ffff_ffff | (path as u64) << 48, Relaxed);
    e.when.store(crate::frame::current() & 0xff_ffff_ffff | thread() << 40, Relaxed);
    e.extra.store(extra, Relaxed);
    e.at.store(at as u64, Relaxed);
}

/// Notes our write of `[at, at + len)` of title memory by `path`.
pub(crate) fn note(at: usize, len: usize, path: Path) {
    if on() && len > 0 {
        put(at, len, path, 0);
    }
}

/// The ends of 64 KiB blocks in `[at, at + len)`: the last 0x18 bytes of each.
fn ends(at: usize, len: usize) -> impl Iterator<Item = usize> {
    const BLOCK: usize = 64 << 10;
    let end = at + len;
    (at.div_ceil(BLOCK).max(1)..=end / BLOCK).map(|b| b * BLOCK - 0x18).filter(move |&e| e >= at && e + 0x18 <= end)
}

fn read6(at: usize) -> Option<[u32; 6]> {
    let mut b = [0u8; 0x18];
    crate::host::read_into(at as *const u8, &mut b).then(|| std::array::from_fn(|i| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())))
}

/// Our write into title memory `[at, at + len)` by `path` (`write` does
/// it): noted, and the dwords of block ends it changed one by one.
pub(crate) fn writing<R>(at: usize, len: usize, path: Path, write: impl FnOnce() -> R) -> R {
    if !on() {
        return write();
    }
    let before: Vec<(usize, Option<[u32; 6]>)> = ends(at, len).map(|e| (e, read6(e))).collect();
    let r = write();
    put(at, len, path, 0);
    for (e, was) in before {
        let (Some(was), Some(now)) = (was, read6(e)) else { continue };
        for i in 0..6 {
            if was[i] != now[i] {
                put(e + i * 4, 4, Path::LandingEnd, u64::from(was[i]) << 32 | u64::from(now[i]));
            }
        }
    }
    r
}

/// The ring's entries that touch `[lo, hi)`, oldest first, as lines.
fn hits(lo: u64, hi: u64) -> Vec<String> {
    let next = NEXT.load(Relaxed);
    let first = next.saturating_sub(N);
    let mut out = Vec::new();
    for k in first..next {
        let e = &RING[k % N];
        let at = e.at.load(Relaxed);
        let len_path = e.len.load(Relaxed);
        let len = len_path & 0xffff_ffff_ffff;
        if at == 0 || at >= hi || at + len <= lo {
            continue;
        }
        let when = e.when.load(Relaxed);
        let path = NAMES.get((len_path >> 48) as usize).copied().unwrap_or("?");
        let extra = e.extra.load(Relaxed);
        let detail = if (len_path >> 48) as u8 == Path::LandingEnd as u8 {
            format!(" {:#010x} -> {:#010x}", extra >> 32, extra & 0xffff_ffff)
        } else {
            String::new()
        };
        out.push(format!(
            "    {path} {at:#x}+{len:#x} frame {} thread {}{detail} ({} writes ago)",
            when & 0xff_ffff_ffff,
            when >> 40,
            next - k
        ));
    }
    out
}

/// For the crash report: at the heap-node walk's faults, the node's and its
/// record's bytes and what of ours wrote them. `regs`: rax rcx rdx rbx rsp
/// rbp rsi rdi r8-r15.
pub fn crash_lines(location: &str, regs: &[u64; 16]) -> Vec<String> {
    let (rcx, rsi, r15) = (regs[1], regs[6], regs[15]);
    let record = if location.ends_with("+0x4e9b578") {
        rsi
    } else if location.ends_with("+0x4e9b54a") {
        rcx
    } else {
        return Vec::new();
    };
    let mut out = vec![format!(
        "  ledger: our writes into title memory, the last {} of {} kept (VICTORY_LEDGER)",
        NEXT.load(Relaxed).min(N),
        NEXT.load(Relaxed)
    )];
    for (name, at) in [("node (r15)", r15), ("node's record", record)] {
        let bytes = read6(at as usize).map_or("not readable".to_string(), |d| format!("{d:08x?}"));
        let found = hits(at, at + 0x18);
        out.push(format!("  ledger: {name} {at:#x}+0x18: {bytes}; {} of our writes touched it", found.len()));
        out.extend(found.into_iter().rev().take(40));
    }
    // The node's 64 KiB block, for context.
    let block = r15 & !0xffff;
    let found = hits(block, block + 0x10000);
    out.push(format!("  ledger: the node's block {block:#x}+0x10000: {} of our writes touched it, the last 20:", found.len()));
    out.extend(found.into_iter().rev().take(20));
    out
}
