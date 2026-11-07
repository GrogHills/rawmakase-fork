//! The measurement the smart preview format was chosen on (decision 3 of the Smart
//! Previews plan): for each RAW in the folder `RAWMAKASE_SMART_PREVIEW_SAMPLES`,
//! the file's size at [`LONG_EDGE`], the time to build, write and read it, and how
//! its renders differ from the original's at the same size. Run with
//! `cargo test -p rawmakase-decode --release smart_preview::measure -- --ignored --nocapture`.
//!
//! Renders are compared in CIEDE2000 over every pixel. "format" compares the
//! reduced image before and after the file, so it is the curve's loss alone; the
//! recipes compare renders of the original and of the smart preview, which also
//! differ by what Stage 1 of the plan fixes (detail radii scaled to the image in
//! memory) and by measurements taken from 2560 pixels.
use super::*;
use crate::color_difference::{delta_e2000, lab};
use crate::develop;
use rawmakase_model::model::recipe::Recipe;
use std::time::Instant;

fn srgb_lab(p: [f32; 3]) -> [f64; 3] {
    lab(p.map(|v| f64::from(rawmakase_model::color::srgb_decode(v.clamp(0., 1.)))))
}

/// Mean, 99th percentile and largest difference between `a` and `b`, compared where
/// they overlap (a render can round its size differently by a pixel).
fn difference(a: (&[[f32; 3]], u32, u32), b: (&[[f32; 3]], u32, u32)) -> [f64; 3] {
    let (w, h) = (a.1.min(b.1) as usize, a.2.min(b.2) as usize);
    let mut d: Vec<f64> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = (i % w, i / w);
            delta_e2000(
                srgb_lab(a.0[y * a.1 as usize + x]),
                srgb_lab(b.0[y * b.1 as usize + x]),
            )
        })
        .collect();
    d.sort_by(f64::total_cmp);
    let mean = d.iter().sum::<f64>() / d.len() as f64;
    [mean, d[(d.len() - 1) * 99 / 100], d[d.len() - 1]]
}

/// The same difference for linear camera values, each shown as sRGB would show
/// them at `exposure` stops: the curve's loss, from shadows to highlights.
fn camera_difference(a: &CameraImage, b: &CameraImage, exposure: f32) -> [f64; 3] {
    let shown = |im: &CameraImage| -> Vec<[f32; 3]> {
        im.pixels
            .par_iter()
            .map(|p| {
                p.map(|v| rawmakase_model::color::srgb_encode((v * exposure.exp2()).clamp(0., 1.)))
            })
            .collect()
    };
    difference(
        (&shown(a), a.width, a.height),
        (&shown(b), b.width, b.height),
    )
}

#[test]
#[ignore = "needs RAWs in RAWMAKASE_SMART_PREVIEW_SAMPLES"]
fn measure() {
    let Some(dir) = std::env::var_os("RAWMAKASE_SMART_PREVIEW_SAMPLES") else {
        eprintln!("RAWMAKASE_SMART_PREVIEW_SAMPLES is not set");
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
    println!(
        "| File | Camera | Original | Preview | Size (MB) | Build (s) | Write (s) | Read (s) | Format ΔE mean/p99/max (+0, +4 EV) | Default ΔE mean/p99/max | Detail off | Detail off +4 EV | Detail off −3 EV |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut sizes = Vec::new();
    for path in paths {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let result = (|| -> Result<String> {
            let identity = Identity::read(&path)?;
            let raw = crate::photo::open(&path)?;
            let full = crate::decode::FullSize::of_version(None, Default::default(), cache.clone());
            let original = full.decode(raw, crate::decode::DecodePolicy::Show, Cancel::NEVER)?;
            let t = Instant::now();
            let preview = build(
                &original,
                &identity,
                crate::photo::camera_exif(&path),
                LONG_EDGE,
                Cancel::NEVER,
            )?;
            let build_s = t.elapsed().as_secs_f64();
            let file = out.path().join(format!("{name}.rmsp"));
            let t = Instant::now();
            write(&file, &preview)?;
            let write_s = t.elapsed().as_secs_f64();
            let t = Instant::now();
            let back = read(&file, Cancel::NEVER)?;
            let read_s = t.elapsed().as_secs_f64();
            let size = std::fs::metadata(&file)?.len() as f64 / 1e6;
            sizes.push(size);
            let image = back.image();
            let reduced = develop::preview(
                &*develop::quality::recovered(&original, Cancel::NEVER)?,
                LONG_EDGE,
            );
            let format0 = camera_difference(&reduced, image, 0.);
            let format4 = camera_difference(&reduced, image, 4.);
            let compare = |edit: &dyn Fn(&mut Recipe)| -> Result<[f64; 3]> {
                let mut r = Recipe::for_metadata(&original.metadata);
                edit(&mut r);
                let r = r.checked()?;
                let a = develop::render(&original, &r, LONG_EDGE, Cancel::NEVER)?;
                let b = develop::render(image, &r, LONG_EDGE, Cancel::NEVER)?;
                Ok(difference(
                    (&a.pixels, a.width, a.height),
                    (&b.pixels, b.width, b.height),
                ))
            };
            let off = |r: &mut Recipe| {
                r.sharpening = 0.;
                r.noise_luma = 0.;
                r.noise_chroma = 0.;
            };
            let default = compare(&|_| {})?;
            let detail_off = compare(&off)?;
            let up = compare(&|r| {
                off(r);
                r.exposure += 4.;
            })?;
            let down = compare(&|r| {
                off(r);
                r.exposure -= 3.;
            })?;
            let f = |d: [f64; 3]| format!("{:.3}/{:.2}/{:.1}", d[0], d[1], d[2]);
            Ok(format!(
                "| {name} | {} {} | {}×{} | {}×{} | {size:.2} | {build_s:.2} | {write_s:.2} | {read_s:.2} | {}, {} | {} | {} | {} | {} |",
                original.metadata.make,
                original.metadata.model,
                original.width,
                original.height,
                image.width,
                image.height,
                f(format0),
                f(format4),
                f(default),
                f(detail_off),
                f(up),
                f(down),
            ))
        })();
        match result {
            Ok(row) => println!("{row}"),
            Err(e) => println!("| {name} | failed: {e:#} |"),
        }
    }
    if !sizes.is_empty() {
        sizes.sort_by(f64::total_cmp);
        println!(
            "\n{} files: mean {:.2} MB, median {:.2} MB, largest {:.2} MB",
            sizes.len(),
            sizes.iter().sum::<f64>() / sizes.len() as f64,
            sizes[sizes.len() / 2],
            sizes[sizes.len() - 1]
        );
    }
}
