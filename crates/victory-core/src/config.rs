//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! See `victory.example.toml` at the repository root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of the title.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
    /// Picture settings (`[graphics]`).
    pub graphics: Graphics,
    /// Mouse and keyboard (`[input]`).
    pub input: Input,
}

/// `[graphics]`: what the title's picture is made with. Each value is the
/// environment's (`VICTORY_UPSCALER`, `VICTORY_RAY_TRACING`), else the
/// file's, else the title's own choice.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graphics {
    /// `"fsr4"` (the title's), `"none"` (bilinear, no anti-aliasing) or
    /// `"fxaa"`.
    pub upscaler: Option<String>,
    /// `false`: the title's ray-tracing passes do not run.
    pub ray_tracing: Option<bool>,
}

impl Graphics {
    fn resolve(file: Graphics, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Graphics> {
        let text = |k: &str| env(k).and_then(|v| v.into_string().ok()).filter(|v| !v.trim().is_empty());
        let ray_tracing = match text("VICTORY_RAY_TRACING") {
            Some(v) => Some(match v.trim().to_ascii_lowercase().as_str() {
                "1" | "on" | "true" | "yes" => true,
                "0" | "off" | "false" | "no" => false,
                other => anyhow::bail!("VICTORY_RAY_TRACING={other}: expected on or off"),
            }),
            None => file.ray_tracing,
        };
        Ok(Graphics { upscaler: text("VICTORY_UPSCALER").or(file.upscaler), ray_tracing })
    }
}

/// `[input]`: the mouse as the title's camera and pad buttons. Each value
/// is the environment's (`VICTORY_MOUSE`, `VICTORY_MOUSE_SENS`,
/// `VICTORY_MOUSE_CURVE`, `VICTORY_MOUSE_BOOST`, `VICTORY_MOUSE_INVERT_Y`),
/// else the file's, else the default (victory.example.toml lists them).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// Mouse mode (on by default; `false` or `VICTORY_MOUSE=0`: off).
    pub mouse: Option<bool>,
    pub sensitivity: Option<f32>,
    pub curve: Option<f32>,
    pub deadzone_boost: Option<f32>,
    pub cap: Option<f32>,
    pub invert_y: Option<bool>,
    pub hold_ms: Option<u32>,
    /// Frees the cursor and takes it back (`"F8"`).
    pub release_key: Option<String>,
    /// Pad buttons (`"rt"`, `"lb"`, `"right"`, ...) for each mouse button.
    pub left_button: Option<String>,
    pub right_button: Option<String>,
    pub middle_button: Option<String>,
    pub back_button: Option<String>,
    pub forward_button: Option<String>,
    pub wheel_up: Option<String>,
    pub wheel_down: Option<String>,
    /// The user's camera file (`CameraFile`), outside the repository.
    pub camera: Option<PathBuf>,
}

/// The user's camera file: where the title keeps each camera's yaw and
/// pitch, found by the user in their own copy (nothing of the title is in
/// this repository).
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraFile {
    #[serde(default)]
    pub camera: Vec<CameraHook>,
}

/// One camera: pointer chains from the title image's base (`[rva, offset,
/// ...]`: each offset after the first is added to the pointer read where
/// the chain has got to) and the angle a mouse count turns.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraHook {
    pub name: String,
    pub yaw: Vec<u64>,
    pub pitch: Vec<u64>,
    pub yaw_per_count: f32,
    pub pitch_per_count: f32,
    #[serde(default = "pitch_limit_low")]
    pub pitch_min: f32,
    #[serde(default = "pitch_limit_high")]
    pub pitch_max: f32,
    /// The yaw's period: 2π for radians (default), 360 for degrees, 0 none.
    #[serde(default = "tau")]
    pub wrap: f32,
    /// A chain to the camera object and the first qword it must hold there
    /// (its type, relative to the image): only a matching camera is turned,
    /// so another camera reusing the memory is left alone.
    #[serde(default)]
    pub object: Vec<u64>,
    #[serde(default)]
    pub kind: Option<u64>,
    /// More chains to copies the title carries the yaw / pitch through from
    /// frame to frame; each moves by the same amount, so the turn sticks
    /// whichever copy the title reads next.
    #[serde(default)]
    pub yaw_copies: Vec<Vec<u64>>,
    #[serde(default)]
    pub pitch_copies: Vec<Vec<u64>>,
    /// A chain to the camera's (or the player's) position, three floats;
    /// logged once a route second in diagnostics builds. None: not logged.
    #[serde(default)]
    pub position: Vec<u64>,
}

fn pitch_limit_low() -> f32 {
    -1.4
}
fn pitch_limit_high() -> f32 {
    1.4
}
fn tau() -> f32 {
    std::f32::consts::TAU
}

impl CameraFile {
    /// Reads and checks a camera file.
    pub fn load(path: &Path) -> Result<CameraFile> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let file: CameraFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        for c in &file.camera {
            anyhow::ensure!(
                !c.yaw.is_empty() && !c.pitch.is_empty(),
                "{}: camera {:?} needs a yaw and a pitch chain",
                path.display(),
                c.name
            );
            anyhow::ensure!(
                c.kind.is_none() || !c.object.is_empty(),
                "{}: camera {:?} names a kind but no object chain",
                path.display(),
                c.name
            );
            let copies = c.yaw_copies.iter().chain(&c.pitch_copies);
            anyhow::ensure!(
                copies.into_iter().all(|chain| !chain.is_empty()),
                "{}: camera {:?} has an empty copy chain",
                path.display(),
                c.name
            );
        }
        Ok(file)
    }
}

impl Input {
    fn resolve(file: Input, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Input> {
        let text = |k: &str| env(k).and_then(|v| v.into_string().ok()).filter(|v| !v.trim().is_empty());
        let flag = |k: &str, file: Option<bool>| -> Result<Option<bool>> {
            Ok(match text(k) {
                Some(v) => Some(match v.trim().to_ascii_lowercase().as_str() {
                    "1" | "on" | "true" | "yes" => true,
                    "0" | "off" | "false" | "no" => false,
                    other => anyhow::bail!("{k}={other}: expected on or off"),
                }),
                None => file,
            })
        };
        let number = |k: &str, file: Option<f32>| -> Result<Option<f32>> {
            Ok(match text(k) {
                Some(v) => Some(
                    v.trim()
                        .parse::<f32>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .with_context(|| format!("{k}={v}: expected a number"))?,
                ),
                None => file,
            })
        };
        Ok(Input {
            mouse: flag("VICTORY_MOUSE", file.mouse)?,
            sensitivity: number("VICTORY_MOUSE_SENS", file.sensitivity)?,
            curve: number("VICTORY_MOUSE_CURVE", file.curve)?,
            deadzone_boost: number("VICTORY_MOUSE_BOOST", file.deadzone_boost)?,
            invert_y: flag("VICTORY_MOUSE_INVERT_Y", file.invert_y)?,
            camera: env("VICTORY_MOUSE_CAMERA").filter(|v| !v.is_empty()).map(PathBuf::from).or(file.camera.clone()),
            ..file
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
    #[serde(default)]
    graphics: Graphics,
    #[serde(default)]
    input: Input,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let explicit = explicit_config_path(|k| std::env::var_os(k));
        let path = explicit
            .clone()
            .unwrap_or_else(|| PathBuf::from("victory.toml"));
        let file = if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            Some((path, text))
        } else if let Some(p) = explicit {
            anyhow::bail!(
                "VICTORY_CONFIG points at {}, which does not exist",
                p.display()
            );
        } else {
            None
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed: FileConfig =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                (Some(path), parsed)
            }
            None => (None, FileConfig::default()),
        };
        let graphics = Graphics::resolve(parsed.graphics, |k| std::env::var_os(k))?;
        let input = Input::resolve(parsed.input, |k| std::env::var_os(k))?;
        let config = Self::resolve(parsed.paths, source, |k| std::env::var_os(k))?;
        Ok(Config { graphics, input, ..config })
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let env_path = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let test_titles = match env("VICTORY_TEST_TITLES") {
            Some(v) if !v.is_empty() => std::env::split_paths(&v)
                .filter(|p| !p.as_os_str().is_empty())
                .collect(),
            _ => file.test_titles,
        };
        let cache_dir = env_path("VICTORY_CACHE_DIR")
            .or(file.cache_dir)
            .map_or_else(|| default_dir("cache"), Ok)?;
        let data_dir = env_path("VICTORY_DATA_DIR")
            .or(file.data_dir)
            .map_or_else(|| default_dir("data"), Ok)?;
        Ok(Config {
            game_dir: env_path("VICTORY_GAME_DIR").or(file.game_dir),
            cache_dir,
            data_dir,
            test_titles,
            source,
            graphics: Graphics::default(),
            input: Input::default(),
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set VICTORY_GAME_DIR or [paths].game_dir in \
             victory.toml (see victory.example.toml)",
        )
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }
}

/// The config file explicitly named by the environment, if any. An empty
/// `VICTORY_CONFIG` means unset, like the other path variables.
fn explicit_config_path(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    env("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// `%LOCALAPPDATA%/victory/<name>` (elsewhere the XDG or home cache).
fn default_dir(name: &str) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CACHE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn graphics_from_file_and_environment() {
        let file: FileConfig =
            toml::from_str("[graphics]
upscaler = 'fxaa'
ray_tracing = false
").unwrap();
        let none = |_: &str| None;
        let g = Graphics::resolve(file.graphics.clone(), none).unwrap();
        assert_eq!(g.upscaler.as_deref(), Some("fxaa"));
        assert_eq!(g.ray_tracing, Some(false));
        let env = |k: &str| match k {
            "VICTORY_UPSCALER" => Some(OsString::from("none")),
            "VICTORY_RAY_TRACING" => Some(OsString::from("on")),
            _ => None,
        };
        let g = Graphics::resolve(file.graphics, env).unwrap();
        assert_eq!(g.upscaler.as_deref(), Some("none"));
        assert_eq!(g.ray_tracing, Some(true));
        let bad = |k: &str| (k == "VICTORY_RAY_TRACING").then(|| OsString::from("maybe"));
        assert!(Graphics::resolve(Graphics::default(), bad).is_err());
        assert_eq!(Graphics::resolve(Graphics::default(), none).unwrap(), Graphics::default());
    }

    #[test]
    fn input_from_file_and_environment() {
        let file: FileConfig = toml::from_str(
            "[input]
mouse = true
sensitivity = 1.5
wheel_up = 'rb'
release_key = 'F9'
",
        )
        .unwrap();
        let none = |_: &str| None;
        let i = Input::resolve(file.input.clone(), none).unwrap();
        assert_eq!((i.mouse, i.sensitivity), (Some(true), Some(1.5)));
        assert_eq!(i.wheel_up.as_deref(), Some("rb"));
        assert_eq!(i.release_key.as_deref(), Some("F9"));
        let env = |k: &str| match k {
            "VICTORY_MOUSE" => Some(OsString::from("0")),
            "VICTORY_MOUSE_SENS" => Some(OsString::from("2.5")),
            "VICTORY_MOUSE_INVERT_Y" => Some(OsString::from("on")),
            _ => None,
        };
        let i = Input::resolve(file.input, env).unwrap();
        assert_eq!((i.mouse, i.sensitivity, i.invert_y), (Some(false), Some(2.5), Some(true)));
        assert_eq!(i.wheel_up.as_deref(), Some("rb"));
        let bad = |k: &str| (k == "VICTORY_MOUSE_SENS").then(|| OsString::from("fast"));
        assert!(Input::resolve(Input::default(), bad).is_err());
        assert!(toml::from_str::<FileConfig>("[input]
mice = true
").is_err());
    }

    #[test]
    fn camera_files_parse() {
        let file: CameraFile = toml::from_str(
            "[[camera]]
name = 'a'
yaw = [0x10, 0x8]
pitch = [0x10, 0xc]
yaw_per_count = 0.001
pitch_per_count = -0.001
",
        )
        .unwrap();
        let c = &file.camera[0];
        assert_eq!((c.yaw.as_slice(), c.pitch.as_slice()), (&[0x10, 0x8][..], &[0x10, 0xc][..]));
        assert_eq!((c.pitch_min, c.pitch_max, c.wrap), (-1.4, 1.4, std::f32::consts::TAU));
        assert!(c.object.is_empty() && c.kind.is_none() && c.yaw_copies.is_empty());
        let file: CameraFile = toml::from_str(
            "[[camera]]
name = 'b'
object = [0x10, 0x100]
kind = 0x2000
yaw = [0x10, 0x148]
pitch = [0x10, 0x14c]
yaw_copies = [[0x10, 0x200], [0x10, 0x300]]
pitch_copies = [[0x10, 0x204]]
yaw_per_count = 0.001
pitch_per_count = -0.001
",
        )
        .unwrap();
        let c = &file.camera[0];
        assert_eq!((c.object.as_slice(), c.kind), (&[0x10, 0x100][..], Some(0x2000)));
        assert_eq!((c.yaw_copies.len(), c.pitch_copies[0].as_slice()), (2, &[0x10, 0x204][..]));
        assert!(toml::from_str::<CameraFile>("[[camera]]
name = 'a'
").is_err());
        let env = |k: &str| (k == "VICTORY_MOUSE_CAMERA").then(|| OsString::from("cam.toml"));
        assert_eq!(Input::resolve(Input::default(), env).unwrap().camera, Some(PathBuf::from("cam.toml")));
    }

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(|| OsString::from("from-env"));
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_GAME_DIR").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths::default();
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(|| value.clone());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn empty_config_env_is_ignored() {
        let empty = |k: &str| (k == "VICTORY_CONFIG").then(OsString::new);
        assert_eq!(explicit_config_path(empty), None);
        let set = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("custom.toml"));
        assert_eq!(
            explicit_config_path(set),
            Some(PathBuf::from("custom.toml"))
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed: FileConfig = toml::from_str(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }
}
