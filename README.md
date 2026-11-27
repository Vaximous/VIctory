# Victory-Blueyes

A fork of [VIctory](https://github.com/Marvisak/VIctory), a compatibility layer that
runs an Xbox Series ("Scarlett") title natively on Windows from files you supply.
Blueyes is about **frame rate on an NVIDIA card while keeping the picture**, with
every change vendor-neutral where possible, and **raw mouse and keyboard**
play.

**This repository contains no game files, code or assets.** You need your own
copy of the game. It is not affiliated with or endorsed by the game's developer
or publisher.

## Test machine

All numbers below come from one PC:

| | |
|---|---|
| GPU | NVIDIA GeForce RTX 4070 Ti, 12 GB (driver 617.42) |
| CPU | AMD Ryzen 9 7900X, 12 cores |
| RAM | 64 GB |
| OS | Windows 11 Pro |
| Profile | Series S (`--console series-s`), the title's own render scale, ray tracing off |

## Frame rate findings

The benchmark is a scripted walk through the open world from a fixed save. The
numbers are the mean frame time over world frames 1000-3000, the same stretch of
route in every run.

| Build | ms / frame | fps | p99 | Notes |
|---|---|---|---|---|
| Starting point (2026-11-25) | 70.0 | 14.3 | | |
| bench7 | 64.8 | 15.4 | | CCD split: our threads on the second die |
| bench8 | 62.5 | 16.0 | 162 ms | hold-waits, clamp waits, ray misses, tiled residency |
| **bench9** | **60.4** | **16.6** | **135 ms** | early depth, copies split across threads, no-freeze pipelines |

Averaged over every frame after the first minute, bench9 runs 64.2 ms. Light
scenes on foot run at 52-58 ms (17-19 fps). Interiors with a lot on screen can
drop to 10 fps.

### Where a frame goes (bench9, moving)

| Part | ms per frame | What it is |
|---|---|---|
| Preparing (CPU) | ~45-51 | The submitting thread translates and joins the title's command lists. Much of this time it spin-waits on 6 helper threads that sit idle about half the time, because job sizes are uneven. |
| ...of which copies | 15-25 | 100-220 MB a frame of title memory copied to the card |
| GPU | ~42 | Drawing |
| Copying back | ~16-18 | ~20-30 MB a frame of GPU results copied back to title memory |

**The limit is serialization, not any single part.** The CPU and GPU parts each
fit under 50 ms, but they still run largely one after the other. Most of the
remaining speed is in overlapping them and in copying less:

Being tested for the next build (not in this one yet):

- The title submits work as about 8 joined parts. Sending each part to the card as
  soon as it is ready reached a 58.7 ms steady state in testing.
- Leaving GPU results in write-combined memory on the card cut copy-back by
  about a third.
- A read probe shows which copied-back memory the title actually reads, so the
  rest can stay on the card.

### What did not help

- `VICTORY_THIN=0`: slower (198 ms) and white, blotchy skin. Keep `THIN=1`.
- Shedding mip levels to save memory: blocky squares wherever shaders read
  textures with `Load`. Tiled residency replaced it.
- Waiting for every slow pipeline each frame froze the picture until you looked
  away. Each pipeline is now waited on once.
- Two copies of the game on one card: when Windows rebalances video memory
  between them, the card stalls for seconds. Run one game at a time.

## Optimizations in Blueyes

Each one has a switch, on by default; `=0` turns it off.

| Switch | What it does |
|---|---|
| `VICTORY_TILED_RESIDENCY` | BC textures are reserved full size and their mip levels mapped on demand, with a min-LOD clamp. Saves about 5 GB of video memory. Steering runs on a thread of its own. |
| `VICTORY_SLOT_FEEDBACK_EVERY=8` | Which texture levels shaders actually sample is recorded every 8th frame instead of every frame (~10 ms GPU). |
| `VICTORY_HOLD_WAITS` | GPU drains between dispatches are held unless something may have written since the last one. |
| `VICTORY_CLAMP_WAITS` | The indirect-argument clamp waits only after work that may have written. It skips ~150 of ~190 waits a frame. |
| `VICTORY_EARLY_DEPTH_SAFE` | Pixel shaders test depth before shading where that is provably safe. Shadow and depth-only passes are left out. |
| `VICTORY_BVH_MISS` | With ray tracing off, ray queries miss at the root instead of walking an empty tree. |
| `VICTORY_HALF_VIEWS`, `VICTORY_TARGET_VARIANTS`, `VICTORY_NAN_SCRUB` | Face-network halves read as integers; pipelines rebuilt for the bound target formats; NaN colours and stores become 0 (no black blobs). |
| `VICTORY_REFRESH_PIECES`, `VICTORY_WIDE_PIECES` | Long copies and big textures are split into pieces that all helper threads share. |
| `VICTORY_WAKE_FEW`, `VICTORY_TOUCHED_LIST` | One helper thread woken per job; changed copies kept in a list instead of rescanned. |
| `VICTORY_PIPELINE_WAIT_ONCE` | A pipeline still compiling is waited on once per run, so the picture never freezes. |
| `VICTORY_KEEP_SHOWN`, `VICTORY_BAKE_REPLAY` | Textures already on screen don't drop back to grey; face bakes that sampled missing inputs are redrawn. |

The grey placeholder is kept on purpose: it shows what hasn't streamed in yet.

## Launch options

You need:

- Windows 10/11 x64 and a CPU with AVX2.
- A Direct3D 12 GPU with shader model 6.6 and mesh shaders.
- The pinned Rust toolchain and the MSVC build tools.
- Your own dump of the title.

```powershell
cargo build --locked --profile diag -p victory --features diagnostics
```

Run it with the settings Blueyes is measured with:

```powershell
$env:VICTORY_GAME_DIR = 'D:\Games\MyDump'
$env:VICTORY_CACHE_DIR = 'D:\victory-cache'      # outside this folder
$env:VICTORY_THIN = '1'; $env:VICTORY_JOIN_PARTS = '8'
$env:VICTORY_RELAY = '2'; $env:VICTORY_RELAY_COPY = '1'; $env:VICTORY_WIDE_BACK = '1'
$env:VICTORY_CARD_STEPS = '2'; $env:VICTORY_CARD_AWAKE = '1'; $env:VICTORY_CARD_REHASH_RUNS = '1'
$env:VICTORY_CARD_BUFFERS_MIN_KB = '64'; $env:VICTORY_JOIN_COPIES = '1'
$env:VICTORY_INDIRECT_WINDOW = '1'; $env:VICTORY_INDIRECT_BATCH = '2'
$env:VICTORY_MAP_THREADS = '1'; $env:VICTORY_SHOW_AFTER = '1'
$env:VICTORY_UPLOAD_AHEAD = '1'; $env:VICTORY_LEVELS_ON_DEMAND = '1'
$env:VICTORY_AMBIENT = '0.8'; $env:VICTORY_AMBIENT_AIR = '1.5'
$env:VICTORY_TEXTURE_NO_WAIT = '0'
$env:VICTORY_SKIP_PIPELINES = '02384a60fda6f2d0,731a39171b568298,5dc3b25986622949,6f225b641c5b0708'
$env:VICTORY_MOUSE = '1'
target\diag\victory.exe run "$env:VICTORY_GAME_DIR\title.exe" --execute --console series-s -- -skipToIIS
```

### Console profile (Scarlett)

| Option | Effect |
|---|---|
| `--console series-s` | The Series S profile, which is what is benchmarked. The title picks its lower Series S resolution and settings, and it is the faster choice. |
| `--console series-x` | The Series X profile: higher resolution and heavier settings. |
| `--execute` | Run the title (rather than only load and check it). |
| `-- -skipToIIS` | Everything after `--` goes to the title itself; this switch skips its intro. |

### Picture and speed trade-offs

| Setting | Effect |
|---|---|
| `VICTORY_TEXTURE_NO_WAIT=0` | Texture refreshes wait for memory the runtime is remapping. Fixes rainbow, pixelated skin, but copies more each frame. `=1` is faster with that artifact. |
| `VICTORY_SKIP_PIPELINES` | A comma list of 16-digit shader hashes whose draws are left out. |
| `...02384a60fda6f2d0` | The decal-heap defrag pass, ~5.6 ms of GPU a frame. |
| `...731a39171b568298` | The heat-haze pass, which smears blocks over hair and outlines. |
| `...5dc3b25986622949,6f225b641c5b0708` | The two open-water light passes. No dark blocks on water and faster on boats, but darker water. Leave these two out to keep it. |
| `VICTORY_RELAY=1`, `VICTORY_CARD_STEPS=0` | Slower but safe against stretched, jumping bodies. |

## Mouse and keyboard

`VICTORY_MOUSE=1` turns on raw mouse input (`WM_INPUT`, unaccelerated counts).
While the window has focus the cursor is hidden and kept inside it. **F8**
releases it; F8 again or a click in the window takes it back, and Alt+Tab
releases it too.

**Camera, 1:1.** With a camera file (`VICTORY_MOUSE_CAMERA`), mouse movement
writes the title's camera yaw and pitch directly: a fixed angle per count, with
no stick curve, acceleration or dead zone. Aiming uses the on-foot camera, so it
is 1:1 too. Vehicle cameras position themselves. The file holds pointer chains
into the title's memory, so it is not part of this repository. Its format is in
`victory.example.toml` under `[input]`. Without a camera file, the mouse drives
the right stick.

**Stick mode settings:**

| Setting | Effect |
|---|---|
| `VICTORY_MOUSE_SENS` | Sensitivity; 2000 counts/s × sensitivity is a full stick. |
| `VICTORY_MOUSE_CURVE` | 1 = linear; above 1 gives finer slow aim. |
| `VICTORY_MOUSE_BOOST` | Smallest stick a moving mouse gives, past the title's dead zone. |
| `VICTORY_MOUSE_INVERT_Y` | Inverts vertical look. |

**Mouse buttons:**

| Button | Pad |
|---|---|
| Left | RT (fire) |
| Right | LT (aim) |
| Middle | RS |
| Back (X1) | LB (weapon wheel) |
| Forward (X2) | RB |
| Wheel | Notches |

All of these can be remapped in `[input]`.

**Keyboard:** with no controller connected, the keyboard drives a stand-in
gamepad, and the mouse merges into it.

| Keys | Pad |
|---|---|
| W A S D | Left stick |
| I J K L | Right stick |
| Z / C | LT / RT |
| Q / E | LB / RB |
| F / H | LS / RS (stick clicks) |
| Enter | A |
| Backspace | B |
| X / Y | X / Y |
| Arrows | D-pad |
| Esc | Menu |
| Tab | View |

A real controller works alongside.

## Known issues

- **Blocky shadows:** shadows on the ground and some window glass come out in
  solid tile-sized blocks. The likely cause is wave barriers dropped on 32-lane
  GPUs in the min/max depth passes that feed shadow-tile classification.
  `VICTORY_EXCHANGE_BARRIERS=2` keeps them all (untested).
- **Grey slots:** some texture slots still flash grey when the title rewrites a 2D
  texture slot with an array view. A fix that keeps showing the old texture is in
  progress.
- **Hitches:** a slow-compiling pipeline or a face bake gives a single hitch of
  about 100 ms.
- **Under memory pressure** (low RAM, or another heavy process on the card),
  texture streaming falls behind and shows blocks.

## Credits

This is a tree of other people's work with more on top. The history is kept, and
commits taken from elsewhere name the commit and its source.

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors: the loader, the runtime services, the Direct3D 12 back end and
  the shader recompiler.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)**,
  **[codec-d/cool_fox](https://github.com/codec-d/cool_fox)** and their
  contributors: the base this fork grew from.
- **[DexNom/VIctory](https://github.com/DexNom/VIctory)**: ray tracing fixes,
  cube gradients, sparse textures.
- **[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD)**:
  the ambient light idea, pipeline workers, and the renderer ports.
- **[rorodev6/victory-nvidia-megafork](https://github.com/rorodev6/victory-nvidia-megafork)**,
  **[trustno1-coder/VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA)**,
  **[pepezin-ai/VIctory-NVIDIA](https://github.com/pepezin-ai/VIctory-NVIDIA)**
  and **[readmet3xt-lab/foxhound](https://github.com/readmet3xt-lab/foxhound)**:
  the face-skinning fix (half views), BVH miss, NaN scrub, target variants,
  pipeline wait-once and submit-thread work ported here.
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**,
  **[myokex/victory-amd](https://github.com/myokex/victory-amd)**, Geckomacabre and
  cxy0072019-sketch.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same
  title behaves under another layer; no code of it is here.
- This fork's changes were made with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
