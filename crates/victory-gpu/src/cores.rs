//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

/// The system's processor sets of group 0: each one's id and its efficiency
/// class (higher is faster).
fn cpu_sets() -> Vec<(u32, u8)> {
    cpu_sets_with_cache().into_iter().map(|(id, class, _, _)| (id, class)).collect()
}

/// [`cpu_sets`] with each one's last-level cache (on a processor of two
/// dies, which die) and its core (two sets of one core are its two hardware
/// threads).
fn cpu_sets_with_cache() -> Vec<(u32, u8, u8, u8)> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            sets.push((set.Id, set.EfficiencyClass, set.LastLevelCacheIndex, set.CoreIndex));
        }
        at += info.Size as usize;
    }
    sets
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.1 == best).map(|c| c.0).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// `VICTORY_FAST_CORES`: 0: no thread asks; 1: the threads a frame waits for
/// one by one; 2 (the default): the threads they share their work with too;
/// 3: those that work beside the rest as well (lists translated, descriptors
/// made): more threads by then than the fast cores run at a time, while the
/// slow ones stood idle.
fn stage() -> u32 {
    static STAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_FAST_CORES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// Asks the system to run the calling thread, which a frame waits for, on
/// the fast cores only, where the processor has two kinds. That fork's submit
/// thread was found on a slow one in 1,529 looks of 24,021. (A selection of
/// processors is a preference the scheduler keeps unless nothing of it is
/// free.) The title's threads are not asked for.
pub(crate) fn keep_on_fast_cores(what: &str) {
    ask(what, 1);
}

/// [`keep_on_fast_cores`] for one of the threads a submission's preparing or
/// a landing shares its work with.
pub(crate) fn keep_helper_on_fast_cores(what: &str) {
    ask(what, 2);
}

/// [`keep_on_fast_cores`] for one of many threads whose work is ahead of what
/// a frame waits for.
pub(crate) fn keep_many_on_fast_cores(what: &str) {
    ask(what, 3);
}

/// `VICTORY_CHAIN_PRIORITY=1`: the threads of ours a frame waits for one by
/// one (submit, relay, the heaps' records, the views beside, the late
/// helper) run above normal priority, so that one the title's or a pool's
/// thread woke runs at once instead of behind them. The timeline of run
/// foxmem3 had 4.3 to 4.5 ms of every frame's chain in threads woken but
/// not running yet, with some 30 threads of ours and the title's on 24
/// processors.
fn chain_priority() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_CHAIN_PRIORITY"))
}

/// On a processor whose cores share two or more last caches (dies): the
/// title's threads and ours a frame waits for one by one keep to the first
/// die, the pools of ours (lists translated, landing, views, the wide jobs)
/// to the others (`VICTORY_CCD_SPLIT=0`: anywhere, as before). A preference
/// the system keeps unless nothing of it is free. On a 7900X, frames
/// 1000-3000 of the same scene: preparing 46 to 35 ms, frames 55.4 to 53.2
/// ms in two pairs of runs each on two builds.
fn split() -> Option<&'static (Vec<u32>, Vec<u32>)> {
    static SPLIT: std::sync::OnceLock<Option<(Vec<u32>, Vec<u32>)>> = std::sync::OnceLock::new();
    SPLIT
        .get_or_init(|| {
            if std::env::var_os("VICTORY_CCD_SPLIT").is_some_and(|v| v == "0") {
                return None;
            }
            let sets = cpu_sets_with_cache();
            let first = sets.iter().map(|s| s.2).min()?;
            let (near, far): (Vec<&(u32, u8, u8, u8)>, Vec<&(u32, u8, u8, u8)>) = sets.iter().partition(|s| s.2 == first);
            if near.is_empty() || far.is_empty() {
                return None;
            }
            let near_cores: Vec<(u32, u8)> = near.iter().map(|s| (s.0, s.3)).collect();
            let (near, far): (Vec<u32>, Vec<u32>) = (near.iter().map(|s| s.0).collect(), far.iter().map(|s| s.0).collect());
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessDefaultCpuSets};
            // SAFETY: our own process and the system's own set ids.
            let ok = unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), near.as_ptr(), near.len() as u32) };
            tracing::info!(
                first = near.len(),
                others = far.len(),
                ok,
                "the title's threads and the chain's keep to the first die, the pools of ours to the others (VICTORY_CCD_SPLIT)"
            );
            master_alone(near_cores);
            Some((near, far))
        })
        .as_ref()
}

/// Under [`split`], keeps the calling thread to its part: `chain`, the
/// first die. Whether it did.
fn placed(what: &str, chain: bool) -> bool {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    let Some((near, far)) = split() else {
        return false;
    };
    // Ours a frame waits for one by one keep to the other dies too: the
    // first is left to the title's threads alone, whose job master runs the
    // title's code for most of a frame while its workers wait for it.
    // Frames 1000-3000: 50.7 and 50.2 ms with them on the first die, 47.7
    // and 46.9 ms without (`VICTORY_CCD_CHAIN_FAR=0`: on the first die).
    static CHAIN_FAR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let chain = chain && !*CHAIN_FAR.get_or_init(|| !std::env::var_os("VICTORY_CCD_CHAIN_FAR").is_some_and(|v| v == "0"));
    let sets = if chain { near } else { far };
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), sets.as_ptr(), sets.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept to its die");
    }
    ok != 0
}

fn ask(what: &str, from: u32) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    if from == 1 && chain_priority() {
        use windows_sys::Win32::System::Threading::{SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        // SAFETY: this thread's own priority.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    }
    if placed(what, from == 1) {
        return;
    }
    if stage() < from {
        return;
    }
    static FAST: std::sync::OnceLock<(Vec<u32>, usize)> = std::sync::OnceLock::new();
    let (fast, of) = FAST.get_or_init(|| {
        let sets = cpu_sets();
        (fast_of(&sets), sets.len())
    });
    // All alike (or nothing known): nothing to ask for.
    if fast.is_empty() {
        return;
    }
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), fast.as_ptr(), fast.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
    } else {
        tracing::info!(what, fast = fast.len(), of, "the thread is kept on the fast cores (VICTORY_FAST_CORES=0: not)");
    }
}

/// `VICTORY_MASTER_ALONE`, under [`split`]: the title's busiest thread (its
/// job master, which runs the title's code for most of a frame while the
/// others wait for it) is kept to one hardware thread of the first die's
/// last core; with `1` the core's other hardware thread is left out of what
/// the title's other threads run on, so the master has the core to itself;
/// with `2` only the master is kept there. Looked at every two seconds: the
/// busiest of the threads not ours that ask for no processors of their own.
fn master_alone(near: Vec<(u32, u8)>) {
    let how = std::env::var("VICTORY_MASTER_ALONE").ok().and_then(|v| v.trim().parse::<u8>().ok()).unwrap_or(0);
    if how == 0 {
        return;
    }
    let Some(&(_, core)) = near.iter().max_by_key(|s| s.1) else {
        return;
    };
    let mine: Vec<u32> = near.iter().filter(|s| s.1 == core).map(|s| s.0).collect();
    if mine.len() < 2 && how == 1 {
        return;
    }
    let one = mine[0];
    let rest: Vec<u32> = near.iter().filter(|s| s.1 != core).map(|s| s.0).collect();
    let _ = std::thread::Builder::new().name("victory-master-watch".into()).spawn(move || {
        if how == 1 {
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessDefaultCpuSets};
            // SAFETY: our own process and the system's own set ids.
            let ok = unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), rest.as_ptr(), rest.len() as u32) };
            tracing::info!(left = rest.len(), ok, "the title's threads leave the first die's last core to its master (VICTORY_MASTER_ALONE)");
        }
        watch_master(one);
    });
}

/// A thread of the process: its handle and its processor time when last looked at.
struct Seen {
    handle: windows_sys::Win32::Foundation::HANDLE,
    cpu: u64,
}

fn watch_master(one: u32) {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadSelectedCpuSets, GetThreadTimes, OpenThread,
        SetThreadSelectedCpuSets, THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_LIMITED_INFORMATION,
    };
    let mut seen: HashMap<u32, Seen> = HashMap::new();
    let mut master: Option<u32> = None;
    const EVERY: std::time::Duration = std::time::Duration::from_secs(2);
    loop {
        std::thread::sleep(EVERY);
        // SAFETY: a snapshot of the system's threads, walked with a record
        // of the size it says; ours are told by the process id.
        let ids = unsafe {
            let mut ids = Vec::new();
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot != INVALID_HANDLE_VALUE {
                let me = GetCurrentProcessId();
                let mut entry: THREADENTRY32 = std::mem::zeroed();
                entry.dwSize = size_of::<THREADENTRY32>() as u32;
                let mut more = Thread32First(snapshot, &mut entry) != 0;
                while more {
                    if entry.th32OwnerProcessID == me {
                        ids.push(entry.th32ThreadID);
                    }
                    more = Thread32Next(snapshot, &mut entry) != 0;
                }
                CloseHandle(snapshot);
            }
            ids
        };
        seen.retain(|id, s| {
            let alive = ids.contains(id);
            if !alive {
                // SAFETY: opened below, closed once.
                unsafe { CloseHandle(s.handle) };
            }
            alive
        });
        let mut busiest: Option<(u64, u32)> = None;
        for id in ids {
            let s = seen.entry(id).or_insert_with(|| Seen {
                // SAFETY: a thread of our own process, kept open while it lives.
                handle: unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION | THREAD_SET_LIMITED_INFORMATION, 0, id) },
                cpu: 0,
            });
            if s.handle.is_null() {
                continue;
            }
            // SAFETY: an open thread; the times are written into the array.
            let cpu = unsafe {
                let mut t: [FILETIME; 4] = std::mem::zeroed();
                let [a, b, c, d] = &mut t;
                GetThreadTimes(s.handle, a, b, c, d);
                let of = |f: &FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
                of(&t[2]) + of(&t[3])
            };
            let ran = cpu.saturating_sub(s.cpu);
            let first = s.cpu == 0;
            s.cpu = cpu;
            if first {
                continue;
            }
            if master != Some(id) {
                // Ours, or one that asked for processors of its own: not the title's master.
                let mut count = 0u32;
                // SAFETY: a count query on an open thread.
                unsafe { GetThreadSelectedCpuSets(s.handle, std::ptr::null_mut(), 0, &mut count) };
                if count > 0 {
                    continue;
                }
                let mut name: windows_sys::core::PWSTR = std::ptr::null_mut();
                // SAFETY: the system allocates the name; freed below.
                let ours = unsafe {
                    if GetThreadDescription(s.handle, &mut name) >= 0 && !name.is_null() {
                        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
                        let text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
                        LocalFree(name.cast());
                        text.starts_with("victory")
                    } else {
                        false
                    }
                };
                if ours {
                    continue;
                }
            }
            if busiest.is_none_or(|(most, _)| ran > most) {
                busiest = Some((ran, id));
            }
        }
        // Half the time at least (100 ns units), and not the one already kept.
        let Some((ran, id)) = busiest.filter(|&(ran, _)| ran > EVERY.as_nanos() as u64 / 200) else {
            continue;
        };
        if master == Some(id) {
            continue;
        }
        if let Some(old) = master.and_then(|m| seen.get(&m)) {
            // SAFETY: an open thread; no sets: as the process says.
            unsafe { SetThreadSelectedCpuSets(old.handle, std::ptr::null(), 0) };
        }
        // SAFETY: an open thread and one of the system's own set ids.
        let ok = unsafe { SetThreadSelectedCpuSets(seen[&id].handle, &one, 1) };
        tracing::info!(thread = id, ms = ran / 10_000, ok, "the title's busiest thread is kept to a core of its own (VICTORY_MASTER_ALONE)");
        master = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_of(&[(256, 1), (257, 1), (258, 0)]), [256, 257]);
        assert!(fast_of(&[(256, 0), (257, 0)]).is_empty());
        assert!(fast_of(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }
}
