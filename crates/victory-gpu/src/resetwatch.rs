//! Diagnostics: who else resets the kernel's write watch.
//!
//! Our askings (`crate::watch::kernel_ask`) reset what the kernel noted of
//! the pages they ask about, and note it in the journal. A reset by anyone
//! else (the graphics runtime, the driver, the title) forgets writes we
//! never hear of: copies of such pages stay stale. The import tables of
//! every module loaded are pointed at wrappers of `GetWriteWatch`,
//! `ResetWriteWatch` and their `Nt`/`Zw` calls (kernelbase's own included,
//! which the others go through), and a reset that is not ours is counted
//! and the first ones said with the modules on the caller's stack
//! (`VICTORY_RESET_WATCH`, on unless `=0`, diagnostics builds).
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

type Handle = *mut c_void;

thread_local! {
    static OURS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks this thread's write-watch calls as ours until dropped.
pub(crate) struct Ours(bool);

impl Ours {
    pub(crate) fn mark() -> Ours {
        Ours(OURS.replace(true))
    }
}

impl Drop for Ours {
    fn drop(&mut self) {
        OURS.set(self.0);
    }
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RESET_WATCH").is_none_or(|v| v != "0"))
}

/// The real functions: GetWriteWatch, ResetWriteWatch, NtGetWriteWatch,
/// NtResetWriteWatch.
static REAL: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// Since the last line: resets not ours, their bytes, calls of ours seen;
/// and since the start, those said.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

const WRITE_WATCH_FLAG_RESET: u32 = 1;

fn foreign(what: &'static str, at: usize, len: usize) {
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    COUNTS[1].fetch_add(len as u64, Ordering::Relaxed);
    let n = COUNTS[3].fetch_add(1, Ordering::Relaxed) + 1;
    if n > 64 && !n.is_power_of_two() {
        return;
    }
    let stack = stack();
    tracing::warn!(n, "the kernel's write watch reset by another than us (VICTORY_RESET_WATCH): {what} {at:#x}+{len:#x}; stack: {stack}");
}

/// The modules and offsets of the caller's stack, a few frames.
fn stack() -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleExW,
    };
    let mut frames = [std::ptr::null_mut::<c_void>(); 12];
    // SAFETY: room for the frames asked.
    let n = unsafe { RtlCaptureStackBackTrace(2, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut()) } as usize;
    let mut out = Vec::new();
    for &frame in &frames[..n] {
        let mut module = std::ptr::null_mut();
        // SAFETY: a query of the module holding an address.
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                frame as *const u16,
                &mut module,
            )
        } != 0;
        if !found {
            out.push(format!("{:#x}", frame as usize));
            continue;
        }
        let mut name = [0u16; 260];
        // SAFETY: room for the name.
        let len = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) } as usize;
        let name = String::from_utf16_lossy(&name[..len]);
        let short = name.rsplit(['\\', '/']).next().unwrap_or(&name).to_string();
        out.push(format!("{short}+{:#x}", frame as usize - module as usize));
    }
    out.join(" < ")
}

unsafe extern "system" fn get_write_watch(flags: u32, at: Handle, size: usize, found: Handle, count: Handle, granularity: Handle) -> u32 {
    if flags & WRITE_WATCH_FLAG_RESET != 0 {
        if OURS.get() {
            COUNTS[2].fetch_add(1, Ordering::Relaxed);
        } else {
            foreign("GetWriteWatch(reset)", at as usize, size);
        }
    }
    let _ours = Ours::mark();
    // SAFETY: the real function, with the caller's arguments.
    let real: unsafe extern "system" fn(u32, Handle, usize, Handle, Handle, Handle) -> u32 = unsafe { std::mem::transmute(REAL[0].load(Ordering::Relaxed)) };
    unsafe { real(flags, at, size, found, count, granularity) }
}

unsafe extern "system" fn reset_write_watch(at: Handle, size: usize) -> u32 {
    if !OURS.get() {
        foreign("ResetWriteWatch", at as usize, size);
    }
    let _ours = Ours::mark();
    // SAFETY: as above.
    let real: unsafe extern "system" fn(Handle, usize) -> u32 = unsafe { std::mem::transmute(REAL[1].load(Ordering::Relaxed)) };
    unsafe { real(at, size) }
}

unsafe extern "system" fn nt_get_write_watch(
    process: Handle,
    flags: u32,
    at: Handle,
    size: usize,
    found: Handle,
    count: Handle,
    granularity: Handle,
) -> i32 {
    if flags & WRITE_WATCH_FLAG_RESET != 0 && !OURS.get() {
        foreign("NtGetWriteWatch(reset)", at as usize, size);
    }
    // SAFETY: as above.
    let real: unsafe extern "system" fn(Handle, u32, Handle, usize, Handle, Handle, Handle) -> i32 =
        unsafe { std::mem::transmute(REAL[2].load(Ordering::Relaxed)) };
    unsafe { real(process, flags, at, size, found, count, granularity) }
}

unsafe extern "system" fn nt_reset_write_watch(process: Handle, at: Handle, size: usize) -> i32 {
    if !OURS.get() {
        foreign("NtResetWriteWatch", at as usize, size);
    }
    // SAFETY: as above.
    let real: unsafe extern "system" fn(Handle, Handle, usize) -> i32 = unsafe { std::mem::transmute(REAL[3].load(Ordering::Relaxed)) };
    unsafe { real(process, at, size) }
}

/// The wrapper of an imported function's name, and which [`REAL`] it uses.
fn wrapper_of(name: &[u8]) -> Option<(usize, usize)> {
    Some(match name {
        b"GetWriteWatch" => (get_write_watch as *const () as usize, 0),
        b"ResetWriteWatch" => (reset_write_watch as *const () as usize, 1),
        b"NtGetWriteWatch" | b"ZwGetWriteWatch" => (nt_get_write_watch as *const () as usize, 2),
        b"NtResetWriteWatch" | b"ZwResetWriteWatch" => (nt_reset_write_watch as *const () as usize, 3),
        _ => return None,
    })
}

/// Points the import tables of the modules loaded now at the wrappers
/// (again for modules loaded since); how many entries changed.
pub(crate) fn arm() -> usize {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    if !on() {
        return 0;
    }
    if REAL[0].load(Ordering::Relaxed) == 0 {
        for (i, (dll, name)) in [
            (c"kernelbase.dll", c"GetWriteWatch"),
            (c"kernelbase.dll", c"ResetWriteWatch"),
            (c"ntdll.dll", c"NtGetWriteWatch"),
            (c"ntdll.dll", c"NtResetWriteWatch"),
        ]
        .into_iter()
        .enumerate()
        {
            // SAFETY: NUL-terminated names of modules every process has.
            let module = unsafe { GetModuleHandleA(dll.as_ptr().cast()) };
            if module.is_null() {
                return 0;
            }
            // SAFETY: as above.
            match unsafe { GetProcAddress(module, name.as_ptr().cast()) } {
                Some(f) => REAL[i].store(f as *const () as usize, Ordering::Relaxed),
                None => return 0,
            }
        }
    }
    let mut changed = 0;
    for module in modules() {
        // SAFETY: a module the loader mapped; its import tables as it bound them.
        changed += unsafe { wrap(module) };
    }
    changed
}

fn modules() -> Vec<usize> {
    use windows_sys::Win32::System::ProcessStatus::EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut handles = vec![std::ptr::null_mut(); 1024];
    let mut needed = 0u32;
    // SAFETY: room for the handles asked.
    let ok = unsafe {
        EnumProcessModules(GetCurrentProcess(), handles.as_mut_ptr(), (handles.len() * size_of::<Handle>()) as u32, &mut needed)
    } != 0;
    if !ok {
        return Vec::new();
    }
    let n = (needed as usize / size_of::<Handle>()).min(handles.len());
    handles[..n].iter().map(|&h| h as usize).collect()
}

/// See [`arm`]: the entries of one module.
///
/// # Safety
/// `base` is a module the loader mapped.
unsafe fn wrap(base: usize) -> usize {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    let mut changed = 0;
    // SAFETY: the headers, the import directory and the tables it names lie
    // in the mapped image; an entry is eight bytes, made writable for the store.
    unsafe {
        let at = |rva: u32| base + rva as usize;
        let read = |at: usize| (at as *const u32).read_unaligned();
        if (base as *const u16).read_unaligned() != 0x5a4d {
            return 0;
        }
        let headers = at(read(base + 0x3c));
        if read(headers) != 0x4550 || (headers as *const u16).add(12).read_unaligned() != 0x20b {
            return 0;
        }
        let (rva, size) = (read(headers + 0x18 + 0x70 + 8), read(headers + 0x18 + 0x70 + 12));
        if rva == 0 || size == 0 {
            return 0;
        }
        let mut descriptor = at(rva);
        let ours: [usize; 4] = [
            get_write_watch as *const () as usize,
            reset_write_watch as *const () as usize,
            nt_get_write_watch as *const () as usize,
            nt_reset_write_watch as *const () as usize,
        ];
        loop {
            let (names, first) = (read(descriptor), read(descriptor + 16));
            if first == 0 {
                break;
            }
            let names = if names != 0 { names } else { first };
            for i in 0.. {
                let entry = (at(names) as *const u64).add(i).read_unaligned();
                if entry == 0 {
                    break;
                }
                if entry >> 63 != 0 {
                    continue;
                }
                let name = std::ffi::CStr::from_ptr((at(entry as u32) + 2) as *const _);
                let Some((wrapper, _)) = wrapper_of(name.to_bytes()) else {
                    continue;
                };
                let slot = (at(first) as *mut usize).add(i);
                let now = (*slot.cast::<AtomicUsize>()).load(Ordering::SeqCst);
                if ours.contains(&now) {
                    continue;
                }
                let (mut old, mut ignored) = (0, 0);
                if VirtualProtect(slot.cast(), 8, PAGE_READWRITE, &mut old) != 0 {
                    (*slot.cast::<AtomicUsize>()).store(wrapper, Ordering::SeqCst);
                    VirtualProtect(slot.cast(), 8, old, &mut ignored);
                    changed += 1;
                }
            }
            descriptor += 20;
        }
    }
    changed
}

/// The frame line: resets not ours per frame (armed again first, for
/// modules loaded since).
pub(crate) fn line(frames: f64) {
    let changed = arm();
    let c: Vec<f64> = COUNTS[..3].iter().map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "the kernel's write watch reset per frame by others than us (VICTORY_RESET_WATCH): {:.2}, {:.1} MB; resets of ours seen through the wrappers {:.0}; import entries pointed at the wrappers now {changed}",
        c[0],
        c[1] / 1e6,
        c[2],
    );
}
