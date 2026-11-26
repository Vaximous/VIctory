//! Native replacements of translated shaders (`VICTORY_NATIVE=1`).
//!
//! A translated shader is the console program spelled out instruction by
//! instruction: correct, but it keeps the console's wave of 64 lanes on a
//! host that runs 32, and every memory access as the console made it. For
//! the few shaders that take most of a frame's time on the card, a shader
//! written for the host by hand does the same work faster. This module finds
//! such shaders by the hash the log names a shader with and puts them in
//! place of the translation.
//!
//! * **Where**: `VICTORY_NATIVE_DIR`, else `native` beside the shader
//!   cache. A replacement is `<hash>.<stage>.hlsl` (`stage` as the log has
//!   it: `compute`, `mesh`, `vertex`, `pixel`), entry point `main`. Nothing
//!   of the title is part of this repository: replacements are local files.
//! * **Compiled** with the PC `dxc.exe` when the shader is first translated
//!   (`VICTORY_PC_DXC`, else the newest Windows SDK's), as `cs_6_6`,
//!   `ms_6_6`, `vs_6_6` or `ps_6_6` (a first line `// dxc: <arguments>`
//!   adds arguments, such as another profile). The DXIL is kept under
//!   `build` in the directory, by the BLAKE3 of the source and arguments.
//!   A replacement that does not compile is logged and left out.
//! * **Bindings**: the translation's ([`victory_shader::emit`]'s module
//!   notes): user data in root constants at `b0, space0`, the title's
//!   root views at `t<n>`/`u<n>`/`b<n>, space1`, descriptors through
//!   `ResourceDescriptorHeap[]` at the console's descriptor index, static
//!   samplers at `s<n>`. The host makes the root signature from the
//!   translation as before, so a replacement may use any part of what the
//!   translation binds and nothing else; a pipeline the host refuses with
//!   it is made from the translation instead. Thread-group size and outputs
//!   must be the translation's.
//! * **Check** (`VICTORY_NATIVE_CHECK=1`, compute only): the first
//!   dispatch of each pipeline with a replacement runs both, on the same
//!   input: what the dispatch can write (the ranges of title buffers its
//!   views and root views declare written) is saved, the translation runs,
//!   its result is saved and the input put back, the replacement runs, and
//!   the two results are compared on the CPU once the card is done; the log
//!   says how many dwords differ. (`VICTORY_NATIVE_CHECKS=n`: the first n
//!   dispatches of each.) Textures written through views and memory written
//!   by address are not compared.
//!
//! What replacements change is not part of the shader cache: the cache
//! keeps translations, and the replacement takes the place of one after it
//! comes from the cache; the driver's pipeline library names pipelines by
//! their code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::HostList;

/// Whether replacements are used (`VICTORY_NATIVE=1`, or the check).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NATIVE").is_some_and(|v| v == "1") || check())
}

/// Whether the first dispatches of replaced compute shaders run both ways
/// and compare (`VICTORY_NATIVE_CHECK=1`).
pub(crate) fn check() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NATIVE_CHECK").is_some_and(|v| v == "1"))
}

/// The shader cache's directory: replacements are beside it by default.
static BESIDE: OnceLock<PathBuf> = OnceLock::new();

/// The shader cache is in `dir`.
pub(crate) fn beside_cache(dir: &Path) {
    let _ = BESIDE.set(dir.with_file_name("native"));
}

fn dir() -> Option<PathBuf> {
    match std::env::var_os("VICTORY_NATIVE_DIR") {
        Some(d) if !d.is_empty() => Some(d.into()),
        _ => BESIDE.get().cloned(),
    }
}

/// The PC `dxc.exe`: `VICTORY_PC_DXC`, else the newest Windows SDK's.
fn dxc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VICTORY_PC_DXC") {
        return Some(p.into());
    }
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&kits)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join(r"x64\dxc.exe"))
        .filter(|p| p.exists())
        .collect();
    found.sort();
    found.pop()
}

/// The profile a stage's replacement is compiled for.
fn profile(stage: &str) -> Option<&'static str> {
    Some(match stage {
        "compute" => "cs_6_6",
        "mesh" => "ms_6_6",
        "vertex" => "vs_6_6",
        "pixel" => "ps_6_6",
        _ => return None,
    })
}

/// The arguments a source asks for on its first line (`// dxc: ...`).
fn extra_arguments(source: &str) -> Vec<String> {
    source
        .lines()
        .next()
        .and_then(|l| l.trim().strip_prefix("// dxc:"))
        .map(|rest| rest.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The arguments for compiling `file` for `stage` into `out`.
fn arguments(stage: &str, source: &str, file: &Path, out: &Path) -> Option<Vec<String>> {
    let extra = extra_arguments(source);
    let mut args = Vec::new();
    if !extra.iter().any(|a| a == "-T") {
        args.extend(["-T".to_owned(), profile(stage)?.to_owned()]);
    }
    args.extend(["-E", "main", "-HV", "2021", "-O3", "-Qstrip_debug", "-Qstrip_reflect"].map(str::to_owned));
    args.extend(extra);
    args.extend([file.display().to_string(), "-Fo".to_owned(), out.display().to_string()]);
    Some(args)
}

/// The replacement of the `stage` shader `hash` (as the log names it), when
/// replacements are on and the directory has one that compiles.
pub(crate) fn replacement(stage: &str, hash: &str) -> Option<Arc<Vec<u8>>> {
    if !on() {
        return None;
    }
    let dir = dir()?;
    let file = dir.join(format!("{hash}.{stage}.hlsl"));
    let source = std::fs::read_to_string(&file).ok()?;
    match compile(stage, &source, &file, &dir) {
        Ok(dxil) => {
            tracing::info!(stage, hash, bytes = dxil.len(), "native replacement");
            Some(Arc::new(dxil))
        }
        Err(e) => {
            tracing::warn!(stage, hash, error = %e, "native replacement does not compile; the translation is used");
            None
        }
    }
}

fn compile(stage: &str, source: &str, file: &Path, dir: &Path) -> Result<Vec<u8>, String> {
    let mut key = blake3::Hasher::new();
    key.update(stage.as_bytes());
    key.update(source.as_bytes());
    for a in extra_arguments(source) {
        key.update(a.as_bytes());
    }
    let build = dir.join("build");
    let out = build.join(format!("{}.dxil", &key.finalize().to_hex()[..32]));
    if let Ok(dxil) = std::fs::read(&out)
        && !dxil.is_empty()
    {
        return Ok(dxil);
    }
    std::fs::create_dir_all(&build).map_err(|e| e.to_string())?;
    let dxc = dxc().ok_or("no PC dxc.exe (set VICTORY_PC_DXC)")?;
    let args = arguments(stage, source, file, &out).ok_or("no profile for the stage")?;
    let ran = std::process::Command::new(&dxc).args(&args).output().map_err(|e| e.to_string())?;
    if !ran.status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(String::from_utf8_lossy(&ran.stderr).chars().take(4000).collect());
    }
    std::fs::read(&out).map_err(|e| e.to_string())
}

// The check: both programs on the same input.

thread_local! {
    /// While a checked dispatch binds: the buffer ranges it declares written.
    static COLLECTING: std::cell::RefCell<Option<Vec<Write>>> = const { std::cell::RefCell::new(None) };
}

/// A buffer range a dispatch can write: the buffer, offset and bytes.
pub(crate) type Write = (ID3D12Resource, u64, u64);

/// A shader may write `len` bytes of `buffer` from `offset` on (from the
/// host copies' declarations of what shaders write).
pub(crate) fn wrote(buffer: &ID3D12Resource, offset: u64, len: u64) {
    if !check() {
        return;
    }
    COLLECTING.with(|c| {
        if let Some(v) = c.borrow_mut().as_mut() {
            v.push((buffer.clone(), offset, len));
        }
    });
}

/// Runs `f` (a dispatch's binding) and returns the ranges it declared
/// written as well.
pub(crate) fn collecting<R>(f: impl FnOnce() -> R) -> (R, Vec<Write>) {
    COLLECTING.with(|c| *c.borrow_mut() = Some(Vec::new()));
    let r = f();
    let writes = COLLECTING.with(|c| c.borrow_mut().take()).unwrap_or_default();
    (r, writes)
}

/// Whether this dispatch of the pipeline `name` is checked: the first
/// `VICTORY_NATIVE_CHECKS` (1) of each.
pub(crate) fn check_due(name: &str) -> bool {
    static LIMIT: OnceLock<u32> = OnceLock::new();
    static DONE: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_NATIVE_CHECKS").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
    });
    let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
    let n = done.get_or_insert_with(HashMap::new).entry(name.to_owned()).or_insert(0);
    if *n >= limit {
        return false;
    }
    *n += 1;
    true
}

/// Bytes a check may save of one dispatch's writes (each saved three
/// times, in the PC's memory).
const CHECK_MOST: u64 = 768 << 20;

/// The copy program: dwords from one root UAV to another, or a marker.
const COPY: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1)"
cbuffer Copy : register(b0) {
    uint quads;   // 16-byte pieces to copy
    uint stride;  // threads in the dispatch
    uint marker;  // nonzero: write it at dst[0] instead
    uint pad;
};
RWByteAddressBuffer src : register(u0);
RWByteAddressBuffer dst : register(u1);
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void main(uint3 group : SV_GroupID, uint thread : SV_GroupIndex) {
    uint i = group.x * 256u + thread;
    if (marker != 0u) {
        if (i == 0u) {
            dst.Store(0, marker);
        }
        return;
    }
    [loop]
    for (; i < quads; i += stride) {
        dst.Store4(i * 16u, src.Load4(i * 16u));
    }
}
"#;

struct CopyPipe {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for CopyPipe {}
// SAFETY: as above.
unsafe impl Sync for CopyPipe {}

fn copy_pipe() -> Option<&'static CopyPipe> {
    static PIPE: OnceLock<Option<CopyPipe>> = OnceLock::new();
    PIPE.get_or_init(|| {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let device = &crate::host::host()?.device;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours; the blob's bytes hold the root signature.
        unsafe {
            D3DCompile(
                COPY.as_ptr().cast(),
                COPY.len(),
                windows::core::s!("native check copy"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .ok()?;
            let code = code?;
            let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes).ok()?;
            let pso: ID3D12PipelineState = device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: bytes.as_ptr().cast(), BytecodeLength: bytes.len() },
                    ..Default::default()
                })
                .ok()?;
            Some(CopyPipe { root, pso })
        }
    })
    .as_ref()
}

/// One range of a check: where in its buffer (on 16 bytes), how long, and
/// where its three saves begin in the scratch buffer.
struct Range {
    buffer: ID3D12Resource,
    offset: u64,
    len: u64,
    at: u64,
}

/// A dispatch run both ways ([`check`]).
pub(crate) struct Check {
    name: String,
    groups: [u32; 3],
    ranges: Vec<Range>,
    /// The bytes of all ranges (each save is this long).
    total: u64,
    /// Marker dword, then the saves: input, translated result, native
    /// result. In the PC's memory, mapped.
    scratch: ID3D12Resource,
    data: *mut u8,
}

// SAFETY: the mapping lives as long as the buffer, which the check holds.
unsafe impl Send for Check {}

const HEADER: u64 = 256;

impl Check {
    /// Saves what the dispatch can write (`writes`, from its binding) before
    /// the translation runs. `None` (and logged) when it cannot be checked.
    pub(crate) fn begin(list: &HostList, name: &str, groups: [u32; 3], writes: Vec<Write>) -> Option<Check> {
        let host = crate::host::host()?;
        // One range per piece of each buffer: overlapping declarations
        // merged.
        let mut by: HashMap<usize, (ID3D12Resource, Vec<(u64, u64)>)> = HashMap::new();
        for (buffer, offset, len) in writes {
            // SAFETY: a live buffer.
            let width = unsafe { buffer.GetDesc() }.Width;
            let lo = offset & !15;
            let hi = (offset + len).next_multiple_of(16).min(width & !15);
            if hi <= lo {
                continue;
            }
            by.entry(buffer.as_raw() as usize).or_insert_with(|| (buffer.clone(), Vec::new())).1.push((lo, hi));
        }
        let mut ranges = Vec::new();
        let mut total = 0u64;
        for (_, (buffer, mut spans)) in by {
            spans.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::new();
            for (lo, hi) in spans {
                match merged.last_mut() {
                    Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                    _ => merged.push((lo, hi)),
                }
            }
            for (lo, hi) in merged {
                ranges.push(Range { buffer: buffer.clone(), offset: lo, len: hi - lo, at: total });
                total += hi - lo;
            }
        }
        if ranges.is_empty() {
            tracing::warn!(pipeline = name, "native check: the dispatch declares no buffer writes; not checked");
            return None;
        }
        if total > CHECK_MOST {
            tracing::warn!(pipeline = name, mb = total >> 20, "native check: the dispatch declares too much written; not checked");
            return None;
        }
        let pipe = copy_pipe()?;
        let (scratch, data) = host.shared_buffer(HEADER + 3 * total)?;
        crate::host::name(&scratch, || format!("native check {name}"));
        list.keep(&scratch);
        let check = Check { name: name.to_owned(), groups, ranges, total, scratch, data };
        check.copies(list, pipe, 0, Direction::Save);
        Some(check)
    }

    /// After the translation ran: its result saved, the input put back.
    pub(crate) fn translated_ran(&self, list: &HostList) {
        let Some(pipe) = copy_pipe() else { return };
        self.copies(list, pipe, 1, Direction::Save);
        self.copies(list, pipe, 0, Direction::Restore);
    }

    /// After the replacement ran: its result saved; compared once the card
    /// is done.
    pub(crate) fn native_ran(self, list: &HostList) {
        let Some(pipe) = copy_pipe() else { return };
        self.copies(list, pipe, 2, Direction::Save);
        // The marker, after every copy: the card has done them when it is
        // there.
        // SAFETY: a fresh mapping of our own buffer.
        unsafe { self.data.cast::<u32>().write_volatile(0) };
        let scratch_va = unsafe { self.scratch.GetGPUVirtualAddress() };
        self.run(list, pipe, [0, 1, MARKER, 0], scratch_va, scratch_va, 1);
        std::thread::spawn(move || self.compare());
    }

    /// Copies every range to or from save `n`.
    fn copies(&self, list: &HostList, pipe: &CopyPipe, n: u64, direction: Direction) {
        // SAFETY: live buffers.
        let scratch = unsafe { self.scratch.GetGPUVirtualAddress() } + HEADER + n * self.total;
        for r in &self.ranges {
            let there = unsafe { r.buffer.GetGPUVirtualAddress() } + r.offset;
            let saved = scratch + r.at;
            let quads = (r.len / 16) as u32;
            let groups = quads.div_ceil(256).clamp(1, 65_535);
            let (src, dst) = match direction {
                Direction::Save => (there, saved),
                Direction::Restore => (saved, there),
            };
            self.run(list, pipe, [quads, groups * 256, 0, 0], src, dst, groups);
        }
    }

    fn run(&self, list: &HostList, pipe: &CopyPipe, words: [u32; 4], src: u64, dst: u64, groups: u32) {
        let l = &list.list;
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: std::mem::ManuallyDrop::new(None) }),
            },
        };
        // SAFETY: an open list; our root signature and pipeline; the
        // addresses lie in live buffers the list keeps.
        unsafe {
            l.ResourceBarrier(std::slice::from_ref(&barrier));
            l.SetComputeRootSignature(&pipe.root);
            l.SetPipelineState(&pipe.pso);
            l.SetComputeRoot32BitConstants(0, 4, words.as_ptr().cast(), 0);
            l.SetComputeRootUnorderedAccessView(1, src);
            l.SetComputeRootUnorderedAccessView(2, dst);
            l.Dispatch(groups, 1, 1);
            l.ResourceBarrier(std::slice::from_ref(&barrier));
        }
    }

    /// Waits for the card (the marker), then compares the two results.
    fn compare(self) {
        let started = std::time::Instant::now();
        // SAFETY: our mapping; the card writes the marker last.
        while unsafe { self.data.cast::<u32>().read_volatile() } != MARKER {
            if started.elapsed() > std::time::Duration::from_secs(300) {
                tracing::warn!(pipeline = self.name, "native check: the card never ran the check");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let n = self.total as usize;
        // SAFETY: the saves lie in our mapping, which the card is done with.
        let (input, translated, native) = unsafe {
            let base = self.data.add(HEADER as usize);
            (
                std::slice::from_raw_parts(base.cast::<u32>(), n / 4),
                std::slice::from_raw_parts(base.add(n).cast::<u32>(), n / 4),
                std::slice::from_raw_parts(base.add(2 * n).cast::<u32>(), n / 4),
            )
        };
        let mut differ = 0u64;
        let mut near = 0u64;
        let mut changed = 0u64;
        let mut worst = 0f32;
        let mut first: Option<String> = None;
        for (i, (&t, &v)) in translated.iter().zip(native).enumerate() {
            if t != input[i] {
                changed += 1;
            }
            if t == v {
                continue;
            }
            differ += 1;
            let (a, b) = (f32::from_bits(t), f32::from_bits(v));
            let diff = (a - b).abs();
            if a.is_finite() && b.is_finite() && diff <= 1e-4 * a.abs().max(b.abs()).max(1e-6) {
                near += 1;
                worst = worst.max(diff / a.abs().max(b.abs()).max(1e-30));
            }
            if first.is_none() {
                let at = i as u64 * 4;
                let r = self.ranges.iter().find(|r| at >= r.at && at < r.at + r.len);
                first = Some(format!(
                    "at {:#x} of range {:#x}+{:#x}: input {:#010x} translated {t:#010x} native {v:#010x}",
                    r.map_or(0, |r| at - r.at),
                    r.map_or(0, |r| r.offset),
                    r.map_or(0, |r| r.len),
                    input[i]
                ));
            }
        }
        let [x, y, z] = self.groups;
        if differ == 0 {
            tracing::info!(
                pipeline = self.name,
                "native check passed: {x}x{y}x{z} groups, {} ranges, {} bytes declared written, {changed} dwords written by the translation, all the same",
                self.ranges.len(),
                self.total
            );
        } else {
            tracing::warn!(
                pipeline = self.name,
                "native check FAILED: {x}x{y}x{z} groups, {} ranges, {} bytes, {changed} dwords written by the translation; {differ} dwords differ ({near} of them as floats within 1e-4, the worst {worst:e} relative); the first {}",
                self.ranges.len(),
                self.total,
                first.unwrap_or_default()
            );
        }
    }
}

const MARKER: u32 = 0x4e41_5456;

enum Direction {
    Save,
    Restore,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_take_the_stage_profile_unless_the_source_names_one() {
        let (file, out) = (Path::new("a.hlsl"), Path::new("a.dxil"));
        let args = arguments("compute", "void main() {}", file, out).unwrap();
        assert_eq!(&args[..2], ["-T", "cs_6_6"]);
        assert!(args.ends_with(&["a.hlsl".to_owned(), "-Fo".to_owned(), "a.dxil".to_owned()]));
        let args = arguments("pixel", "// dxc: -T ps_6_7 -enable-16bit-types\nvoid main() {}", file, out).unwrap();
        assert!(!args.contains(&"ps_6_6".to_owned()));
        assert!(args.contains(&"-enable-16bit-types".to_owned()));
        assert!(arguments("hull", "", file, out).is_none());
    }
}
