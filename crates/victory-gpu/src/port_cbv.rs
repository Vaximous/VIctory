//! Constant buffers read by the card as raw bytes where the host already has
//! them (`VICTORY_PORT_CBV`, on by default; `0` turns it off; read in [`on`]).
//!
//! What the console allows: a constant buffer anywhere on 16 bytes. What
//! D3D12 asks: a constant buffer view on 256. So every constant buffer a
//! draw's tables name was copied afresh at each submission: its bytes, out
//! of title memory on the CPU, into upload memory of the list's on 256, and
//! a view of that ([`crate::draw`]'s tables, `defer_constants`): ~22,000 a
//! frame of the city, ~9 MB, and as many steps of the submitting thread.
//!
//! What is done instead: shaders are translated to read every constant
//! buffer they take from a view heap slot as raw bytes
//! (`victory_shader::emit::constants_raw`), and every slot the host makes of
//! a title's constant buffer is a raw view of its bytes, which needs them on
//! 4 bytes only. A table's constant buffer that lies in a host copy of title
//! memory (`crate::shadow`, as every title buffer a view names does) is then
//! a raw view of that copy, brought up to date at submission as any buffer
//! view's range is ([`crate::shadow::Shadow::refresh`]), with no copy of its
//! own; one that does not (or that the host cannot view raw) is copied as
//! before, and its slot is a raw view of the copy.
//!
//! The bounds: a raw view's size is exactly the title's constant buffer size
//! (rounded down to whole dwords), never more than the host buffer holds
//! past its offset ([`raw_window`]). D3D12 bounds-checks buffer views, so a
//! read past the view's end gives 0 on every host, as the console's past a
//! buffer's records does; a raw read through a constant buffer view would
//! have no such end (AMD: none at all).
//!
//! Root constant buffers (an address without a size) stay constant buffer
//! views.
//!
//! What it assumes: what the card reads of a constant buffer at a draw is
//! what the title wrote before it submitted, as for every other title
//! buffer a view names. The copy at submission took the same moment.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether constant buffers are read as raw bytes: the shaders' side
/// (`victory_shader::emit::constants_raw`) and the host's views follow the
/// same switch.
pub(crate) fn on() -> bool {
    victory_shader::emit::constants_raw()
}

/// Whether a table's constant buffer in a host copy of title memory is viewed
/// there (on with [`on`]; `VICTORY_PORT_CBV_PLACE=0`: copied as before, and
/// read as raw bytes all the same). With `VICTORY_AB=<s>,VICTORY_PORT_CBV_PLACE`
/// (diagnostics builds) on and off while the title runs.
pub(crate) fn in_place_on() -> bool {
    on() && crate::toggled!("VICTORY_PORT_CBV_PLACE")
}

/// Since the last line: tables' constant buffers viewed where they lie, their
/// bytes; copied as before (no host copy holds them, or not on 4 bytes in it).
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A table's constant buffer of `size` bytes was viewed where it lies.
pub(crate) fn in_place(size: u32) {
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(u64::from(size), Relaxed);
}

/// A table's constant buffer was copied as before.
pub(crate) fn copied() {
    STATS[2].fetch_add(1, Relaxed);
}

/// The most elements a buffer view takes (`D3D12_REQ_BUFFER_RESOURCE_TEXEL_COUNT_2_TO_EXP`).
const MOST_ELEMENTS: u64 = 1 << 27;

/// The raw view (`R32_TYPELESS`, first element and count, in dwords) of the
/// constant buffer of `size` bytes at byte `offset` of a host buffer of
/// `width` bytes; `None` if there is none: not on 4 bytes (the console puts
/// them on 16), outside the buffer, or less than a dword.
///
/// It covers the title's `size` bytes and no more (a read past them gives 0,
/// as on the console), and never more than the buffer holds past `offset`
/// (a view past a resource's end is refused, or worse).
pub(crate) fn raw_window(offset: u64, width: u64, size: u32) -> Option<(u64, u32)> {
    if !offset.is_multiple_of(4) || offset >= width {
        return None;
    }
    let bytes = u64::from(size).min(width - offset) & !3;
    let count = (bytes / 4).min(MOST_ELEMENTS);
    (count > 0).then_some((offset / 4, count as u32))
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64 / frames.max(1e-9)).collect();
    tracing::info!(
        "constant buffers as raw bytes (VICTORY_PORT_CBV) per frame: {:.0} tables' constant buffers viewed where they lie \
         ({:.1} KB not copied); {:.0} copied as before",
        s[0],
        s[1] / 1024.0,
        s[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_windows_hold_the_titles_bytes_and_no_more() {
        // On 16 bytes, inside: exactly the title's size.
        assert_eq!(raw_window(0x1010, 0x10_0000, 480), Some((0x1010 / 4, 120)));
        // Never past the buffer's end.
        assert_eq!(raw_window(0xff00, 0x1_0000, 4096), Some((0xff00 / 4, 64)));
        // Whole dwords only.
        assert_eq!(raw_window(0, 0x1_0000, 18), Some((0, 4)));
        assert_eq!(raw_window(0, 0x1_0000, 3), None);
        assert_eq!(raw_window(0xfffe, 0x1_0000, 16), None);
        // Not on 4 bytes, or outside.
        assert_eq!(raw_window(0x1002, 0x10_0000, 16), None);
        assert_eq!(raw_window(0x1_0000, 0x1_0000, 16), None);
        // A size the title's heap holds in slots it does not use.
        assert_eq!(raw_window(0, u64::MAX, u32::MAX), Some((0, 1 << 27)));
    }
}
