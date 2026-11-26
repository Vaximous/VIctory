//! A stand-in for the title's upscaler: its picture at render size brought to
//! the output size by a plain filter, with or without FXAA first.
//!
//! The title hands its upscaler (a FidelityFX upscaler DLL, an ML model of
//! some 25 compute passes) the frame's colour at render size and the texture
//! to fill at output size, in one call (`victory_hle::modules::ffx`). On this
//! host those passes take about two thirds of the card's time a frame. With
//! `VICTORY_UPSCALER=none` or `fxaa` that call records this instead, on the
//! same list at the same place: the output is written in full every frame,
//! from this frame's colour alone (no history, so nothing trails).
//!
//! - `none`: bilinear from render size to output size.
//! - `fxaa`: FXAA at render size into a texture of ours, then bilinear.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// What replaces the title's upscaler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Bilinear only.
    Plain = 1,
    /// FXAA at render size, then bilinear.
    Fxaa = 2,
}

impl Mode {
    pub fn from_word(word: u32) -> Option<Mode> {
        match word {
            1 => Some(Mode::Plain),
            2 => Some(Mode::Fxaa),
            _ => None,
        }
    }
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=8, b0), DescriptorTable(SRV(t0), UAV(u0)), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
cbuffer Set : register(b0) {
    // The part of the source that holds the picture, the source texture's
    // size, and the size to write.
    uint src_w; uint src_h; uint tex_w; uint tex_h;
    uint out_w; uint out_h; uint unused0; uint unused1;
};
Texture2D<float4> src : register(t0);
RWTexture2D<float4> dst : register(u0);
SamplerState smooth_ : register(s0);

float4 at_uv(float2 uv) {
    // Never between the picture and what lies beyond it in the texture.
    float2 most = (float2(src_w, src_h) - 0.5) / float2(tex_w, tex_h);
    return src.SampleLevel(smooth_, min(uv, most), 0);
}

#ifdef SCALE
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void scale(uint3 at : SV_DispatchThreadID) {
    if (at.x >= out_w || at.y >= out_h) return;
    float2 uv = (float2(at.xy) + 0.5) / float2(out_w, out_h) * float2(src_w, src_h) / float2(tex_w, tex_h);
    float4 c = at_uv(uv);
    dst[at.xy] = float4(max(c.rgb, 0.0), c.a);
}
#endif

#ifdef FXAA
// Luma on a compressed scale: the picture is linear light, often above 1.
float luma(float2 uv) {
    float3 c = max(at_uv(uv).rgb, 0.0);
    float l = dot(c, float3(0.299, 0.587, 0.114));
    return sqrt(l / (1.0 + l));
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void fxaa(uint3 at : SV_DispatchThreadID) {
    if (at.x >= src_w || at.y >= src_h) return;
    float2 px = 1.0 / float2(tex_w, tex_h);
    float2 uv = (float2(at.xy) + 0.5) * px;
    float4 centre = at_uv(uv);
    float lM = luma(uv);
    float lN = luma(uv + float2(0, -px.y)), lS = luma(uv + float2(0, px.y));
    float lE = luma(uv + float2(px.x, 0)), lW = luma(uv + float2(-px.x, 0));
    float most = max(lM, max(max(lN, lS), max(lE, lW)));
    float least = min(lM, min(min(lN, lS), min(lE, lW)));
    float range = most - least;
    if (range < max(0.0312, most * 0.125)) {
        dst[at.xy] = centre;
        return;
    }
    float lNW = luma(uv - px), lSE = luma(uv + px);
    float lNE = luma(uv + float2(px.x, -px.y)), lSW = luma(uv + float2(-px.x, px.y));
    float across = abs(lNW + lNE - 2.0 * lN) + 2.0 * abs(lW + lE - 2.0 * lM) + abs(lSW + lSE - 2.0 * lS);
    float down = abs(lNW + lSW - 2.0 * lW) + 2.0 * abs(lN + lS - 2.0 * lM) + abs(lNE + lSE - 2.0 * lE);
    bool flat = across >= down;
    float l1 = flat ? lN : lW, l2 = flat ? lS : lE;
    float g1 = l1 - lM, g2 = l2 - lM;
    bool first = abs(g1) >= abs(g2);
    float scaled = 0.25 * max(abs(g1), abs(g2));
    float step = flat ? px.y : px.x;
    float local = 0.5 * ((first ? l1 : l2) + lM);
    if (first) step = -step;
    float2 edge = uv;
    if (flat) edge.y += 0.5 * step; else edge.x += 0.5 * step;
    float2 along = flat ? float2(px.x, 0) : float2(0, px.y);
    float2 p1 = edge - along, p2 = edge + along;
    float e1 = luma(p1) - local, e2 = luma(p2) - local;
    bool done1 = abs(e1) >= scaled, done2 = abs(e2) >= scaled;
    static const float STEPS[10] = { 1.0, 1.0, 1.0, 1.0, 1.5, 2.0, 2.0, 2.0, 4.0, 8.0 };
    [loop] for (int i = 0; i < 10 && !(done1 && done2); i++) {
        if (!done1) { p1 -= along * STEPS[i]; e1 = luma(p1) - local; done1 = abs(e1) >= scaled; }
        if (!done2) { p2 += along * STEPS[i]; e2 = luma(p2) - local; done2 = abs(e2) >= scaled; }
    }
    float d1 = flat ? uv.x - p1.x : uv.y - p1.y;
    float d2 = flat ? p2.x - uv.x : p2.y - uv.y;
    bool nearer1 = d1 < d2;
    float offset = 0.5 - min(d1, d2) / max(d1 + d2, 1e-9);
    bool smaller = lM < local;
    float shift = (((nearer1 ? e1 : e2) < 0.0) != smaller) ? offset : 0.0;
    // Below a texel: blend by how far the centre stands out.
    float mean = (2.0 * (lN + lS + lE + lW) + lNW + lNE + lSW + lSE) / 12.0;
    float sub = saturate(abs(mean - lM) / range);
    sub = (-2.0 * sub + 3.0) * sub * sub;
    shift = max(shift, sub * sub * 0.75);
    float2 to = uv;
    if (flat) to.y += shift * step; else to.x += shift * step;
    float4 c = at_uv(to);
    dst[at.xy] = float4(c.rgb, centre.a);
}
#endif
"#;

struct Pass {
    root: ID3D12RootSignature,
    scale: ID3D12PipelineState,
    fxaa: ID3D12PipelineState,
    heap: ID3D12DescriptorHeap,
    step: u32,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

/// Descriptor slots in our heap: two a pass, two passes a frame; used in turn,
/// so a slot is written again only thousands of frames later.
const SLOTS: u32 = 4096;

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    let mut root: Option<ID3D12RootSignature> = None;
    let mut pipeline = |part: &str, entry| -> windows::core::Result<ID3D12PipelineState> {
        let code = crate::host::compile(&format!("#define {part}\n{SOURCE}"), entry, s!("cs_5_1"))?;
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            if root.is_none() {
                root = Some(device.CreateRootSignature(0, &code)?);
            }
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            };
            device.CreateComputePipelineState(&desc)
        }
    };
    let scale = pipeline("SCALE", s!("scale"))?;
    let fxaa = pipeline("FXAA", s!("fxaa"))?;
    // SAFETY: plain creations.
    let (heap, step) = unsafe {
        (
            device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: SLOTS,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })?,
            device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
        )
    };
    Ok(Pass {
        root: root.ok_or_else(windows::core::Error::empty)?,
        scale,
        fxaa,
        heap,
        step,
    })
}

fn pass() -> Option<&'static Pass> {
    static PASS: std::sync::OnceLock<Option<Pass>> = std::sync::OnceLock::new();
    PASS.get_or_init(|| {
        make(&host()?.device)
            .inspect_err(|e| tracing::warn!(error = %e, "no stand-in for the title's upscaler"))
            .ok()
    })
    .as_ref()
}

/// A format shaders can read a texture of format `f` as.
fn readable(f: DXGI_FORMAT) -> DXGI_FORMAT {
    match f {
        DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R32G32B32A32_TYPELESS => DXGI_FORMAT_R32G32B32A32_FLOAT,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        f => f,
    }
}

/// A format shaders can store to a texture of format `f` in (an sRGB one
/// as its plain twin).
fn writable(f: DXGI_FORMAT) -> DXGI_FORMAT {
    match readable(f) {
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => DXGI_FORMAT_B8G8R8A8_UNORM,
        f => f,
    }
}

/// The texture FXAA writes into, at render size.
struct Between {
    size: (u32, u32),
    texture: ID3D12Resource,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Between {}

fn between(size: (u32, u32)) -> Option<ID3D12Resource> {
    static BETWEEN: Mutex<Option<Between>> = Mutex::new(None);
    let mut kept = BETWEEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(b) = kept.as_ref()
        && b.size == size
    {
        return Some(b.texture.clone());
    }
    let host = host()?;
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(size.0),
        Height: size.1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; it starts in the state every use leaves it in.
    let made = unsafe {
        host.device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            None,
            &mut texture,
        )
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, ?size, "no texture for FXAA");
        return None;
    }
    let texture = texture?;
    *kept = Some(Between {
        size,
        texture: texture.clone(),
    });
    Some(texture)
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Writes title texture `output` (its `out` part) from title texture `color`
/// (its `render` part) in `list`. Replaces the list's descriptor heaps,
/// compute root signature and pipeline (the caller has them set again);
/// false if it recorded nothing.
pub(crate) fn record(
    list: &HostList,
    mode: Mode,
    color: This,
    output: This,
    render: (u32, u32),
    out: (u32, u32),
) -> bool {
    const SHADER_RESOURCE: u32 = 0x40 | 0x80;
    const UNORDERED_ACCESS: u32 = 0x8;
    let Some(pass) = pass() else { return false };
    let (Some(from), Some(to)) = (
        crate::resource::host_resource(color),
        crate::resource::host_resource(output),
    ) else {
        once!(?color, ?output, "the upscaler's stand-in: no host texture");
        return false;
    };
    // SAFETY: live resources.
    let (fd, td) = unsafe { (from.GetDesc(), to.GetDesc()) };
    if fd.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || td.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || !td
            .Flags
            .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
    {
        once!(from = ?fd, to = ?td, "the upscaler's stand-in: textures it cannot use");
        return false;
    }
    let tex = (fd.Width as u32, fd.Height);
    let clamp = |(w, h): (u32, u32), (mw, mh): (u32, u32)| {
        if w == 0 || h == 0 {
            (mw, mh)
        } else {
            (w.min(mw), h.min(mh))
        }
    };
    let render = clamp(render, tex);
    let out = clamp(out, (td.Width as u32, td.Height));
    {
        static SAID: Mutex<u32> = Mutex::new(0);
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if *said < 3 {
            *said += 1;
            tracing::info!(
                ?mode,
                ?render,
                texture = ?tex,
                ?out,
                from = fd.Format.0,
                to = td.Format.0,
                "the title's upscaler replaced (VICTORY_UPSCALER)"
            );
        }
    }
    let fxaa_into = match mode {
        Mode::Fxaa => match between(render) {
            Some(t) => Some(t),
            None => return false,
        },
        Mode::Plain => None,
    };

    // The colour is read: what the title filled itself goes up first, and a
    // depth twin is settled, as for any read (`crate::draw`).
    if let Some(t) = crate::resource::host_copy(color)
        && t.flags() & 0x7 == 0
    {
        crate::texture::upload_from(list, color, 0);
    }
    list.twin_use(color, false, false);
    crate::list::ensure_state(list, color, SHADER_RESOURCE);
    list.texture_written(output);
    if !list.twin_use(output, true, true) {
        crate::list::ensure_state(list, output, UNORDERED_ACCESS);
    }

    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let device = &host().expect("a pass means a host").device;
    let l = &list.list;
    // SAFETY: an open list; live textures in the states the views need; slots
    // no list in flight reads; our objects live as long as the process.
    unsafe {
        let cpu = pass.heap.GetCPUDescriptorHandleForHeapStart();
        let gpu = pass.heap.GetGPUDescriptorHandleForHeapStart();
        let slot = |i: u32| {
            (
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu.ptr + (i * pass.step) as usize,
                },
                D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: gpu.ptr + u64::from(i * pass.step),
                },
            )
        };
        let srv = |texture: &ID3D12Resource, format: DXGI_FORMAT, at| {
            let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: readable(format),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            device.CreateShaderResourceView(texture, Some(&desc), at);
        };
        let uav = |texture: &ID3D12Resource, format: DXGI_FORMAT, at| {
            let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: writable(format),
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            device.CreateUnorderedAccessView(texture, None::<&ID3D12Resource>, Some(&desc), at);
        };
        let mut words = [render.0, render.1, tex.0, tex.1, out.0, out.1, 0, 0];
        uav_barrier(l);
        l.SetDescriptorHeaps(&[Some(pass.heap.clone())]);
        l.SetComputeRootSignature(&pass.root);
        let source = if let Some(between) = &fxaa_into {
            list.keep(between);
            let first = NEXT.fetch_add(2, std::sync::atomic::Ordering::Relaxed) % SLOTS;
            let (s0, g0) = slot(first);
            let (s1, _) = slot(first + 1);
            srv(from, fd.Format, s0);
            uav(between, DXGI_FORMAT_R16G16B16A16_FLOAT, s1);
            l.SetComputeRoot32BitConstants(0, 8, words.as_ptr().cast(), 0);
            l.SetComputeRootDescriptorTable(1, g0);
            l.SetPipelineState(&pass.fxaa);
            l.Dispatch(render.0.div_ceil(8), render.1.div_ceil(8), 1);
            let to_read = crate::host::transition(
                between,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            l.ResourceBarrier(&[to_read]);
            words[2] = render.0;
            words[3] = render.1;
            (between, DXGI_FORMAT_R16G16B16A16_FLOAT)
        } else {
            (from, fd.Format)
        };
        let first = NEXT.fetch_add(2, std::sync::atomic::Ordering::Relaxed) % SLOTS;
        let (s0, g0) = slot(first);
        let (s1, _) = slot(first + 1);
        srv(source.0, source.1, s0);
        uav(to, td.Format, s1);
        l.SetComputeRoot32BitConstants(0, 8, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(1, g0);
        l.SetPipelineState(&pass.scale);
        l.Dispatch(out.0.div_ceil(8), out.1.div_ceil(8), 1);
        if let Some(between) = &fxaa_into {
            let back = crate::host::transition(
                between,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            l.ResourceBarrier(&[back]);
        }
        uav_barrier(l);
    }
    #[cfg(feature = "diagnostics")]
    list.note_op(|| {
        format!("upscaler stand-in {mode:?} {color:?} {render:?} to {output:?} {out:?}")
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_by_word() {
        assert_eq!(Mode::from_word(Mode::Plain as u32), Some(Mode::Plain));
        assert_eq!(Mode::from_word(Mode::Fxaa as u32), Some(Mode::Fxaa));
        assert_eq!(Mode::from_word(0), None);
    }

    #[test]
    fn views_of_typeless_and_srgb_textures() {
        assert_eq!(
            readable(DXGI_FORMAT_R16G16B16A16_TYPELESS),
            DXGI_FORMAT_R16G16B16A16_FLOAT
        );
        assert_eq!(
            writable(DXGI_FORMAT_R8G8B8A8_UNORM_SRGB),
            DXGI_FORMAT_R8G8B8A8_UNORM
        );
        assert_eq!(
            writable(DXGI_FORMAT_R11G11B10_FLOAT),
            DXGI_FORMAT_R11G11B10_FLOAT
        );
    }

    /// The passes compile with the system's compiler and the host takes them.
    #[test]
    fn the_passes_build() {
        let Some(host) = host() else {
            return;
        };
        make(&host.device).expect("the passes");
    }
}
