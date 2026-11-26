//! The pipeline book: every pipeline a draw or dispatch of the title has
//! asked for, by its description (its shader containers and graphics
//! state), kept across runs and merged, and the host pipelines built from
//! it ahead of the title ([`Plan`], [`Build`]): at launch, before the title
//! starts, and on in the background when that is cut short.
//!
//! The book holds nothing of the host: the pipeline library (`library`),
//! which is the driver's, is rebuilt from it after a driver update or on a
//! new install, and the library is kept per adapter and driver
//! ([`key_library`]) so that a change of either finds no library of the
//! other. A book can be passed on ([`export`], [`import`]); without its
//! containers it holds only hashes, and the containers are found again in
//! the title's own files ([`missing_shaders`], [`add_containers`]).
//!
//! Files beside the library, the first two only ever added to (a torn end
//! is cut off):
//! - `book-pipelines.bin`, in the order the pipelines were first asked
//!   for: length (u32), key (16 bytes), the number of shaders, each
//!   shader's stage and BLAKE3, whether there is a graphics state, the
//!   graphics state (`GraphicsState::put`).
//! - `book-shaders.bin`: stage, BLAKE3, length (u32), container.
//! - `book-built.txt`: the pipelines the library holds: a stamp (adapter,
//!   driver, translator), then their keys.
//! - `book-bad.txt`: pipelines that were being built when a launch build
//!   ended without finishing, and how often; at two they are left to the
//!   title's own workers.
//!
//! `VICTORY_CACHE_BOOK=0`: nothing is written down.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

const PIPELINES: &str = "book-pipelines.bin";
const SHADERS: &str = "book-shaders.bin";
const BUILT: &str = "book-built.txt";
const BAD: &str = "book-bad.txt";
const BUILDING: &str = "book-building.txt";
/// The adapter and driver the library file is of ([`key_library`]).
const OWNER: &str = "host-pipelines.adapter.txt";
/// The library's file (`library::LIBRARY_FILE`), and a save left beside it.
const LIBRARY: &str = "host-pipelines.bin";
const LIBRARY_NEXT: &str = "host-pipelines.next";
/// Libraries of other adapters or drivers kept, at most.
const KEPT_LIBRARIES: usize = 2;
/// Stages by their number in the files.
const STAGES: [&str; 8] = [
    "vertex",
    "pixel",
    "domain",
    "hull",
    "geometry",
    "compute",
    "amplification",
    "mesh",
];
/// The head of an exported book.
const MAGIC: &[u8; 16] = b"VIctoryPipeBook1";
/// Times a pipeline may be in flight when a launch build ends early.
const SUSPECT: u32 = 2;

type Key = [u8; 16];
type Hash = [u8; 32];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lower-case hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A pipeline's key as `make` writes it (32 hex digits).
fn key_of(text: &str) -> Key {
    let mut id = [0u8; 16];
    for (at, b) in id.iter_mut().enumerate() {
        *b = text
            .get(2 * at..2 * at + 2)
            .and_then(|x| u8::from_str_radix(x, 16).ok())
            .unwrap_or(0);
    }
    id
}

/// `VICTORY_CACHE_BOOK=0`: the pipelines asked for are not written down.
fn recording() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CACHE_BOOK").is_none_or(|v| v != "0"))
}

// ---------------------------------------------------------------------------
// The files.

/// Each whole entry of a pipelines file, in order, and where the last whole
/// one ends.
fn entries(path: &Path) -> (Vec<Vec<u8>>, u64) {
    let bytes = std::fs::read(path).unwrap_or_default();
    let (mut at, mut all) = (0, Vec::new());
    while let Some(head) = bytes.get(at..at + 4) {
        let size = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(entry) = bytes.get(at + 4..at + 4 + size) else {
            break;
        };
        all.push(entry.to_vec());
        at += 4 + size;
    }
    (all, at as u64)
}

/// A container in a shaders file: stage, hash, where its bytes are, length.
#[derive(Clone, Copy)]
struct Place {
    stage: u8,
    hash: Hash,
    at: u64,
    len: u32,
}

/// Each whole container of a shaders file, and where the last whole one ends.
fn containers(path: &Path) -> (Vec<Place>, u64) {
    let Ok(file) = std::fs::File::open(path) else {
        return (Vec::new(), 0);
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let mut file = std::io::BufReader::new(file);
    let (mut at, mut all) = (0, Vec::new());
    let mut head = [0u8; 37];
    while file.read_exact(&mut head).is_ok() {
        let size = u32::from_le_bytes([head[33], head[34], head[35], head[36]]);
        if at + 37 + u64::from(size) > len || file.seek_relative(i64::from(size)).is_err() {
            break;
        }
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..33]);
        all.push(Place {
            stage: head[0],
            hash,
            at: at + 37,
            len: size,
        });
        at += 37 + u64::from(size);
    }
    (all, at)
}

/// Cuts a torn end off `path`.
fn cut(path: &Path, end: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > end)
        && let Ok(f) = std::fs::OpenOptions::new().write(true).open(path)
    {
        let _ = f.set_len(end);
    }
}

fn append(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(bytes)
}

/// A pipelines file's entry, read.
struct Entry<'a> {
    key: Key,
    shaders: Vec<(u8, Hash)>,
    /// The graphics state's bytes, if it has one.
    graphics: Option<&'a [u8]>,
}

fn parse(entry: &[u8]) -> Option<Entry<'_>> {
    let (key, rest) = entry.split_first_chunk::<16>()?;
    let (&count, mut rest) = rest.split_first()?;
    let mut shaders = Vec::new();
    for _ in 0..count {
        let (head, after) = rest.split_first_chunk::<33>()?;
        rest = after;
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..]);
        STAGES.get(usize::from(head[0]))?;
        shaders.push((head[0], hash));
    }
    let (&graphics, rest) = rest.split_first()?;
    Some(Entry {
        key: *key,
        shaders,
        graphics: (graphics != 0).then_some(rest),
    })
}

/// The entry for a pipeline.
fn entry(key: Key, shaders: &[(u8, Hash)], graphics: Option<&GraphicsState>) -> Vec<u8> {
    let mut entry = key.to_vec();
    entry.push(shaders.len() as u8);
    for (stage, hash) in shaders {
        entry.push(*stage);
        entry.extend(hash);
    }
    entry.push(u8::from(graphics.is_some()));
    if let Some(g) = graphics {
        g.put(&mut entry);
    }
    entry
}

fn stage_number(stage: &str) -> u8 {
    STAGES.iter().position(|&s| s == stage).unwrap_or(0) as u8
}

// ---------------------------------------------------------------------------
// Writing down what the title asks for.

struct Book {
    dir: PathBuf,
    keys: HashSet<Key>,
    shaders: HashSet<Hash>,
    /// Not yet written: shaders, then pipelines.
    new: [Vec<u8>; 2],
    added: usize,
}

static BOOK: OnceLock<Option<Mutex<Book>>> = OnceLock::new();
/// The pipelines the title has asked for in this run: a build in the
/// background leaves them to the title's own workers.
static ASKED: Mutex<Option<HashSet<Key>>> = Mutex::new(None);

fn book() -> Option<&'static Mutex<Book>> {
    BOOK.get_or_init(|| {
        if !recording() {
            return None;
        }
        let dir = super::library::folder()?;
        std::fs::create_dir_all(&dir).ok()?;
        let (pipelines, end) = entries(&dir.join(PIPELINES));
        cut(&dir.join(PIPELINES), end);
        let (shaders, end) = containers(&dir.join(SHADERS));
        cut(&dir.join(SHADERS), end);
        let keys: HashSet<Key> = pipelines
            .iter()
            .filter_map(|e| e.first_chunk::<16>().copied())
            .collect();
        let shaders: HashSet<Hash> = shaders.iter().map(|p| p.hash).collect();
        tracing::info!(
            pipelines = keys.len(),
            shaders = shaders.len(),
            "pipeline book"
        );
        std::thread::Builder::new()
            .name("victory-book".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    flush();
                }
            })
            .ok()?;
        Some(Mutex::new(Book {
            dir,
            keys,
            shaders,
            new: Default::default(),
            added: 0,
        }))
    })
    .as_ref()
}

/// A draw or dispatch asked for the pipeline with `key` for the first time
/// in this run.
pub(super) fn used(key: &str, shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) {
    let id = key_of(key);
    lock(&ASKED).get_or_insert_with(HashSet::new).insert(id);
    if shaders.is_empty() {
        return;
    }
    let Some(book) = book() else { return };
    if lock(book).keys.contains(&id) {
        return;
    }
    // Hashed outside the lock: containers are tens of kilobytes.
    let hashes: Vec<(u8, Hash)> = shaders
        .iter()
        .map(|s| {
            (
                stage_number(s.stage),
                *blake3::hash(&s.container).as_bytes(),
            )
        })
        .collect();
    let entry = entry(id, &hashes, graphics);
    let mut book = lock(book);
    if !book.keys.insert(id) {
        return;
    }
    for (s, (stage, hash)) in shaders.iter().zip(&hashes) {
        if book.shaders.insert(*hash) {
            let new = &mut book.new[0];
            new.push(*stage);
            new.extend(hash);
            new.extend((s.container.len() as u32).to_le_bytes());
            new.extend(&s.container);
        }
    }
    book.new[1].extend((entry.len() as u32).to_le_bytes());
    book.new[1].extend(entry);
    book.added += 1;
}

/// Whether the title asked for the pipeline with `key` in this run.
fn asked(key: &Key) -> bool {
    lock(&ASKED).as_ref().is_some_and(|s| s.contains(key))
}

/// Writes what was noted: shaders before the pipelines that name them.
fn flush() {
    let Some(Some(book)) = BOOK.get() else { return };
    let (dir, new, added) = {
        let mut book = lock(book);
        if book.added == 0 && book.new[0].is_empty() {
            return;
        }
        (
            book.dir.clone(),
            std::mem::take(&mut book.new),
            std::mem::take(&mut book.added),
        )
    };
    for (file, bytes) in [SHADERS, PIPELINES].iter().zip(&new) {
        if let Err(e) = append(&dir.join(file), bytes) {
            tracing::warn!(error = %e, file, "pipeline book not written");
            return;
        }
    }
    tracing::info!(pipelines = added, "pipeline book: pipelines written down");
}

// ---------------------------------------------------------------------------
// Passing a book on.

/// What a book file holds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub pipelines: usize,
    pub shaders: usize,
}

/// Writes the book in `dir` to `out`: its pipelines in order, and with
/// `with_shaders` the containers they name (the title's own code: such a
/// file is for owners of the title only). Without them, the containers are
/// found in the title's files on import ([`missing_shaders`]).
pub fn export(dir: &Path, out: &Path, with_shaders: bool) -> std::io::Result<Counts> {
    let (pipelines, _) = entries(&dir.join(PIPELINES));
    let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
    w.write_all(MAGIC)?;
    w.write_all(&(pipelines.len() as u32).to_le_bytes())?;
    for e in &pipelines {
        w.write_all(&(e.len() as u32).to_le_bytes())?;
        w.write_all(e)?;
    }
    let mut counts = Counts {
        pipelines: pipelines.len(),
        shaders: 0,
    };
    let places = if with_shaders {
        containers(&dir.join(SHADERS)).0
    } else {
        Vec::new()
    };
    w.write_all(&(places.len() as u32).to_le_bytes())?;
    if !places.is_empty() {
        let file = std::fs::File::open(dir.join(SHADERS))?;
        for p in &places {
            let bytes = read_at(&file, p.at, p.len)?;
            w.write_all(&[p.stage])?;
            w.write_all(&p.hash)?;
            w.write_all(&p.len.to_le_bytes())?;
            w.write_all(&bytes)?;
            counts.shaders += 1;
        }
    }
    w.flush()?;
    Ok(counts)
}

/// Adds the pipelines and containers of the exported book `from` that the
/// book in `dir` lacks, after its own (in `from`'s order); what was added.
pub fn import(dir: &Path, from: &Path) -> std::io::Result<Counts> {
    let invalid = |what: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("not a pipeline book: {what}"),
        )
    };
    let bytes = std::fs::read(from)?;
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| invalid("no head"))?;
    let mut rest = rest;
    let mut take = |n: usize| -> std::io::Result<&[u8]> {
        let (head, after) = rest
            .split_at_checked(n)
            .ok_or_else(|| invalid("cut short"))?;
        rest = after;
        Ok(head)
    };
    let word = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let mut pipelines = Vec::new();
    for _ in 0..word(take(4)?) {
        let len = word(take(4)?) as usize;
        let e = take(len)?;
        parse(e).ok_or_else(|| invalid("an entry"))?;
        pipelines.push(e);
    }
    let mut shaders = Vec::new();
    for _ in 0..word(take(4)?) {
        let head = take(37)?;
        let container = take(word(&head[33..]) as usize)?;
        if blake3::hash(container).as_bytes() != &head[1..33] {
            return Err(invalid("a container whose hash differs"));
        }
        shaders.push((head, container));
    }
    std::fs::create_dir_all(dir)?;
    let (have, end) = entries(&dir.join(PIPELINES));
    cut(&dir.join(PIPELINES), end);
    let (places, end) = containers(&dir.join(SHADERS));
    cut(&dir.join(SHADERS), end);
    let mut keys: HashSet<Key> = have
        .iter()
        .filter_map(|e| e.first_chunk::<16>().copied())
        .collect();
    let mut hashes: HashSet<Hash> = places.iter().map(|p| p.hash).collect();
    let mut counts = Counts::default();
    let mut out = Vec::new();
    for (head, container) in shaders {
        let mut hash = [0; 32];
        hash.copy_from_slice(&head[1..33]);
        if hashes.insert(hash) {
            out.extend(head);
            out.extend(container);
            counts.shaders += 1;
        }
    }
    append(&dir.join(SHADERS), &out)?;
    out.clear();
    for e in pipelines {
        if e.first_chunk::<16>().is_some_and(|k| keys.insert(*k)) {
            out.extend((e.len() as u32).to_le_bytes());
            out.extend(e);
            counts.pipelines += 1;
        }
    }
    append(&dir.join(PIPELINES), &out)?;
    Ok(counts)
}

/// The containers the book in `dir` names and does not hold (an import
/// without them), and the stage each is named for.
pub fn missing_shaders(dir: &Path) -> HashMap<[u8; 32], u8> {
    let have: HashSet<Hash> = containers(&dir.join(SHADERS))
        .0
        .iter()
        .map(|p| p.hash)
        .collect();
    let mut missing = HashMap::new();
    for e in entries(&dir.join(PIPELINES)).0 {
        for (stage, hash) in parse(&e).map(|e| e.shaders).unwrap_or_default() {
            if !have.contains(&hash) {
                missing.insert(hash, stage);
            }
        }
    }
    missing
}

/// Adds containers found elsewhere (the title's files) that the book in
/// `dir` names and lacks; how many were added.
pub fn add_containers(dir: &Path, found: &[Vec<u8>]) -> std::io::Result<usize> {
    let missing = missing_shaders(dir);
    let (mut out, mut added, mut seen) = (Vec::new(), 0, HashSet::new());
    for c in found {
        let hash = *blake3::hash(c).as_bytes();
        if let Some(&stage) = missing.get(&hash)
            && seen.insert(hash)
        {
            out.push(stage);
            out.extend(hash);
            out.extend((c.len() as u32).to_le_bytes());
            out.extend(c);
            added += 1;
        }
    }
    append(&dir.join(SHADERS), &out)?;
    Ok(added)
}

/// Forgets the pipelines a launch build ended in (`book-bad.txt`).
pub fn forget_bad(dir: &Path) -> usize {
    let n = bad(dir).len();
    let _ = std::fs::remove_file(dir.join(BAD));
    n
}

// ---------------------------------------------------------------------------
// The adapter and driver.

/// The host's adapter and its driver.
#[derive(Debug, Clone)]
pub struct Adapter {
    pub vendor: u32,
    pub device: u32,
    pub subsystem: u32,
    pub revision: u32,
    pub name: String,
    /// As the system counts it (`IDXGIAdapter::CheckInterfaceSupport`).
    pub driver: u64,
}

impl Adapter {
    pub fn vendor_name(&self) -> &'static str {
        match self.vendor {
            0x10de => "NVIDIA",
            0x1002 | 0x1022 => "AMD",
            0x8086 => "Intel",
            0x1414 => "Microsoft",
            0x5143 => "Qualcomm",
            _ => "unknown vendor",
        }
    }

    /// The driver's version as Windows shows it (`a.b.c.d`).
    pub fn driver_version(&self) -> String {
        let d = self.driver;
        format!(
            "{}.{}.{}.{}",
            d >> 48,
            (d >> 32) & 0xffff,
            (d >> 16) & 0xffff,
            d & 0xffff
        )
    }

    /// The vendor's own name for the driver, where it has one (NVIDIA's
    /// `560.94` is the last five digits of the version).
    pub fn vendor_driver(&self) -> Option<String> {
        let (c, d) = ((self.driver >> 16) & 0xffff, self.driver & 0xffff);
        (self.vendor == 0x10de && self.driver != 0).then(|| {
            let n = (c % 10) * 10000 + d;
            format!("{}.{:02}", n / 100, n % 100)
        })
    }

    /// What a library is of: adapter and driver, fit for a file name.
    pub fn stamp(&self) -> String {
        format!(
            "{:04x}-{:04x}-{:08x}-{:02x}-{}",
            self.vendor,
            self.device,
            self.subsystem,
            self.revision,
            self.driver_version()
        )
    }
}

impl std::fmt::Display for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} {:04x}:{:04x}), driver {}",
            self.name,
            self.vendor_name(),
            self.vendor,
            self.device,
            self.driver_version()
        )?;
        if let Some(v) = self.vendor_driver() {
            write!(f, " ({v})")?;
        }
        Ok(())
    }
}

/// The adapter the host device is on (the host is made if it is not yet).
pub fn adapter() -> Option<Adapter> {
    use windows::Win32::Graphics::Dxgi::*;
    use windows::core::Interface;
    let host = crate::host::host()?;
    // SAFETY: plain DXGI queries on live objects.
    unsafe {
        let luid = host.device.GetAdapterLuid();
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        let driver = adapter
            .CheckInterfaceSupport(&IDXGIDevice::IID)
            .unwrap_or(0);
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        Some(Adapter {
            vendor: desc.VendorId,
            device: desc.DeviceId,
            subsystem: desc.SubSysId,
            revision: desc.Revision,
            name: String::from_utf16_lossy(&desc.Description[..end])
                .trim()
                .to_owned(),
            driver: driver as u64,
        })
    }
}

/// What the library in `dir` became, by [`key_library`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keyed {
    /// It is this adapter's and driver's (or there is none yet).
    Same,
    /// It was another's: it is kept aside, and one kept earlier for this
    /// adapter and driver took its place (`true`) or none was there.
    Changed { from: String, restored: bool },
}

/// Keeps the pipeline library in `dir` per adapter and driver: one made
/// on another (a driver update, another card) is set aside under its
/// stamp, and one set aside earlier for this one takes its place. The host
/// would refuse the other's library anyway, and the next save would write
/// an empty one over it. Call before anything opens the library.
pub fn key_library(dir: &Path, adapter: &Adapter) -> Keyed {
    let now = adapter.stamp();
    let (file, next, owner) = (dir.join(LIBRARY), dir.join(LIBRARY_NEXT), dir.join(OWNER));
    // A save the last run left beside the library (`library::Blob::of`).
    if next.exists() {
        let _ = std::fs::rename(&next, &file);
    }
    let old = std::fs::read_to_string(&owner)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let keyed = match old {
        Some(old) if old != now => {
            if file.exists() {
                let _ = std::fs::rename(&file, dir.join(format!("host-pipelines.{old}.bin")));
            }
            let mine = dir.join(format!("host-pipelines.{now}.bin"));
            let restored = mine.exists() && std::fs::rename(&mine, &file).is_ok();
            prune_libraries(dir);
            Keyed::Changed {
                from: old,
                restored,
            }
        }
        _ => Keyed::Same,
    };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&owner, &now);
    keyed
}

/// Keeps the [`KEPT_LIBRARIES`] newest libraries of other adapters or drivers.
fn prune_libraries(dir: &Path) {
    let Ok(list) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kept: Vec<(std::time::SystemTime, PathBuf)> = list
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("host-pipelines.") && name.ends_with(".bin") && name != LIBRARY
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    kept.sort_by_key(|k| std::cmp::Reverse(k.0));
    for (_, path) in kept.into_iter().skip(KEPT_LIBRARIES) {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Building.

/// What the built pipelines are good for: the adapter, the driver and the
/// translator with its settings.
fn stamp(adapter: &Adapter) -> String {
    format!(
        "{} {}",
        adapter.stamp(),
        &hex(&crate::shader_cache::translation())[..16]
    )
}

/// The pipelines the library in `dir` holds of the book, if it was built
/// for `stamp` and is there.
fn built(dir: &Path, stamp: &str) -> HashSet<Key> {
    let Ok(text) = std::fs::read_to_string(dir.join(BUILT)) else {
        return HashSet::new();
    };
    let mut lines = text.lines();
    if lines.next() != Some(stamp) || !dir.join(LIBRARY).exists() {
        return HashSet::new();
    }
    lines.filter(|l| l.len() == 32).map(key_of).collect()
}

fn write_built(dir: &Path, stamp: &str, keys: &HashSet<Key>) {
    let mut text = String::with_capacity(keys.len() * 33 + 64);
    text.push_str(stamp);
    for k in keys {
        text.push('\n');
        text.push_str(&hex(k));
    }
    let tmp = dir.join(format!("{BUILT}.tmp-{}", std::process::id()));
    if std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, dir.join(BUILT)))
        .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The pipelines a launch build ended in, and how often.
fn bad(dir: &Path) -> HashMap<Key, u32> {
    let text = std::fs::read_to_string(dir.join(BAD)).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let (key, n) = l.split_once(' ')?;
            (key.len() == 32).then(|| (key_of(key), n.trim().parse().unwrap_or(1)))
        })
        .collect()
}

/// Counts the pipelines a build left in flight (`book-building.txt`: the
/// process ended during a launch build) as suspects.
fn take_suspects(dir: &Path) -> usize {
    let Ok(file) = std::fs::File::open(dir.join(BUILDING)) else {
        return 0;
    };
    let left: Vec<Key> = std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| l.len() == 32)
        .map(|l| key_of(&l))
        .collect();
    let _ = std::fs::remove_file(dir.join(BUILDING));
    if left.is_empty() {
        return 0;
    }
    let mut all = bad(dir);
    for k in &left {
        *all.entry(*k).or_default() += 1;
    }
    let text: Vec<String> = all.iter().map(|(k, n)| format!("{} {n}", hex(k))).collect();
    let _ = std::fs::write(dir.join(BAD), text.join("\n"));
    left.len()
}

/// What a build would do: the book's pipelines that the library does not
/// hold yet for this adapter, driver and translator.
pub struct Plan {
    dir: PathBuf,
    stamp: String,
    /// The book's pipelines, all of them.
    pub book: usize,
    /// Those the library holds already.
    pub built: usize,
    /// Those left out: a build ended in them before ([`SUSPECT`] times),
    /// or a container of theirs is not in the book.
    pub bad: usize,
    pub unreadable: usize,
    /// Pipelines in flight when the last launch build ended early.
    pub suspects: usize,
    /// What is to be translated and built, in the order of first use.
    shaders: Vec<Place>,
    places: HashMap<Hash, Place>,
    entries: Vec<Vec<u8>>,
    /// What each of them took to make in an earlier run, if that was long
    /// (`library`'s list of slow pipelines; 0: quick or not known).
    cost: Vec<u32>,
    keys_built: HashSet<Key>,
}

/// The pipelines that took long to make in earlier runs, and how long (ms):
/// `library`'s list beside the book (`key ms` lines).
fn slow_list(dir: &Path) -> HashMap<Key, u32> {
    let text = std::fs::read_to_string(dir.join("slow-pipelines.txt")).unwrap_or_default();
    let mut all = HashMap::new();
    for (key, ms) in text.lines().filter_map(|l| l.split_once(' ')) {
        if let (32, Ok(ms)) = (key.len(), ms.trim().parse::<u32>()) {
            let known = all.entry(key_of(key)).or_insert(0);
            *known = ms.max(*known);
        }
    }
    all
}

/// Known to take long: built first, on half the workers, longest first.
const SLOW_MS: u32 = 1000;

impl Plan {
    /// Shaders to translate (or read from the cache).
    pub fn shaders(&self) -> usize {
        self.shaders.len()
    }

    /// Pipelines to build.
    pub fn pipelines(&self) -> usize {
        self.entries.len()
    }
}

/// What the book in `dir` wants built (`again`: whatever the library
/// holds). Makes the host if it is not made yet.
pub fn plan(dir: &Path, again: bool) -> Option<Plan> {
    let adapter = adapter()?;
    let stamp = stamp(&adapter);
    let suspects = take_suspects(dir);
    let keys_built = if again {
        HashSet::new()
    } else {
        built(dir, &stamp)
    };
    let bad = bad(dir);
    let (all, _) = entries(&dir.join(PIPELINES));
    let places: HashMap<Hash, Place> = containers(&dir.join(SHADERS))
        .0
        .into_iter()
        .map(|p| (p.hash, p))
        .collect();
    let mut plan = Plan {
        dir: dir.to_owned(),
        stamp,
        book: 0,
        built: 0,
        bad: 0,
        unreadable: 0,
        suspects,
        shaders: Vec::new(),
        places,
        entries: Vec::new(),
        cost: Vec::new(),
        keys_built,
    };
    let slow = slow_list(dir);
    let (mut seen, mut shaders) = (HashSet::new(), HashSet::new());
    for e in all {
        let Some(parsed) = parse(&e) else { continue };
        if !seen.insert(parsed.key) {
            continue;
        }
        plan.book += 1;
        if plan.keys_built.contains(&parsed.key) {
            plan.built += 1;
            continue;
        }
        if bad.get(&parsed.key).is_some_and(|&n| n >= SUSPECT) {
            plan.bad += 1;
            continue;
        }
        if parsed
            .shaders
            .iter()
            .any(|(_, h)| !plan.places.contains_key(h))
        {
            plan.unreadable += 1;
            continue;
        }
        for (_, h) in &parsed.shaders {
            if shaders.insert(*h) {
                plan.shaders.push(plan.places[h]);
            }
        }
        plan.cost.push(slow.get(&parsed.key).copied().unwrap_or(0));
        plan.entries.push(e);
    }
    Some(plan)
}

/// How far a build is.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    /// Shaders translated (or read from the cache), of all.
    pub shaders: (usize, usize),
    /// Pipelines built, failed or left to the title, of all.
    pub pipelines: (usize, usize),
    /// Pipelines that could not be built (a shader that does not translate,
    /// a pipeline the host refuses).
    pub failed: usize,
    pub elapsed: Duration,
    /// Until all are built, once that can be told.
    pub left: Option<Duration>,
    pub over: bool,
    pub background: bool,
}

impl Progress {
    /// Of the whole, 0 to 1 (shaders count for a tenth).
    pub fn fraction(&self) -> f32 {
        let part = |(done, all): (usize, usize)| {
            if all == 0 {
                1.0
            } else {
                done as f32 / all as f32
            }
        };
        if self.pipelines.1 == 0 {
            return part(self.shaders);
        }
        0.1 * part(self.shaders) + 0.9 * part(self.pipelines)
    }
}

/// A build under way: workers that translate the plan's shaders, then build
/// its pipelines into the library, in the order the title first used them.
#[derive(Clone)]
pub struct Build(Arc<Run>);

struct Run {
    plan: Plan,
    file: Option<std::fs::File>,
    /// The shaders made so far, by hash (they are in the shader table too).
    made: Mutex<HashMap<Hash, Arc<Shader>>>,
    next: AtomicUsize,
    /// The pipelines known to take long, longest first, and the next of
    /// them; every pipeline is taken once, from either list.
    slow: Vec<usize>,
    next_slow: AtomicUsize,
    taken: Vec<AtomicBool>,
    /// For the time left: the known cost of the slow ones not started, the
    /// quick ones not started, and what the quick ones took so far.
    slow_left_ms: AtomicU64,
    quick_left: AtomicUsize,
    quick_ms: AtomicU64,
    quick_done: AtomicUsize,
    workers_at_start: usize,
    shaders_done: AtomicUsize,
    pipelines_done: AtomicUsize,
    failed: AtomicUsize,
    /// Pipelines whose build ended (built or not): written down as built.
    built: Mutex<HashSet<Key>>,
    /// In flight, while the build is in the foreground.
    building: Mutex<HashSet<Key>>,
    background: AtomicBool,
    background_workers: usize,
    workers: AtomicUsize,
    over: (Mutex<bool>, Condvar),
    began: Instant,
    pipelines_began: OnceLock<Instant>,
}

impl Plan {
    /// Starts `workers` threads (`background_workers` of them go on at the
    /// lowest priority once the build is sent to the background).
    pub fn start(self, workers: usize, background_workers: usize) -> Build {
        let file = std::fs::File::open(self.dir.join(SHADERS)).ok();
        let workers = workers.max(1);
        let mut slow: Vec<usize> = (0..self.entries.len())
            .filter(|&n| self.cost[n] >= SLOW_MS)
            .collect();
        slow.sort_by_key(|&n| std::cmp::Reverse(self.cost[n]));
        let slow_left_ms = slow.iter().map(|&n| u64::from(self.cost[n])).sum();
        let quick_left = self.entries.len() - slow.len();
        let run = Arc::new(Run {
            file,
            made: Mutex::new(HashMap::new()),
            next: AtomicUsize::new(0),
            next_slow: AtomicUsize::new(0),
            taken: (0..self.entries.len())
                .map(|_| AtomicBool::new(false))
                .collect(),
            slow,
            slow_left_ms: AtomicU64::new(slow_left_ms),
            quick_left: AtomicUsize::new(quick_left),
            quick_ms: AtomicU64::new(0),
            quick_done: AtomicUsize::new(0),
            workers_at_start: workers,
            shaders_done: AtomicUsize::new(0),
            pipelines_done: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
            built: Mutex::new(self.keys_built.clone()),
            building: Mutex::new(HashSet::new()),
            background: AtomicBool::new(false),
            background_workers,
            workers: AtomicUsize::new(workers),
            over: (Mutex::new(false), Condvar::new()),
            began: Instant::now(),
            pipelines_began: OnceLock::new(),
            plan: self,
        });
        for n in 0..workers {
            let worker = run.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("victory-book-{n}"))
                .spawn(move || worker.work(n));
            if spawned.is_err() && run.workers.fetch_sub(1, Ordering::AcqRel) == 1 {
                run.finish();
            }
        }
        Build(run)
    }
}

fn read_at(file: &std::fs::File, at: u64, len: u32) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0; len as usize];
    let mut done = 0;
    while done < bytes.len() {
        match file.seek_read(&mut bytes[done..], at + done as u64)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => done += n,
        }
    }
    Ok(bytes)
}

fn set_priority(lowest: bool) {
    use windows::Win32::System::Threading::*;
    let priority = if lowest {
        THREAD_PRIORITY_IDLE
    } else {
        THREAD_PRIORITY_BELOW_NORMAL
    };
    // SAFETY: a plain call on our own thread.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), priority) };
}

impl Run {
    fn work(&self, me: usize) {
        set_priority(false);
        let mut low = false;
        let shaders = self.plan.shaders.len();
        loop {
            if self.background.load(Ordering::Relaxed) {
                if me >= self.background_workers {
                    break;
                }
                if !low {
                    set_priority(true);
                    low = true;
                }
            }
            // Half the workers start with the pipelines known to take long
            // (in the foreground): the build ends when the longest do.
            let background = self.background.load(Ordering::Relaxed);
            if me.is_multiple_of(2) && !background && self.take_slow() {
                continue;
            }
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            if let Some(place) = self.plan.shaders.get(n) {
                if let Some(s) = self.shader(place) {
                    s.translated();
                }
                self.shaders_done.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match n
                .checked_sub(shaders)
                .filter(|&n| n < self.plan.entries.len())
            {
                Some(n) => self.build(n),
                // The list in order is through: what is left of the slow ones.
                None if self.take_slow() => {}
                None => break,
            }
        }
        if self.workers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.finish();
        }
    }

    /// Builds the next slow pipeline not taken yet; whether there was one.
    fn take_slow(&self) -> bool {
        loop {
            let k = self.next_slow.fetch_add(1, Ordering::Relaxed);
            let Some(&n) = self.slow.get(k) else {
                return false;
            };
            if !self.taken[n].load(Ordering::Relaxed) {
                self.build(n);
                return true;
            }
        }
    }

    /// Builds entry `n` unless another worker took it.
    fn build(&self, n: usize) {
        if self.taken[n].swap(true, Ordering::Relaxed) {
            return;
        }
        let cost = self.plan.cost[n];
        if cost >= SLOW_MS {
            self.slow_left_ms
                .fetch_sub(u64::from(cost), Ordering::Relaxed);
        } else {
            self.quick_left.fetch_sub(1, Ordering::Relaxed);
        }
        self.pipelines_began.get_or_init(Instant::now);
        let began = Instant::now();
        self.pipeline(&self.plan.entries[n]);
        if cost < SLOW_MS {
            self.quick_ms
                .fetch_add(began.elapsed().as_millis() as u64, Ordering::Relaxed);
            self.quick_done.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn shader(&self, place: &Place) -> Option<Arc<Shader>> {
        if let Some(s) = lock(&self.made).get(&place.hash) {
            return Some(s.clone());
        }
        let bytes = read_at(self.file.as_ref()?, place.at, place.len).ok()?;
        let s = super::shader(STAGES.get(usize::from(place.stage))?, &bytes);
        lock(&self.made).insert(place.hash, s.clone());
        Some(s)
    }

    fn pipeline(&self, entry: &[u8]) {
        let Some(e) = parse(entry) else {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let background = self.background.load(Ordering::Relaxed);
        // The title asked for it: its own workers make it, before anything
        // of ours.
        if background && asked(&e.key) {
            self.pipelines_done.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if !background {
            lock(&self.building).insert(e.key);
        }
        let shaders: Option<Vec<Arc<Shader>>> = e
            .shaders
            .iter()
            .map(|(_, h)| self.plan.places.get(h).and_then(|p| self.shader(p)))
            .collect();
        let graphics = match e.graphics {
            Some(bytes) => GraphicsState::get(bytes).map(Some),
            None => Some(None),
        };
        let made = match (shaders, graphics) {
            (Some(shaders), Some(g)) => HostPipeline::new(&shaders, g.as_ref()).is_some(),
            _ => false,
        };
        if made {
            self.pipelines_done.fetch_add(1, Ordering::Relaxed);
        } else {
            self.failed.fetch_add(1, Ordering::Relaxed);
        }
        lock(&self.built).insert(e.key);
        lock(&self.building).remove(&e.key);
    }

    /// The last worker is out: the library is saved and what it holds
    /// written down.
    fn finish(&self) {
        super::library::save(true);
        let _ = std::fs::remove_file(self.plan.dir.join(BUILDING));
        write_built(&self.plan.dir, &self.plan.stamp, &lock(&self.built));
        tracing::info!(
            built = self.pipelines_done.load(Ordering::Relaxed),
            failed = self.failed.load(Ordering::Relaxed),
            seconds = self.began.elapsed().as_secs_f64(),
            "pipeline book built"
        );
        let (over, signal) = &self.over;
        *lock(over) = true;
        signal.notify_all();
    }
}

impl Build {
    pub fn progress(&self) -> Progress {
        let r = &self.0;
        let shaders = (r.shaders_done.load(Ordering::Relaxed), r.plan.shaders.len());
        let ended = r.pipelines_done.load(Ordering::Relaxed) + r.failed.load(Ordering::Relaxed);
        let pipelines = (ended, r.plan.entries.len());
        let elapsed = r.began.elapsed();
        // The work not started (known costs of the slow ones, the quick
        // ones at what they took so far) over the workers; pipelines in
        // flight are not counted.
        let quick_done = r.quick_done.load(Ordering::Relaxed);
        let background = r.background.load(Ordering::Relaxed);
        let workers = if background {
            r.background_workers
        } else {
            r.workers_at_start
        }
        .max(1);
        let quick_left = r.quick_left.load(Ordering::Relaxed);
        let left = (quick_done >= 8 || quick_left == 0).then(|| {
            let each = r.quick_ms.load(Ordering::Relaxed) as f64 / quick_done.max(1) as f64;
            let ms = r.slow_left_ms.load(Ordering::Relaxed) as f64 + each * quick_left as f64;
            Duration::from_secs_f64(ms / 1000.0 / workers as f64)
        });
        Progress {
            shaders,
            pipelines,
            failed: r.failed.load(Ordering::Relaxed),
            elapsed,
            left,
            over: *lock(&r.over.0),
            background: r.background.load(Ordering::Relaxed),
        }
    }

    /// Lets the title start: the rest is built on the background workers at
    /// the lowest priority, leaving what the title asks for to its own.
    pub fn to_background(&self) {
        self.0.background.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(self.0.plan.dir.join(BUILDING));
    }

    /// Takes no more work; what is in flight ends, then the library is saved.
    pub fn stop(&self) {
        self.0.next.store(usize::MAX / 2, Ordering::Relaxed);
    }

    /// Waits until the build is over or `tick` (called about four times a
    /// second) says to stop waiting; whether it is over. While it waits, the
    /// pipelines in flight are written down, so that one that ends the
    /// process is known at the next start.
    pub fn wait_while(&self, mut tick: impl FnMut(&Progress) -> bool) -> bool {
        let (over, signal) = &self.0.over;
        let mut last = Vec::new();
        loop {
            {
                let held = lock(over);
                if *held {
                    return true;
                }
                let (held, _) = signal
                    .wait_timeout(held, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner());
                if *held {
                    return true;
                }
            }
            if !self.0.background.load(Ordering::Relaxed) {
                let mut now: Vec<Key> = lock(&self.0.building).iter().copied().collect();
                now.sort_unstable();
                if now != last {
                    let text: Vec<String> = now.iter().map(|k| hex(k)).collect();
                    let _ = std::fs::write(self.0.plan.dir.join(BUILDING), text.join("\n"));
                    last = now;
                }
            }
            if !tick(&self.progress()) {
                return *lock(over);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some_entry(n: u8) -> Vec<u8> {
        let mut g = GraphicsState {
            sample_mask: u32::from(n),
            ..Default::default()
        };
        g.rasterizer.DepthBias = i32::from(n);
        entry([n; 16], &[(0, [n; 32]), (1, [n + 1; 32])], Some(&g))
    }

    #[test]
    fn an_entry_reads_back() {
        let e = some_entry(3);
        let p = parse(&e).expect("whole");
        assert_eq!(p.key, [3; 16]);
        assert_eq!(p.shaders, [(0, [3; 32]), (1, [4; 32])]);
        let g = GraphicsState::get(p.graphics.expect("graphics")).expect("state");
        assert_eq!(g.sample_mask, 3);
        assert!(parse(&e[..20]).is_none());
        let compute = entry([1; 16], &[(5, [9; 32])], None);
        assert!(parse(&compute).expect("compute").graphics.is_none());
    }

    #[test]
    fn a_book_goes_out_and_comes_back_in_order() {
        let from = tempfile::tempdir().unwrap();
        let to = tempfile::tempdir().unwrap();
        let mut pipelines = Vec::new();
        for n in [5u8, 1, 9] {
            let e = some_entry(n);
            pipelines.extend((e.len() as u32).to_le_bytes());
            pipelines.extend(e);
        }
        std::fs::write(from.path().join(PIPELINES), &pipelines).unwrap();
        let mut shaders = Vec::new();
        for c in [b"first container".as_slice(), b"second"] {
            shaders.push(0);
            shaders.extend(blake3::hash(c).as_bytes());
            shaders.extend((c.len() as u32).to_le_bytes());
            shaders.extend(c);
        }
        std::fs::write(from.path().join(SHADERS), &shaders).unwrap();
        let file = from.path().join("book.vpb");
        assert_eq!(
            export(from.path(), &file, true).unwrap(),
            Counts {
                pipelines: 3,
                shaders: 2
            }
        );
        // One of them is there already: the others go after it.
        let one = some_entry(9);
        let mut have = (one.len() as u32).to_le_bytes().to_vec();
        have.extend(&one);
        std::fs::write(to.path().join(PIPELINES), &have).unwrap();
        assert_eq!(
            import(to.path(), &file).unwrap(),
            Counts {
                pipelines: 2,
                shaders: 2
            }
        );
        let keys: Vec<u8> = entries(&to.path().join(PIPELINES))
            .0
            .iter()
            .map(|e| e[0])
            .collect();
        assert_eq!(keys, [9, 5, 1]);
        assert_eq!(import(to.path(), &file).unwrap(), Counts::default());
        assert_eq!(containers(&to.path().join(SHADERS)).0.len(), 2);
        // Without containers, the book names what it lacks.
        let bare = from.path().join("bare.vpb");
        export(from.path(), &bare, false).unwrap();
        let third = tempfile::tempdir().unwrap();
        assert_eq!(
            import(third.path(), &bare).unwrap(),
            Counts {
                pipelines: 3,
                shaders: 0
            }
        );
        let missing = missing_shaders(third.path());
        assert_eq!(missing.len(), 6);
        assert_eq!(missing.get(&[1; 32]), Some(&0));
        assert!(import(third.path(), &third.path().join(PIPELINES)).is_err());
    }

    #[test]
    fn found_containers_fill_the_book() {
        let dir = tempfile::tempdir().unwrap();
        let c = b"a container".to_vec();
        let hash = *blake3::hash(&c).as_bytes();
        let e = entry([7; 16], &[(1, hash)], None);
        let mut bytes = (e.len() as u32).to_le_bytes().to_vec();
        bytes.extend(e);
        std::fs::write(dir.path().join(PIPELINES), &bytes).unwrap();
        assert_eq!(missing_shaders(dir.path()).len(), 1);
        assert_eq!(
            add_containers(dir.path(), &[b"other".to_vec(), c.clone(), c]).unwrap(),
            1
        );
        assert!(missing_shaders(dir.path()).is_empty());
        let places = containers(&dir.path().join(SHADERS)).0;
        assert_eq!((places[0].stage, places[0].hash), (1, hash));
    }

    #[test]
    fn suspects_become_bad_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let key = hex(&[0xab; 16]);
        for round in 1..=2 {
            std::fs::write(dir.path().join(BUILDING), &key).unwrap();
            assert_eq!(take_suspects(dir.path()), 1);
            assert_eq!(bad(dir.path()).get(&[0xab; 16]), Some(&round));
        }
        assert_eq!(take_suspects(dir.path()), 0);
        assert_eq!(forget_bad(dir.path()), 1);
        assert!(bad(dir.path()).is_empty());
    }

    /// Every pipeline is taken once, from the list in order or from the
    /// slow ones, and the build ends with all of them counted. (Entries that
    /// do not read end at once, as failed: no host is needed.)
    #[test]
    fn each_pipeline_is_built_once_slow_ones_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let n = 40;
        let plan = Plan {
            dir: dir.path().to_owned(),
            stamp: "test".into(),
            book: n,
            built: 0,
            bad: 0,
            unreadable: 0,
            suspects: 0,
            shaders: Vec::new(),
            places: HashMap::new(),
            entries: (0..n).map(|k| vec![k as u8]).collect(),
            cost: (0..n)
                .map(|k| if k % 5 == 0 { 2000 + k as u32 } else { 0 })
                .collect(),
            keys_built: HashSet::new(),
        };
        let build = plan.start(6, 0);
        assert!(build.wait_while(|_| true));
        let p = build.progress();
        assert_eq!(p.pipelines, (n, n));
        assert_eq!(p.failed, n);
        assert!(build.0.taken.iter().all(|t| t.load(Ordering::Relaxed)));
        assert_eq!(build.0.slow_left_ms.load(Ordering::Relaxed), 0);
        assert_eq!(build.0.quick_left.load(Ordering::Relaxed), 0);
        assert_eq!(build.0.slow.len(), 8);
        assert!(
            build
                .0
                .slow
                .windows(2)
                .all(|w| build.0.plan.cost[w[0]] >= build.0.plan.cost[w[1]])
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(BUILT))
                .unwrap()
                .lines()
                .next(),
            Some("test")
        );
    }

    #[test]
    fn the_built_list_is_kept_per_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let keys: HashSet<Key> = [[1; 16], [2; 16]].into();
        write_built(dir.path(), "a 1", &keys);
        // No library: nothing is built.
        assert!(built(dir.path(), "a 1").is_empty());
        std::fs::write(dir.path().join(LIBRARY), b"x").unwrap();
        assert_eq!(built(dir.path(), "a 1"), keys);
        assert!(built(dir.path(), "a 2").is_empty());
    }

    #[test]
    fn libraries_are_kept_per_adapter_and_driver() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Adapter {
            vendor: 0x10de,
            device: 0x2782,
            subsystem: 1,
            revision: 0xa1,
            name: "card".into(),
            driver: (32 << 48) | (15 << 16) | 6094,
        };
        assert_eq!(a.vendor_driver().as_deref(), Some("560.94"));
        assert_eq!(a.driver_version(), "32.0.15.6094");
        std::fs::write(dir.path().join(LIBRARY), b"old driver").unwrap();
        // A library of no known adapter is taken as this one's.
        assert_eq!(key_library(dir.path(), &a), Keyed::Same);
        assert_eq!(key_library(dir.path(), &a), Keyed::Same);
        let first = a.stamp();
        a.driver += 1;
        assert_eq!(
            key_library(dir.path(), &a),
            Keyed::Changed {
                from: first.clone(),
                restored: false
            }
        );
        assert!(!dir.path().join(LIBRARY).exists());
        std::fs::write(dir.path().join(LIBRARY), b"new driver").unwrap();
        let second = a.stamp();
        a.driver -= 1;
        assert_eq!(
            key_library(dir.path(), &a),
            Keyed::Changed {
                from: second,
                restored: true
            }
        );
        assert_eq!(
            std::fs::read(dir.path().join(LIBRARY)).unwrap(),
            b"old driver"
        );
        // A save the last run left beside the library takes its place first.
        std::fs::write(dir.path().join(LIBRARY_NEXT), b"saved").unwrap();
        assert_eq!(key_library(dir.path(), &a), Keyed::Same);
        assert_eq!(std::fs::read(dir.path().join(LIBRARY)).unwrap(), b"saved");
        for n in 0..4u64 {
            std::fs::write(dir.path().join(format!("host-pipelines.x{n}.bin")), b"").unwrap();
        }
        prune_libraries(dir.path());
        let others = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                let n = e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .into_owned();
                n.starts_with("host-pipelines.") && n.ends_with(".bin") && n != LIBRARY
            })
            .count();
        assert_eq!(others, KEPT_LIBRARIES);
    }
}
