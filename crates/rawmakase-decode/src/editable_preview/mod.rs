//! Editable previews (docs/editable-previews.md): a reduced copy of a photo,
//! [`LONG_EDGE`] pixels on its long edge, built while its original is online
//! so that Develop can edit the photo while the original is offline. This
//! module owns the copy's file: [`write()`] makes it from the photo's full-size
//! decode, and [`open`] opens it as the photo again.
//!
//! The file is a DNG 1.7 linear raw of the image with highlights recovered,
//! in lossless JPEG XL tiles (`dng`), which RAWmakase's DNG reader opens
//! unchanged and another DNG reader can open too. The original's facts that
//! its standard tags carry only in part or only approximately are in
//! RAWmakase's own versioned record in DNGPrivateData (`record`).
//!
//! Opening a copy needs three things beyond the DNG reader, all in [`open`]:
//! the record's facts in place of the tags'; the image marked as a reduced
//! decode of a photo of the original's size ([`CameraImage::photo_scale`]); and
//! its highlights marked as recovered already
//! (`develop::quality::recovered`), since recovering a reduced copy again
//! would not give the original's.
//!
//! A copy that can't be opened as this release opens it (a record of a
//! version it doesn't know, no record, a file cut short or damaged) is an
//! error, never a panic; whoever opens it treats the copy as out of date.
mod dng;
mod record;
#[cfg(test)]
mod tests;

use crate::camera_data::{CameraImage, Decode, Demosaic};
use crate::storage::Identity;
use anyhow::{Context, Result, ensure};
use rawmakase_cancel::Cancel;
use rawmakase_model::camera_profiles::CameraProfile;
use record::Record;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

pub use record::VERSION;

/// The long edge editable previews are built at, as Lightroom Classic's Smart
/// Previews are.
pub const LONG_EDGE: u32 = 2560;

/// An editable preview, opened: the image Develop edits the photo from.
pub struct EditablePreview {
    /// The copy's pixels, a reduced decode ([`CameraImage::fast`]) of a photo of
    /// the original's size, with the original's metadata and its highlights
    /// recovered already.
    pub image: CameraImage,
    /// The original's identity when the copy was built.
    pub original: Identity,
    /// The photo's default Camera Look (fitted), for a camera without colour
    /// data: its fit needs the original, so offline it comes only from here.
    pub default_look: Option<CameraProfile>,
}

/// Writes the editable preview of `original`, the full-size decode of the file
/// `identity` names, to `path`, with the photo's `default_look` when it has one
/// (a fitted Camera Look, for a camera without colour data).
///
/// Highlights are recovered first unless they are already, as [`crate::decode`]
/// gives Develop the image, then it is reduced to [`LONG_EDGE`] (never
/// enlarged). Auto CA is measured on `original`, as every render of the photo
/// measures it, and kept. The file is written beside `path` under a staging
/// name and renamed into place only once complete, replacing a file there.
pub fn write(
    path: &Path,
    original: &CameraImage,
    identity: &Identity,
    default_look: Option<&CameraProfile>,
    cancel: Cancel<'_>,
) -> Result<()> {
    write_at(path, original, identity, default_look, LONG_EDGE, cancel)
}

/// [`write()`], reduced to `long_edge` pixels.
fn write_at(
    path: &Path,
    original: &CameraImage,
    identity: &Identity,
    default_look: Option<&CameraProfile>,
    long_edge: u32,
    cancel: Cancel<'_>,
) -> Result<()> {
    let bytes = build(original, identity, default_look, long_edge, cancel)?;
    ensure!(!cancel.is_cancelled(), "Editable preview cancelled");
    crate::storage::write_atomic(path, crate::storage::Replace::Overwrite, |file| {
        use std::io::Write as _;
        file.write_all(&bytes)?;
        Ok(())
    })
    .with_context(|| format!("Could not write the editable preview {}", path.display()))
}

/// The bytes of the editable preview [`write_at`] writes.
fn build(
    original: &CameraImage,
    identity: &Identity,
    default_look: Option<&CameraProfile>,
    long_edge: u32,
    cancel: Cancel<'_>,
) -> Result<Vec<u8>> {
    ensure!(
        !original.fast,
        "An editable preview is built from a full-size decode"
    );
    ensure!(
        original.width > 0
            && original.height > 0
            && original.pixels.len() == original.width as usize * original.height as usize,
        "An editable preview's original has no pixels"
    );
    ensure!(
        original.hdr().is_none(),
        "An editable preview can't keep an HDR file's light"
    );
    rawmakase_model::lens::auto_ca::prime(original);
    let recovered = crate::develop::quality::recovered(original, cancel)?;
    ensure!(!cancel.is_cancelled(), "Editable preview cancelled");
    let reduced = crate::develop::preview(&recovered, long_edge);
    ensure!(!cancel.is_cancelled(), "Editable preview cancelled");
    let private = Record::of(
        &original.metadata,
        identity,
        [reduced.width, reduced.height],
        default_look,
    )?
    .to_bytes()?;
    dng::file(&reduced, &original.metadata, &private)
}

/// The editable preview at `path`, opened as its photo: see [`EditablePreview`].
/// Its DNG is read by the DNG reader as any DNG is; then the record's facts
/// replace the tags', and the imported lens profiles that fit are matched, as
/// opening the original would match them.
pub fn open(path: &Path, cancel: Cancel<'_>) -> Result<EditablePreview> {
    let record = read_record(path)?;
    let mut metadata = record.metadata()?;
    let default_look = record.default_look(&metadata)?;
    let decoded = crate::photo::open(path)
        .and_then(|raw| raw.develop(Decode::Full(Demosaic::default()), cancel))
        .context("An editable preview's pixels can't be read")?;
    ensure!(
        (decoded.width, decoded.height) == (record.copy_width, record.copy_height),
        "An editable preview reads as {}×{} pixels, not the {}×{} it was written with",
        decoded.width,
        decoded.height,
        record.copy_width,
        record.copy_height
    );
    metadata.lens_profiles = crate::photo::lens_profiles(&metadata);
    let image = CameraImage {
        width: decoded.width,
        height: decoded.height,
        recovered: Default::default(),
        pixels: decoded.pixels,
        metadata,
        // A reduced decode of the photo, whose metadata keeps the photo's size.
        fast: true,
        scale_factor: 1.,
        scale_clipped: 0,
        hdr: Default::default(),
    };
    // Its pixels are recovered already: a second recovery of a reduced copy
    // would not be the original's.
    let _ = image.recovered.set(Arc::new(image.clone()));
    Ok(EditablePreview {
        image,
        original: record.original(),
        default_look,
    })
}

/// The identity of the original the editable preview at `path` was built from,
/// read from its record alone, without decoding its pixels: an error for a copy
/// [`open`] would refuse for its record.
pub fn original_of(path: &Path) -> Result<Identity> {
    Ok(read_record(path)?.original())
}

/// The record in the DNGPrivateData of the DNG at `path`, read on its own, at
/// most [`record::MAX_RECORD`] bytes of it.
fn read_record(path: &Path) -> Result<Record> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let entry = {
        let mut tiff = crate::tiff::Tiff::open(&mut file, 0)
            .context("Not an editable preview: not a TIFF file")?;
        // Little-endian, as `dng` writes it: a big-endian file is not
        // RAWmakase's.
        ensure!(tiff.little, "Not an editable preview: a big-endian TIFF");
        let first = tiff.first;
        tiff.ifd(first)
            .context("An editable preview's directory can't be read")?
            .remove(&50740)
            .context("Not an editable preview: its DNG holds no RAWmakase record")?
    };
    ensure!(
        matches!(
            entry.kind,
            crate::tiff::kind::BYTE | crate::tiff::kind::UNDEFINED
        ),
        "An editable preview's record is damaged"
    );
    let len = u64::from(entry.count);
    ensure!(
        len <= record::MAX_RECORD,
        "An editable preview's record is larger than one may be"
    );
    let mut bytes = vec![0; len as usize];
    if len <= 4 {
        bytes.copy_from_slice(&entry.value[..len as usize]);
    } else {
        let at = u64::from(u32::from_le_bytes(entry.value));
        ensure!(
            at.checked_add(len).is_some_and(|end| end <= size),
            "An editable preview is cut short"
        );
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut bytes)?;
    }
    Record::parse(&bytes)
}
