# Progress

## 2026-09-17/18 — Calligraphy pen matches Samsung Notes

### Problem

Handwriting drawn with the calligraphy pen (Samsung Notes UI name) came out of sdocx2pdf looking
nearly monoline: the thick/thin contrast of the original was mostly lost. The uncommitted working
tree already contained a first chisel-nib implementation (`Spread::Chisel`, `Nib`,
`--calligraphy-nib-angle`, default 40°) with nib length 1.05× pen size and thickness ratio 0.42
(≈2.4× contrast) — too weak next to the real thing.

### What the input file contains

Inspected `/tmp/Notes_260907_115501.sdocx` (a Chinese practice notebook, 6 pages):

- All 2,027 strokes use `pen_name = com.samsung.android.sdk.pen.pen.preload.ObliquePen` (the
  calligraphy pen), `tool_type = Pen`, `pen_size = 8.404`, `advanced_pen_settings = "4;"` (just the
  size level — Samsung stores **no nib angle** for this pen).
- Every stroke carries **tilt + orientation** data per event (tilt ≈ 0–60°, mean ~21°; orientation
  clusters around −75°).
- Pen names live in the string registry in `note.note`, referenced by ID from `.page` files.
- New tool for inspecting this: `sdocx/examples/dump_strokes.rs`
  (`cargo run --example dump_strokes -p sdocx -- file.sdocx`), plus new public getters on
  `Stroke`: `advanced_pen_settings()`, `size_level()`, `original_width()`.

### Reverse engineering Samsung's renderer

Source: decompiled Samsung Notes APK <https://github.com/Yanndroid/Samsung-Notes-Port> — each
preload pen ships as its own native library (`libSPenObliquePen.so`, 341 KB, stripped but with
exported C++ symbols). Disassembled with `xcrun llvm-objdump -d` (works on ELF aarch64; symbols
via `nm -D`, shaders via `strings`).

Findings (all in `SPen::ObliquePenStrokeDrawableRTV1::lineToMesh`, shared by GL drawable
versions V1–V4):

- The stroke body is a **fixed 45° chisel ribbon**. Each segment is a parallelogram with corners
  offset from the centreline by the constant diagonal `(±penSize/2, ∓penSize/2)`, plus miter-style
  "skirts" that pad the joins and the thin direction. Vertex alpha (z) fades to 0 across the
  skirts (anti-aliasing).
- Width therefore depends **only on direction of travel**: widest (√2 × penSize) along the
  down-right diagonal, near-hairline perpendicular to that. Verified numerically by transliterating
  the function to Python.
- **Pressure, tilt and orientation are not used for the body.** They only feed the end cap
  (`ObliquePenStrokeTipDrawableRT`, `alphaByDistance` in its shader). The canvas scale
  (`SetInverseScale`) and pen size (`SetPenSizeData`, truncated to int) are the only size inputs.
- So there is nothing more to recover from the file for this pen: the look is purely geometry.

### Changes

- `sdocx2pdf/src/tool.rs`
  - `NIB_LENGTH_PER_PEN_SIZE` 1.05 → **1.35** (Samsung: √2 × penSize; kept slightly under to
    account for the skirts)
  - `NIB_THICKNESS_RATIO` 0.42 → **0.15** (Samsung's thin direction is near-hairline)
  - Documented the RE findings and the APK repo pointer in the code.
- `sdocx2pdf/src/main.rs` — `--calligraphy-nib-angle` default 40° → **45°**, help text updated.
- `sdocx/src/page/object/stroke.rs` — new getters (see above).
- `sdocx/examples/dump_strokes.rs` — new example dumping per-pen stats + tilt histograms.
- `README.md` — limitations paragraph no longer says the calligraphy pen looks like the fountain
  pen; regenerated the embedded `--help` block (COLUMNS=90, same as the `stty` comment marker).

### Result

Output for the practice notebook (`/tmp/output_calligraphy.pdf`) now matches the Samsung Notes
screenshot: thick horizontals/down-right diagonals, tapered 撇 strokes, hairline ends, correct
weight for both the handwriting and the pinyin annotations. Verified by rendering with
`mutool draw` and comparing crops page 1 vs screenshot. Tests: `cargo test` passes (no test suite
in repo), clean release build.

### References

- Decompiled APK (pen .so libraries, smali): <https://github.com/Yanndroid/Samsung-Notes-Port>
- Format RE writeup (delta/pressure/tilt encodings, `libSPenModel.so`):
  <https://musaab.io/posts/2026/reverse-engineering-samsung-notes/> and
  <https://github.com/bxff/samsung-notes-format>
- Other sdocx tooling: <https://github.com/twangodev/sdocx>,
  <https://github.com/cable729/inkterop>
- No official documentation of `.sdocx` exists; Samsung's S Pen Remote SDK is unrelated to the
  file format.

## 2026-09-18 — Artifact-free chisel rendering (matches Samsung's construction)

The first chisel implementation computed a per-sample width from the stroke tangent (ellipse
support-width model) and stitched per-segment Bézier outlines. Against a high-resolution Samsung
Notes screenshot this showed small artifacts: ragged edges (tangent noise amplified into width
noise), notches at segment joins, and blobby ends from the nib stamps.

Fix: replace the model with the construction Samsung itself uses (as found in the RE above):

- The nib is a zero-thickness line segment (the nib edge) of length ~pen size held at 45°, and the
  mark is the surface it sweeps — per-segment parallelograms whose corners are the segment
  endpoints offset by ±half the nib edge vector.
- Consecutive parallelograms share the cross-section at their junction, so the union over a whole
  stroke is exactly **one polygon**: sample points offset by `+edge`, forwards, then by `−edge`,
  backwards. Every edge is straight; width variation emerges from the geometry instead of being
  computed, so tangent noise cannot roughen the outline.
- A small offset perpendicular to the travel direction (`NIB_THICKNESS_RATIO` of the nib length)
  gives strokes a minimum width where travel is parallel to the nib edge (Samsung gets this from
  its mesh's anti-aliasing skirts).
- Removed: `Spread::Chisel`, `Nib::half_width_across`, `ArcMode::None`, end-of-stroke nib stamps
  (the polygon's flat end is the nib-shaped cut), and `ContinuousStroke::length`.
- `append_events_chisel` in `sdocx2pdf/src/tool.rs` replaces the chisel branch of
  `append_events_basic` (which now handles only round nibs).

One bug during the change: both polygon sides were offset by `+edge` (sign never flipped),
collapsing every stroke to a ~2·thickness sliver — caught immediately by rendering.

Result: clean-edged strokes matching the reference screenshot; PDF shrank 2.9 → 1.05 MiB (straight
lines instead of Bézier outlines). Remaining minor differences vs Samsung: stroke ends are flat
cuts rather than pressure-tapered (Samsung's `ObliquePenStrokeTipDrawableRT` tapers ends using
pressure + tilt + total length), and hairlines are marginally thinner.

## 2026-09-18 — Final chisel construction (body + core parallelograms)

The single offset polygon still showed white slashes where strokes reverse sharply: the polygon
self-crosses at cusps and the overlapping lobes have opposite winding, cancelling to zero under
the non-zero fill rule (bevel-every-join variants bit into strokes instead). Two intermediate
rewrites (single polygon with bevels; Samsung's 8-corner skirt formula) also failed validation —
the skirt corner pairing couldn't be recovered reliably from the disassembly.

Final construction, correct by design:

- **Body**: per sample interval, a parallelogram with corners at the endpoints offset by ±half the
  nib edge vector (constant for the whole stroke). Constant offset ⇒ consecutive parallelograms
  share the junction cross-section exactly (no cracks, no whiskers); convex quads as separate
  closed subpaths with uniform winding ⇒ overlaps add winding, never cancel (no holes); no miter
  anywhere ⇒ no spikes. This is Samsung's quad structure minus the skirts.
- **Core**: the same with a constant offset of half the nib thickness perpendicular to the nib
  edge, covering the near-nib-parallel strokes the body alone leaves hairline-thin. Samsung's
  equivalent is the skirt floor (≈0.28 × half pen size, measured); `NIB_THICKNESS_RATIO` = 0.22
  matches.
- Event tangents are no longer used at all in the chisel path — no tangent-noise artifacts are
  possible. Dots still stamp the nib mark.

Verified at 400 dpi: no seams, cracks, holes, whiskers or spikes; contrast matches the Samsung
reference. PDF 2.63 MiB. Remaining known differences: tiny chamfers at direction reversals
(physically what a lifted-and-repositioned nib does) and slightly more angular strokes than
Samsung (their beautifier smooths the input before rendering).

## 2026-09-18 — Vanishing oblique hairlines + miter spikes

User-visible issues after the offset-polygon rewrite: oblique strokes travelling near the nib
angle were too thin to be seen (strokes looked broken), and sharp turns grew miter spikes.

Both causes are documented in the RE material — the fix just hadn't followed it far enough:

- Samsung's ribbon has zero ideal width along the nib edge, but its **miter skirts** keep a
  minimum across-width of ≈0.28 × half the pen size (measured in the numerical transliteration of
  `lineToMesh` earlier: thin-direction ratio 0.28 vs thick-direction 1.70). Our first pass modelled
  the minimum as 0.15 × nib length ≈ half that. `NIB_THICKNESS_RATIO` is now **0.22**.
- The single offset polygon miters every vertex, so sharp direction reversals could spike far
  outside the nib footprint. Samsung emits **one parallelogram per segment** (bounded by its own
  corners, so never spikes). `append_events_chisel` now does the same: each sample interval's
  parallelogram is its own closed subpath, wound consistently (signed area positive) so overlapping
  subpaths can't cancel under the non-zero fill rule. Dots still stamp the nib mark.

PDF is 1.89 MiB (per-segment subpaths cost more than the single polygon but still well under the
2.9 MiB Bézier version). Verified: hairlines continuous on 门/我, no spikes, no seams between
abutting parallelograms at 3× zoom, `--calligraphy-nib-angle` still works.

## 2026-09-18 — Jitter pruning + how Samsung's smoothing/beautifier actually works

### Pruning

Last visible artifacts were thin barbs at stroke entries and tight turns: the recorded points
micro-jitter there, and each jitter sliver became its own parallelogram. Fix: drop sample points
that move less than `2 × nib half-thickness` from the kept predecessor (stroke ends always kept).
Safe because `sample_points` spaces samples by direction change, so only near-stationary clusters
collapse. PDF 1.08 MiB. 我 now matches the Samsung screenshot closely at 400 dpi.

### How Samsung's smoothing/beautifier works (symbol-level RE)

No public reverse-engineering writeup exists. From the decompiled APK's libraries:

- `libSPenPenCommon.so` — `SPen::PenKalmanFilter` (+ `PenKalmanFilterHandler(float, float)`,
  `timeUpdateStep`, `measurementUpdateStep(Vector4f)`): a per-axis **Kalman filter** smooths pen
  events for live ink (latency-free drawing). Crucially, the *redraw* path we disassembled
  (`ObliquePenStrokeDrawableGLV1::GetDrawnPointData → redraw → redrawLine → drawPoint`) contains
  **no Kalman call** — stored strokes are fed straight to the pen engine. The stored `.sdocx`
  points are therefore already Kalman-filtered, which is why our rendering matches without
  re-smoothing.
- `libSPenPredictor.so` — `SPen::NNPredictor` (Kalman-assisted, vsync-driven,
  `SetPredictionTime`/`SetPredictionLength`): neural prediction of the pen's future positions so
  ink appears under the pen tip. Affects live ink only.
- `libSPenHwrBeautifier.so` — the "Handwriting Help" *layout* engine: line straightening via
  RANSAC (`LineFitness`, `LineModel`), line alignment and word spacing (`LineSpacing`,
  `WordSpacingByGaps`, `LineCoreHeightInfo`). Layout only, not ink shape.
- `libSPenBeautify.so` — the calligraphy style-conversion engine ("Cb…"): `CbBeautifyTraj`/
  `CbBeautifyHwr` beautify a trajectory; `FitCurveEx` fits Béziers; `DivideStroke`/
  `AnalyzeSegment`/`AdjustSegmentType` segment into line/arc pieces; `CbComputeContour`/
  `CalContour`/`CalJointContour_new` compute the filled outline by sweeping a simulated tip
  (`SimulatePen`, `SimulatePenTip`, `SimulateCrayon`, `SimulateWritingBrush`); `CbGetSlantDir`/
  `CbSetPenTipDir` hold the nib angle; `CbLoadPDB` loads tuning parameters. Only used when
  handwriting is converted with Handwriting Help — not for normal notes like this one.

### Remaining differences vs Samsung's rendering

- Sampling: Samsung re-samples a quadratic-Bézier path (`SmPath.getPosTan`), we sample the PCHIP
  arc-length curve. Sub-pixel differences at normal zoom.
- Tiny chamfers at direction reversals (physically plausible nib behaviour).
- Their curves are a touch rounder because the stored points were shaped by the live Kalman
  pipeline on-device; nothing further to replicate for already-stored notes.

## 2026-09-18 — Samsung's actual input pipeline replicated; barbs eliminated

Remaining barbs traced to a missing stage of Samsung's pipeline: `redrawLine` (disassembly) never
draws straight between recorded points — it builds an `SmPath` of **quadratic Béziers through the
midpoints of consecutive points** (`moveTo(prev anchor); quadTo(prev point, midpoint)`), then
subdivides that smoothed path uniformly. Sharp direction reversals are therefore rounded before
the nib sweep, so thin legs never carry a full-width cross-section into a turn (the barb
mechanism).

`append_events_chisel` now replicates the whole redraw pipeline:

1. Point filter — a point is accepted only if ≥ 1.5 pt from the previous accepted point on either
   axis (the entry check at the top of `redrawLine`).
2. Midpoint quadratic smoothing (as above). The path starts at the first point and ends at the
   *midpoint* of the last two — the final raw point is only a control.
3. Uniform sampling: `max(int(L / divisor), 2) + 10` steps where divisor is `L/2` for `L ≤ 5`,
   then `3, L/4, 5, L/8, 7, 9`, else `10` (the fmov/fcmp chain in `redrawLine`).
4. Body: constant-offset nib-sweep parallelograms (unchanged).
5. Minimum-width core: now a **stroked polyline** (round joins/caps, width `2 × half_thickness`)
   instead of filled quads — a PDF stroke cannot produce corner artifacts, which kills the last
   nubs (previously the core's quad corners poked out of the body silhouette at thin/fat
   transitions).

`draw_events` was restructured: paths are collected into a `Vec` (no more `Either`) and the chisel
core is stroked after the body fill via a new `Tool::stroke_path` helper. PCHIP resampling and the
Gaussian filters are no longer used for the chisel path — the pipeline is Samsung's own. Verified
barb-free at 400 dpi on the previously affected characters; PDF 3.13 MiB.

### Beautifier research summary (symbols of the APK libs)

- `libSPenPenCommon.so` — `SPen::PenKalmanFilter`: live-ink Kalman smoothing. Not called on the
  redraw path, so stored points are already filtered and are used as-is by both Samsung and us.
- `libSPenPredictor.so` — `SPen::NNPredictor`: Kalman-assisted neural prediction for latency
  compensation. Live-only.
- `libSPenHwrBeautifier.so` — "Handwriting Help" layout engine: RANSAC line straightening
  (`LineFitness`/`LineModel`), line alignment, word spacing. Layout only.
- `libSPenBeautify.so` — calligraphy style conversion (`Cb*`): `FitCurveEx` Bézier fitting,
  `DivideStroke`/`AnalyzeSegment`, contour computation (`CbComputeContour`, `CalJointContour_new`),
  tip simulation (`SimulatePen`, `SimulateCrayon`, `SimulateWritingBrush`), nib slant
  (`CbGetSlantDir`), parameter DB (`CbLoadPDB`). Only for Handwriting-Help-converted notes.
- No public RE writeups exist for any of these; the decompiled APK is the only source.

## 2026-09-18 — Barb/blister elimination: Samsung's redraw pipeline fully replicated

Two rounds on top of the previous construction:

1. **Per-segment tessellation** (not whole-stroke resampling): `redrawLine` subdivides *each*
   smoothed segment (one raw-point interval) separately — `max(int(L/div), 2) + 10` samples with
   L the segment's own length, i.e. ~12 parallelograms per raw point. Our earlier whole-stroke
   uniform sampling was ~20× coarser, which is what let corner artifacts survive. The chisel now
   flattens each midpoint quad into 4 samples directly (visually equivalent to Samsung's ~12 at a
   fraction of the output size) and drops consecutive samples closer than 0.3 pt.

2. **Core as a stroked polyline**: the minimum-width floor is now a PDF stroke (round joins,
   butt caps, width `2 × half_thickness`) over the sample polyline, emitted via a new
   `Tool::stroke_path` helper, instead of filled quads. Filled-quads corners produced blisters
   wherever the core crossed the body silhouette; a stroked polyline cannot. The core is also
   trimmed back `2 × half_thickness` from both stroke ends — with butt caps it would otherwise
   poke out of the thin stroke entries as round blisters (the round-cap variant did exactly that).
   Short strokes whose entire length is under the trim distance get no core.

`draw_events` collects filled paths into a `Vec` and strokes the cores after the body fill.
Pipeline for the chisel is now Samsung's own end to end: 1.5 pt point filter → midpoint quadratic
smoothing → per-segment fine tessellation → constant-offset nib sweep + stroked min-width core.
Verified barb-free at 600 dpi against Samsung's own high-resolution rendering of the same note;
PDF 5.48 MiB (the finer tessellation costs size — the old Bézier-outline version was 2.9 MiB).

## 2026-09-18 — Notch density fix + adaptive end tapers

- **Tessellation density**: the sharp steps on tapering tails were tessellation notches —
  Samsung subdivides every raw-point interval into ~12 parallelograms (their divisor table applied
  per tiny segment); we were at 4. Now 12, matching. (7.3 MiB — finer tessellation costs size.)
- **End tapers**: the stroked core was trimmed at a fixed distance, leaving an abrupt width step
  where it ends. Now the core exists only where the body's own swept width (`|e × u|`) is below
  the core width, and a filled wedge tapers from the true stroke endpoint to the core's butt end
  at that point. Fat-ended strokes keep flat nib cuts (wedge length ≈ 0); thin tails get the long
  gentle taper seen in Samsung's rendering.

Verified against Samsung's 1440px rendering of the same note: 我 is now a near pixel match in
weight, contrast, taper length and hairline continuity.

- `libSPenBase.so` — `SPen::SmPath` (the smoothed-path class used by `redrawLine`): quad/cubic
  verbs are flattened into a segment table by `helper_buildSegments` /
  `helper_compute_quad_segs` (with per-segment float t — `Segment::getFloatT`), and
  `getLength`/`getPosTan` read that table. Our per-quad flattening is the same construction; a
  byte-exact port of the segment-table parameterisation would only move samples by < 0.1 pt.

## 2026-09-18 — Ghidra decompilation pass; exact redraw pipeline port

Ran Ghidra headless over `libSPenObliquePen.so` and `libSPenBase.so` (install: `brew install
ghidra openjdk@21`; dump script at `/tmp/ghidra_scripts/DumpDecomp.java`; outputs
`/tmp/decomp_oblique.c` 1.2 MB, `/tmp/decomp_base.c` 4.4 MB). The decompiled C corrected several
hand-traced assembly conclusions:

- The `redrawLine` divisor table is indexed by **pen size** (`getSize() = trunc(size)/2`), not
  path length: `≤5 → L/2`, `≤10 → 3`, `≤15 → L/4`, `≤20 → 5`, `≤30 → L/8`, `≤40 → 7`,
  `≤55 → 9`, else `10`. For an ~8.4 pen that is a constant **12 samples per raw interval**.
- `redrawLine` has a **decimation state machine**: skip if the new point is within a 1.5 pt box
  of the previous accepted one; movements under 5 pt (50 for finger) draw only every *other*
  interval (a toggle flag); the first movement after init only sets the Bézier control point.
- Anchors re-anchor at the **last sampled position** (state +0x74 → +0x54), and the control
  point commits only on drawn intervals (slides silently on skipped ones).
- The `lineToMesh` "skirts" (corner extensions `2·normalize(w∓u)` with
  `w = unit(diagonal)`, `u = unit(chord)`) carry **vertex alpha 0** — they are anti-aliasing
  gradients in the GPU mesh, invisible in an opaque fill. The visible redraw silhouette is the
  plain constant-offset parallelogram sweep; filling the skirts opaquely (attempted) shreds the
  edges. Skirts were therefore not ported.
- `SmPath::getLength`/`getPosTan` walk an adaptively flattened segment table (de Casteljau
  recursion, tolerance 0.5, `helper_compute_quad_segs`) — sub-0.1 pt sample placement
  differences vs our uniform-t flattening; not ported.

`append_events_chisel` is now a direct port of the redraw pipeline: 1.5 pt point filter →
decimation state machine → midpoint quadratic smoothing (anchor = last sample) → per-interval
12× tessellation → constant-offset parallelogram sweep. The stroked-core min-width hack was
removed (the 12× density covers thin tails; the old 4× density was what left gaps). All 6 pages
verified; PDF 5.63 MiB.

### Why it still isn't byte-identical to the app's display

- The app display draws the **live** mesh path (skirts with alpha gradients + a pressure/tilt
  driven tip drawable) on top of the redraw geometry; a vector PDF has no per-vertex alpha
  ramps, so Samsung's screen output is intrinsically softer at stroke tips.
- Their stored points were shaped on-device by the live Kalman/predictor pipeline.

## 2026-09-18 — Full Ghidra decompilation pass; exact `redrawLine` port

Installed Ghidra (brew) + JDK 21; decompiled `libSPenObliquePen.so` and `libSPenBase.so` headless
(dump script: `/tmp/ghidra_scripts/DumpDecomp.java`; outputs `/tmp/decomp_oblique.c`,
`/tmp/decomp_base.c`). The readable C corrected three assembly-tracing mistakes and one gap:

1. **The divisor table in `redrawLine` is indexed by pen size** (`getSize()` = trunc(size)/2),
   not path length: `≤5 → L/2`, `≤10 → 3`, `≤15 → L/4`, `≤20 → 5`, `≤30 → L/8`, `≤40 → 7`,
   `≤55 → 9`, else `10`. For the practice note's 8.4 pen: divisor = L/2 → steps = 2 → **exactly
   12 samples per accepted raw interval**, constant.
2. **`redrawLine` is a state machine**, not a straight loop:
   - skip the point entirely if within a 1.5 pt box of the previously *accepted* point;
   - movements ≥ 5 pt (50 for finger) always draw; movements < 5 pt draw only every *other*
     interval (toggle flag 0x49);
   - the first movement after initialization only sets the Bézier control point (flag 0x48);
   - the control point commits only on drawn intervals; on skipped intervals nothing updates
     except `prev`.
3. **The anchor re-anchors at the last sampled position** (state +0x74 → +0x54), so consecutive
   intervals connect at the previous interval's penultimate sample, not at its end anchor.
4. **`endPen` extends the stroke to the final raw event position** — quadratic when moving,
   cubic through the control midpoint when stalled — with the same density table, inclusive of
   the end point. The raw final point is used even if the 1.5 pt filter had dropped it.
5. The `lineToMesh` skirts carry **vertex alpha 0** (invisible AA gradients); the visible redraw
   silhouette is the plain constant-offset parallelogram sweep. Porting the skirts as opaque
   fills was wrong and was reverted.
6. `SmPath::getLength`/`getPosTan` walk an adaptively flattened chord table (de Casteljau
   recursion, 0.5 tolerance, `helper_compute_quad_segs`, depth-capped at 0x400 segments per
   quad). Our uniform-t flattening at 12 samples/interval is visually identical at this density;
   a byte-exact port of the chord table remains the only known sub-pixel delta.

`append_events_chisel` now implements the decompiled pipeline verbatim: 1.5 pt filter → 5 pt /
toggle decimation → first-call control point → midpoint quadratic intervals → per-interval
`max(int(L/div),2)+10` sampling → constant-diagonal nib sweep parallelograms, plus the endPen
final stretch. Verified against Samsung's 1440 px capture of the same note (我 is a near pixel
match in silhouette, weight, tapers, hairlines). PDF 5.90 MiB, all 6 pages clean.

## Open / next steps

- [ ] End caps: Samsung tapers stroke starts/ends using pressure + tilt + total length
      (`ObliquePenStrokeTipDrawableRT::lineToMesh`, `SetTotalLength`). Our ends are flat cuts —
      close, but slightly blunter than Samsung's on tapering strokes.
- [ ] Same RE treatment for the remaining undistinguished pens, each has its own `.so`:
      `libSPenPencil2.so`, `libSPenBrushPen.so`, `libSPenInkPen2.so`, `libSPenChineseBrush.so`,
      `libSPenMarker3/4.so`, …
- [ ] `advanced_pen_settings` is a semicolon-separated string (`"4;"` here); collect samples from
      other pens/documents to map its fields.
- [ ] Dashed/dotted lines and tape are still unsupported (pre-existing).
- [ ] Working tree contains all of the above uncommitted (chisel implementation + this session's
      constants/RE docs); commit when ready.
