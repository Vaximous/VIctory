//! Host copies of streamed textures as reserved (tiled) resources: only
//! the levels whose title memory is mapped take card memory.
//!
//! A texture the title streams in has its small levels mapped first and
//! its large ones later, or never while it is far away. A committed host
//! copy takes the whole chain on the card all the same: more than half of
//! the card's texture memory held for levels the title has not loaded, and
//! the card at 9.9 of 10.8 GB in use, past where the system starts paging
//! it to the PC's memory (runs fxdef1, fxg6: 0.1-2 fps).
//!
//! With [`on`], a host copy of a texture the title fills, made while part
//! of its title memory is not mapped, is a reserved resource. Before each
//! upload (`crate::texture::Staging::detile`, on the submitting thread,
//! before the submission's lists run) the subresources whose title memory
//! is now mapped get 64 KiB tiles of card memory from a shared pool
//! ([`Tiles::map`]); the others stay unmapped. With tiled resources tier 2
//! or more, reads of unmapped tiles return zeros, as unmapped title memory
//! detiled did, and writes to them (the upload's copy) are dropped.
//! A copy the host itself writes (a copy or resolve into it, the alias
//! copy of another format) has all of its tiles mapped first
//! ([`Tiles::map_all`]).
//!
//! The tiles go back to the pool when the host copy is dropped, which is
//! after the card finished with it (`crate::host::retire`).
//! `VICTORY_TILED_TEXTURES=0`: committed host copies, as before.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

/// Tiles of a pool heap: 64 MiB.
const HEAP_TILES: u32 = 1024;
const TILE: u64 = 64 << 10;

/// Whether streamed textures' host copies are reserved resources: on
/// unless `VICTORY_TILED_TEXTURES=0`, and only with tiled resources tier 2
/// or more.
pub(crate) fn on(device: &ID3D12Device) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if std::env::var_os("VICTORY_TILED_TEXTURES").is_some_and(|v| v == "0") {
            return false;
        }
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: a plain query that fills our structure.
        let asked = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        let tier = options.TiledResourcesTier.0;
        let on = asked.is_ok() && tier >= D3D12_TILED_RESOURCES_TIER_2.0;
        tracing::info!(tier, on, "tiled host copies of streamed textures");
        on
    })
}

/// Since the start: reserved host copies alive, their bytes were they
/// committed, and the bytes of their tiles mapped.
pub(crate) static TILED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The pool's heaps and their free tiles.
struct Pool {
    heaps: Vec<(ID3D12Heap, Vec<u32>)>,
}

// SAFETY: heaps are free-threaded objects.
unsafe impl Send for Pool {}

static POOL: Mutex<Pool> = Mutex::new(Pool { heaps: Vec::new() });

/// Bytes of the pool's heaps alive.
pub(crate) static POOL_BYTES: AtomicU64 = AtomicU64::new(0);

impl Pool {
    /// `n` tiles, as (heap, tile) runs of one heap each; None (and nothing
    /// taken) if a heap could not be made.
    fn take(&mut self, device: &ID3D12Device, n: u32) -> Option<Vec<(usize, Vec<u32>)>> {
        let mut out: Vec<(usize, Vec<u32>)> = Vec::new();
        let mut left = n;
        let mut i = 0;
        while left > 0 {
            if i == self.heaps.len() {
                let desc = D3D12_HEAP_DESC {
                    SizeInBytes: u64::from(HEAP_TILES) * TILE,
                    Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                    Alignment: 0,
                    Flags: D3D12_HEAP_FLAG_DENY_BUFFERS | D3D12_HEAP_FLAG_DENY_RT_DS_TEXTURES,
                };
                let mut heap: Option<ID3D12Heap> = None;
                // SAFETY: a plain description; the out pointer is ours.
                let made = unsafe { device.CreateHeap(&desc, &mut heap) };
                let Some(heap) = made.ok().and(heap) else {
                    tracing::warn!("no heap for the tiles of streamed textures");
                    for (h, tiles) in out {
                        self.heaps[h].1.extend(tiles);
                    }
                    return None;
                };
                crate::host::name(&heap, || "tiles of streamed textures".into());
                POOL_BYTES.fetch_add(desc.SizeInBytes, Ordering::Relaxed);
                self.heaps.push((heap, (0..HEAP_TILES).rev().collect()));
            }
            let free = &mut self.heaps[i].1;
            let k = (free.len() as u32).min(left);
            if k > 0 {
                let at = free.len() - k as usize;
                out.push((i, free.split_off(at)));
                left -= k;
            }
            i += 1;
        }
        Some(out)
    }

    fn give(&mut self, tiles: &[(usize, Vec<u32>)]) {
        for (h, t) in tiles {
            self.heaps[*h].1.extend_from_slice(t);
        }
    }
}

/// A reserved host copy's tiles.
pub(crate) struct Tiles {
    state: Mutex<State>,
    /// Its bytes were it committed.
    whole: u64,
}

struct State {
    mips: u32,
    slices: u32,
    /// Levels with tiles of their own; the others are packed, per slice.
    standard: u32,
    packed_tiles: u32,
    /// Tiles of each standard subresource (mip + slice * mips), and its
    /// width in tiles.
    tiles: Vec<u32>,
    widths: Vec<u32>,
    /// Whether each subresource has its tiles (a packed level: whether its
    /// slice's packed levels have).
    mapped: Vec<bool>,
    held: Vec<(usize, Vec<u32>)>,
    mapped_bytes: u64,
}

/// A reserved resource for `desc` in `state`, and its tiles' bookkeeping;
/// None if it cannot be made (the caller makes a committed one).
pub(crate) fn reserved(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<(ID3D12Resource, Tiles)> {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count > 1 {
        return None;
    }
    let mut d = *desc;
    d.Layout = D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE;
    d.Alignment = 0;
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: the description is the committed copy's with the layout
    // reserved resources take; the out pointer is ours.
    let made = unsafe { device.CreateReservedResource(&d, state, None, &mut texture) };
    let texture = made.ok().and(texture)?;
    let (mips, slices) = (u32::from(d.MipLevels), u32::from(d.DepthOrArraySize));
    let count = mips * slices;
    let mut total = 0u32;
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut n = count;
    let mut subs = vec![D3D12_SUBRESOURCE_TILING::default(); count as usize];
    // SAFETY: outputs for `count` subresources.
    unsafe {
        device.GetResourceTiling(
            &texture,
            Some(&mut total),
            Some(&mut packed),
            None,
            Some(&mut n),
            0,
            subs.as_mut_ptr(),
        );
    }
    let standard = u32::from(packed.NumStandardMips).min(mips);
    let tiles = subs
        .iter()
        .enumerate()
        .map(|(i, s)| if (i as u32 % mips) < standard { s.WidthInTiles * u32::from(s.HeightInTiles) * u32::from(s.DepthInTiles) } else { 0 })
        .collect();
    let widths = subs.iter().map(|s| s.WidthInTiles.max(1)).collect();
    // SAFETY: a plain query.
    let whole = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) }.SizeInBytes;
    TILED[0].fetch_add(1, Ordering::Relaxed);
    TILED[1].fetch_add(whole, Ordering::Relaxed);
    Some((
        texture,
        Tiles {
            state: Mutex::new(State {
                mips,
                slices,
                standard,
                packed_tiles: packed.NumTilesForPackedMips,
                tiles,
                widths,
                mapped: vec![false; count as usize],
                held: Vec::new(),
                mapped_bytes: 0,
            }),
            whole,
        },
    ))
}

impl Tiles {
    /// Maps the tiles of every subresource `wanted` (mip, slice) names
    /// that has none yet, before the submission's lists run.
    pub(crate) fn map(&self, texture: &ID3D12Resource, wanted: impl Fn(u32, u32) -> bool) {
        let Some(host) = crate::host::host() else { return };
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut coords: Vec<D3D12_TILED_RESOURCE_COORDINATE> = Vec::new();
        let mut sizes: Vec<D3D12_TILE_REGION_SIZE> = Vec::new();
        let mut marks: Vec<usize> = Vec::new();
        let mut need = 0u32;
        for slice in 0..s.slices {
            for mip in 0..s.standard {
                let i = (slice * s.mips + mip) as usize;
                if !s.mapped[i] && s.tiles[i] > 0 && wanted(mip, slice) {
                    coords.push(D3D12_TILED_RESOURCE_COORDINATE { X: 0, Y: 0, Z: 0, Subresource: i as u32 });
                    sizes.push(D3D12_TILE_REGION_SIZE { NumTiles: s.tiles[i], ..Default::default() });
                    marks.push(i);
                    need += s.tiles[i];
                }
            }
            let first_packed = (slice * s.mips + s.standard) as usize;
            if s.standard < s.mips
                && s.packed_tiles > 0
                && !s.mapped[first_packed]
                && (s.standard..s.mips).any(|mip| wanted(mip, slice))
            {
                coords.push(D3D12_TILED_RESOURCE_COORDINATE { X: 0, Y: 0, Z: 0, Subresource: first_packed as u32 });
                sizes.push(D3D12_TILE_REGION_SIZE { NumTiles: s.packed_tiles, ..Default::default() });
                marks.extend((s.standard..s.mips).map(|mip| (slice * s.mips + mip) as usize));
                need += s.packed_tiles;
            }
        }
        if need == 0 {
            return;
        }
        let Some(runs) = POOL.lock().unwrap_or_else(|e| e.into_inner()).take(&host.device, need) else {
            return;
        };
        // The regions' tiles in order, heap run by heap run: one call per
        // run, the regions cut where a run ends.
        let (mut region, mut into) = (0usize, 0u32);
        for (h, tiles) in &runs {
            let mut c: Vec<D3D12_TILED_RESOURCE_COORDINATE> = Vec::new();
            let mut z: Vec<D3D12_TILE_REGION_SIZE> = Vec::new();
            let mut left = tiles.len() as u32;
            while left > 0 {
                let of = sizes[region].NumTiles;
                let k = (of - into).min(left);
                // A region's later part starts at its tile `into`: the
                // coordinate counts tiles in the subresource's order.
                c.push(tile_coordinate(&s, coords[region].Subresource, into));
                z.push(D3D12_TILE_REGION_SIZE { NumTiles: k, ..Default::default() });
                into += k;
                left -= k;
                if into == of {
                    region += 1;
                    into = 0;
                }
            }
            let flags = vec![D3D12_TILE_RANGE_FLAG_NONE; tiles.len()];
            let counts = vec![1u32; tiles.len()];
            let heap = POOL.lock().unwrap_or_else(|e| e.into_inner()).heaps[*h].0.clone();
            // SAFETY: regions of this resource, tiles of this heap, as
            // many tiles in the ranges as in the regions.
            unsafe {
                host.queue().UpdateTileMappings(
                    texture,
                    c.len() as u32,
                    Some(c.as_ptr()),
                    Some(z.as_ptr()),
                    &heap,
                    tiles.len() as u32,
                    Some(flags.as_ptr()),
                    Some(tiles.as_ptr()),
                    Some(counts.as_ptr()),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
        }
        for i in marks {
            s.mapped[i] = true;
        }
        let bytes = u64::from(need) * TILE;
        s.mapped_bytes += bytes;
        TILED[2].fetch_add(bytes, Ordering::Relaxed);
        s.held.extend(runs);
    }

    /// Maps every tile: the host writes the copy itself.
    pub(crate) fn map_all(&self, texture: &ID3D12Resource) {
        self.map(texture, |_, _| true);
    }
}

/// The coordinate of tile `n` of subresource `sub` in its own tile order.
/// A region given by a tile count and no box runs over a standard
/// subresource's tiles row by row from its start coordinate (2D: one
/// slice of tiles); in the packed levels X counts the tiles.
fn tile_coordinate(s: &State, sub: u32, n: u32) -> D3D12_TILED_RESOURCE_COORDINATE {
    if sub % s.mips >= s.standard {
        return D3D12_TILED_RESOURCE_COORDINATE { X: n, Y: 0, Z: 0, Subresource: sub };
    }
    let w = s.widths[sub as usize];
    D3D12_TILED_RESOURCE_COORDINATE { X: n % w, Y: n / w, Z: 0, Subresource: sub }
}

impl Drop for Tiles {
    fn drop(&mut self) {
        let s = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        TILED[0].fetch_sub(1, Ordering::Relaxed);
        TILED[1].fetch_sub(self.whole, Ordering::Relaxed);
        TILED[2].fetch_sub(s.mapped_bytes, Ordering::Relaxed);
        POOL.lock().unwrap_or_else(|e| e.into_inner()).give(&s.held);
    }
}

/// For the card accounting line: reserved host copies alive, the MB they
/// would take committed, the MB of their tiles mapped, and the pool's MB.
pub(crate) fn line() -> String {
    format!(
        "tiled host copies of streamed textures: {} alive, {} MB were they committed, {} MB of tiles mapped; the pool {} MB",
        TILED[0].load(Ordering::Relaxed),
        TILED[1].load(Ordering::Relaxed) >> 20,
        TILED[2].load(Ordering::Relaxed) >> 20,
        POOL_BYTES.load(Ordering::Relaxed) >> 20,
    )
}

#[cfg(test)]
mod tests {
    use super::{State, tile_coordinate};

    /// A region resumed in another heap starts at its tile's place: rows of
    /// the subresource's width in tiles, and in the packed levels X alone.
    #[test]
    fn a_region_resumes_at_its_tile() {
        let s = State {
            mips: 3,
            slices: 2,
            standard: 2,
            packed_tiles: 1,
            tiles: vec![16, 4, 0, 16, 4, 0],
            widths: vec![4, 2, 1, 4, 2, 1],
            mapped: vec![false; 6],
            held: Vec::new(),
            mapped_bytes: 0,
        };
        let c = tile_coordinate(&s, 3, 6);
        assert_eq!((c.X, c.Y, c.Subresource), (2, 1, 3));
        let c = tile_coordinate(&s, 1, 3);
        assert_eq!((c.X, c.Y), (1, 1));
        let c = tile_coordinate(&s, 5, 2);
        assert_eq!((c.X, c.Y, c.Subresource), (2, 0, 5));
    }
}
