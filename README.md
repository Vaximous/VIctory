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
route in every run (about 5,000-6,500 draws a frame).

| Build | ms / frame | fps | p99 | Notes |
|---|---|---|---|---|
| Starting point (2026-11-25) | 70.0 | 14.3 | | |
| bench7 | 64.8 | 15.4 | | CCD split: our threads on the second die |
| bench8 | 62.5 | 16.0 | 162 ms | hold-waits, clamp waits, ray misses, tiled residency |
| bench9 | 60.4 | 16.6 | 135 ms | early depth, copies split across threads, no-freeze pipelines |
| bench10 | 58.2 | 17.2 | | parts of a joined submission sent as soon as each is ready; only changed 4 KiB parts go up |
| bench11 | 50.1 | 20.0 | 77 ms | textures copied out under a short hold; never-read buffers stay on the card |
| bench12 | 48.4 | 20.7 | 110 ms | sharded mapping lock, dead passes skipped, list resets on their own thread |
| bench13 | 48.8 | 20.5 | 68 ms | landed card copies compared per 4 KiB part (bus 89 -> 8 MB a frame) |
| bench15 | 45.6 | 21.9 | 74 ms | a write fault opens only the 4 KiB part written |
| bench16 | 44.7 | 22.4 | 68 ms | blocks the title rewrites whole are opened whole |
| bench17 | ~44 | ~22.7 | | changed-part compare done in place (AVX2), alignment-correct |
| bench18 | 42.5 | 23.5 | 66 ms | scalar buffer loads read four dwords a call, fitted to the view's end |
| bench19 | 41.4 | 24.1 | 70 ms | the graphics heap is write-watched by the kernel instead of guarded; landing merge fixed |
| **bench20** | **42.3** | **23.6** | **74 ms** | kernel writes into title memory noted (banding fixed); decal bake asynchronous; small copies cut from shared buffers |

bench20 drew about 6,900 draws a frame on the route against bench19's 6,000, so
its raw time is slightly higher; per 1000 draws both cost 7.27 ms a frame, with
bench20's preparing 0.4 ms lower (6.11 against 6.53) and its worst frame 113 ms
(bench19: 124 ms). It is the base because it fixes the last cause of banding.

### Where a frame goes (bench19, moving)

| Part | ms per frame | What it is |
|---|---|---|
| Preparing (CPU) | ~37 | The submitting thread translates the title's command lists and works out which title memory changed |
| ...of which list steps | ~22 | Refreshes of changed title memory (~15) and the rest of translating the lists |
| ...of which asking the write watch | ~4.6 | Which 4 KiB pages of the watched heap were written |
| GPU | ~33 | Drawing |
| Copying back | ~6 | GPU results copied back to title memory (was 17 ms at bench9) |

**The CPU sets the pace, and most of it is memory tracking.** To know what the
title changed, its memory used to be write-protected in 64 KiB blocks: every
write to a protected block faulted into our handler, about 750 faults a frame,
roughly 17-20 ms a frame of the title's own threads spent in our code. In
bench19 the 185 MB graphics heap, which took about 690 of those faults, is
ordinary write-watched memory at the title's own addresses instead, and the
kernel says which pages were written. Faults fell to about 47 a frame and bytes
sent to the card from 118 to 52 MB a frame.

What was learned along the way:

- **Copy less, not faster.** The biggest wins (bench10-13) came from sending only
  the 4 KiB parts that actually changed, and from keeping buffers the title never
  reads on the card. Bus traffic fell from 100-220 MB to under 10 MB a frame.
- **Fault granularity matters both ways.** Opening only the 4 KiB part written
  (bench15) cut refreshed bytes; reopening blocks the title rewrites whole as a
  whole (bench16) cut the extra faults that caused.
- **Remembering blocks longer loses.** Keeping blocks open for 2 s, or raising the
  part cap from 4 to 8, refreshed more bytes and was slower each of the three
  times it was tried.
- **Locks:** sharding the mapping lock into 256 pieces, and making the kernel call
  for a part outside the shared map's lock, removed those waits. Most of the wait
  left is title threads writing different parts of the same block at once and
  taking turns on that block's claim.
- **Ask the kernel once, not per submission.** The first write-watch version asked
  about the whole heap at each of ~14 submissions a frame and spent 28-35 ms doing
  it, mostly in VirtualQuery splitting ranges across the heap's mapped runs. One
  walk a frame (0.2 ms), runs that meet treated as one span, cached region splits
  and asking only the pieces each submission covers brought it to 4.6 ms.
- **A stale card side must never win.** Landing card results back used to store
  every byte where the card's copy differed from ours, even where the CPU had
  written since. That reverted the title's own pointers (a crash at world load)
  and its lighting curves (posterized banding), seemingly at random. The merge now
  stores a byte only where the title still holds what we last knew.
- **The kernel's write watch only sees user-mode writes.** Bytes the kernel puts
  into watched memory itself (file reads, DirectStorage, page writes) leave no
  trace, so our copies of them stayed stale and the picture banded now and then.
  Noting those ranges ourselves when each read completes cleared it: a sampled
  check of 17,000 copies found none changed without a write being noted.
- **Part of the frame is the title's own CPU time.** Stretches where 8-15 ms pass
  between the title's first list call and its first submission are not waits on
  us: its submitting thread waits on no fence, event or label at all.
- **The GPU is the next ceiling.** With our CPU cost at zero, ~31-33 ms of GPU
  work still caps this card near 30 fps.
- **Shared memory: reading title memory in place loses here.** The card can open
  system memory directly (OpenExistingHeapFromAddress, custom L0 heaps), but every
  shader read then crosses PCIe. Our test was 73.5 against 60.5 ms, and another
  fork measured about 5 ms less CPU for 15 ms more GPU on NVIDIA. With under
  60 MB a frame copied, the copies are not the cost; the tracking is.

### What did not help

- `VICTORY_THIN=0`: slower (198 ms) and white, blotchy skin. Keep `THIN=1`.
- Shedding mip levels to save memory: blocky squares wherever shaders read
  textures with `Load`. Tiled residency replaced it.
- Waiting for every slow pipeline each frame froze the picture until you looked
  away. Each pipeline is now waited on once.
- Two copies of the game on one card: when Windows rebalances video memory
  between them, the card stalls for seconds. Run one game at a time.
- Copying resting blocks every time they are asked about (471 MB a frame).
- Telling labels earlier: about 40 labels a frame wait on a card-copy compare,
  but the title never waits on labels, so moving them changed nothing.
- Wide scalar loads cost about 0.6 ms of GPU on NVIDIA in later tests; their
  first version read past the end of views and drew glowing halos.
- A compare-in-place version that counted 32-byte records from an unaligned start
  resent the previous 4 KiB piece and overwrote bytes the card had written:
  posterized banding in dim scenes. Fixed in bench17.
- Faster fault handling (shorter lock holds, per-part claims, leaving hot blocks
  unguarded): each made a fault cheaper or rarer but none beat bench18; the fix
  was to stop faulting.
- Per-part claims and some refresh orderings made the title crash, which turned
  out to be the landing-merge bug above, exposed by different timing.

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
| `VICTORY_SEND_PARTS_EARLY=1` (set it; off in code) | Each part of a joined submission goes to the card when ready; card copies are compared once, on the last part. |
| `VICTORY_SEND_PARTS`, `VICTORY_LANDED_PARTS` | Only the changed 4 KiB parts of written pages, and of landed card copies, are sent up. |
| `VICTORY_COMPARE_IN_PLACE` | The changed-part compare runs in place with AVX2 instead of through a copy. |
| `VICTORY_GUARD_PARTS`, `VICTORY_PARTS_WIDE` | A write fault opens only the 4 KiB part written; a block lately opened whole is opened whole again. |
| `VICTORY_TEXTURE_SHORT_HOLD`, `VICTORY_RANGED_MAPPING` | Changed textures are copied out under a short lock and detiled after; the mapping lock is split into 256 shards by address range. |
| `VICTORY_LEAVE_UNREAD`, `VICTORY_UC_NO_BACK`, `VICTORY_WC_NO_BACK` | Buffers the title never reads, and large uncached or write-combined copies, stay on the card. |
| `VICTORY_REAPER` | Host command-list resets and retired-object drops run on a thread of their own. |
| `VICTORY_SKIP_DEAD_PASSES` | Three passes proven to have no visible output are left out; `VICTORY_DEAD_PASSES=<hashes>` overrides the list. |
| `VICTORY_LIGHT_GAPS`, `VICTORY_EACH_ITEMS` | Empty refreshes of GPU-written ranges are skipped; per-item work is batched. |
| `VICTORY_WIDE_LOADS` | Scalar buffer loads read four dwords a host call, checking the view's size and falling back near its end. |
| `VICTORY_WATCHED_PAGES` | Long runs of fresh physical pages mapped below 4 GiB become write-watched private memory at the title's addresses; remaps copy them back first. |
| `VICTORY_WATCH_NOT_GUARD`, `VICTORY_WATCH_ONCE` | Watched memory is never guarded; it is walked once a frame and later submissions ask only what they cover. |
| `VICTORY_REGION_CACHE`, `VICTORY_WATCH_PIECES` | Region splits of private memory are remembered; watched copies are asked about piece by piece. |
| `VICTORY_COPY_CHECK` | One refresh in 16 checks a copied part against title memory later and logs any change no write explains. |
| `VICTORY_SUBALLOC_COPIES` | CPU sides of small title-buffer copies are cut from shared buffers instead of made one by one. |
| `VICTORY_BAKE_SLICE_MS` | The decal bake's texture pass runs in time slices with a catch-up of the slots written lately. |

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
  about 100 ms. The first sight of a new area creates a few thousand small card
  copies one by one and can stall for one to three seconds;
  `VICTORY_SUBALLOC_CARD=1` cuts that to about one second but slows steady play
  for now. The decal-bake texture pass now runs in time slices instead of
  stalling a frame.
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
- **[adanaport](https://github.com/adanaport/adanaport)**: the idea of turning the
  graphics heap into write-watched memory, and measurements of in-place heaps.
- **[NitricAcidDev/VIctory-Omega-Fork](https://github.com/NitricAcidDev/VIctory-Omega-Fork)**
  (per-item batching), and **[Xeltax/VIcky](https://github.com/Xeltax/VIcky)**
  with Grein and Grimkaaah (the list reaper thread).
- **[FQY55kai/ZJOU-VICTORY](https://github.com/FQY55kai/ZJOU-VICTORY)**,
  **[myokex/victory-amd](https://github.com/myokex/victory-amd)**, Geckomacabre and
  cxy0072019-sketch.
- **[XCL](https://github.com/ivall/xcl)** (GPL-2.0-only) was read for how the same
  title behaves under another layer; no code of it is here.
- This fork's changes were made with [Claude Code](https://claude.com/claude-code).

## License

GPL-3.0-or-later, as VIctory. See [LICENSE](LICENSE); the one file that is not
covered is named in [docs/asset-policy.md](docs/asset-policy.md).
