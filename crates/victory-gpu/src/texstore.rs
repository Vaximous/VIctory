//! A store of textures' larger levels by the bytes of their smaller ones
//! (`VICTORY_TEXTURE_STORE=<folder>`; off unless set), for the textures made
//! level by level (`crate::tiles`). The title streams a texture's small
//! levels first and its large ones later, or never while it is far away: a
//! texture met before with all of its levels is known by its smaller ones,
//! and its larger levels are filled from here into the levels the title does
//! not hold yet, so it shows whole at once. What the title later writes there
//! replaces them, as any upload does.
//!
//! Key: the texture's shape (width, height, format, levels) and a 128-bit
//! digest (`crate::texture::digest`) of the host bytes of its levels from
//! the key level on: the largest level from which the rest take 32 KiB or
//! more (so a few blocks of a flat colour do not stand for a texture).
//! Kept in memory up to 2 GB (`VICTORY_TEXTURE_STORE_MB`), least lately used
//! going first, and in the folder, written by a thread of its own.
//!
//! After Belobug/VIctory-Belobug-AMD 9386b828 and 7e82cf51 (GPL-3.0), which
//! keep such a store for the copies made from their first viewed level.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

/// Since the start: textures stored, bytes stored, lookups that found
/// levels (memory, folder), levels filled, lookups that found none.
pub(crate) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// The least bytes of the levels a key is taken of.
const KEY_BYTES: usize = 32 << 10;

/// The folder, if on.
fn folder() -> Option<&'static PathBuf> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let folder = PathBuf::from(std::env::var_os("VICTORY_TEXTURE_STORE")?);
            std::fs::create_dir_all(&folder).ok()?;
            tracing::info!(folder = %folder.display(), mb = most() >> 20, "texture store on (VICTORY_TEXTURE_STORE)");
            Some(folder)
        })
        .as_ref()
}

/// Whether the store is on.
pub(crate) fn on() -> bool {
    folder().is_some()
}

fn most() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_STORE_MB").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(2048) << 20
    })
}

/// The levels below the key level, as the staging buffer lays them out:
/// one part a level, in order.
type Levels = Arc<Vec<Vec<u8>>>;

#[derive(Default)]
struct Kept {
    map: HashMap<u128, Levels>,
    order: VecDeque<u128>,
    bytes: u64,
    /// The keys in the folder (read once at the start, added to as written).
    on_disk: Option<HashSet<u128>>,
}

static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

fn with_kept<T>(f: impl FnOnce(&mut Kept) -> T) -> T {
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    let kept = kept.get_or_insert_with(Kept::default);
    if kept.on_disk.is_none() {
        let mut keys = HashSet::new();
        if let Some(folder) = folder()
            && let Ok(dir) = std::fs::read_dir(folder)
        {
            for entry in dir.flatten() {
                if let Some(key) = entry
                    .file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".lv"))
                    .and_then(|hex| u128::from_str_radix(hex, 16).ok())
                {
                    keys.insert(key);
                }
            }
        }
        kept.on_disk = Some(keys);
    }
    f(kept)
}

fn keep(kept: &mut Kept, key: u128, levels: Levels) {
    let len: u64 = levels.iter().map(|l| l.len() as u64).sum();
    if kept.map.insert(key, levels).is_none() {
        kept.order.push_back(key);
        kept.bytes += len;
    }
    while kept.bytes > most()
        && let Some(old) = kept.order.pop_front()
    {
        if let Some(gone) = kept.map.remove(&old) {
            kept.bytes -= gone.iter().map(|l| l.len() as u64).sum::<u64>();
        }
    }
}

/// The key level of a texture whose levels (level 0 first) take `sizes`
/// bytes each: the largest level from which the rest take [`KEY_BYTES`] or
/// more. `None` if all of them take less, or the key level is level 0.
pub(crate) fn key_level(sizes: &[usize]) -> Option<usize> {
    let mut rest = 0;
    for level in (0..sizes.len()).rev() {
        rest += sizes[level];
        if rest >= KEY_BYTES {
            if level > 0 {
                return Some(level);
            }
            // Only with the first level is there enough: the rest if it
            // is half enough still.
            return (sizes.len() > 1 && rest - sizes[0] >= KEY_BYTES / 2).then_some(1);
        }
    }
    None
}

/// The key of a texture of shape `shape` whose levels from the key level on
/// are `small` (in order).
pub(crate) fn key(shape: [u32; 4], small: &[&[u8]]) -> u128 {
    let mut bytes = Vec::with_capacity(16 + small.iter().map(|s| s.len()).sum::<usize>());
    for word in shape {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    for part in small {
        bytes.extend_from_slice(part);
    }
    crate::texture::digest(&bytes).value()
}

/// The larger levels of the texture of `key`, if stored: from memory, or
/// read from the folder.
pub(crate) fn find(key: u128) -> Option<Levels> {
    let (found, on_disk) = with_kept(|kept| {
        (kept.map.get(&key).cloned(), kept.on_disk.as_ref().is_some_and(|d| d.contains(&key)))
    });
    if let Some(found) = found {
        STATS[2].fetch_add(1, Relaxed);
        return Some(found);
    }
    if !on_disk {
        STATS[5].fetch_add(1, Relaxed);
        return None;
    }
    let levels = read(&folder()?.join(format!("{key:032x}.lv")))?;
    let levels = Arc::new(levels);
    with_kept(|kept| keep(kept, key, levels.clone()));
    STATS[3].fetch_add(1, Relaxed);
    Some(levels)
}

/// Whether the texture of `key` is stored.
pub(crate) fn known(key: u128) -> bool {
    with_kept(|kept| kept.map.contains_key(&key) || kept.on_disk.as_ref().is_some_and(|d| d.contains(&key)))
}

/// Keeps the larger levels `levels` of the texture of `key`, and has them
/// written to the folder.
pub(crate) fn store(key: u128, levels: Vec<Vec<u8>>) {
    let Some(folder) = folder() else { return };
    if known(key) {
        return;
    }
    let levels = Arc::new(levels);
    let len: u64 = levels.iter().map(|l| l.len() as u64).sum();
    with_kept(|kept| {
        keep(kept, key, levels.clone());
        if let Some(d) = kept.on_disk.as_mut() {
            d.insert(key);
        }
    });
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(len, Relaxed);
    writer(folder.join(format!("{key:032x}.lv")), levels);
}

/// The file: the count of levels, each level's bytes, then the levels.
fn read(path: &std::path::Path) -> Option<Vec<Vec<u8>>> {
    let all = std::fs::read(path).ok()?;
    let word = |at: usize| all.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or([0; 8])) as usize);
    let count = word(0)?;
    if count > 16 {
        return None;
    }
    let mut at = 8 + 8 * count;
    let mut levels = Vec::with_capacity(count);
    for i in 0..count {
        let len = word(8 + 8 * i)?;
        levels.push(all.get(at..at + len)?.to_vec());
        at += len;
    }
    Some(levels)
}

fn writer(path: PathBuf, levels: Levels) {
    type Job = (PathBuf, Levels);
    static SEND: OnceLock<Option<Mutex<std::sync::mpsc::Sender<Job>>>> = OnceLock::new();
    let send = SEND.get_or_init(|| {
        let (send, jobs) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("victory-texture-store".into())
            .spawn(move || {
                for (path, levels) in jobs {
                    let mut bytes = Vec::new();
                    bytes.extend_from_slice(&(levels.len() as u64).to_le_bytes());
                    for level in levels.iter() {
                        bytes.extend_from_slice(&(level.len() as u64).to_le_bytes());
                    }
                    for level in levels.iter() {
                        bytes.extend_from_slice(level);
                    }
                    let partial = path.with_extension("part");
                    if std::fs::write(&partial, &bytes).is_ok() {
                        let _ = std::fs::rename(&partial, &path);
                    }
                }
            })
            .ok()
            .map(|_| Mutex::new(send))
    });
    if let Some(send) = send {
        let _ = send.lock().unwrap_or_else(|e| e.into_inner()).send((path, levels));
    }
}

/// For the log: what the store did since the start.
pub(crate) fn line() -> Option<String> {
    if !on() {
        return None;
    }
    let s = STATS.each_ref().map(|s| s.load(Relaxed));
    Some(format!(
        "texture store (VICTORY_TEXTURE_STORE), since the start: {} textures stored ({} MB), {} lookups found levels in memory and {} in the folder, {} levels filled, {} lookups found none",
        s[0],
        s[1] >> 20,
        s[2],
        s[3],
        s[4],
        s[5]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_level_leaves_enough_bytes_behind_it() {
        assert_eq!(key_level(&[65536, 16384, 4096, 1024]), Some(1));
        assert_eq!(key_level(&[65536, 32768, 8192]), Some(1));
        assert_eq!(key_level(&[40000, 100]), None);
        assert_eq!(key_level(&[1000, 100]), None);
        assert_eq!(key_level(&[1 << 20, 1 << 18, 1 << 16, 1 << 14, 1 << 12]), Some(2));
    }

    #[test]
    fn a_stored_file_reads_back() {
        let dir = std::env::temp_dir().join(format!("victory-texstore-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.lv");
        let levels = vec![vec![1u8, 2, 3], vec![], vec![9u8; 100]];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(levels.len() as u64).to_le_bytes());
        for l in &levels {
            bytes.extend_from_slice(&(l.len() as u64).to_le_bytes());
        }
        for l in &levels {
            bytes.extend_from_slice(l);
        }
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read(&path), Some(levels));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
