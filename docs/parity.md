# Measuring parity with Studio

ror's rules are measured, not guessed: a dedicated capture Studio renders the same fixtures, and ror
is compared with it pixel by pixel, or with Studio's own counters for `ror cost`. The code is in
`crates/ror/src/studio`: `mod.rs` (the capture Studio), `place.rs` (the capture place), `capture.rs`
and `capture_mac.rs` (capturing its viewport on Windows and macOS), `compare.rs`, `export.rs`, `mcp.rs`
(the bridge client) and `calibrate.rs` with `calibrate/*` (the experiments, Windows only).

## What you need

- Roblox Studio on Windows or macOS (captures read the viewport's pixels from its window; on macOS see
  [below](#macos)). Calibration (`ror studio calibrate`) needs Windows.
- A Studio MCP bridge: an MCP server reached over TCP (JSON-RPC, one message a line) that lists the
  open Studios and runs Luau in one of them by id, through the tools `list_roblox_studios` and
  `execute_luau` (`studio_id`, `datamodel_type`, `code`). Set `ROR_STUDIO_MCP` to its address
  (`host:port`, or a port on 127.0.0.1). It only ever needs to listen on localhost (see
  [SECURITY.md](../SECURITY.md)). On macOS, rur's `tools/studio-mcp-bridge.mjs` is one: it serves
  Studio's own StudioMCP on a local port (`node ../roblox-ui-renderer/tools/studio-mcp-bridge.mjs
  --port 44755`, then `ROR_STUDIO_MCP=44755`).
- Nothing else for the offline half: stored captures replay without a Studio (below).

## The capture Studio

`ror studio open` writes a local place, `ror-capture.rbxl`, and opens it in Studio; captures only ever
run in the Studio that has this place open, and every other Studio gets read-only queries
(`SerializationService` exports, property reads). The place is never a game place: models, lighting
and materials are loaded into it per capture. One process at a time drives a capture Studio (a lock
file in `<state dir>/studio/`); several can run side by side under other names
(`ROR_CAPTURE_PLACE=<name>`, e.g. one with `--pre2022` materials). `--technology` writes the place
with another Lighting.Technology (scripts cannot set it), `--mesh-lods` its
`Workspace.MeshStreamingAndImprovedLods`.

```sh
ror studio open                                   # the capture Studio (--pre2022 for the old materials)
ror studio status                                 # its windows and viewport
ror studio luau -e "return workspace.CurrentCamera.ViewportSize"
ror studio export "My Place" Workspace.Map --out map.rbxm   # read-only export from any open Studio
ror studio close
```

## Comparing

```sh
ror compare Model.rbxm --views default,iso        # Studio vs ror: report.json + diffs
ror compare Model.rbxm --camera '{"mode":"orbit","yaw":25,"pitch":10,"fov":35}'
ror compare Map.rbxm --camera-from out/map-cost --views path1-0-far --lod roblox  # as a game draws, beside ror-full.png
ror studio calibrate material-swatches          # an experiment; see calibrate.rs
```

`ror compare <model>` loads the model into the capture Studio under the same lighting and camera,
captures Studio's viewport, renders ror's view at the viewport's size, and writes `report.json`
(per view the share of pixels within 2 and within 8 of 255), `studio.png`, `ror.png`, `diff.png` and
`side-by-side.png`. Comparisons hold every Beam still (TextureSpeed 0) on both sides and leave out
Terrain's clouds, whose frames never settle in Studio. `tools/parity.py` reruns a fixed set (Studio's
R15 and R6 rigs and four classic catalog items, by asset id); `tools/survey.py` compares many instances
of one place; `ror render --ids` plus `tools/analysis/part_errors.py` ranks a compare's parts by error.

Captures are cached in `.cache/captures`, keyed by model, lighting, camera, size, Studio version and
material generation: a rerun only renders ror's side. `ROR_RECAPTURE=1` captures again;
`ROR_CAPTURE_OFFLINE="<version>|<materials>"` replays stored captures without a Studio.
Several capture Studios can run side by side (`ROR_CAPTURE_PLACE=<name>`); each is driven by one
process at a time (a lock file). Analysis scripts: `tools/analysis/`.

## macOS

`capture_mac.rs` captures with ScreenCaptureKit through `tools/maccap/maccap.swift` (embedded in ror and
compiled on first use into `<state dir>/maccap/` with `swiftc`, from Xcode or the Command Line Tools;
macOS 14 or later):

- **Screen Recording permission** for the app that runs ror, in System Settings > Privacy & Security.
  Without it Studio's window titles are unreadable and captures fail.
- **Lossless sRGB.** The capture asks for sRGB output, which returns the values Studio drew (256 random
  colours and a full-viewport fill came back exact on a P3 display, 2026-11-06). `screencapture -l`
  colour-matches into the display's space instead (sRGB 255,0,255 came back as 234,51,247).
- **Finding the viewport.** It has no window of its own, so the first capture puts a full-viewport
  magenta frame under CoreGui, finds it in a capture of the window and removes it; later captures crop
  that rectangle until the window or viewport changes size.
- **Scale.** `Camera.ViewportSize` is in points and captures are at the display's backing scale, so
  `CAPTURE_VIEWPORT` (1644 x 1116 px) is 822 x 558 points on a Retina display. Resizing the window to it
  goes through System Events, which needs the Accessibility permission (or resize it by hand until
  `ror studio status` shows that viewport).
- **The camera's Focus.** Studio's editor camera on macOS turns a scripted camera to face
  `Camera.Focus` once the script returns, so setting a view also moves the Focus onto its line of sight
  (without it the shapes fixture's default view came back turned several degrees: 18 % within 8).
- **Edit quality.** Studio's Automatic edit quality on macOS draws below level 21 (no Neon glow, Glass
  without refraction), so a capture session pins `EditQualityLevel` at level 21 and `ror studio close`
  puts the earlier value back.
- `ror studio open` starts another Studio process with the capture place (`-task EditFile`), as on
  Windows; `ror studio close` ends it. Both pick the newest Studio install by its executable's date
  (the updater's `~/Library/Roblox/UnzippedBundle`, else `/Applications` or `~/Applications`).

Measured on Apple Silicon with Studio 0.746 (2026-11-06), a capture Studio opened with `ror studio open`,
recaptured (`ROR_RECAPTURE=1`): `tools/parity.py` gives within 2 / within 8 of R15 93.0 / 98.5 %, R6
94.2 / 98.9 %, catalog gear and hats 93.3-94.6 / 98.8-99.6 %; the shapes fixture's default, iso,
front and top views 92.5 / 98.3, 91.1 / 98.5, 95.0 / 98.8 and 86.6 / 94.3 %. What remains is mostly
1 px silhouette edges, faint sky noise and ambient-occlusion halos; the top view's sky also shows a
moon ror leaves out. ror's rules were calibrated on Windows captures, not refit to these.

## Agreement

Agreement is the share of pixels within 2 / within 8 of 255 of Studio's: R15 97.7 / 99.6 %, R6
98.3 / 99.7 %, catalog gear and hats 99.0-99.9 / 99.7-100.0 %, game characters, props and places
98.0-99.99 / 99.4-100.0 %.


## Calibration experiments

`ror studio calibrate <experiment>` builds a fixture model in code (tiles, balls, panels, emitters,
lights), captures it in Studio under controlled lighting from known cameras, renders ror's side, and
writes `out/calibrate/<experiment>/<run>/studio.png` and `ror.png` with `samples.json`: pixel values
at known world points on both sides, with the inputs a fit needs. The experiment names are listed in
`crates/ror/src/studio/calibrate.rs`, `experiment()`; families include `material-*`, `ibl-*`,
`sky-*`, `atmosphere-*`, `fog*`, `post-*`, `bloom-*`, `neon-*`, `glass-*`, `light-*`, `lg-*`
(the light grid), `ao-*`, `shadow-*`, `particle-*`, `beam-*`, `trail-*`, `legacy-*`, `gui-*` and
`forcefield-*`.

- Timeline experiments (particles over time, effects) run a playback in Studio frame by frame and
  compare each captured frame with ror's playback of the same frame time (`calibrate/timeline.rs`).
- `vfx-attack:<bundle.rbxm>|auto:<effect path>` plays any effect of a bundle by its emit attributes;
  `vfx-emitter:<bundle>|<emitter path>|<moment>|<variants>` copies one emitter per variant with one
  property varied; `exposure:<timeline experiment>` averages a long exposure of many plays, since a
  single frame of random particles matches Studio's only by chance; `vfx-sweep:<spec.json>` plays many
  effects a few to a view.
- `tools/analysis/` holds the fits and metrics (numpy and scipy): each script's docstring names the
  experiments it reads and the constants it fits.

Checking a change without a Studio: `ROR_RERENDER=1 ror studio calibrate <experiment>` draws ror's side
again against each run's stored `studio.png`, and `python tools/analysis/scorecard.py snapshot
<before.json> <prefixes...>` / `compare <before.json> <after.json>` scores every stored run before and
after. The stored captures are not in Git (`out/` and `.cache/` are ignored): run the experiments in
your own Studio first.

`cargo run -p ror --release --example tools -- vfx_fuzz 500 1 out/vfx-fuzz extreme` draws random
effect scenes (every property of particles, beams, trails, lights, decals, Glass and Highlights, at
their usual values and their edges) and reports panics, slow renders, drawings that differ twice,
effects that draw nothing (`sane`) and non-finite light (`ROR_NAN_CHECK=1` shows it magenta in any
render).

Some of the maintainers' experiments are not in this repository: those built on test images, meshes
and models that are not published (uniform gray maps, effect bundles and models from a game). The
rules they measured are in the code and in these docs.

## Reporting a difference

Use the "Looks different from Studio" issue form: the Studio version and graphics settings, a minimal
scene you built yourself, the command, and Studio's image next to ror's. See
[CONTRIBUTING.md](../CONTRIBUTING.md) "Measuring against Studio".
