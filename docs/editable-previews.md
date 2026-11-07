# Editable previews

Status: Plan (2026-11-06), partly built: stages 1a, 2, 3, 4 and 5 are in (2026-11-07).

Proposed implementation, 2026-11-06. Repository inspected at `509dda0`
(`pch/rawmakase` main). This is a plan, not a claim of implemented support.
Each decision below was reviewed and approved one at a time; the options set
aside are recorded with the reason, so they are not argued again.

## Recommendation

Let a photo whose original is offline be edited in Develop from a smaller copy
RAWmakase built while the original was online: an *editable preview*, about 2560
pixels on the long edge, as Lightroom Classic's are. The edit made from it is the
photo's edit, saved in the catalog for the original, and renders from the
original again once it is back.

RAWmakase builds its own editable previews. Lightroom's (`<Catalog> Smart
Previews.lrdata`) are not imported.

## What exists, and what blocks it

Already there:

- Positions of masks, spots and red eye are normalised image space
  (`model::image_frame`), so they don't depend on the resolution edited at.
- Background preview builds with progress, cancellation and stale-result checks
  (`src/app/library/preview_build.rs`), Standard and 1:1 previews, and an offline
  RAW shown and zoomed in the Loupe (`src/app/develop/stand_in.rs`).
- The work table (`src/app/work.rs`), which gives each kind of work its policy
  for closing, the status line and the exit sequence.
- A lossless JPEG XL encoder (`zune-jpegxl`, `rawmakase-export/src/export/jxl.rs`)
  and decoder (`jxl-oxide`), both pure Rust.

Blocking:

1. **Develop refuses an offline photo**: `Refusal::Offline` from
   `develop_refusal` (`src/app/library/mod.rs`).
2. **Saving and loading an edit read the original.** `Catalog::check_edit`
   reads `Identity::read(e.path)` and compares it with the identity read when
   the photo was opened (`catalog/edit_rows.rs`); `EditRecord::saved` reads it
   to protect an edit from a replaced file (`rawmakase-interop/src/edits.rs`).
   Both fail when the file is not there.
3. **Radius effects are scaled to the full-size image in memory.**
   `render_level` takes the output scale relative to `full.image`
   (`develop/quality/render.rs`). From a 2560 px editable preview, sharpening,
   noise reduction, Texture, Clarity and grain would be sized as if 2560 px
   were the whole photo.
4. **Development needs the original's facts**: lens tables embedded in the RAW,
   DNG hints, white balance, profile matching, and the camera's EXIF, which
   `photo::open` and `photo::camera_exif` read from the file.

## Decisions

### 1. Scope: build our own

RAWmakase builds editable previews from originals that are online (Previews ›
Build Editable Previews). Importing Lightroom's was set aside: it would need a
lossy JPEG DNG decoder (compression 34892, refused today), research on private
Lightroom catalogs, and a way to match each to an original RAWmakase never saw;
and its 8-bit files have no HDR headroom and lack the RAW's lens data.

### 2. Identity of the original: read when building

Moot with decision 1. The builder reads the original's `Identity` when it makes
each editable preview and stores it in the file and the catalog row.

### 3. Format: RAWmakase's own file, lossless 16-bit JPEG XL

A versioned file of RAWmakase's own, holding:

- the demosaiced camera image with highlights recovered, as Develop gets it from
  `decode`, downscaled to 2560 px on the long edge;
- pixels as lossless 16-bit JPEG XL on a log-style curve with headroom above
  white, so HDR edits keep rendering from it;
- the original's facts: `Identity`, full size and frame, camera, white balance,
  DNG hints, embedded lens tables, and the camera's EXIF (a few KB, kept so
  offline export can be added later without rebuilding);
- a codec field, so a smaller lossy codec can be added without a new format.

Editable previews are not a cache. Once the original is offline one can't be
rebuilt, so **every version ever written stays readable**. The layout is
documented, the facts are stored with explicit defaults for missing fields
(never as serialised internal structs that change between releases), and golden
files of each version are opened by tests.

Measured on 25 public CC0 RAWs (11 makes) at 2560 px: lossless files average
15.7 MB (median 15.3, 13.4–22.2), above the roughly 12 MB first set as the bar;
building takes 1.4 s, writing 0.2 s and reading 0.35 s a photo, and the format
itself loses ΔE2000 0.003 on average. Decided 2026-11-07: lossless ships as the
default anyway, since previews are built for chosen photos, not a whole catalog.
A **"Smaller" choice comes later, in its own change**: 12-bit codes on the same
curve (mean 9.1 MB, 42% smaller; worst p99 about 2 ΔE2000 under Saturation and
Vibrance +100), opt-in, with dither added when writing, because on a clean,
noise-free gradient undithered 12-bit codes show contour steps under strong
colour or curve edits. It was to ship only if the dither removed most of those
steps while adding under about 1 ΔE2000.

Measured 2026-11-07 (codec 2, `editable_preview::depths`, the same 25 RAWs):
dithered 12-bit files average 9.2 MB (median 8.8, largest 14.6). On the
noise-free gradient, under every edit measured, the largest contour step is
0.7–1.3 times the lossless preview's (undithered 12-bit: 1.6–9 times), and each
excess over the lossless step is within what that edit's grain left after
averaging makes by itself (the largest, under Saturation and Vibrance +100, 26
levels of 16-bit output over the lossless step against that edit's noise floor
of 57), so no contour is left; the cost is fine grain, at
most about 0.6 of an 8-bit level. Against the lossless
preview, 16 px blocks differ by at most 0.32 ΔE2000 (p99, worst photo) and pixels
by 0.05–0.31 on average; per-pixel p99 stays under 1 for defaults, ±EV and
Shadows (worst photo 0.90), but reaches 2.3–3.3 on the worst photo under
Contrast, Clarity and Texture +100, a steep curve, or Saturation and Vibrance
+100. Undithered 12-bit codes already exceed 1 there (1.3–1.9), so no 12-bit
choice meets a per-pixel bar; the excess is the dither's grain. Decided
2026-11-07: the bar is restated as what shows — no contour steps beyond the
lossless preview's on smooth gradients, 16 px block p99 under 1 ΔE2000, per-pixel
mean under about 0.35, and the grain under extreme edits said where the choice
is offered — which Smaller meets, and it ships opt-in, lossless staying the
default. Shipped as Preferences › Catalog's Lossless or Smaller choice: read once
when a build is asked for (captured in each item, so a change applies to the
next request), saved in the session as `smaller_editable_previews` (absent reads
as Lossless), and followed by automation's `build_editable_previews`, which takes
no precision of its own. It never rebuilds an editable preview that is up to
date: the build's skip rule goes by the original's identity alone, so changing
the choice keeps those built already at theirs. 10-bit codes (6.1 MB) were set aside: 5–7
ΔE2000 p99 under strong edits, and banding under ordinary ones. Version 1 already
allows the new codec or curve code.

Set aside: an 8-bit lossy file (no HDR headroom, banding under strong edits);
a lossy DNG like Adobe's (needs a DNG writer and the 34892 decoder, loses the
embedded lens tables).

### 4. AI masks: disabled while offline

Select Subject, Sky and Background are disabled on an editable preview, with
"Connect the original to use AI masks". Masks already saved render as usual.
AI Denoise is disabled too: it needs the RAW's sensor data.

Set aside for now: allowing them, with "made from editable preview" in the mask's
recorded inputs and a Regenerate note once the original is back. The models see
about 1,000 pixels either way; only edge refinement (2560 px instead of up to
4096) differs. This can be added later without a format change.

### 5. Export: not from editable previews

An offline photo stays un-exportable, listed as offline in the Export dialog as
today. The EXIF stored by decision 3 keeps offline export possible later.
Automation's `export` is refused for a photo open from its editable preview too;
its `preview` (MCP `preview_photo`) is allowed (decided 2026-11-07), so an agent can
see its own offline edit. A preview is for looking at, not a file to deliver, so it
is a JPEG only, at most the editable preview's size: a TIFF or PNG of it would be
the capped export set aside below.

Set aside: exporting when the requested size fits the editable preview after crop;
Lightroom's way, capping output at the editable preview's size, which silently
makes full-size exports small.

### 6. Name: editable previews

Lightroom calls its own feature Smart Previews. RAWmakase's are **editable
previews**, so they aren't taken for Lightroom's (whose files RAWmakase doesn't
read), and so the name says what sets them apart from Standard and 1:1
previews, which are pictures only. No on-disk or schema name says "smart": the
files are `<photo id>-<identity key>.rmep`, in
`<catalog name>.rawmakase-editable-previews/`, recorded in the `editable_previews`
table. Decided 2026-11-07, before any of them was released.

### 7. Base: upstream main

Develop on `pch/rawmakase` main. The fork's HEIF work (rust_h265, where upstream
uses libheif) and Library preview speed-up are not needed here; syncing the fork
with upstream is separate work.

### 8. Camera Look: the default fit is kept, fitting waits for the original

A photo whose camera RAWmakase has no colour data for starts from a look fitted to
its embedded JPEG (`camera_look::with_default`). Its fit cache is keyed by the file
itself, so offline it can't be found, and an editable preview keeps no embedded
JPEG. So, decided 2026-11-07:

- When an editable preview is built for such a photo (`fitted::eligibility` is
  `Absent`), its default fit is stored in the file as an optional section, in the
  profile's own versioned form (`rawmakase:FittedProfile`, about 100 KB). Other
  photos' files don't grow, and version 1 readers skip the section.
- Offline, the photo opens with the stored fit, so it looks as it does online.
  It comes from the release that built the preview; after a change to the fitter
  it may differ slightly from the original's until the preview is rebuilt.
- **Profile › Fit Camera Look** is disabled on an editable preview ("Connect the
  original to fit a Camera Look"), as AI masks are. A fit already applied is part
  of the edit and keeps working.

Set aside: storing the embedded JPEG (1–5 MB) so fitting works offline too.

## Ownership

| Concern | Owner |
| --- | --- |
| The file format: writing, reading every version, the original's facts | `rawmakase-decode`, an `editable_preview` module beside `decode_cache` (it already reaches the decoders, `jxl-oxide` and colour; `zune-jpegxl` joins its dependencies) |
| Which photos have one, where it is, whether it is stale, removal with its photo | `rawmakase-catalog`, a `catalog::editable_previews` module; the catalog records file names and identities and never decodes |
| Opening a photo from its editable preview, as the original's frame and size | `decode`, behind `photo::open_source`'s caller (a source that is the original or its editable preview) |
| Radius scale relative to the original's size | `rawmakase-engine` (`render_level` and the export path) |
| Whether Develop opens an offline photo, and what is disabled | `app::library::Refusal`, Develop's tools, `model::hdr::check_recipe` for HDR |
| Build and Discard, progress, cancellation | `app::library::preview_build` and a row in `app::work` |

Editable preview files are RAWmakase's own derived files, not a user's photos:
their owner deletes them outright, as the preview cache's are. State that in
`docs/architecture.md` beside the rule that RAWmakase never deletes a user's file.

## Stages

Each stage is one pull request with its own review and `make check`. Mechanical
refactors go in their own commits, before the behaviour they enable.

### Stage 1: radius scale from the original's size (engine)

`CameraImage` (or `Retouched`) carries the long edge of the photo it stands for,
the image's own size by default. `render_level` and exports take the output scale
from it. Tests: a 2560 px image standing for a 6000 px photo renders within the
existing Fit bound of the 6000 px photo resized to 2560; images standing for
themselves render byte for byte as before. No reference is re-blessed.

### Stage 2: the file format (decode)

`editable_preview::write` and `read`, version 1, with the measurement above as the
gate on the codec. Tests: round trip of pixels and facts, headroom above white
kept, a truncated or corrupt file refused without a panic, a golden version-1 file
opened, an unknown later version refused.

### Stage 3: the store (catalog)

An `editable_previews` table (photo, file, the original's identity, sizes, format
version, built at), added without a catalog format change, as `photo_checksums`
was; portable SQL, so it is prepared on Postgres in CI too. Files beside the
catalog in `<catalog name>.rawmakase-editable-previews/`, or in the data folder
for a catalog that is not a file (`CatalogLocation`). Keyed by master photo, so
virtual copies share one and moves and renames need nothing. Removed with its
photo (Remove from Catalog, Move to Trash), in the transaction that removes the
photo's rows, the file after commit. Kept with Export as Catalog
(`catalog/export_catalog.rs`). An original seen online with an identity that is
not `same_photo` marks it stale.

### Stage 4: Build and Discard Editable Previews (app)

Previews › **Build Editable Previews** and **Discard Editable Previews** in the
thumbnail menu, through `preview_build`'s worker and a row in `app::work`. Each
item's inputs (photo, path, identity, demosaic) are captured once; a result for a
photo deleted, replaced or rebuilt meanwhile is dropped. Cancel, failure and
panic clear the busy state. Preferences › Catalog shows the space they take.
Automation and MCP get `build_editable_previews`.

### Stage 5: edit offline (app, catalog, interop)

- `DevelopSource` names the original or its editable preview; `develop_refusal`
  refuses only an offline photo without a usable one.
- The photo opens from the editable preview with the original's frame, size and
  facts; its identity for saving is the one the editable preview recorded.
- Saving offline goes through the catalog's own checks: `check_edit` takes that
  identity in place of reading the missing file, and `EditRecord::saved`
  protects an edit against it. Older releases fail closed on nothing new: the
  stored identity is an ordinary one.
- Disabled with a reason: the demosaic choice, AI masks and AI Denoise
  (decision 4); HDR stays available, as the file keeps headroom.
- Develop and the Loupe say "Editable Preview" or "Original"; the Library filter
  gains "Has Editable Preview".
- When the original comes back, Develop uses it on the next open.

Tests: save offline, reconnect the same file, the edit loads; reconnect a
replaced file, the edit is protected; the editable preview deleted while open; a
failed save offline stays pending and blocks navigation; the original returning
while the photo is open; the photo removed mid-build.

### Stage 6: documentation

`docs/catalogs.md` (the feature), `docs/architecture.md` and `docs/code-map.md`
(the owners above and the deletion rule), `scripts/deps-allowed.txt` only if a
dependency direction changes, and the measurement in
`docs/records/preview-performance-measurements.md`.

## Risks

- **Measured tone.** The scene tone stage and Auto measure the photo from a
  full-resolution copy (`Toned::measured`), and from an editable preview that copy
  is of the 2560 px image. Both read reduced copies (the stage 512 px and 128 px,
  Auto 1024 px of at most 2048), so the difference is small. Measured 2026-11-07
  (`editable_preview::measured_tone`) on 25 CC0 RAWs from 11 makes of
  `tests/corpus/pixls.json`, chosen by the rule that test states. Decision 3's own
  list was not recorded, so this set is a reconstruction, not necessarily the same
  files. Each preview was built as the app builds it and read back as Develop opens
  an offline photo. Both were developed with the built-in profile from
  `Recipe::for_metadata`, not with user defaults, an installed DCP or a fitted Camera
  Look. Lossless and Smaller give the same Auto values; other figures are
  Lossless's, with Smaller's where they differ.
  - Auto estimates Exposure, Highlights, Shadows, Whites, Blacks and Saturation
    (Contrast and Vibrance are constants). From the editable preview it sets the
    same values as from the original on every photo, except Whites on one photo,
    which is 1 lower (mean |Δ| 0.04, p95 0, max 1). Exposure differs by 0.00 EV on
    every photo.
  - The test compares the same reductions of both images, by luminance of camera
    RGB, a proxy for the stage's own values, which are internal. They hold the same
    light: the 0.1th percentile, which the default black follows, differs by at most
    0.04 stops (0.16 with Smaller's dither), and the brightest channel by at most
    0.03. The 99th percentile, which Whites reads, differs by at most 0.13 stops, on
    one photo (p95 0.003). The darkest level, which Blacks reads, differs by at most
    0.22 stops (p95 0.06, mean 0.03). In the default render, the darkest 0.1% of
    16 px blocks differ by at most 0.27 L* (mean 0.05).
  - Rendered at 2560 px with detail off, the original and the preview differ by
    ΔE2000 0.18 on average over 16 px blocks for the default edit (p99 1.43 at the
    p95 photo, 1.67 on the worst). For the Auto edit the difference is 0.22 (p99
    1.48 at the p95 photo). That changes by at most 0.001 on average when both
    images take the original's Auto values, so the Auto values add almost nothing.
    The measures can't be swapped between the two images, so their share is
    bounded rather than isolated. A different measure moves tone in flat areas as
    much as anywhere. In flat blocks (L* deviation under 2 within the block), the
    default edit differs by 0.05 on average, p99 at most 0.66 on the worst photo.
    The Auto edit, whose Blacks reads the darkest level, differs there by 0.08,
    p99 0.55 at the p95 photo (Smaller 0.63). The largest differences are in blocks
    of fine detail, which a 2560 px image renders differently from a full-size one.
    Per pixel, that detail makes renders differ more (mean 1.2, worst photo's
    p99 14.5).

  So the measured tone of an offline edit differs from its original's by at most
  about the flat-block figures above, well under a visible difference. Auto gives
  the same values beyond its own rounding. Nothing about measured tone needs to
  change, and no reference was re-blessed.
- **Size on disk.** The measurement in decision 3 may force the lossy option
  into the first release.
- **Format permanence.** Version 1 is a commitment; review its layout as
  carefully as the catalog schema.

## Not covered

Importing Lightroom's smart previews, AI masks and export from editable previews,
and syncing the fork with upstream: each is set aside above with its reason.
Parity measurements need the private corpus and follow
`tests/corpus/README.md`.
