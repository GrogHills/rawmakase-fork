//! One channel of a Laplacian pyramid: the 5-tap binomial reduce and expand that
//! Texture and luminance noise reduction share, and the photo's level a reduced
//! decode's pyramid starts at, which colour noise reduction uses too.
use crate::camera_data::CameraImage;
use rawmakase_cancel::Cancel;
use rayon::prelude::*;

/// The photo's pyramid level that `im`'s finest level shows, `−log₂` of its
/// [`photo_scale`](CameraImage::photo_scale): 0 for a photo decoded in full, 1 for a
/// half-size decode. Within [`WHOLE_LEVEL`] of a whole level it is that level: a
/// half-size decode of a photo with an odd side is a pixel more than half of it
/// (`demosaic::half` rounds up), and its levels are still the photo's whole ones.
pub(crate) fn photo_level_offset(im: &CameraImage) -> f32 {
    let offset = -im.photo_scale().log2();
    let whole = offset.round();
    if (offset - whole).abs() < WHOLE_LEVEL {
        whole
    } else {
        offset
    }
}
/// How far from a whole level [`photo_level_offset`] still takes it: one pixel of
/// rounding on a long edge of 72 pixels or more is less.
const WHOLE_LEVEL: f32 = 0.02;

/// How much of a level a level wide, starting at the photo's fractional level
/// `photo_level`, lies within the photo's first `levels` levels, the ones its noise
/// reduction filters: 1 for a level wholly within them, 0 for one that starts at their
/// end or past it, and between them the share inside. A reduced decode's last partial
/// level blends its filtered detail with its unfiltered detail by this share, so how
/// much of it is filtered changes smoothly with the decode's size. For a photo decoded
/// in full every level is whole, so the share is exactly 1 or exactly 0.
pub(crate) fn filtered_share(photo_level: f32, levels: usize) -> f32 {
    (levels as f32 - photo_level).clamp(0., 1.)
}
/// `gain`, a level's detail gain, applied to only `share` of its detail: the rest is
/// kept unfiltered. Exactly `gain` at a share of 1.
pub(crate) fn share_gain(gain: f32, share: f32) -> f32 {
    if share < 1. {
        1. - share * (1. - gain)
    } else {
        gain
    }
}

/// One channel at one pyramid level.
pub(crate) struct Plane {
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) data: Vec<f32>,
}
/// The 5-tap binomial filter, and its expanding weights for even and odd samples.
const TAPS: [f32; 5] = [1. / 16., 4. / 16., 6. / 16., 4. / 16., 1. / 16.];
const EVEN: [f32; 3] = [TAPS[0] * 2., TAPS[2] * 2., TAPS[4] * 2.];
const ODD: f32 = TAPS[1] * 2.;
/// `v` reflected into `0..n` about its ends, without repeating them.
pub(crate) fn mirror(v: isize, n: usize) -> usize {
    let n = n as isize;
    let v = v.abs();
    (if v >= n { 2 * n - 2 - v } else { v }).clamp(0, n - 1) as usize
}
impl Plane {
    /// Blurred and halved. Rows started after `cancel` is set are left at 0, so a
    /// caller that can cancel checks it before using the result.
    pub(crate) fn down(&self, cancel: Cancel<'_>) -> Self {
        let (w, h) = (self.w.div_ceil(2), self.h.div_ceil(2));
        let mut wide = vec![0.; w * self.h];
        wide.par_chunks_mut(w)
            .zip(self.data.par_chunks(self.w))
            .for_each(|(out, row)| {
                if cancel.is_cancelled() {
                    return;
                }
                for (x, o) in out.iter_mut().enumerate() {
                    *o = (0..5)
                        .map(|k| row[mirror(2 * x as isize + k - 2, self.w)] * TAPS[k as usize])
                        .sum();
                }
            });
        let mut data = vec![0.; w * h];
        data.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
            if cancel.is_cancelled() {
                return;
            }
            let rows: [&[f32]; 5] = std::array::from_fn(|k| {
                let r = mirror(2 * y as isize + k as isize - 2, self.h);
                &wide[r * w..(r + 1) * w]
            });
            for (x, o) in out.iter_mut().enumerate() {
                *o = (0..5).map(|k| rows[k][x] * TAPS[k]).sum();
            }
        });
        Plane { w, h, data }
    }
    /// Doubled to `w` × `h` and blurred, as a Laplacian pyramid expands a level. Rows
    /// started after `cancel` is set are left at 0, as in [`Plane::down`].
    pub(crate) fn up(&self, w: usize, h: usize, cancel: Cancel<'_>) -> Self {
        let expand = |i: usize, at: &dyn Fn(isize) -> f32| -> f32 {
            let c = (i / 2) as isize;
            if i.is_multiple_of(2) {
                at(c - 1) * EVEN[0] + at(c) * EVEN[1] + at(c + 1) * EVEN[2]
            } else {
                (at(c) + at(c + 1)) * ODD
            }
        };
        let mut wide = vec![0.; w * self.h];
        wide.par_chunks_mut(w)
            .zip(self.data.par_chunks(self.w))
            .for_each(|(out, row)| {
                if cancel.is_cancelled() {
                    return;
                }
                for (x, o) in out.iter_mut().enumerate() {
                    *o = expand(x, &|c| row[mirror(c, self.w)]);
                }
            });
        let mut data = vec![0.; w * h];
        data.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
            if cancel.is_cancelled() {
                return;
            }
            let row = |c: isize| {
                let r = mirror(c, self.h);
                &wide[r * w..(r + 1) * w]
            };
            for (x, o) in out.iter_mut().enumerate() {
                *o = expand(y, &|c| row(c)[x]);
            }
        });
        Plane { w, h, data }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A photo decoded in full (offset 0) or by a whole number of levels, as a
    /// half-size decode is, has each level wholly inside the photo's filtered levels
    /// or wholly past them, and [`share_gain`] then leaves every gain exactly as it is:
    /// their noise reduction is bit for bit what it was before partial levels were
    /// blended.
    #[test]
    fn whole_levels_are_filtered_in_full_or_not_at_all() {
        // Luminance filters five bands, colour five levels.
        let levels = 5;
        for offset in [0., 1., 2.] {
            for level in 0..10 {
                let photo_level = level as f32 + offset;
                let share = filtered_share(photo_level, levels);
                let expected: f32 = if photo_level < levels as f32 { 1. } else { 0. };
                assert_eq!(share.to_bits(), expected.to_bits(), "{photo_level}");
            }
        }
        for gain in [0., 1e-7, 0.3, 0.999_999, 1.] {
            assert_eq!(share_gain(gain, 1.).to_bits(), gain.to_bits());
        }
    }

    /// A level that starts inside the photo's last filtered level and ends past it
    /// is filtered on the share of it inside, which shrinks to nothing as it
    /// approaches the photo's unfiltered residual.
    #[test]
    fn a_partial_level_is_filtered_on_its_share_inside_the_photo_s_levels() {
        assert!((filtered_share(4.97, 5) - 0.03).abs() < 1e-5);
        assert!((filtered_share(4.25, 5) - 0.75).abs() < 1e-6);
        assert_eq!(filtered_share(5., 5), 0.);
        assert_eq!(filtered_share(3.99, 5), 1.);
        // A quarter of the detail filtered to a gain of 0.2, the rest kept.
        assert!((share_gain(0.2, 0.25) - 0.8).abs() < 1e-6);
        assert_eq!(share_gain(0.2, 0.), 1.);
    }
}
