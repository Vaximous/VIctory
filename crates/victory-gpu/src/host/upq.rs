//! What goes up to copies in the card's memory crosses the bus on a copy
//! queue of its own (`VICTORY_UP_QUEUE`, on by default; `0`: on the drawing
//! queue, as before; A/B-able with `VICTORY_AB=<s>,VICTORY_UP_QUEUE`).
//!
//! Without it the drawing queue copies a submission's sends out of the PC's
//! memory (the submission's own, `send`, or the copies' CPU sides) before
//! the submission's lists: PCIe-bound copies in which the drawing queue does
//! nothing else (in the replay this was measured on, 269 copies and 76.6 MB,
//! about 5 ms a frame). With it the copy queue brings the same bytes into a
//! buffer of ours in the card's memory as soon as the submission is
//! prepared, while the drawing queue still runs earlier submissions; the
//! drawing queue waits for that copy (a fence, queued ahead of the list
//! that reads our buffer) and then sends from our buffer, in the card's
//! memory (about 0.2 ms for those 76 MB), at the same place in its order as
//! before. So every list of the submission and of later ones finds the new
//! bytes and no earlier one does: the card sides are still written only by
//! the drawing queue, and the copy queue writes nothing but our buffer.
//!
//! With the sends of `send` (`VICTORY_SEND_OWN`) the bytes are in the
//! submission's own memory: that comes up whole, in one copy, and the sends
//! (copies, and the pairs of offset and word the scatter pass reads) read
//! our buffer instead. Without them each changed range comes up from the
//! copy's CPU side.
//!
//! The copy queue reads the PC's memory when the drawing queue would have,
//! or earlier, and never before the bytes were put there (it is sent after
//! that, on this thread). Our buffer and list belong to the submission's
//! plan (`CardPlan::up`) and come back with it when its submission has run
//! (`Host::card_done`), so neither is written again while the card may
//! still read it. Our buffer is made in the common state; buffers decay to
//! it at the end of every execution, so each use (copy destination on the
//! copy queue, copy source and shader resource on the drawing queue) is an
//! implicit promotion.
//!
//! Ported from VIctory-Belobug-AMD d3a6c2c3da and 53ed64ec41 (GPL-3.0).

use std::sync::atomic::AtomicU64;

use super::*;

/// Whether it is on now (`VICTORY_UP_QUEUE`, or its phase of `VICTORY_AB`).
pub(super) fn on() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_UP_QUEUE") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UP_QUEUE").is_none_or(|v| v != "0"))
}

/// The most bytes one submission sends this way; more go as before (our
/// buffers are kept per submission in the air, so they are not let grow
/// without end).
const MOST: u64 = 512 << 20;

/// Our buffers are made in sizes of powers of two from this on.
const LEAST: u64 = 4 << 20;

/// Each range's place in our buffer is aligned to this.
const ALIGN: u64 = 256;

/// One submission's: a copy list and our buffer in the card's memory.
pub(super) struct Slot {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    bounce: Option<(ID3D12Resource, u64)>,
}

// SAFETY: free-threaded D3D12 objects, used by one submission at a time.
unsafe impl Send for Slot {}

impl Slot {
    /// Our buffer, which holds what went up.
    pub(super) fn bounce(&self) -> Option<&ID3D12Resource> {
        self.bounce.as_ref().map(|b| &b.0)
    }
}

struct Queue {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`.
    sent: u64,
    free: Vec<Slot>,
}

// SAFETY: as `Slot`.
unsafe impl Send for Queue {}

/// `Some(None)`: none could be made (then nothing goes this way).
static QUEUE: Mutex<Option<Option<Queue>>> = Mutex::new(None);

/// Since the last frame-rate line: submissions, bytes, ranges sent this
/// way; submissions that went as before because they were too big or
/// something could not be made.
pub(super) static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn make_queue(device: &ID3D12Device) -> Option<Queue> {
    // SAFETY: plain object creation.
    let made = unsafe {
        (|| -> windows::core::Result<Queue> {
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                Priority: D3D12_COMMAND_QUEUE_PRIORITY_HIGH.0,
                ..Default::default()
            })?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            Ok(Queue {
                queue,
                fence,
                sent: 0,
                free: Vec::new(),
            })
        })()
    };
    match made {
        Ok(q) => {
            name(&q.queue, || "copies in the card's memory: up queue".into());
            Some(q)
        }
        Err(e) => {
            tracing::warn!(error = %e, "no copy queue for what goes up to copies in the card's memory: it goes on the drawing queue");
            None
        }
    }
}

fn make_slot(device: &ID3D12Device) -> Option<Slot> {
    // SAFETY: plain object creation.
    unsafe {
        (|| -> windows::core::Result<Slot> {
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.Close()?;
            Ok(Slot {
                allocator,
                list,
                bounce: None,
            })
        })()
        .ok()
    }
}

/// One range: the PC's buffer it comes from, its offset there, its bytes.
pub(super) type Range<'a> = (&'a ID3D12Resource, u64, u64);

/// Sends `ranges` up on the copy queue into our buffer and has `drawing`
/// wait for that before anything sent to it after this; then `record` gets,
/// for each range, our buffer and the range's offset in it, to send from
/// there in the drawing queue's list. `None`: nothing was sent (nothing to
/// send, too many bytes, or nothing could be made), and `record` was not
/// called; the caller sends as before.
///
/// # Safety
/// Live buffers, the ranges lie in them; the PC's buffers hold the bytes to
/// send; `drawing` is the queue the lists that read our buffer go to, and
/// the next of them is the one `record` records into.
pub(super) unsafe fn send(
    host: &Host,
    drawing: &ID3D12CommandQueue,
    ranges: &[Range<'_>],
    mut record: impl FnMut(usize, &ID3D12Resource, u64),
) -> Option<Slot> {
    let total: u64 = ranges.iter().map(|r| r.2.next_multiple_of(ALIGN)).sum();
    if total == 0 {
        return None;
    }
    if total > MOST {
        STATS[3].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut guard = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(q) = guard.get_or_insert_with(|| make_queue(&host.device)).as_mut() else {
        STATS[3].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let Some(mut slot) = q.free.pop().or_else(|| make_slot(&host.device)) else {
        STATS[3].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    if slot.bounce.as_ref().is_none_or(|b| b.1 < total) {
        let size = total.next_power_of_two().max(LEAST);
        slot.bounce = None;
        let Some(made) = host.uav_buffer(size) else {
            q.free.push(slot);
            STATS[3].fetch_add(1, Ordering::Relaxed);
            return None;
        };
        name(&made, || format!("copies in the card's memory: what goes up, {} MB", size >> 20));
        slot.bounce = Some((made, size));
    }
    let Some(bounce) = slot.bounce().cloned() else {
        q.free.push(slot);
        return None;
    };
    // SAFETY: the slot's last submission has run (it came back with its
    // plan); the caller's for the buffers.
    let sent = unsafe {
        (|| -> windows::core::Result<()> {
            slot.allocator.Reset()?;
            slot.list.Reset(&slot.allocator, None)?;
            let mut at = 0u64;
            for &(pc, offset, bytes) in ranges {
                slot.list.CopyBufferRegion(&bounce, at, pc, offset, bytes);
                at += bytes.next_multiple_of(ALIGN);
            }
            slot.list.Close()?;
            q.queue.ExecuteCommandLists(&[Some(slot.list.cast()?)]);
            q.queue.Signal(&q.fence, q.sent + 1)
        })()
    };
    if let Err(e) = sent {
        // (Nothing waits for it. The list may have gone to the queue and
        // only the signal failed: the slot is let go of unreleased, not
        // reused, as the card may still be running it.)
        tracing::warn!(error = %e, "what goes up to copies in the card's memory could not go on the copy queue: as before");
        std::mem::forget(slot);
        STATS[3].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    q.sent += 1;
    // SAFETY: a fence of ours that the copy queue signals.
    if let Err(e) = unsafe { drawing.Wait(&q.fence, q.sent) } {
        // The drawing queue cannot be made to wait: wait here, so that what
        // follows still finds the bytes.
        tracing::warn!(error = %e, "the drawing queue could not wait for the up queue: waited on the CPU");
        // SAFETY: as above; a null event waits until the value is reached.
        let _ = unsafe { q.fence.SetEventOnCompletion(q.sent, HANDLE::default()) };
    }
    drop(guard);
    let mut at = 0u64;
    for (i, r) in ranges.iter().enumerate() {
        record(i, &bounce, at);
        at += r.2.next_multiple_of(ALIGN);
    }
    STATS[0].fetch_add(1, Ordering::Relaxed);
    STATS[1].fetch_add(ranges.iter().map(|r| r.2).sum::<u64>(), Ordering::Relaxed);
    STATS[2].fetch_add(ranges.len() as u64, Ordering::Relaxed);
    Some(slot)
}

/// A plan's slot, back once its submission has run.
pub(super) fn give_back(slot: Slot) {
    let mut guard = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Some(q)) = guard.as_mut() {
        q.free.push(slot);
    }
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let [subs, bytes, ranges, before] = [0, 1, 2, 3].map(|i| STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if subs + before > 0.0 {
        tracing::info!(
            "sends to copies in the card's memory brought up on a copy queue per frame (VICTORY_UP_QUEUE): {subs:.1} submissions, \
             {ranges:.0} ranges, {:.1} MB; {before:.1} submissions sent up on the drawing queue (too big, or nothing to send with)",
            bytes / 1048576.0
        );
    }
}
