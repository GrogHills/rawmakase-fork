//! An editable preview's file: a DNG 1.7 linear raw (PhotometricInterpretation
//! 34892) of three 16-bit samples a pixel in lossless JPEG XL tiles
//! (Compression 52546), which RAWmakase's DNG reader opens as it opens any.
//!
//! Integer linear raw develops as `(sample − black) / (white − black)` times the
//! white balance, with no clamp at 1, so a WhiteLevel below 65535 keeps
//! highlights above white. [`WHITE`] keeps 2 stops: measured on 13 RAWs of 9
//! makes (pch/rawmakase#1022), none clipped renders at +4 or −3 EV, where no
//! headroom clipped the brightest ones and 4 stops started to show the loss of
//! precision under white. Floating-point samples would be clamped at white by
//! the reader, so they are not used.
//!
//! The standard tags carry what another DNG reader needs: the size, the colour
//! matrix, white balance, orientation, the default crop, the basic EXIF and
//! Adobe's original-size tags (51089–51091). RAWmakase's own facts are in
//! DNGPrivateData ([`super::record`]).
use anyhow::{Result, ensure};
use rawmakase_model::camera_data::{CameraImage, Metadata};
use rayon::prelude::*;

/// The sample the sensor's white is written as: 2 stops of headroom.
pub(super) const WHITE: u16 = 16383;
/// Tiles of 512 × 512 pixels, coded in parallel; the edge tiles are smaller.
const TILE: u32 = 512;
/// DNG 1.7's compression code for JPEG XL.
const JPEG_XL: u16 = 52546;
/// PhotometricInterpretation's linear raw.
const LINEAR_RAW: u16 = 34892;
/// The XYZ (D65) to linear sRGB matrix, written as the colour matrix for other
/// readers when the original's decoder had none: such a photo renders from its
/// fitted Camera Look in RAWmakase, which takes its facts from the record.
const XYZ_TO_SRGB: [[f32; 3]; 3] = [
    [3.240_454_2, -1.537_138_5, -0.498_531_4],
    [-0.969_266, 1.876_010_8, 0.041_556],
    [0.055_643_4, -0.204_025_9, 1.057_225_2],
];

/// A TIFF entry: tag, type, count and its value's bytes, little-endian.
struct Entry(u16, u16, u32, Vec<u8>);

fn shorts(tag: u16, v: &[u16]) -> Entry {
    Entry(
        tag,
        3,
        v.len() as u32,
        v.iter().flat_map(|v| v.to_le_bytes()).collect(),
    )
}
fn longs(tag: u16, v: &[u32]) -> Entry {
    Entry(
        tag,
        4,
        v.len() as u32,
        v.iter().flat_map(|v| v.to_le_bytes()).collect(),
    )
}
fn ascii(tag: u16, s: &str) -> Entry {
    let mut b: Vec<u8> = s.bytes().filter(|b| *b != 0).collect();
    b.push(0);
    Entry(tag, 2, b.len() as u32, b)
}
fn bytes(tag: u16, v: &[u8]) -> Entry {
    Entry(tag, 1, v.len() as u32, v.to_vec())
}
/// Unsigned rationals over 2^24: 1 in 16 million, values up to 256.
fn rationals(tag: u16, v: &[f64]) -> Entry {
    let d = 1u32 << 24;
    let b = v
        .iter()
        .flat_map(|v| {
            let n = (v.max(0.) * f64::from(d)).round().min(f64::from(u32::MAX)) as u32;
            [n.to_le_bytes(), d.to_le_bytes()].concat()
        })
        .collect();
    Entry(tag, 5, v.len() as u32, b)
}
/// Signed rationals over 10^6.
fn srationals(tag: u16, v: &[f64]) -> Entry {
    let d = 1_000_000i32;
    let b = v
        .iter()
        .flat_map(|v| {
            let n = (v * f64::from(d))
                .round()
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
            [n.to_le_bytes(), d.to_le_bytes()].concat()
        })
        .collect();
    Entry(tag, 10, v.len() as u32, b)
}

/// The IFD of `entries` at byte `at` of the file, followed by the values that
/// don't fit in an entry; no next IFD.
fn ifd(mut entries: Vec<Entry>, at: u32) -> Result<Vec<u8>> {
    entries.sort_by_key(|e| e.0);
    let head = 2 + 12 * entries.len() + 4;
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    let mut values = Vec::new();
    for Entry(tag, kind, count, value) in &entries {
        out.extend(tag.to_le_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(count.to_le_bytes());
        if value.len() <= 4 {
            let mut v = value.clone();
            v.resize(4, 0);
            out.extend(v);
        } else {
            let offset = u32::try_from(at as usize + head + values.len())?;
            out.extend(offset.to_le_bytes());
            values.extend(value);
            if values.len() % 2 == 1 {
                values.push(0);
            }
        }
    }
    out.extend(0u32.to_le_bytes());
    out.extend(values);
    Ok(out)
}

/// TIFF's Orientation for RAWmakase's `flip`, the inverse of the DNG reader's.
fn orientation(flip: i32) -> u16 {
    match flip {
        1 => 2,
        3 => 3,
        2 => 4,
        4 => 5,
        6 => 6,
        7 => 7,
        5 => 8,
        _ => 1,
    }
}

/// One tile of `codes` (three samples a pixel, `width` pixels a row) as a lossless
/// 16-bit JPEG XL codestream.
fn tile(codes: &[u16], width: u32, [x0, y0, w, h]: [u32; 4]) -> Result<Vec<u8>> {
    use zune_core::{bit_depth::BitDepth, colorspace::ColorSpace, options::EncoderOptions};
    let mut samples = Vec::with_capacity((w * h * 3 * 2) as usize);
    for y in y0..y0 + h {
        let row = &codes[((y * width + x0) * 3) as usize..][..(w * 3) as usize];
        samples.extend(row.iter().flat_map(|s| s.to_ne_bytes()));
    }
    let options = EncoderOptions::new(w as usize, h as usize, ColorSpace::RGB, BitDepth::Sixteen);
    let mut out = Vec::new();
    zune_jpegxl::JxlSimpleEncoder::new(&samples, options)
        .encode(&mut out)
        .map_err(|e| anyhow::anyhow!("JPEG XL encoding failed: {e:?}"))?;
    Ok(out)
}

/// The samples of `reduced`, a recovered image in its camera space, white
/// balanced by `wb` with 1 the sensor's white: each value divided by its white
/// balance (DNG's samples are before it), [`WHITE`] at white, rounded.
pub(super) fn samples(reduced: &CameraImage, wb: [f32; 3]) -> Vec<u16> {
    reduced
        .pixels
        .par_iter()
        .flat_map_iter(|p| {
            (0..3).map(move |c| (p[c] / wb[c] * f32::from(WHITE)).round().clamp(0., 65535.) as u16)
        })
        .collect()
}

/// The white balance the samples are divided by: the original's, kept away
/// from 0.
pub(super) fn white_balance(original: &Metadata) -> [f32; 3] {
    original.wb.map(|v| if v > 1e-6 { v } else { 1. })
}

/// The DNG of `reduced` (a recovered image of the photo `original` describes,
/// in its camera space, white balanced, 1 the sensor's white), with `private`
/// as its DNGPrivateData.
pub(super) fn file(reduced: &CameraImage, original: &Metadata, private: &[u8]) -> Result<Vec<u8>> {
    let (w, h) = (reduced.width, reduced.height);
    ensure!(
        w >= 2 && h >= 2 && reduced.pixels.len() == w as usize * h as usize,
        "A photo this small can't have an editable preview"
    );
    let wb = white_balance(original);
    let codes = samples(reduced, wb);
    let mut boxes = Vec::new();
    for y in (0..h).step_by(TILE as usize) {
        for x in (0..w).step_by(TILE as usize) {
            boxes.push([x, y, TILE.min(w - x), TILE.min(h - y)]);
        }
    }
    let tiles: Vec<Vec<u8>> = boxes
        .par_iter()
        .map(|b| tile(&codes, w, *b))
        .collect::<Result<_>>()?;
    // The header, then the tiles, the EXIF IFD and IFD0.
    let mut file = b"II*\0\0\0\0\0".to_vec();
    let mut offsets = Vec::new();
    for t in &tiles {
        offsets.push(u32::try_from(file.len())?);
        file.extend(t);
        if file.len() % 2 == 1 {
            file.push(0);
        }
    }
    let positive = |v: f32| f64::from(if v.is_finite() { v.max(0.) } else { 0. });
    let mut exif = vec![
        rationals(33434, &[positive(original.shutter)]),
        rationals(33437, &[positive(original.aperture)]),
        shorts(34855, &[original.iso.clamp(0., 65535.) as u16]),
        rationals(37386, &[positive(original.focal)]),
    ];
    if original.focal_35mm > 0. {
        exif.push(shorts(
            41989,
            &[original.focal_35mm.clamp(0., 65535.) as u16],
        ));
    }
    if !original.lens_model.is_empty() {
        exif.push(ascii(42036, &original.lens_model));
    }
    let exif_at = u32::try_from(file.len())?;
    file.extend(ifd(exif, exif_at)?);
    let ifd0_at = u32::try_from(file.len())?;
    let s = f64::from(w) / f64::from(original.width.max(1));
    // The default crop, when the decoder gave one inside the frame; the whole
    // frame otherwise, as DNG's defaults have it.
    let [left, top, crop_w, crop_h] = [
        original.crop_left,
        original.crop_top,
        original.crop_width,
        original.crop_height,
    ];
    let crop = if crop_w > 0
        && crop_h > 0
        && left
            .checked_add(crop_w)
            .is_some_and(|r| r <= original.width)
        && top
            .checked_add(crop_h)
            .is_some_and(|b| b <= original.height)
    {
        [left, top, crop_w, crop_h]
    } else {
        [0, 0, original.width, original.height]
    };
    let matrix = if original.cam_xyz.iter().flatten().any(|v| *v != 0.) {
        original.cam_xyz
    } else {
        XYZ_TO_SRGB
    };
    let mut entries = vec![
        longs(254, &[0]),
        longs(256, &[w]),
        longs(257, &[h]),
        shorts(258, &[16, 16, 16]),
        shorts(259, &[JPEG_XL]),
        shorts(262, &[LINEAR_RAW]),
        ascii(271, &original.make),
        ascii(272, &original.model),
        shorts(274, &[orientation(original.flip)]),
        shorts(277, &[3]),
        shorts(284, &[1]),
        ascii(305, "RAWmakase"),
        longs(322, &[TILE]),
        longs(323, &[TILE]),
        longs(324, &offsets),
        longs(
            325,
            &tiles
                .iter()
                .map(|t| u32::try_from(t.len()))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        shorts(339, &[1, 1, 1]),
        longs(34665, &[exif_at]),
        bytes(50706, &[1, 7, 0, 0]),
        bytes(50707, &[1, 7, 0, 0]),
        ascii(50708, &format!("{} {}", original.make, original.model)),
        longs(50714, &[0, 0, 0]),
        longs(50717, &[u32::from(WHITE); 3]),
        rationals(50719, &[f64::from(crop[0]) * s, f64::from(crop[1]) * s]),
        rationals(50720, &[f64::from(crop[2]) * s, f64::from(crop[3]) * s]),
        srationals(
            50721,
            &matrix
                .iter()
                .flatten()
                .map(|v| f64::from(*v))
                .collect::<Vec<_>>(),
        ),
        rationals(50728, &wb.map(|v| 1. / f64::from(v))),
        bytes(50740, private),
        shorts(50778, &[21]),
        // The tags Adobe added for Smart Previews: the original's sizes.
        longs(51089, &[crop[2], crop[3]]),
        longs(51090, &[original.width, original.height]),
        rationals(51091, &[f64::from(crop[2]), f64::from(crop[3])]),
    ];
    if let Some(b) = original.baseline_exposure {
        entries.push(srationals(50730, &[f64::from(b)]));
    }
    file.extend(ifd(entries, ifd0_at)?);
    file[4..8].copy_from_slice(&ifd0_at.to_le_bytes());
    Ok(file)
}
