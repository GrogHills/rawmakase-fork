//! What editing from an editable preview does to what is measured of the photo (the
//! "Measured tone" risk of the editable previews plan): Auto and the scene tone
//! stage measure the photo they are given, and offline that is the 2560 px editable
//! preview, not the original's full-size decode.
//!
//! For each RAW in the folder `RAWMAKASE_EDITABLE_PREVIEW_SAMPLES`, the editable
//! preview is built as Previews › Build Editable Previews builds it ([`build`] at
//! [`LONG_EDGE`], from the original's full-size decode), written, and read back as
//! Develop opens an offline photo ([`read`], then [`EditablePreview::image`], the
//! image Develop and Auto get). Lossless and Smaller are both measured. Then:
//!
//! - Auto: [`develop::auto_tone`] on the original's decode and on the preview's image,
//!   from the photo's default edit, as the Basic panel's Auto runs it on the open
//!   photo. Each slider's difference (preview minus original) is in slider units:
//!   EV for Exposure, the panel's −100…+100 for the others.
//! - The rendered difference, in CIEDE2000, of the original and the preview both
//!   rendered at the preview's size with the camera's default crop taken off, as
//!   [`super::measure`] renders them: per pixel, and over 16 px blocks, which
//!   average out what the reduction does to detail and leave what the measures do
//!   to tone. Three edits: the default edit; the Auto edit, each image with its own
//!   Auto values, which is what a user gets; and the original's Auto values on both,
//!   which leaves out the Auto sliders' own difference. Detail (sharpening and noise
//!   reduction) is off in all three, as the measures leave detail out. The blocks are
//!   also compared where the original's render is flat (within-block L* standard
//!   deviation under [`FLAT`]): a different measure moves tone there too, while what
//!   the reduction does to detail does not.
//! - The measures' input: the scene tone stage measures a copy of the photo reduced to
//!   512 px (`MAP_EDGE`), its brightest channel on one of 128 px (`MAX_EDGE`). The
//!   same reductions ([`develop::preview`]) of the original's recovered decode and of
//!   the preview's image are compared at the levels it reads: luminance at the 0.1th
//!   percentile (the default black's `dark`), at the 99th, its minimum, and the
//!   brightest channel's maximum, as the preview's level over the original's, in
//!   stops. Luminance here is of camera RGB, so it shows whether the two copies hold
//!   the same light, not the stage's own scene values.
//! - The default black: the default render's darkest levels, L* at the 0.1th and 1st
//!   percentiles of its 16 px blocks, preview minus original. The scene tone stage
//!   sets its default black from the 0.1th percentile of its measurement copy
//!   (`PhotoMeasures::dark`), which this follows without reaching the engine's
//!   private measures.
//!
//! The photo set used for the recorded numbers is 25 CC0 RAWs from 11 makes of
//! `tests/corpus/pixls.json`: per make, its smallest files (by `size_mb`, then URL)
//! whose extension RAWmakase opens as a RAW (`RAW_EXTENSIONS`, so no `.ORI`), three each of Sony, Canon, Fujifilm and Nikon, two each of Panasonic, Leica,
//! Olympus, OM System, Pentax and Ricoh, and one Hasselblad. Run with
//! `cargo test -p rawmakase-decode --release --lib
//! editable_preview::measured_tone -- --ignored --nocapture --test-threads=1`.
use super::*;
use crate::color_difference::{delta_e2000, lab};
use crate::develop;
use rawmakase_model::model::recipe::Recipe;
use rawmakase_model::rendered::Rendered;

/// The sliders Auto sets, by name, read from a recipe in slider units.
const SLIDERS: [(&str, fn(&Recipe) -> f32); 8] = [
    ("Exposure (EV)", |r| r.exposure),
    ("Contrast", |r| r.contrast * 100.),
    ("Highlights", |r| r.highlights * 100.),
    ("Shadows", |r| r.shadows * 100.),
    ("Whites", |r| r.whites * 100.),
    ("Blacks", |r| r.blacks * 100.),
    ("Vibrance", |r| r.vibrance * 100.),
    ("Saturation", |r| r.saturation * 100.),
];

/// Below this standard deviation of L* within a 16 px block, the block is flat.
const FLAT: f64 = 2.;

/// `im` averaged over blocks of 16 × 16 pixels, in display-referred sRGB (0–1), with
/// each block's standard deviation of L*.
fn blocks(im: &Rendered) -> (Vec<[f32; 3]>, Vec<f64>) {
    let (w, h) = (im.width / 16, im.height / 16);
    (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (bx, by) = (i % w, i / w);
            let mut sum = [0f32; 3];
            let mut l = [0f64; 256];
            for y in 0..16 {
                for x in 0..16 {
                    let p = im.pixels[((by * 16 + y) * im.width + bx * 16 + x) as usize];
                    (0..3).for_each(|c| sum[c] += p[c] / 256.);
                    l[(y * 16 + x) as usize] = srgb_lab(p)[0];
                }
            }
            let mean = l.iter().sum::<f64>() / 256.;
            let sd = (l.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / 256.).sqrt();
            (sum, sd)
        })
        .unzip()
}

fn srgb_lab(p: [f32; 3]) -> [f64; 3] {
    lab(p.map(|v| f64::from(rawmakase_model::color::srgb_decode(v.clamp(0., 1.)))))
}

/// The value at `q` (0–1) of `d`, sorted.
fn quantile(d: &[f64], q: f64) -> f64 {
    d[((d.len() - 1) as f64 * q) as usize]
}

/// Mean, 99th percentile and largest ΔE2000 between same-sized pixel lists.
fn difference(a: &[[f32; 3]], b: &[[f32; 3]]) -> [f64; 3] {
    let mut d: Vec<f64> = a
        .par_iter()
        .zip(b)
        .map(|(a, b)| delta_e2000(srgb_lab(*a), srgb_lab(*b)))
        .collect();
    d.sort_by(f64::total_cmp);
    [
        d.iter().sum::<f64>() / d.len() as f64,
        quantile(&d, 0.99),
        d[d.len() - 1],
    ]
}

/// L* of `blocks` at the 0.1th and 1st percentiles.
fn darkest(blocks: &[[f32; 3]]) -> [f64; 2] {
    let mut l: Vec<f64> = blocks.iter().map(|p| srgb_lab(*p)[0]).collect();
    l.sort_by(f64::total_cmp);
    [quantile(&l, 0.001), quantile(&l, 0.01)]
}

/// The levels the scene tone stage reads of its measurement copies of `im` (see the
/// module's notes): luminance at the 0.1th and 99th percentiles and its minimum on
/// 512 px, and the brightest channel's maximum on 128 px.
fn copy_levels(im: &CameraImage) -> Result<[f64; 4]> {
    let recovered = develop::quality::recovered(im, Cancel::NEVER)?;
    let luminance = |p: &[f32; 3]| f64::from(0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]);
    let mut l: Vec<f64> = develop::preview(&recovered, 512)
        .pixels
        .iter()
        .map(luminance)
        .collect();
    l.sort_by(f64::total_cmp);
    let max = develop::preview(&recovered, 128)
        .pixels
        .iter()
        .flat_map(|p| p.map(f64::from))
        .fold(0., f64::max);
    Ok([quantile(&l, 0.001), quantile(&l, 0.99), l[0], max])
}

/// One comparison of two renders: per pixel and 16 px blocks, each mean/p99/max.
struct Compared {
    pixels: [f64; 3],
    blocks: [f64; 3],
    /// The blocks flat in the original's render (see [`FLAT`]).
    flat: [f64; 3],
    /// Preview minus original: the darkest blocks' L* (see [`darkest`]).
    dark: [f64; 2],
}

fn compare(a: &Rendered, b: &Rendered) -> Result<Compared> {
    ensure!(
        (a.width, a.height) == (b.width, b.height),
        "frame differs: {}×{} vs {}×{}",
        a.width,
        a.height,
        b.width,
        b.height
    );
    let (pa, sd) = blocks(a);
    let (pb, _) = blocks(b);
    let (da, db) = (darkest(&pa), darkest(&pb));
    let (fa, fb): (Vec<_>, Vec<_>) = pa
        .iter()
        .zip(&pb)
        .zip(&sd)
        .filter(|(_, sd)| **sd < FLAT)
        .map(|(pair, _)| pair)
        .unzip();
    Ok(Compared {
        pixels: difference(&a.pixels, &b.pixels),
        blocks: difference(&pa, &pb),
        flat: if fa.is_empty() {
            [0.; 3]
        } else {
            difference(&fa, &fb)
        },
        dark: [db[0] - da[0], db[1] - da[1]],
    })
}

/// What one photo's editable preview at one precision gave.
struct Measured {
    /// Auto from the preview minus Auto from the original, per slider of [`SLIDERS`].
    auto: [f32; 8],
    /// log2 of the preview's [`copy_levels`] over the original's.
    levels: [f64; 4],
    default: Compared,
    auto_edit: Compared,
    same_auto: Compared,
}

/// Mean |x|, p95 |x|, max |x| and mean x.
fn summary(mut v: Vec<f64>) -> [f64; 4] {
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let mut a: Vec<f64> = v.drain(..).map(f64::abs).collect();
    a.sort_by(f64::total_cmp);
    [
        a.iter().sum::<f64>() / a.len() as f64,
        quantile(&a, 0.95),
        a[a.len() - 1],
        mean,
    ]
}

#[test]
#[ignore = "needs RAWs in RAWMAKASE_EDITABLE_PREVIEW_SAMPLES"]
fn measured_tone() {
    let Some(dir) = std::env::var_os("RAWMAKASE_EDITABLE_PREVIEW_SAMPLES") else {
        eprintln!("RAWMAKASE_EDITABLE_PREVIEW_SAMPLES is not set");
        return;
    };
    let out = tempfile::tempdir().unwrap();
    let cache = crate::decode_cache::DecodeCache::new(out.path().join("cache"), 0);
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| rawmakase_model::storage::is_raw(p))
        .collect();
    paths.sort();
    if paths.is_empty() {
        eprintln!("No RAWs in RAWMAKASE_EDITABLE_PREVIEW_SAMPLES");
        return;
    }
    let precisions = [
        ("Lossless", Precision::Lossless),
        ("Smaller", Precision::Smaller),
    ];
    let mut results: Vec<Vec<Measured>> = precisions.iter().map(|_| Vec::new()).collect();
    let f3 = |d: [f64; 3]| format!("{:.2}/{:.2}/{:.2}", d[0], d[1], d[2]);
    println!(
        "| File | Camera | Precision | Auto Δ: {} | Default ΔE px; blocks; flat blocks | Auto edit ΔE px; blocks | Original's Auto on both ΔE px; blocks | Default darkest ΔL* (0.1%, 1%) | Measured copy, stops (dark, p99, min, max) |",
        SLIDERS.map(|s| s.0).join(", ")
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let result = (|| -> Result<Vec<(String, Measured)>> {
            let identity = Identity::read(&path)?;
            let raw = crate::photo::open(&path)?;
            let full = crate::decode::FullSize::of_version(None, Default::default(), cache.clone());
            let original = full.decode(raw, crate::decode::DecodePolicy::Show, Cancel::NEVER)?;
            let base = Recipe::for_metadata(&original.metadata);
            let auto_original = develop::auto_tone(&original, &base, Cancel::NEVER)?;
            let detail_off = |mut r: Recipe| {
                r.sharpening = 0.;
                r.noise_luma = 0.;
                r.noise_chroma = 0.;
                r
            };
            let flat = super::measure::uncropped(&original);
            let render = |im: &CameraImage, r: &Recipe| -> Result<Rendered> {
                let r = detail_off(r.clone());
                develop::render(im, &r.checked()?, LONG_EDGE, Cancel::NEVER)
            };
            let original_default = render(&flat, &base)?;
            let original_auto = render(&flat, &auto_original)?;
            let original_levels = copy_levels(&original)?;
            let mut rows = Vec::new();
            for (label, precision) in precisions {
                let preview = build(
                    &original,
                    &identity,
                    crate::photo::camera_exif(&path),
                    LONG_EDGE,
                    precision,
                    Cancel::NEVER,
                )?;
                let file = out.path().join(format!("{name}-{label}.rmep"));
                write(&file, &preview)?;
                let back = read(&file, Cancel::NEVER)?;
                std::fs::remove_file(&file)?;
                let image = back.image();
                let auto_preview = develop::auto_tone(image, &base, Cancel::NEVER)?;
                let flat_preview = super::measure::uncropped(image);
                let levels = copy_levels(image)?;
                let m = Measured {
                    levels: std::array::from_fn(|i| {
                        (levels[i].max(1e-6) / original_levels[i].max(1e-6)).log2()
                    }),
                    auto: SLIDERS.map(|(_, v)| v(&auto_preview) - v(&auto_original)),
                    default: compare(&original_default, &render(&flat_preview, &base)?)?,
                    auto_edit: compare(&original_auto, &render(&flat_preview, &auto_preview)?)?,
                    same_auto: compare(&original_auto, &render(&flat_preview, &auto_original)?)?,
                };
                rows.push((
                    format!(
                        "| {name} | {} {} | {label} | {} | {}; {}; {} | {}; {} | {}; {} | {:+.2}, {:+.2} | {} |",
                        original.metadata.make,
                        original.metadata.model,
                        m.auto
                            .iter()
                            .enumerate()
                            .map(|(i, d)| if i == 0 {
                                format!("{d:+.2}")
                            } else {
                                format!("{d:+.0}")
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        f3(m.default.pixels),
                        f3(m.default.blocks),
                        f3(m.default.flat),
                        f3(m.auto_edit.pixels),
                        f3(m.auto_edit.blocks),
                        f3(m.same_auto.pixels),
                        f3(m.same_auto.blocks),
                        m.default.dark[0],
                        m.default.dark[1],
                        m.levels.map(|v| format!("{v:+.3}")).join(", "),
                    ),
                    m,
                ));
            }
            Ok(rows)
        })();
        match result {
            Ok(rows) => {
                for (i, (row, m)) in rows.into_iter().enumerate() {
                    println!("{row}");
                    results[i].push(m);
                }
            }
            Err(e) => println!("| {name} | failed: {e:#} |"),
        }
    }
    for ((label, _), results) in precisions.iter().zip(&results) {
        if results.is_empty() {
            continue;
        }
        println!(
            "\n{label}, {} photos: Auto from the preview minus Auto from the original",
            results.len()
        );
        println!("| Slider | Mean abs | p95 abs | Max abs | Mean (signed) |");
        println!("| --- | --- | --- | --- | --- |");
        for (i, (slider, _)) in SLIDERS.iter().enumerate() {
            let s = summary(results.iter().map(|m| f64::from(m.auto[i])).collect());
            println!(
                "| {slider} | {:.3} | {:.3} | {:.3} | {:+.3} |",
                s[0], s[1], s[2], s[3]
            );
        }
        println!("\n{label}: ΔE2000 over photos (mean of means, p95 of p99s, largest max)");
        println!("| Edit | Per pixel | 16 px blocks | Flat blocks |");
        println!("| --- | --- | --- | --- |");
        let line = |edit: &str, of: fn(&Measured) -> &Compared| {
            let over = |stat: fn(&Compared) -> [f64; 3]| {
                let mean = summary(results.iter().map(|m| stat(of(m))[0]).collect())[0];
                let p99 = summary(results.iter().map(|m| stat(of(m))[1]).collect())[1];
                let max = summary(results.iter().map(|m| stat(of(m))[2]).collect())[2];
                format!("{mean:.3}/{p99:.2}/{max:.2}")
            };
            println!(
                "| {edit} | {} | {} | {} |",
                over(|c| c.pixels),
                over(|c| c.blocks),
                over(|c| c.flat)
            );
        };
        line("Default", |m| &m.default);
        line("Auto, each its own", |m| &m.auto_edit);
        line("Original's Auto on both", |m| &m.same_auto);
        for (i, level) in ["dark (0.1%)", "p99", "min", "max"].iter().enumerate() {
            let s = summary(results.iter().map(|m| m.levels[i]).collect());
            println!(
                "\n{label}: measured copy's {level}, preview over original in stops: mean abs {:.4}, p95 abs {:.4}, max abs {:.4}, mean {:+.4}",
                s[0], s[1], s[2], s[3]
            );
        }
        for (i, q) in ["0.1%", "1%"].iter().enumerate() {
            let s = summary(results.iter().map(|m| m.default.dark[i]).collect());
            println!(
                "\n{label}: default render's darkest blocks, L* at {q}, preview minus original: mean abs {:.3}, p95 abs {:.3}, max abs {:.3}, mean {:+.3}",
                s[0], s[1], s[2], s[3]
            );
        }
    }
}
