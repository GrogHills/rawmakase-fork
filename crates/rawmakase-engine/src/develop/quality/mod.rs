//! Full-resolution detail processing shared by Fit, 100% regions and exports.
use crate::develop::masks::{MaskWeights, local::slot};
use crate::develop::sharpening::Sharpener;
use crate::develop::{
    pipeline::Toned,
    preview_renderer::Stages,
    stage_cache::{StageCache, TextureKey},
};
use crate::rendered::Rendered;
use crate::{
    camera_data::CameraImage,
    develop::{self, Geometry},
    model::{recipe::Recipe, valid::ValidRecipe},
};
use anyhow::{Result, ensure};
use rawmakase_cancel::Cancel;
use rayon::prelude::*;
use std::sync::Arc;

fn check_cancel(cancel: Cancel<'_>) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Render superseded");
    Ok(())
}

pub fn fit_edge(width: u32, height: u32, viewport: [u32; 2]) -> u32 {
    let scale = (viewport[0].max(1) as f64 / width as f64)
        .min(viewport[1].max(1) as f64 / height as f64)
        .min(1.);
    ((width.max(height) as f64 * scale).round() as u32).max(1)
}
pub(crate) fn output_size(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    if max_edge == 0 || width.max(height) <= max_edge {
        return (width, height);
    }
    let scale = max_edge as f64 / width.max(height) as f64;
    (
        (width as f64 * scale).round().max(1.) as u32,
        (height as f64 * scale).round().max(1.) as u32,
    )
}
/// A preview render: pixels on the CPU, or a frame presented into a texture on the
/// GPU when a display was given.
pub enum Output {
    Pixels(Rendered),
    Frame(Box<develop::gpu::Frame>),
}
impl Output {
    /// The pixels of a render made without a display, which is never a frame.
    pub fn pixels(self) -> Rendered {
        match self {
            Output::Pixels(pixels) => pixels,
            Output::Frame(_) => unreachable!("Frames are only presented to a display"),
        }
    }
}
pub fn resize(image: Rendered, max_edge: u32) -> Rendered {
    resize_inner(image, max_edge, true)
}
/// [`resize`] without clipping at 1: for an HDR master's encoded values, which its
/// caller bounds. `image`'s resize clips float samples to 0–1, so values above 1 are
/// scaled into that range first, with room for Lanczos's overshoot, and back after;
/// values at most 1 (every SDR render's) are resized as they are.
pub(crate) fn resize_unbounded(image: Rendered, max_edge: u32) -> Rendered {
    resize_inner(image, max_edge, false)
}

fn sinc(t: f32) -> f32 {
    if t == 0. {
        1.
    } else {
        let a = t * std::f32::consts::PI;
        a.sin() / a
    }
}

fn lanczos_taps(input: usize, output: usize) -> (Vec<(usize, usize, usize)>, Vec<f32>) {
    let ratio = input as f32 / output as f32;
    let scale = ratio.max(1.);
    let support = 3. * scale;
    let max_taps = (2. * support).ceil() as usize + 2;
    let mut spans = Vec::with_capacity(output);
    let mut weights = Vec::with_capacity(output * max_taps);
    for out in 0..output {
        let center = (out as f32 + 0.5) * ratio;
        let start = ((center - support).floor() as isize).clamp(0, input as isize - 1) as usize;
        let end =
            ((center + support).ceil() as isize).clamp(start as isize + 1, input as isize) as usize;
        let w_start = weights.len();
        let center = center - 0.5;
        let mut sum = 0.;
        for i in start..end {
            let v = (i as f32 - center) / scale;
            let w = if v.abs() < 3. {
                sinc(v) * sinc(v / 3.)
            } else {
                0.
            };
            weights.push(w);
            sum += w;
        }
        let len = end - start;
        for w in &mut weights[w_start..w_start + len] {
            *w /= sum;
        }
        spans.push((start, w_start, len));
    }
    (spans, weights)
}

fn resize_inner(mut image: Rendered, max_edge: u32, clamp01: bool) -> Rendered {
    if max_edge == 0 || image.width.max(image.height) <= max_edge {
        if clamp01 {
            image.pixels.par_iter_mut().for_each(|p| {
                *p = p.map(|v| v.clamp(0., 1.));
            });
        }
        return image;
    }
    let (w, h) = output_size(image.width, image.height, max_edge);
    if (w, h) == (image.width, image.height) {
        if clamp01 {
            image.pixels.par_iter_mut().for_each(|p| {
                *p = p.map(|v| v.clamp(0., 1.));
            });
        }
        return image;
    }
    let (sw, sh) = (image.width as usize, image.height as usize);
    let (dw, dh) = (w as usize, h as usize);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || image.pixels.len() != sw * sh {
        return Rendered {
            width: w,
            height: h,
            pixels: vec![[0.; 3]; dw * dh],
        };
    }
    let top = image
        .pixels
        .par_iter()
        .flatten()
        .copied()
        .reduce(|| 0f32, f32::max);
    let scale = if top > 1. { 2. * top } else { 1. };
    let ((y_spans, y_weights), (x_spans, x_weights)) =
        rayon::join(|| lanczos_taps(sh, dh), || lanczos_taps(sw, dw));
    // Vertical pass first (matching `image`'s pass order, unclamped intermediate) into `sw × dh`.
    let mut tmp = vec![[0f32; 3]; sw * dh];
    tmp.par_chunks_mut(sw).enumerate().for_each(|(y, dst_row)| {
        let (start, w_start, len) = y_spans[y];
        let row_weights = &y_weights[w_start..w_start + len];
        for (k, &weight) in row_weights.iter().enumerate() {
            let src_row = &image.pixels[(start + k) * sw..(start + k + 1) * sw];
            for (dst, src) in dst_row.iter_mut().zip(src_row) {
                if scale == 1. {
                    dst[0] += src[0] * weight;
                    dst[1] += src[1] * weight;
                    dst[2] += src[2] * weight;
                } else {
                    dst[0] += (src[0] / scale) * weight;
                    dst[1] += (src[1] / scale) * weight;
                    dst[2] += (src[2] / scale) * weight;
                }
            }
        }
    });
    // Horizontal pass into `dw × dh`, clamping to [0, 1] before restoring `scale`.
    let mut pixels = vec![[0f32; 3]; dw * dh];
    pixels
        .par_chunks_mut(dw)
        .zip(tmp.par_chunks(sw))
        .for_each(|(dst_row, src_row)| {
            for (x, &(start, w_start, len)) in x_spans.iter().enumerate() {
                let col_weights = &x_weights[w_start..w_start + len];
                let mut sum = [0f32; 3];
                for (src, &weight) in src_row[start..start + len].iter().zip(col_weights) {
                    sum[0] += src[0] * weight;
                    sum[1] += src[1] * weight;
                    sum[2] += src[2] * weight;
                }
                let clamped = sum.map(|v| v.clamp(0., 1.));
                let unscaled = if scale == 1. {
                    clamped
                } else {
                    [clamped[0] * scale, clamped[1] * scale, clamped[2] * scale]
                };
                dst_row[x] = if clamp01 {
                    unscaled.map(|v| v.clamp(0., 1.))
                } else {
                    unscaled
                };
            }
        });
    Rendered {
        width: w,
        height: h,
        pixels,
    }
}

mod detail;
mod hdr;
mod highlights;
mod local;
mod prepare;
mod render;
mod samples;
pub(crate) use detail::*;
pub use hdr::{render_hdr, render_hdr_region};
pub use highlights::*;
pub(crate) use local::*;
pub(crate) use prepare::{Resolved, resolve};
pub use render::*;
pub use samples::*;

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    #[test]
    fn cancelled_highlight_recovery_keeps_nothing() {
        let cancel = AtomicBool::new(true);
        let image = fixture();
        assert!(recover_highlights(&image, Cancel::new(&cancel)).is_err());
        assert!(image.recovered.get().is_none());
    }
    pub(super) fn fixture() -> CameraImage {
        CameraImage {
            width: 96,
            height: 80,
            pixels: Arc::new(
                (0..96 * 80)
                    .map(|i| {
                        let y = (i % 96) as f32 / 192. + 0.05;
                        [y * 0.8, y, y * 0.6]
                    })
                    .collect(),
            ),
            metadata: crate::camera_data::Metadata {
                width: 96,
                height: 80,
                matrix: [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]],
                wb: [1.; 3],
                ..Default::default()
            },
            fast: false,
            scale_factor: 1.,
            scale_clipped: 0,
            hdr: Default::default(),
            recovered: Default::default(),
        }
    }
    #[test]
    fn preset_effects_match_tiles_and_survive_serialization() -> Result<()> {
        let im = fixture();
        let mut r = Recipe::default();
        r.effects.grain = 0.5;
        r.effects.vignette = -0.3;
        r.effects.clarity = 0.4;
        r.effects.texture = -0.2;
        r.effects.channels[0].insert([0.4, 0.5]);
        r.effects.calibration[2] = [0.2, -0.1];
        r.effects.parametric = [0.1, -0.2, 0.1, 0.];
        let saved = serde_json::to_vec(&r)?;
        let restored: Recipe = serde_json::from_slice(&saved)?;
        assert_eq!(r, restored);
        let full = render(&im, &r.checked()?, 0, None, Cancel::NEVER)?;
        let tile = render(&im, &r.checked()?, 0, Some([30, 25, 40, 40]), Cancel::NEVER)?;
        for y in 0..40 {
            for x in 0..40 {
                let a = tile.pixels[y * 40 + x];
                let b = full.pixels[(y + 25) * 96 + x + 30];
                for c in 0..3 {
                    assert!((a[c] - b[c]).abs() < 2e-6);
                }
            }
        }
        assert_eq!(
            full.pixels,
            render(&im, &r.checked()?, 0, None, Cancel::NEVER)?.pixels
        );
        assert_ne!(
            full.pixels,
            render(&im, &Recipe::default().checked()?, 0, None, Cancel::NEVER)?.pixels
        );
        Ok(())
    }
    #[test]
    fn region_matches_full_with_large_radius_and_local_tones() -> Result<()> {
        let im = fixture();
        // Every scene stage control: a region measures the whole photo.
        let mut r = Recipe {
            sharpening_radius: 3.,
            sharpening: 0.8,
            shadows: 0.5,
            highlights: -0.4,
            whites: 0.5,
            blacks: -0.3,
            ..Default::default()
        };
        r.effects.clarity = -0.4;
        r.effects.texture = 0.3;
        // Negative Dehaze from its tables, positive from the photo's haze.
        for dehaze in [-0.3, 0.6] {
            r.effects.dehaze = dehaze;
            let full = render(&im, &r.checked()?, 0, None, Cancel::NEVER)?;
            for [x, y, w, h] in [[0, 0, 20, 30], [30, 25, 40, 40], [80, 60, 16, 20]] {
                let tile = render(&im, &r.checked()?, 0, Some([x, y, w, h]), Cancel::NEVER)?;
                for yy in 0..h {
                    for xx in 0..w {
                        let a = tile.pixels[(yy * w + xx) as usize];
                        let b = full.pixels[((yy + y) * full.width + xx + x) as usize];
                        for c in 0..3 {
                            assert!((a[c] - b[c]).abs() < 2e-6, "{dehaze}");
                        }
                    }
                }
            }
        }
        Ok(())
    }
    #[test]
    fn final_fit_is_export_resized_after_detail() -> Result<()> {
        let im = fixture();
        let r = Recipe {
            sharpening: 0.8,
            ..Default::default()
        };
        let full = render(&im, &r.checked()?, 0, None, Cancel::NEVER)?;
        let expected = resize(full, 48);
        let fit = render(&im, &r.checked()?, 48, None, Cancel::NEVER)?;
        assert_eq!(fit.pixels, expected.pixels);
        Ok(())
    }
    #[test]
    fn cancellation_does_not_publish_partial_frame() {
        let im = fixture();
        assert!(
            render(
                &im,
                &Recipe::default().checked().unwrap(),
                0,
                None,
                Cancel::new(&std::sync::atomic::AtomicBool::new(true))
            )
            .is_err()
        );
    }
    #[test]
    fn sharpening_increases_edge_contrast_without_tint() {
        let mut im = Rendered {
            width: 32,
            height: 32,
            pixels: (0..1024)
                .map(|i| [if i % 32 < 16 { 0.3 } else { 0.6 }; 3])
                .collect(),
        };
        sharpen(&mut im, &Recipe::default(), None, Cancel::NEVER).unwrap();
        assert!(im.pixels[15][0] < 0.3);
        assert!(im.pixels[16][0] > 0.6);
        assert!(im.pixels.iter().all(|p| p[0] == p[1] && p[1] == p[2]));
    }
    #[test]
    fn physical_fit_size_has_no_fixed_ceiling() {
        assert_eq!(fit_edge(6000, 4000, [3000, 2000]), 3000);
        assert_eq!(fit_edge(6000, 4000, [1000, 2000]), 1000);
    }
    #[test]
    fn sharpening_preserves_flat_fields() {
        let mut im = Rendered {
            width: 20,
            height: 20,
            pixels: vec![[0.4, 0.3, 0.2]; 400],
        };
        let before = im.pixels.clone();
        sharpen(&mut im, &Recipe::default(), None, Cancel::NEVER).unwrap();
        for (a, b) in im.pixels.iter().zip(before) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn lanczos_resize_matches_image_crate_across_dimensions() {
        for (w, h) in [(97u32, 63u32), (1, 33), (33, 1), (1, 1), (157, 103)] {
            let src = Rendered {
                width: w,
                height: h,
                pixels: (0..w * h)
                    .map(|i| {
                        let x = (i % w) as f32 / w as f32;
                        let y = (i / w) as f32 / h as f32;
                        [x, y, if i % 7 < 3 { 0.03 } else { 0.96 }]
                    })
                    .collect(),
            };
            for max_edge in [0u32, 1, 17, 48, 90] {
                let actual = resize(src.clone(), max_edge);
                let (ow, oh) = output_size(w, h, max_edge);
                assert_eq!((actual.width, actual.height), (ow, oh));
                let expected = if max_edge == 0 || w.max(h) <= max_edge {
                    src.pixels.clone()
                } else {
                    let buf = image::Rgb32FImage::from_raw(
                        w,
                        h,
                        src.pixels.iter().flatten().copied().collect(),
                    )
                    .unwrap();
                    let resized = image::imageops::resize(
                        &buf,
                        ow,
                        oh,
                        image::imageops::FilterType::Lanczos3,
                    );
                    resized
                        .into_raw()
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .map(|p| p.map(|v| v.clamp(0., 1.)))
                        .collect()
                };
                let max_err = actual
                    .pixels
                    .iter()
                    .flatten()
                    .zip(expected.iter().flatten())
                    .map(|(a, b)| (a - b).abs())
                    .fold(0f32, f32::max);
                assert!(max_err < 1e-5, "{w}x{h} edge={max_edge}: {max_err}");
            }
        }
    }

    #[test]
    fn resize_unbounded_clips_signed_hdr_channels_like_image_crate() {
        let signed = Rendered {
            width: 2,
            height: 1,
            pixels: vec![[-0.25, 2.0, 0.5], [-0.25, 2.0, 0.5]],
        };
        let actual = resize_unbounded(signed.clone(), 1);
        assert_eq!((actual.width, actual.height), (1, 1));
        let scale = 4.0f32;
        let buf = image::Rgb32FImage::from_raw(
            2,
            1,
            signed.pixels.iter().flatten().map(|&v| v / scale).collect(),
        )
        .unwrap();
        let expected: Vec<f32> =
            image::imageops::resize(&buf, 1, 1, image::imageops::FilterType::Lanczos3)
                .into_raw()
                .into_iter()
                .map(|v| v * scale)
                .collect();
        assert_eq!(actual.pixels[0], [expected[0], expected[1], expected[2]]);
        assert_eq!(actual.pixels[0][0], 0.0);

        let large = Rendered {
            width: 37,
            height: 19,
            pixels: (0..37 * 19)
                .map(|i| {
                    let t = (i % 13) as f32 / 6.0 - 0.5;
                    [t, t * 2.5 + 0.5, 1.8 - t]
                })
                .collect(),
        };
        let top = large.pixels.iter().flatten().copied().fold(0f32, f32::max);
        let scale = if top > 1. { 2. * top } else { 1. };
        let actual = resize_unbounded(large.clone(), 16);
        let (ow, oh) = output_size(37, 19, 16);
        let buf = image::Rgb32FImage::from_raw(
            37,
            19,
            large.pixels.iter().flatten().map(|&v| v / scale).collect(),
        )
        .unwrap();
        let expected: Vec<f32> =
            image::imageops::resize(&buf, ow, oh, image::imageops::FilterType::Lanczos3)
                .into_raw()
                .into_iter()
                .map(|v| v * scale)
                .collect();
        for (a, b) in actual.pixels.iter().flatten().zip(&expected) {
            assert!((a - b).abs() < 1e-5, "{a} vs {b}");
        }
    }

    #[test]
    fn resize_zero_axis_preserves_output_pixel_count() {
        let out = resize(
            Rendered {
                width: 0,
                height: 4,
                pixels: Vec::new(),
            },
            2,
        );
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(out.pixels, vec![[0.; 3]; 2]);
    }
}
