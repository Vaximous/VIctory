//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

/// The system's processor sets of group 0: each one's id and its efficiency
/// class (higher is faster).
fn cpu_sets() -> Vec<(u32, u8)> {
    cpu_sets_with_cache().into_iter().map(|(id, class, _)| (id, class)).collect()
}

/// [`cpu_sets`] with each one's last-level cache (on a processor of two
/// dies, which die).
fn cpu_sets_with_cache() -> Vec<(u32, u8, u8)> {
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
            sets.push((set.Id, set.EfficiencyClass, set.LastLevelCacheIndex));
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
            let (near, far): (Vec<&(u32, u8, u8)>, Vec<&(u32, u8, u8)>) = sets.iter().partition(|s| s.2 == first);
            if near.is_empty() || far.is_empty() {
                return None;
            }
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
