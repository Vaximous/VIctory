//! The title's FidelityFX upscaler, optionally replaced.
//!
//! The title calls its upscaler through the FidelityFX API of the loader DLL
//! it ships (`ffxCreateContext`, `ffxDispatch`, `ffxQuery`, ...). Its upscaler
//! is an ML model: some 25 compute passes a frame, about two thirds of the
//! card's time a frame on this host. With `upscaler = "none"` or `"fxaa"`
//! (config `[graphics]`, env `VICTORY_UPSCALER`) the title's imports of
//! `ffxDispatch` and `ffxQuery` come here:
//!
//! - an upscale dispatch records `victory_gpu::upscale` instead, from the
//!   colour the title hands over to the output it names, at the sizes it
//!   names; any other dispatch goes to the title's DLL.
//! - the query for the camera's jitter answers no jitter: without a
//!   temporal upscaler the sub-texel offsets the title would put on its
//!   camera only make the picture shake. (`VICTORY_UPSCALER_JITTER=1`
//!   keeps them.)
//!
//! The title still creates its upscaler (its pipelines are made only when a
//! dispatch uses them, which none does now). The render size stays the
//! title's own.
//!
//! The structures are the FidelityFX API's (`ffx_api.h`, `ffx_upscale.h`,
//! MIT licence), as public headers lay them out.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What upscales the title's picture.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Upscaler {
    /// The title's own (its FidelityFX upscaler).
    #[default]
    Title,
    /// A plain bilinear upscale, no anti-aliasing.
    Plain,
    /// FXAA at render size, then bilinear.
    Fxaa,
}

impl Upscaler {
    /// From a setting: `fsr4` (or `title`, `fsr`), `none` (or `off`,
    /// `bilinear`), `fxaa`; `None` for anything else.
    pub fn parse(value: &str) -> Option<Upscaler> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "fsr4" | "fsr" | "title" | "default" => Some(Upscaler::Title),
            "none" | "off" | "plain" | "bilinear" => Some(Upscaler::Plain),
            "fxaa" => Some(Upscaler::Fxaa),
            _ => None,
        }
    }

    /// `victory_gpu::upscale::Mode`'s word.
    fn word(self) -> u32 {
        match self {
            Upscaler::Title => 0,
            Upscaler::Plain => 1,
            Upscaler::Fxaa => 2,
        }
    }
}

static MODE: OnceLock<Upscaler> = OnceLock::new();

/// Sets what upscales, before the title's imports are bound (the config's
/// value, the environment's first). Later calls change nothing.
pub fn set_upscaler(mode: Upscaler) {
    let _ = MODE.set(mode);
    if mode != Upscaler::Title {
        tracing::info!(?mode, "the title's upscaler is replaced");
    }
}

fn mode() -> Upscaler {
    *MODE.get_or_init(|| {
        std::env::var("VICTORY_UPSCALER")
            .ok()
            .and_then(|v| Upscaler::parse(&v))
            .unwrap_or_default()
    })
}

const LOADER: &str = "amd_fidelityfx_loader_xbox.dll";

/// The title's own functions, for what is not replaced.
static DISPATCH: AtomicUsize = AtomicUsize::new(0);
static QUERY: AtomicUsize = AtomicUsize::new(0);

type Entry = unsafe extern "C" fn(*mut c_void, *mut Header) -> u32;

/// An import of the title as bound (`found`): ours instead, if it is one we
/// take over with the upscaler set now.
pub fn hook(dll: &str, name: &str, found: usize) -> Option<usize> {
    if mode() == Upscaler::Title || !dll.eq_ignore_ascii_case(LOADER) || found == 0 {
        return None;
    }
    let (ours, keep) = match name {
        "ffxDispatch" => (ffx_dispatch as *const () as usize, &DISPATCH),
        "ffxQuery" => (ffx_query as *const () as usize, &QUERY),
        _ => return None,
    };
    keep.store(found, Ordering::Relaxed);
    Some(ours)
}

/// `ffxApiHeader`.
#[repr(C)]
pub(crate) struct Header {
    kind: u64,
    next: *mut Header,
}

/// `FfxApiResourceDescription`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceDescription {
    kind: u32,
    format: u32,
    width: u32,
    height: u32,
    depth: u32,
    mips: u32,
    flags: u32,
    usage: u32,
}

/// `FfxApiResource`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Resource {
    resource: *mut c_void,
    description: ResourceDescription,
    state: u32,
}

/// `ffxDispatchDescUpscale`, as far as it is read here.
#[repr(C)]
struct DispatchUpscale {
    header: Header,
    command_list: *mut c_void,
    color: Resource,
    depth: Resource,
    motion_vectors: Resource,
    exposure: Resource,
    reactive: Resource,
    transparency_and_composition: Resource,
    output: Resource,
    jitter: [f32; 2],
    motion_vector_scale: [f32; 2],
    render_size: [u32; 2],
    upscale_size: [u32; 2],
}

/// `ffxQueryDescUpscaleGetJitterOffset`.
#[repr(C)]
struct QueryJitterOffset {
    header: Header,
    index: i32,
    phase_count: i32,
    out_x: *mut f32,
    out_y: *mut f32,
}

const DISPATCH_UPSCALE: u64 = 0x0001_0001;
const QUERY_JITTER_OFFSET: u64 = 0x0001_0005;
const RETURN_OK: u32 = 0;

/// The first time a kind of call is seen (to check the layouts in a log).
fn first(kind: u64, what: &str) -> bool {
    static SEEN: std::sync::Mutex<Vec<(u64, &'static str)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let what: &'static str = if what == "dispatch" {
        "dispatch"
    } else {
        "query"
    };
    if seen.contains(&(kind, what)) {
        return false;
    }
    seen.push((kind, what));
    true
}

fn original(keep: &AtomicUsize) -> Option<Entry> {
    let at = keep.load(Ordering::Relaxed);
    // SAFETY: the address the title's import of that function was bound to.
    (at != 0).then(|| unsafe { std::mem::transmute::<usize, Entry>(at) })
}

unsafe extern "C" fn ffx_dispatch(context: *mut c_void, desc: *mut Header) -> u32 {
    // SAFETY: the title passes a description or null.
    let kind = unsafe { desc.as_ref() }.map_or(0, |h| h.kind);
    if kind == DISPATCH_UPSCALE {
        // SAFETY: an upscale dispatch's description.
        let d = unsafe { &*desc.cast::<DispatchUpscale>() };
        if first(kind, "dispatch") {
            tracing::info!(
                list = ?d.command_list,
                color = ?d.color.resource,
                color_size = ?(d.color.description.width, d.color.description.height),
                output = ?d.output.resource,
                output_size = ?(d.output.description.width, d.output.description.height),
                render = ?d.render_size,
                upscale = ?d.upscale_size,
                jitter = ?d.jitter,
                "the title's upscale dispatch"
            );
        }
        #[cfg(windows)]
        if victory_gpu::list::upscale(
            d.command_list,
            mode().word(),
            d.color.resource,
            d.output.resource,
            (d.render_size[0], d.render_size[1]),
            (d.upscale_size[0], d.upscale_size[1]),
        ) {
            return RETURN_OK;
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!("the upscaler's stand-in could not be recorded: the title's runs")
        });
    } else if first(kind, "dispatch") {
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            "a FidelityFX dispatch, left to the title's DLL"
        );
    }
    match original(&DISPATCH) {
        // SAFETY: the title's own function, with its own arguments.
        Some(f) => unsafe { f(context, desc) },
        None => 1,
    }
}

unsafe extern "C" fn ffx_query(context: *mut c_void, desc: *mut Header) -> u32 {
    let Some(f) = original(&QUERY) else { return 1 };
    // SAFETY: the title's own function, with its own arguments.
    let result = unsafe { f(context, desc) };
    // SAFETY: the title passes a description or null.
    let kind = unsafe { desc.as_ref() }.map_or(0, |h| h.kind);
    if first(kind, "query") {
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            result,
            "a FidelityFX query"
        );
    }
    if kind == QUERY_JITTER_OFFSET && result == RETURN_OK && !keep_jitter() {
        // SAFETY: a jitter query's description; its out pointers are the
        // title's or null.
        unsafe {
            let q = &*desc.cast::<QueryJitterOffset>();
            if first(u64::MAX, "query") {
                tracing::info!(
                    index = q.index,
                    phases = q.phase_count,
                    x = ?q.out_x.as_ref(),
                    y = ?q.out_y.as_ref(),
                    "the title's camera jitter (made zero)"
                );
            }
            if let Some(x) = q.out_x.as_mut() {
                *x = 0.0;
            }
            if let Some(y) = q.out_y.as_mut() {
                *y = 0.0;
            }
        }
    }
    result
}

fn keep_jitter() -> bool {
    static KEEP: OnceLock<bool> = OnceLock::new();
    *KEEP.get_or_init(|| std::env::var_os("VICTORY_UPSCALER_JITTER").is_some_and(|v| v == "1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(Upscaler::parse("FSR4"), Some(Upscaler::Title));
        assert_eq!(Upscaler::parse(""), Some(Upscaler::Title));
        assert_eq!(Upscaler::parse("none"), Some(Upscaler::Plain));
        assert_eq!(Upscaler::parse(" fxaa "), Some(Upscaler::Fxaa));
        assert_eq!(Upscaler::parse("dlss"), None);
    }

    /// The layouts of the public headers (x64).
    #[test]
    fn layouts() {
        assert_eq!(std::mem::size_of::<Resource>(), 48);
        assert_eq!(std::mem::offset_of!(DispatchUpscale, command_list), 16);
        assert_eq!(std::mem::offset_of!(DispatchUpscale, color), 24);
        assert_eq!(std::mem::offset_of!(DispatchUpscale, output), 312);
        assert_eq!(std::mem::offset_of!(DispatchUpscale, jitter), 360);
        assert_eq!(std::mem::offset_of!(DispatchUpscale, render_size), 376);
        assert_eq!(std::mem::offset_of!(DispatchUpscale, upscale_size), 384);
        assert_eq!(std::mem::offset_of!(QueryJitterOffset, out_x), 24);
    }
}
