//! The shader and pipeline cache built ahead of the title: `victory cache`,
//! and the launch stage `run` goes through before the title starts.
//!
//! Consoles ship their shaders compiled; here every pipeline the title has
//! ever asked for (the pipeline book, `victory_gpu::pipeline::book`) is
//! translated and built into the host pipeline library before the title
//! starts, with progress in a window and on the console. Esc (or closing
//! the window, or `VICTORY_CACHE_BUILD_SECONDS`) starts the title at once:
//! what is left is built in the background at the lowest priority, and what
//! the title asks for meanwhile is made by its own workers first, as
//! without a build.
//!
//! Switches: `VICTORY_CACHE_BUILD` = `build` (default), `background`
//! (the title starts at once, the build goes on behind it) or `skip` (`0`);
//! `run --cache` overrides it. `VICTORY_CACHE_BUILD_THREADS` (default:
//! all logical processors but two), `VICTORY_CACHE_BUILD_BACKGROUND_THREADS`
//! (2), `VICTORY_CACHE_BUILD_SECONDS` (no limit).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use victory_hle::launch_window;
use victory_hle::pipeline_book as book;

/// What `run` does with the cache before the title starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Build what is missing, then start the title (Esc: start it now).
    Build,
    /// Start the title at once and build behind it.
    Background,
    /// Build nothing.
    Skip,
}

impl Mode {
    /// `asked`, else `VICTORY_CACHE_BUILD`, else [`Mode::Build`].
    pub fn pick(asked: Option<Mode>) -> Mode {
        asked.unwrap_or_else(|| {
            match std::env::var("VICTORY_CACHE_BUILD")
                .unwrap_or_default()
                .trim()
            {
                "skip" | "0" | "off" => Mode::Skip,
                "background" => Mode::Background,
                _ => Mode::Build,
            }
        })
    }
}

#[derive(Subcommand)]
pub enum Cache {
    /// Build the cache and do not start the title: translate the book's
    /// shaders and build its pipelines into the pipeline library.
    Build {
        /// Worker threads (default: all logical processors but two).
        #[arg(long)]
        jobs: Option<usize>,
        /// Build again what the library holds already.
        #[arg(long)]
        again: bool,
        /// The title's folder, where containers a book names and lacks are
        /// looked for (default: the configured game folder).
        #[arg(long)]
        game: Option<PathBuf>,
    },
    /// Say what the book holds and what a build would do.
    Info,
    /// Write the pipeline book to a file to pass on.
    Export {
        file: PathBuf,
        /// Put the shader containers in too (the title's own code: only for
        /// someone who owns the title). Without them they are found in the
        /// title's files on import.
        #[arg(long)]
        with_shaders: bool,
    },
    /// Add the pipelines of an exported book to this one.
    Import { file: PathBuf },
    /// Build again the pipelines an earlier build ended in.
    ForgetBad,
    /// Look for the shader containers the book names and lacks (an import
    /// without them) in the title's files, without building anything.
    Fill {
        /// The title's folder (default: the configured game folder).
        #[arg(long)]
        game: Option<PathBuf>,
    },
}

/// The pipeline cache's folder (beside the shaders').
fn folder() -> Result<(victory_core::Config, PathBuf)> {
    let cfg = victory_core::Config::load()?;
    let shaders = cfg.cache_subdir("shaders")?;
    victory_hle::modules::d3d12x::use_shader_cache(shaders.clone());
    let dir = shaders.with_file_name("pipelines");
    Ok((cfg, dir))
}

pub fn command(c: Cache) -> Result<()> {
    let (cfg, dir) = folder()?;
    match c {
        Cache::Build { jobs, again, game } => {
            let game = game.or(cfg.game_dir);
            prepare(&dir, game.as_deref())?;
            let Some(plan) = plan(&dir, again) else {
                return Ok(());
            };
            if plan.pipelines() == 0 {
                return Ok(());
            }
            let build = plan.start(jobs.unwrap_or_else(workers), 0);
            if !watch(&build, None) {
                println!("stopped: the pipelines in flight end, then the library is saved");
                build.stop();
                build.wait_while(|_| true);
            }
            report(&build.progress());
            Ok(())
        }
        Cache::Info => {
            if let Some(adapter) = book::adapter() {
                println!("gpu            {adapter}");
            }
            let missing = book::missing_shaders(&dir).len();
            if missing > 0 {
                println!(
                    "pipeline book  lacks {missing} shader containers (they are looked for in the title's files)"
                );
            }
            plan(&dir, false);
            Ok(())
        }
        Cache::Export { file, with_shaders } => {
            let n = book::export(&dir, &file, with_shaders)
                .with_context(|| format!("writing {}", file.display()))?;
            println!(
                "{} pipelines and {} shader containers written to {}",
                n.pipelines,
                n.shaders,
                file.display()
            );
            Ok(())
        }
        Cache::Import { file } => {
            let n =
                book::import(&dir, &file).with_context(|| format!("reading {}", file.display()))?;
            println!(
                "{} new pipelines and {} new shader containers added",
                n.pipelines, n.shaders
            );
            let missing = book::missing_shaders(&dir).len();
            if missing > 0 {
                println!(
                    "{missing} shader containers are not in the book: the next build looks for them in the title's files"
                );
            }
            Ok(())
        }
        Cache::ForgetBad => {
            println!("{} pipelines will be built again", book::forget_bad(&dir));
            Ok(())
        }
        Cache::Fill { game } => {
            let game = game
                .or(cfg.game_dir)
                .context("no game folder: pass --game")?;
            let _ = std::fs::remove_file(dir.join(UNFOUND));
            fill(&dir, &game)
        }
    }
}

/// The launch stage of `run`, before the title starts: the library keyed by
/// adapter and driver, then (unless `mode` is skip) the book's pipelines
/// built, in the foreground until done or cancelled.
pub fn launch(mode: Mode, title_dir: &Path) -> Result<()> {
    let (_, dir) = folder()?;
    prepare(&dir, Some(title_dir))?;
    if mode == Mode::Skip {
        println!("cache          not built (skip)");
        return Ok(());
    }
    let Some(plan) = plan(&dir, false) else {
        return Ok(());
    };
    if plan.pipelines() == 0 {
        return Ok(());
    }
    let background = env_number("VICTORY_CACHE_BUILD_BACKGROUND_THREADS").unwrap_or(2);
    let build = plan.start(workers(), background);
    if mode == Mode::Background {
        build.to_background();
        println!("cache          built behind the title ({background} threads)");
        return Ok(());
    }
    let limit = env_number("VICTORY_CACHE_BUILD_SECONDS").map(|s| Duration::from_secs(s as u64));
    let began = Instant::now();
    if watch(&build, limit) {
        report(&build.progress());
    } else {
        build.to_background();
        let p = build.progress();
        println!(
            "cache          the title starts now; {} pipelines are left to build behind it ({background} threads)",
            p.pipelines.1 - p.pipelines.0
        );
    }
    println!(
        "cache          launch stage took {:.1} s",
        began.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Names the adapter, keys the library by it and fills the book's missing
/// containers from the title's files.
fn prepare(dir: &Path, game: Option<&Path>) -> Result<()> {
    let Some(adapter) = book::adapter() else {
        println!("cache          no host GPU: nothing built");
        return Ok(());
    };
    println!("gpu            {adapter}");
    match book::key_library(dir, &adapter) {
        book::Keyed::Same => {}
        book::Keyed::Changed {
            from,
            restored: true,
        } => {
            println!(
                "cache          GPU or driver changed (was {from}): the library kept for this one is used"
            )
        }
        book::Keyed::Changed {
            from,
            restored: false,
        } => {
            println!(
                "cache          GPU or driver changed (was {from}): the library is rebuilt from the pipeline book"
            )
        }
    }
    if let Some(game) = game {
        fill(dir, game)?;
    }
    Ok(())
}

/// The containers the last look through the title's files did not find
/// (BLAKE3s, one per line): a book whose missing containers are all there
/// is not looked for again at every start.
const UNFOUND: &str = "book-unfound.txt";

/// Looks for the containers the book lacks in the title's files, unless
/// the last look found none of the same.
fn fill(dir: &Path, game: &Path) -> Result<()> {
    let missing = book::missing_shaders(dir);
    if missing.is_empty() {
        return Ok(());
    }
    let unfound: std::collections::HashSet<String> = std::fs::read_to_string(dir.join(UNFOUND))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let hex = |h: &[u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if missing.keys().all(|h| unfound.contains(&hex(h))) {
        println!(
            "cache          {} shader containers the book names are not in the title's files (`victory cache fill` looks again)",
            missing.len()
        );
        return Ok(());
    }
    let began = Instant::now();
    println!(
        "cache          looking for {} shader containers in {}",
        missing.len(),
        game.display()
    );
    let found = find_containers(game, &missing);
    let added = book::add_containers(dir, &found)?;
    let left: Vec<String> = book::missing_shaders(dir).keys().map(hex).collect();
    let _ = std::fs::write(dir.join(UNFOUND), left.join("\n"));
    println!(
        "cache          {added} found, {} not ({:.0} s)",
        left.len(),
        began.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The containers with the given hashes in the files under `root`.
fn find_containers(root: &Path, wanted: &HashMap<[u8; 32], u8>) -> Vec<Vec<u8>> {
    let (mut found, mut seen) = (Vec::new(), std::collections::HashSet::new());
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if seen.len() >= wanted.len() {
            break;
        }
        if !entry.file_type().is_file()
            || entry.metadata().map_or(0, |m| m.len()) > crate::shader::MAX_FILE
        {
            continue;
        }
        let Ok(data) = std::fs::read(entry.path()) else {
            continue;
        };
        for source in crate::shader::sources(&data) {
            let Some(bytes) = source.bytes() else {
                continue;
            };
            let bytes = whole_container(&bytes);
            let hash = *blake3::hash(bytes).as_bytes();
            if wanted.contains_key(&hash) && seen.insert(hash) {
                found.push(bytes.to_vec());
            }
        }
    }
    found
}

/// A container found as is runs on to the file's end: cut at the size its
/// header gives.
fn whole_container(bytes: &[u8]) -> &[u8] {
    match bytes.get(24..28) {
        Some(size) if bytes.starts_with(b"DXBC") => {
            let size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as usize;
            bytes.get(..size).unwrap_or(bytes)
        }
        _ => bytes,
    }
}

fn plan(dir: &Path, again: bool) -> Option<book::Plan> {
    let plan = book::plan(dir, again)?;
    let mut text = format!(
        "pipeline book  {} pipelines, {} built for this GPU, driver and translator",
        plan.book, plan.built
    );
    for (n, what) in [
        (plan.bad, "left out (a build ended in them twice)"),
        (plan.unreadable, "lack a shader container"),
        (plan.suspects, "were in flight when the last build ended"),
    ] {
        if n > 0 {
            text.push_str(&format!(", {n} {what}"));
        }
    }
    println!("{text}");
    if plan.pipelines() > 0 {
        println!(
            "cache          {} pipelines to build, {} shaders to translate or read",
            plan.pipelines(),
            plan.shaders()
        );
    }
    Some(plan)
}

fn env_number(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

/// Foreground workers: all logical processors but two.
fn workers() -> usize {
    env_number("VICTORY_CACHE_BUILD_THREADS")
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .saturating_sub(2)
        })
        .clamp(1, 64)
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The progress as lines.
fn lines(p: &book::Progress) -> Vec<String> {
    let percent = |(done, all): (usize, usize)| {
        if all == 0 {
            100.0
        } else {
            100.0 * done as f64 / all as f64
        }
    };
    let mut out = vec![
        format!(
            "Shaders     {} / {} ({:.0}%)",
            p.shaders.0,
            p.shaders.1,
            percent(p.shaders)
        ),
        format!(
            "Pipelines   {} / {} ({:.0}%){}",
            p.pipelines.0,
            p.pipelines.1,
            percent(p.pipelines),
            if p.failed > 0 {
                format!(", {} failed", p.failed)
            } else {
                String::new()
            }
        ),
    ];
    out.push(match p.left {
        Some(left) => format!("Elapsed {}, about {} left", clock(p.elapsed), clock(left)),
        None => format!("Elapsed {}", clock(p.elapsed)),
    });
    out
}

/// Shows the build's progress (window and console) until it is over, Esc
/// is pressed or `limit` passes; whether it is over.
fn watch(build: &book::Build, limit: Option<Duration>) -> bool {
    let window = if std::env::var_os("VICTORY_CACHE_WINDOW").is_some_and(|v| v == "0") {
        None
    } else {
        launch_window::open("VIctory: building the shader cache")
    };
    let began = Instant::now();
    let mut said = Instant::now();
    let over = build.wait_while(|p| {
        if let Some(w) = &window {
            let mut shown =
                vec!["Building shaders and pipelines before the game starts".to_owned()];
            shown.extend(lines(p));
            shown.push("Esc: start the game now (the rest is built in the background)".to_owned());
            w.show(shown, p.fraction());
        }
        if said.elapsed() >= Duration::from_secs(2) {
            said = Instant::now();
            println!("cache          {}", lines(p).join(", "));
        }
        let cancelled = window.as_ref().is_some_and(|w| w.cancelled());
        let timed_out = limit.is_some_and(|l| began.elapsed() >= l);
        if cancelled {
            println!("cache          cancelled (Esc)");
        } else if timed_out {
            println!("cache          VICTORY_CACHE_BUILD_SECONDS reached");
        }
        !cancelled && !timed_out
    });
    drop(window);
    over
}

fn report(p: &book::Progress) {
    println!(
        "cache          built: {} shaders, {} pipelines ({} failed) in {:.1} s",
        p.shaders.1,
        p.pipelines.0 - p.failed.min(p.pipelines.0),
        p.failed,
        p.elapsed.as_secs_f64()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_container_found_as_is_is_cut_at_its_size() {
        let mut bytes = b"DXBC".to_vec();
        bytes.extend([0; 20]);
        bytes.extend(40u32.to_le_bytes());
        bytes.resize(100, 7);
        assert_eq!(whole_container(&bytes).len(), 40);
        assert_eq!(whole_container(b"SGD5 record").len(), 11);
    }

    #[test]
    fn the_mode_comes_from_the_flag_first() {
        assert_eq!(Mode::pick(Some(Mode::Skip)), Mode::Skip);
        assert_eq!(Mode::pick(Some(Mode::Background)), Mode::Background);
    }
}
