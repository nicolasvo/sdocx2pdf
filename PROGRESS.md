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

## Open / next steps

- [ ] End caps: Samsung tapers stroke starts/ends using pressure + tilt + total length
      (`ObliquePenStrokeTipDrawableRT::lineToMesh`, `SetTotalLength`). We currently draw flat ends
      (`ArcMode::None` for the chisel).
- [ ] Same RE treatment for the remaining undistinguished pens, each has its own `.so`:
      `libSPenPencil2.so`, `libSPenBrushPen.so`, `libSPenInkPen2.so`, `libSPenChineseBrush.so`,
      `libSPenMarker3/4.so`, …
- [ ] `advanced_pen_settings` is a semicolon-separated string (`"4;"` here); collect samples from
      other pens/documents to map its fields.
- [ ] Dashed/dotted lines and tape are still unsupported (pre-existing).
- [ ] Working tree contains all of the above uncommitted (chisel implementation + this session's
      constants/RE docs); commit when ready.
