//! RAWmakase's record in an editable preview's DNGPrivateData (tag 50740): the
//! original's facts as its decode had them, which a DNG's standard tags carry
//! only in part or only approximately.
//!
//! # Layout
//!
//! `RAWmakase\0`, as DNGPrivateData records begin with their maker's name; the
//! format version, a big-endian `u16` (Adobe's own records are big-endian); then
//! the fields, as one JSON object, to the end of the tag's bytes.
//!
//! Every field is written by name, and one missing reads as the default stated
//! beside it in [`Record`]. Fields are this record's own, never a serialised
//! internal struct, so a change to [`Metadata`] or the lens types doesn't change
//! the format: the conversions destructure them whole, so a field added there
//! doesn't compile until it is given a field here or listed as not stored.
//!
//! A record of a version this release doesn't know is refused, never read: the
//! copy is out of date, and is built again while the original is online. A later
//! release that adds a field whose absence would change how the copy renders
//! raises the version; one whose absence changes nothing (a fact shown, not
//! rendered) may keep it, and this release ignores the field. A value this
//! release doesn't know where it reads a name (a matrix source, say) is refused
//! too.
use anyhow::{Context, Result, bail, ensure};
use rawmakase_model::camera_data::{
    EmbeddedProfileStatus, HighlightTonePriority, MatrixSource, Metadata, NeutralCalibration,
    Source, WhiteBalancePresets,
};
use rawmakase_model::camera_profiles::{CameraProfile, fitted};
use rawmakase_model::optics::{GainGrid, LensCorrection, Radial};
use rawmakase_model::storage::Identity;
use serde::{Deserialize, Serialize};

/// What every record starts with.
pub(super) const MAGIC: &[u8; 10] = b"RAWmakase\0";
/// The format version this release writes and the only one it reads.
pub const VERSION: u16 = 1;
/// The largest record read: well beyond what is written (a DNG's embedded
/// profile and a fitted Camera Look are the large parts, together about
/// 2 MB at most), yet small enough that a damaged count can't ask for a huge
/// allocation.
pub(super) const MAX_RECORD: u64 = 16 << 20;

/// Version 1's fields. Each missing field reads as its type's default, which is
/// [`Metadata::default`]'s unless said otherwise; the ones marked required are
/// checked when read ([`Record::parse`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Record {
    /// The original's file when the copy was built. Required.
    pub original: OriginalFile,
    /// The original's full decoded width and height, which the copy develops as
    /// a reduced image of. Required.
    pub width: u32,
    pub height: u32,
    /// The copy's own size, which its DNG decodes to. Required.
    pub copy_width: u32,
    pub copy_height: u32,
    pub raw_width: u32,
    pub raw_height: u32,
    /// The camera's default crop as `[left, top, width, height]` in the
    /// original's pixels, exactly as its decoder gave it, including the values
    /// a decoder writes for none.
    pub crop: [u32; 4],
    pub camera_crop: Option<[f32; 4]>,
    pub make: String,
    pub model: String,
    pub exif_make: String,
    pub exif_model: String,
    pub sensor_crop: Option<f32>,
    pub flip: i32,
    pub xtrans: bool,
    pub sraw: bool,
    pub monochrome: bool,
    pub fuji_dynamic_range: u32,
    pub fuji_exposure_shift: Option<f32>,
    pub highlight_tone_priority: Htp,
    pub iso: f32,
    pub shutter: f32,
    pub aperture: f32,
    pub focal: f32,
    pub focal_35mm: f32,
    /// In metres; 0 when unknown.
    pub focus_distance: f32,
    /// The camera focused at infinity, which JSON can't write as a number.
    pub focus_at_infinity: bool,
    pub wb: [f32; 3],
    pub daylight_wb: [f32; 3],
    pub wb_presets: Presets,
    /// Camera to sRGB, row by row.
    pub matrix: [[f32; 3]; 3],
    /// XYZ (D65) to camera, row by row.
    pub cam_xyz: [[f32; 3]; 3],
    pub matrix_source: MatrixFrom,
    pub camera_look: Option<String>,
    pub embedded_profile_status: ProfileStatus,
    /// A DNG original's embedded camera profile, in DCP form, as hexadecimal.
    pub embedded_profile: Option<String>,
    pub lens: Option<Lens>,
    pub lens_stand_in: Option<Lens>,
    pub lens_model: String,
    pub lens_id: u32,
    pub lens_info: Option<[f32; 4]>,
    pub adobe_names: Vec<String>,
    pub baseline_exposure: Option<f32>,
    pub dng_neutral_calibration: Option<Calibration>,
    pub dng_analog_balance: Option<[f32; 3]>,
    pub dng_matrix_profile_signature: Option<String>,
    /// Black, white and maximum of the sensor data.
    pub levels: Option<[u32; 3]>,
    /// Auto CA as measured on the original's full-size decode. Missing: not
    /// measured, so the copy measures its own.
    pub auto_ca: Option<AutoCa>,
    /// The photo's default Camera Look (fitted), for a camera without colour
    /// data, as an XMP sidecar carries it ([`fitted::carried`]). Its fit needs
    /// the original's embedded JPEG, so offline it comes only from here.
    pub default_look: Option<String>,
}

/// The original file's [`Identity`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct OriginalFile {
    pub bytes: u64,
    /// Its modification time, in nanoseconds since the Unix epoch.
    pub modified_ns: u128,
    /// The hash of its first bytes, as [`Identity`] writes it.
    pub prefix_hash: String,
}

/// The camera's white balance presets.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Presets {
    pub daylight: Option<[f32; 3]>,
    pub shade: Option<[f32; 3]>,
    pub d65: Option<[f32; 3]>,
    pub fine_weather: Option<[f32; 3]>,
}

/// A DNG's camera calibration gains and the signature they were made for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Calibration {
    pub gains: [f32; 3],
    pub signature: String,
}

/// A lens correction, as [`LensCorrection`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Lens {
    pub source: String,
    pub default_on: bool,
    pub vignetting: Option<Table>,
    pub distortion: Option<Table>,
    /// Red, then blue.
    pub chromatic: Option<[Table; 2]>,
    pub chromatic_at_source: bool,
    pub gain_map: Option<Gains>,
    /// The optical centre, in fractions of the image's width and height.
    /// Missing: the image's centre.
    pub centre: [f32; 2],
}
impl Default for Lens {
    fn default() -> Self {
        Self {
            source: String::new(),
            default_on: false,
            vignetting: None,
            distortion: None,
            chromatic: None,
            chromatic_at_source: false,
            gain_map: None,
            centre: LensCorrection::image_centre(),
        }
    }
}

/// A radial table, as [`Radial`]. Both lists are required.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Table {
    pub knots: Vec<f32>,
    pub values: Vec<f32>,
}

/// A gain map, as [`GainGrid`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Gains {
    /// `[top, left, bottom, right]`. Missing: the whole image.
    pub area: [f32; 4],
    pub after_warp: bool,
    /// Rows, then columns.
    pub points: [u32; 2],
    pub origin: [f32; 2],
    pub spacing: [f32; 2],
    pub planes: u32,
    pub gains: Vec<f32>,
}
impl Default for Gains {
    fn default() -> Self {
        Self {
            area: [0., 0., 1., 1.],
            after_warp: false,
            points: [0; 2],
            origin: [0.; 2],
            spacing: [0.; 2],
            planes: 0,
            gains: Vec::new(),
        }
    }
}

/// Auto CA's measurement: red and blue, or neither when it found nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct AutoCa {
    pub red: Option<Table>,
    pub blue: Option<Table>,
}

/// Canon's Highlight Tone Priority. Missing: `off`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Htp {
    #[default]
    Off,
    On,
    Enhanced,
}

/// Where `cam_xyz` came from. Missing: `absent`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MatrixFrom {
    #[default]
    Absent,
    AdobeDng,
    CompatObserved,
    File,
    FileTungstenOnly,
    MakerNote,
    Neutral,
}

/// What a DNG original's embedded profile was to it. Missing: `none`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProfileStatus {
    #[default]
    None,
    Unreadable,
    MatrixOnly,
    OtherCamera,
    Usable,
}

impl Record {
    /// The record of `m`, the metadata of the full-size decode of the file
    /// `original` names, for a copy of `copy` pixels, with the photo's
    /// `default_look`. Refused for what version 1 can't keep: a rendered file, an
    /// HDR file's light, or a transparency mask.
    pub(super) fn of(
        m: &Metadata,
        original: &Identity,
        copy: [u32; 2],
        default_look: Option<&CameraProfile>,
    ) -> Result<Self> {
        let Metadata {
            make,
            model,
            exif_make,
            exif_model,
            sensor_crop,
            width,
            height,
            raw_width,
            raw_height,
            crop_width,
            crop_height,
            crop_left,
            crop_top,
            camera_crop,
            flip,
            xtrans,
            fuji_dynamic_range,
            highlight_tone_priority,
            sraw,
            monochrome,
            fuji_exposure_shift,
            iso,
            shutter,
            aperture,
            focal,
            focal_35mm,
            focus_distance,
            wb,
            daylight_wb,
            wb_presets,
            matrix,
            cam_xyz,
            matrix_source,
            camera_look,
            embedded_profile_status,
            lens,
            lens_stand_in,
            lens_model,
            lens_id,
            lens_info,
            adobe_names,
            baseline_exposure,
            dng_neutral_calibration,
            dng_analog_balance,
            dng_matrix_profile_signature,
            source,
            hdr_source,
            levels,
            // The user's imported lens profiles, matched again when the copy
            // opens, as `photo::open` matches them.
            lens_profiles: _,
            lateral_ca,
            embedded_dcp,
            coverage,
        } = m;
        ensure!(
            *source == Source::Raw,
            "Editable previews are built for RAWs only"
        );
        ensure!(
            hdr_source.is_none(),
            "An editable preview can't keep an HDR file's light"
        );
        ensure!(
            coverage.is_none(),
            "An editable preview can't keep a DNG's transparency yet"
        );
        let Identity {
            size: bytes,
            modified_ns,
            prefix_hash,
        } = original;
        let crop = [*crop_left, *crop_top, *crop_width, *crop_height];
        let WhiteBalancePresets {
            daylight,
            shade,
            d65,
            fine_weather,
        } = wb_presets;
        Ok(Self {
            original: OriginalFile {
                bytes: *bytes,
                modified_ns: *modified_ns,
                prefix_hash: prefix_hash.clone(),
            },
            width: *width,
            height: *height,
            copy_width: copy[0],
            copy_height: copy[1],
            raw_width: *raw_width,
            raw_height: *raw_height,
            crop,
            camera_crop: *camera_crop,
            make: make.clone(),
            model: model.clone(),
            exif_make: exif_make.clone(),
            exif_model: exif_model.clone(),
            sensor_crop: *sensor_crop,
            flip: *flip,
            xtrans: *xtrans,
            sraw: *sraw,
            monochrome: *monochrome,
            fuji_dynamic_range: *fuji_dynamic_range,
            fuji_exposure_shift: *fuji_exposure_shift,
            highlight_tone_priority: match highlight_tone_priority {
                HighlightTonePriority::Off => Htp::Off,
                HighlightTonePriority::On => Htp::On,
                HighlightTonePriority::Enhanced => Htp::Enhanced,
            },
            iso: *iso,
            shutter: *shutter,
            aperture: *aperture,
            focal: *focal,
            focal_35mm: *focal_35mm,
            focus_distance: if focus_distance.is_finite() {
                *focus_distance
            } else {
                0.
            },
            focus_at_infinity: *focus_distance == f32::INFINITY,
            wb: *wb,
            daylight_wb: *daylight_wb,
            wb_presets: Presets {
                daylight: *daylight,
                shade: *shade,
                d65: *d65,
                fine_weather: *fine_weather,
            },
            matrix: *matrix,
            cam_xyz: *cam_xyz,
            matrix_source: match matrix_source {
                MatrixSource::Absent => MatrixFrom::Absent,
                MatrixSource::AdobeDng => MatrixFrom::AdobeDng,
                MatrixSource::CompatObserved => MatrixFrom::CompatObserved,
                MatrixSource::File => MatrixFrom::File,
                MatrixSource::FileTungstenOnly => MatrixFrom::FileTungstenOnly,
                MatrixSource::MakerNote => MatrixFrom::MakerNote,
                MatrixSource::Neutral => MatrixFrom::Neutral,
            },
            camera_look: camera_look.clone(),
            embedded_profile_status: match embedded_profile_status {
                EmbeddedProfileStatus::None => ProfileStatus::None,
                EmbeddedProfileStatus::Unreadable => ProfileStatus::Unreadable,
                EmbeddedProfileStatus::MatrixOnly => ProfileStatus::MatrixOnly,
                EmbeddedProfileStatus::OtherCamera => ProfileStatus::OtherCamera,
                EmbeddedProfileStatus::Usable => ProfileStatus::Usable,
            },
            embedded_profile: embedded_dcp.as_deref().map(hex),
            lens: lens.as_ref().map(Lens::of),
            lens_stand_in: lens_stand_in.as_ref().map(Lens::of),
            lens_model: lens_model.clone(),
            lens_id: *lens_id,
            lens_info: *lens_info,
            adobe_names: adobe_names.clone(),
            baseline_exposure: *baseline_exposure,
            dng_neutral_calibration: dng_neutral_calibration.as_ref().map(
                |NeutralCalibration { gains, signature }| Calibration {
                    gains: *gains,
                    signature: signature.clone(),
                },
            ),
            dng_analog_balance: *dng_analog_balance,
            dng_matrix_profile_signature: dng_matrix_profile_signature.clone(),
            levels: *levels,
            auto_ca: lateral_ca.get().map(|measured| match measured {
                Some([red, blue]) => AutoCa {
                    red: Some(Table::of(red)),
                    blue: Some(Table::of(blue)),
                },
                None => AutoCa::default(),
            }),
            default_look: default_look.map(fitted::carried),
        })
    }

    /// The bytes of DNGPrivateData holding this record. Refused unless they
    /// read back as this record, so nothing is written that can't be read (a
    /// value JSON can't hold, such as a NaN, among them).
    pub(super) fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = MAGIC.to_vec();
        out.extend(VERSION.to_be_bytes());
        out.extend(serde_json::to_vec(self)?);
        ensure!(
            out.len() as u64 <= MAX_RECORD,
            "An editable preview's record would be larger than one may be"
        );
        ensure!(
            Self::parse(&out).ok().as_ref() == Some(self),
            "An editable preview's record doesn't read back as written"
        );
        Ok(out)
    }

    /// The record DNGPrivateData `bytes` hold. An error for anything but a
    /// complete record of this version: another maker's data, a version this
    /// release doesn't know, damage or a field missing that is required.
    pub(super) fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() as u64 <= MAX_RECORD,
            "An editable preview's record is larger than one may be"
        );
        let rest = bytes
            .strip_prefix(MAGIC)
            .context("Not an editable preview: its DNG holds no RAWmakase record")?;
        let (version, json) = rest
            .split_first_chunk::<2>()
            .context("An editable preview's record is cut short")?;
        let version = u16::from_be_bytes(*version);
        ensure!(version != 0, "An editable preview's record is damaged");
        ensure!(
            version <= VERSION,
            "This editable preview's record is version {version}, which only a later release of RAWmakase reads (this one reads version {VERSION})"
        );
        let record: Self =
            serde_json::from_slice(json).context("An editable preview's record is damaged")?;
        ensure!(
            !record.original.prefix_hash.is_empty(),
            "An editable preview's record doesn't name its original"
        );
        ensure!(
            record.width > 0
                && record.height > 0
                && record.copy_width > 0
                && record.copy_height > 0,
            "An editable preview's record has no size"
        );
        Ok(record)
    }

    /// The original's identity when the copy was built.
    pub(super) fn original(&self) -> Identity {
        let OriginalFile {
            bytes,
            modified_ns,
            prefix_hash,
        } = &self.original;
        Identity {
            size: *bytes,
            modified_ns: *modified_ns,
            prefix_hash: prefix_hash.clone(),
        }
    }

    /// The original's metadata as its decode had it, apart from the imported lens
    /// profiles that fit, which the caller matches. An error for a table, gain map
    /// or profile no reader of the original would have made.
    pub(super) fn metadata(&self) -> Result<Metadata> {
        let d = Metadata::default();
        let [crop_left, crop_top, crop_width, crop_height] = self.crop;
        let lateral_ca = std::sync::Arc::new(std::sync::OnceLock::new());
        if let Some(AutoCa { red, blue }) = &self.auto_ca {
            let measured = match (red, blue) {
                (None, None) => None,
                (Some(red), Some(blue)) => Some([red.radial()?, blue.radial()?]),
                _ => bail!("An editable preview's Auto CA has one channel"),
            };
            let _ = lateral_ca.set(measured);
        }
        let Presets {
            daylight,
            shade,
            d65,
            fine_weather,
        } = self.wb_presets.clone();
        Ok(Metadata {
            make: self.make.clone(),
            model: self.model.clone(),
            exif_make: self.exif_make.clone(),
            exif_model: self.exif_model.clone(),
            sensor_crop: self.sensor_crop,
            width: self.width,
            height: self.height,
            raw_width: self.raw_width,
            raw_height: self.raw_height,
            crop_width,
            crop_height,
            crop_left,
            crop_top,
            camera_crop: self.camera_crop,
            flip: self.flip,
            xtrans: self.xtrans,
            fuji_dynamic_range: self.fuji_dynamic_range,
            highlight_tone_priority: match self.highlight_tone_priority {
                Htp::Off => HighlightTonePriority::Off,
                Htp::On => HighlightTonePriority::On,
                Htp::Enhanced => HighlightTonePriority::Enhanced,
            },
            sraw: self.sraw,
            monochrome: self.monochrome,
            fuji_exposure_shift: self.fuji_exposure_shift,
            iso: self.iso,
            shutter: self.shutter,
            aperture: self.aperture,
            focal: self.focal,
            focal_35mm: self.focal_35mm,
            focus_distance: if self.focus_at_infinity {
                f32::INFINITY
            } else {
                self.focus_distance
            },
            wb: self.wb,
            daylight_wb: self.daylight_wb,
            wb_presets: WhiteBalancePresets {
                daylight,
                shade,
                d65,
                fine_weather,
            },
            matrix: self.matrix,
            cam_xyz: self.cam_xyz,
            matrix_source: match self.matrix_source {
                MatrixFrom::Absent => MatrixSource::Absent,
                MatrixFrom::AdobeDng => MatrixSource::AdobeDng,
                MatrixFrom::CompatObserved => MatrixSource::CompatObserved,
                MatrixFrom::File => MatrixSource::File,
                MatrixFrom::FileTungstenOnly => MatrixSource::FileTungstenOnly,
                MatrixFrom::MakerNote => MatrixSource::MakerNote,
                MatrixFrom::Neutral => MatrixSource::Neutral,
            },
            camera_look: self.camera_look.clone(),
            embedded_profile_status: match self.embedded_profile_status {
                ProfileStatus::None => EmbeddedProfileStatus::None,
                ProfileStatus::Unreadable => EmbeddedProfileStatus::Unreadable,
                ProfileStatus::MatrixOnly => EmbeddedProfileStatus::MatrixOnly,
                ProfileStatus::OtherCamera => EmbeddedProfileStatus::OtherCamera,
                ProfileStatus::Usable => EmbeddedProfileStatus::Usable,
            },
            lens: self.lens.as_ref().map(Lens::correction).transpose()?,
            lens_stand_in: self
                .lens_stand_in
                .as_ref()
                .map(Lens::correction)
                .transpose()?,
            lens_model: self.lens_model.clone(),
            lens_id: self.lens_id,
            lens_info: self.lens_info,
            adobe_names: self.adobe_names.clone(),
            baseline_exposure: self.baseline_exposure,
            dng_neutral_calibration: self.dng_neutral_calibration.as_ref().map(
                |Calibration { gains, signature }| NeutralCalibration {
                    gains: *gains,
                    signature: signature.clone(),
                },
            ),
            dng_analog_balance: self.dng_analog_balance,
            dng_matrix_profile_signature: self.dng_matrix_profile_signature.clone(),
            source: Source::Raw,
            hdr_source: None,
            levels: self.levels,
            lens_profiles: d.lens_profiles,
            lateral_ca,
            embedded_dcp: self
                .embedded_profile
                .as_deref()
                .map(unhex)
                .transpose()?
                .map(std::sync::Arc::from),
            coverage: None,
        })
    }

    /// The photo's default Camera Look (fitted), checked as a sidecar's is, for
    /// the photo `m` describes.
    pub(super) fn default_look(&self, m: &Metadata) -> Result<Option<CameraProfile>> {
        self.default_look
            .as_deref()
            .map(|text| {
                fitted::from_carried(text, m)
                    .context("An editable preview's Camera Look (fitted) can't be read")
            })
            .transpose()
    }
}

impl Lens {
    fn of(l: &LensCorrection) -> Self {
        let LensCorrection {
            source,
            default_on,
            vignetting,
            distortion,
            chromatic,
            chromatic_at_source,
            gain_map,
            centre,
        } = l;
        Self {
            source: source.clone(),
            default_on: *default_on,
            vignetting: vignetting.as_ref().map(Table::of),
            distortion: distortion.as_ref().map(Table::of),
            chromatic: chromatic
                .as_ref()
                .map(|[r, b]| [Table::of(r), Table::of(b)]),
            chromatic_at_source: *chromatic_at_source,
            gain_map: gain_map.as_ref().map(Gains::of),
            centre: *centre,
        }
    }
    /// The correction, checked as every reader of one checks it.
    fn correction(&self) -> Result<LensCorrection> {
        let l = LensCorrection {
            source: self.source.clone(),
            default_on: self.default_on,
            vignetting: self.vignetting.as_ref().map(Table::radial).transpose()?,
            distortion: self.distortion.as_ref().map(Table::radial).transpose()?,
            chromatic: match &self.chromatic {
                Some([r, b]) => Some([r.radial()?, b.radial()?]),
                None => None,
            },
            chromatic_at_source: self.chromatic_at_source,
            gain_map: self.gain_map.as_ref().map(Gains::grid),
            centre: self.centre,
        };
        ensure!(
            l.validate(),
            "An editable preview's lens correction is not a valid one"
        );
        Ok(l)
    }
}

impl Table {
    fn of(r: &Radial) -> Self {
        let Radial { knots, values } = r;
        Self {
            knots: knots.clone(),
            values: values.clone(),
        }
    }
    fn radial(&self) -> Result<Radial> {
        Radial::new(self.knots.clone(), self.values.clone())
            .context("An editable preview's lens table is not a valid one")
    }
}

impl Gains {
    fn of(g: &GainGrid) -> Self {
        let GainGrid {
            area,
            after_warp,
            points,
            origin,
            spacing,
            planes,
            gains,
        } = g;
        Self {
            area: *area,
            after_warp: *after_warp,
            points: points.map(|v| u32::try_from(v).unwrap_or(u32::MAX)),
            origin: *origin,
            spacing: *spacing,
            planes: u32::try_from(*planes).unwrap_or(u32::MAX),
            gains: gains.clone(),
        }
    }
    /// The grid; [`Lens::correction`] checks it.
    fn grid(&self) -> GainGrid {
        GainGrid {
            area: self.area,
            after_warp: self.after_warp,
            points: self.points.map(|v| v as usize),
            origin: self.origin,
            spacing: self.spacing,
            planes: self.planes as usize,
            gains: self.gains.clone(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    ensure!(
        text.len().is_multiple_of(2),
        "An editable preview's embedded profile is damaged"
    );
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .context("An editable preview's embedded profile is damaged")
        })
        .collect()
}
