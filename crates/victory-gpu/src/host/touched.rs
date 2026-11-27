//! Which view-heap slots shaders read through (`VICTORY_SLOT_FEEDBACK`, on
//! by default; [`victory_shader::emit::SLOT_TOUCHED`]): a buffer in the
//! card's memory, bound to every translated shader that makes a handle from
//! the view heap, where such a shader stores the current epoch into the
//! slot's dword. Every [`frames_per_epoch`] frames one small list starts the
//! next epoch (dword 0) and, when asked for since, copies the buffer into
//! memory the PC reads (nobody asking costs nothing but the stores). The
//! slots' epochs are read with [`slot_epochs`] and [`touched_slots`]: a slot
//! whose last epoch is old has not been read by any shader since. Only
//! shaders read through slots: copies, clears and the host's own passes do
//! not count.

use std::sync::{Arc, Mutex};

use victory_shader::emit::{SLOT_TOUCHED_SLOTS, slot_feedback};
use windows::Win32::Graphics::Direct3D12::*;

use super::{Host, host, name, transition};

/// Bytes of the buffer: dword 0 the epoch, then a dword per slot (its last epoch), then
/// another (the finest level asked for: `epoch << 4 | 15 - level`).
const BYTES: u64 = (2 * SLOT_TOUCHED_SLOTS as u64 + 1) * 4;

/// `VICTORY_SLOT_FEEDBACK_FRAMES`: frames an epoch lasts (8 by default).
pub fn frames_per_epoch() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_SLOT_FEEDBACK_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|&n: &u64| n > 0)
            .unwrap_or(8)
    })
}

/// `VICTORY_SLOT_FEEDBACK_EVERY`: shaders note their slots on one frame in this many
/// (8 by default; 1: every frame). On the others dword 0 holds 0, and the translated
/// shaders leave their notes out (`victory_shader` guards each on a non-zero epoch).
/// The feedback cost about 10 ms of the card's time a frame noted every frame (runs
/// ow11, ow13 against ow9).
pub fn feedback_every() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_SLOT_FEEDBACK_EVERY")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|&n: &u64| n > 0)
            .unwrap_or(8)
    })
}

struct Feedback {
    card: ID3D12Resource,
    /// What the PC reads (and, zeroed, the first fill of `card`).
    back: ID3D12Resource,
    mapped: *const u32,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    list2: ID3D12GraphicsCommandList2,
    fence: ID3D12Fence,
    /// Lists sent (the fence's value once they ran).
    sent: u64,
    /// The epoch shaders store now.
    epoch: u32,
    /// The frame the last epoch began.
    began: u64,
    /// Per slot, the last epoch a shader read through it (as of the last
    /// copy that ran), and that copy's epoch.
    seen: Arc<Vec<u32>>,
    seen_epoch: u32,
    /// Per slot, as brought back: `epoch << 4 | 15 - level` of the finest level a
    /// sample through it asked for in its last epoch (0: none).
    levels: Arc<Vec<u32>>,
    /// The last list sent copied the buffer back.
    copied: bool,
    /// The epoch the copy back was made in.
    copied_epoch: u32,
    /// What dword 0 holds now: the epoch, or 0 on frames without notes.
    shown: u32,
}

// SAFETY: free-threaded objects; the mapping is read under the mutex.
unsafe impl Send for Feedback {}

static FEEDBACK: Mutex<Option<Option<Feedback>>> = Mutex::new(None);

/// The card address of the buffer for a shader that notes its slots, made on
/// first use; `None` when it could not be made (the shader is not bound
/// then: its stores go nowhere).
pub(crate) fn address() -> Option<u64> {
    // (Asked at every bind of such a shader: no lock once known.)
    static ADDRESS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *ADDRESS.get_or_init(make_address)
}

fn make_address() -> Option<u64> {
    let mut f = FEEDBACK.lock().unwrap_or_else(|e| e.into_inner());
    if f.is_none() {
        *f = Some(host().and_then(|h| match h.make_feedback() {
            Ok(made) => {
                tracing::info!(
                    slots = SLOT_TOUCHED_SLOTS,
                    frames = frames_per_epoch(),
                    "slot feedback: shaders note the view-heap slots they read through"
                );
                Some(made)
            }
            Err(e) => {
                tracing::warn!(error = %e, "slot feedback: no buffer");
                None
            }
        }));
    }
    let made = f.as_ref()?.as_ref()?;
    // SAFETY: a live buffer.
    Some(unsafe { made.card.GetGPUVirtualAddress() })
}

/// Asked for since the last copy was taken: the next epoch's list copies
/// the buffer back (no copy while nobody asks).
static WANTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// At each shown frame: every [`frames_per_epoch`] frames, takes what the
/// last copy brought back (if one was sent) and starts the next epoch, with
/// a copy back when [`slot_epochs`] was asked since.
pub(crate) fn tick(frame: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if slot_feedback() == 0 {
        return;
    }
    let mut f = FEEDBACK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(fb)) = f.as_mut() else {
        return;
    };
    if fb.sent > 0 && frame < fb.began + frames_per_epoch() {
        // Notes on one frame in [`feedback_every`] of the epoch.
        let want = if (frame - fb.began).is_multiple_of(feedback_every()) { fb.epoch } else { 0 };
        if want != fb.shown {
            // SAFETY: as below.
            unsafe { show(fb, want) };
        }
        return;
    }
    // SAFETY: plain queries and recording on objects of our own; the
    // allocator is reset only after the list that used it ran.
    unsafe {
        if fb.fence.GetCompletedValue() < fb.sent {
            return;
        }
        if fb.copied {
            let n = SLOT_TOUCHED_SLOTS as usize;
            let words = std::slice::from_raw_parts(fb.mapped, 2 * n + 1);
            fb.seen = Arc::new(words[1..=n].to_vec());
            fb.levels = Arc::new(words[n + 1..].to_vec());
            fb.seen_epoch = if words[0] != 0 { words[0] } else { fb.copied_epoch };
            fb.copied = false;
        }
        let Some(h) = host() else { return };
        if fb.allocator.Reset().is_err() || fb.list.Reset(&fb.allocator, None).is_err() {
            return;
        }
        let (card, back) = (&fb.card, &fb.back);
        let next = if fb.sent == 0 {
            // The first list fills the buffer with zeros (`back` is zeroed).
            fb.list.CopyBufferRegion(card, 0, back, 0, BYTES);
            D3D12_RESOURCE_STATE_COPY_DEST
        } else if WANTED.swap(false, Relaxed) {
            fb.list.CopyBufferRegion(back, 0, card, 0, BYTES);
            fb.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            fb.copied = true;
            fb.copied_epoch = fb.epoch;
            D3D12_RESOURCE_STATE_COPY_DEST
        } else {
            D3D12_RESOURCE_STATE_COPY_DEST
        };
        fb.epoch += 1;
        let words = [D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: card.GetGPUVirtualAddress(),
            Value: fb.epoch,
        }];
        fb.list2.WriteBufferImmediate(1, words.as_ptr(), None);
        fb.list
            .ResourceBarrier(&[transition(card, next, D3D12_RESOURCE_STATE_COMMON)]);
        if fb.list.Close().is_err() {
            return;
        }
        h.queue.ExecuteCommandLists(&[Some(fb.list.clone().into())]);
        fb.sent += 1;
        let _ = h.queue.Signal(&fb.fence, fb.sent);
    }
    fb.began = frame;
    fb.shown = fb.epoch;
}

/// Dword 0 of the buffer becomes `value` (the epoch, or 0: no notes), in a list of its
/// own once the last one ran (if it did not yet: at a later frame).
///
/// # Safety
/// The list and allocator are reset only after the list that used them ran.
unsafe fn show(fb: &mut Feedback, value: u32) {
    let Some(h) = host() else { return };
    // SAFETY: the caller's.
    unsafe {
        if fb.fence.GetCompletedValue() < fb.sent {
            return;
        }
        if fb.allocator.Reset().is_err() || fb.list.Reset(&fb.allocator, None).is_err() {
            return;
        }
        let card = &fb.card;
        fb.list.ResourceBarrier(&[transition(card, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST)]);
        let words = [D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: card.GetGPUVirtualAddress(), Value: value }];
        fb.list2.WriteBufferImmediate(1, words.as_ptr(), None);
        fb.list.ResourceBarrier(&[transition(card, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
        if fb.list.Close().is_err() {
            return;
        }
        h.queue.ExecuteCommandLists(&[Some(fb.list.clone().into())]);
        fb.sent += 1;
        let _ = h.queue.Signal(&fb.fence, fb.sent);
    }
    fb.shown = value;
}

/// The epoch shaders note now (0 before the first), and per view-heap slot
/// the last epoch a shader read through it (0: never), as of the last copy
/// brought back. The buffer is copied back only when asked: the first call
/// asks (and gives `None`), later ones give the last copy and ask for the
/// next, one to two epochs ([`frames_per_epoch`] frames each) later. `None`
/// with `VICTORY_SLOT_FEEDBACK=0` or before a copy came back.
pub fn slot_epochs() -> Option<(u32, Arc<Vec<u32>>)> {
    WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let f = FEEDBACK.lock().unwrap_or_else(|e| e.into_inner());
    let fb = f.as_ref()?.as_ref()?;
    (fb.seen_epoch > 0).then(|| (fb.seen_epoch, fb.seen.clone()))
}

/// The view-heap slots a shader read through in the last `epochs` epochs
/// (of [`frames_per_epoch`] frames) before the last copy brought back, as
/// bits: slot `i` is bit `i % 64` of word `i / 64`. `None` as for
/// [`slot_epochs`].
pub fn touched_slots(epochs: u32) -> Option<Vec<u64>> {
    let (now, seen) = slot_epochs()?;
    let from = now.saturating_sub(epochs.max(1));
    let mut bits = vec![0u64; seen.len().div_ceil(64)];
    for (i, &e) in seen.iter().enumerate() {
        if e != 0 && e > from {
            bits[i / 64] |= 1 << (i % 64);
        }
    }
    Some(bits)
}

/// Per view-heap slot, the finest mip level (0 the largest) a sample through it asked
/// for in the last epochs epochs before the last copy brought back, 255 where none
/// did (a slot only loaded from, or not read at all). The level is the one the
/// sample's derivatives select in a pixel program (bias and least level left out),
/// the one named elsewhere; 15 stands for 15 or coarser. Within an epoch the finest
/// wins; a later epoch's level replaces an earlier one's. None as for
/// [slot_epochs] (and with VICTORY_SLOT_LOD=0 every slot reads 255).
pub fn slot_min_lod(epochs: u32) -> Option<Vec<u8>> {
    WANTED.store(true, std::sync::atomic::Ordering::Relaxed);
    let levels = {
        let f = FEEDBACK.lock().unwrap_or_else(|e| e.into_inner());
        let fb = f.as_ref()?.as_ref()?;
        if fb.seen_epoch == 0 {
            return None;
        }
        (fb.seen_epoch, fb.levels.clone())
    };
    let (now, levels) = levels;
    let from = now.saturating_sub(epochs.max(1));
    Some(
        levels
            .iter()
            .map(|&v| {
                if v >> 4 > from {
                    15 - (v & 15) as u8
                } else {
                    255
                }
            })
            .collect(),
    )
}

impl Host {
    fn make_feedback(&self) -> windows::core::Result<Feedback> {
        let empty = || windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY);
        let card = self.uav_buffer(BYTES).ok_or_else(empty)?;
        name(&card, || "slot feedback".into());
        let (back, mapped) = self.shared_buffer(BYTES).ok_or_else(empty)?;
        name(&back, || "slot feedback: brought back".into());
        // SAFETY: plain object creation; a fresh mapping of that size.
        unsafe {
            std::ptr::write_bytes(mapped, 0, BYTES as usize);
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            )?;
            list.Close()?;
            let list2 = windows::core::Interface::cast(&list)?;
            let fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            Ok(Feedback {
                card,
                back,
                mapped: mapped.cast::<u32>(),
                allocator,
                list,
                list2,
                fence,
                sent: 0,
                epoch: 0,
                began: 0,
                seen: Arc::new(Vec::new()),
                seen_epoch: 0,
                levels: Arc::new(Vec::new()),
                copied: false,
                copied_epoch: 0,
                shown: 0,
            })
        }
    }
}
