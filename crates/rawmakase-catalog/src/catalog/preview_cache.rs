//! Disposable Library previews, separate from user catalogs and edit recipes.
//!
//! Two kinds of rows share the file: the Library's 640 px thumbnails
//! (`previews`), and larger previews built on request for selected photos
//! (`sized_previews`, issue #213), each kind with its own budget. Which catalog
//! photos had previews built is kept apart, in `preview_intent`, because pixel
//! rows are keyed by file and shared by virtual copies and catalogs.
use super::{CatalogLocation, PhotoId};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
const APP_ID: i64 = 0x4f4d5052;
const VERSION: i64 = 1;
// 2: previews keep their aspect ratio (generation 1 forced 360×240).
// 3: the one engine renders every edit differently from earlier releases.
// 4: highlight recovery keeps a lone clipped channel's recorded value, which saved
//    edits of RAWs with clipped light sources render differently.
// 5: a DNG's OpcodeList3 GainMaps apply (drones' lens shading), which saved edits of
//    those DNGs render differently.
// 6: Highlights -100, and the positions between -50 and -100, keep pulling areas more
//    than 7 stops over the photo's key, which saved edits of photos with such areas
//    render differently.
// 7: built-in lens corrections apply as Lightroom applies them (Olympus, OM System
//    and Panasonic distortion and CA, Nikon Z, Sony's lateral CA), and correction
//    the camera always applies ignores the profile amounts while profile corrections
//    are off, which saved edits of those RAWs render differently.
// 8: Coolpix NRW lens tables apply.
// 9: Samsung's built-in distortion and lateral CA apply, which saved edits of SRWs
//    render differently.
// 10: the EOS R5 Mark II's and R6 Mark III's built-in lens correction, which saved
//     edits with Enable Profile Corrections on render with.
// 11: lens corrections are centred on their optical centre and framed by green's
//     samples for the photo's edge (Nikon Z, Olympus, OM System, Samsung, DNG
//     opcodes, Adobe profiles).
// 12: Fujifilm's distortion table is scaled as Adobe's warps of it are, so its photos
//     frame as in Lightroom.
// 13: a half-size decode sharpens and grains at its photo's scale, which edited
//     thumbnails rendered from it show differently.
// 14: a half-size decode reduces luminance and colour noise at its photo's scales.
const GENERATION: i64 = 14;
// 3: a JPEG, PNG or TIFF is converted from its embedded ICC profile.
// 4: the one engine, as `GENERATION` 3.
// 5: an edited JPEG, PNG or TIFF renders from the scene its rendering is of.
// 6: Highlights, as `GENERATION` 6.
const RASTER_GENERATION: i64 = 6;
const LIMIT: i64 = 512 * 1024 * 1024;
// 2: the one engine, as `GENERATION` 3.
// 3: highlight recovery, as `GENERATION` 4.
// 4: OpcodeList3 GainMaps, as `GENERATION` 5.
// 5: Highlights, as `GENERATION` 6.
// 6: built-in lens corrections, as `GENERATION` 7.
// 7: Coolpix NRW lens tables, as `GENERATION` 8.
// 8: Samsung's built-in lens corrections, as `GENERATION` 9.
// 9: Canon's built-in lens correction, as `GENERATION` 10.
// 10: optical centres and framing, as `GENERATION` 11.
// 11: Fujifilm's distortion framing, as `GENERATION` 12.
// 12: a half-size decode's sharpening and grain, as `GENERATION` 13 (Standard
//     previews built from it).
// 13: a half-size decode's noise reduction, as `GENERATION` 14.
const SIZED_GENERATION: i64 = 13;
const SIZED_QUALITY: u8 = 85;
const THUMBNAIL_MAX_EDGE: u32 = 1024;
const THUMBNAIL_QUALITY: u8 = 88;
/// Tables added after version 1, created on open so older builds, which ignore
/// them, keep opening the file.
const SIZED_SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS sized_previews(source_path TEXT NOT NULL,identity TEXT NOT NULL,kind TEXT NOT NULL,source_size INTEGER NOT NULL,modified_ns TEXT NOT NULL,generation INTEGER NOT NULL,requested_edge INTEGER NOT NULL,width INTEGER NOT NULL,height INTEGER NOT NULL,jpeg BLOB NOT NULL,last_used INTEGER NOT NULL,PRIMARY KEY(source_path,identity,kind));
    CREATE INDEX IF NOT EXISTS sized_previews_last_used ON sized_previews(kind,last_used);
    CREATE TABLE IF NOT EXISTS preview_intent(catalog TEXT NOT NULL,photo_id INTEGER NOT NULL,source_path TEXT NOT NULL,kind TEXT NOT NULL,PRIMARY KEY(catalog,photo_id,kind));";
/// A preview built on request, larger than a Library thumbnail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PreviewKind {
    /// Fits the Standard Preview Size; Develop shows it while a photo opens.
    Standard,
    /// The photo at full resolution.
    OneToOne,
}
impl PreviewKind {
    pub const ALL: [Self; 2] = [Self::Standard, Self::OneToOne];
    fn code(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::OneToOne => "one_to_one",
        }
    }
    /// Payload bytes this kind may keep before its least recently used rows go.
    pub fn budget(self) -> i64 {
        match self {
            Self::Standard => 2 << 30,
            Self::OneToOne => 4 << 30,
        }
    }
}
/// Image bytes held by each kind of row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreviewUsage {
    pub thumbnails: u64,
    pub standard: u64,
    pub one_to_one: u64,
}
pub use crate::storage::Stamp;
/// A JPEG-encoded preview with its pixel dimensions, produced off-lock for
/// [`PreviewCache::store_tagged_jpeg`] or [`PreviewCache::store_sized_jpeg`],
/// or read by [`PreviewCache::load_sized_jpeg`] for [`PreviewCache::decode_sized`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPreview {
    width: u32,
    height: u32,
    jpeg: Vec<u8>,
}
impl EncodedPreview {
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn jpeg(&self) -> &[u8] {
        &self.jpeg
    }
}
pub struct PreviewCache {
    db: Connection,
    writes: u32,
    /// Built previews stored, counted apart so thumbnails never delay their pruning.
    sized_writes: u32,
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn catalog_key(catalog: &CatalogLocation) -> String {
    match catalog {
        CatalogLocation::File(path) => format!("file:{}", key(path)),
    }
}
fn tagged(path: &Path, tag: &str) -> String {
    if tag.is_empty() {
        key(path)
    } else {
        format!("{}#{tag}", key(path))
    }
}
/// The generation a thumbnail of `path` is made in now.
fn generation(path: &Path) -> i64 {
    if crate::storage::is_raw(path) {
        GENERATION
    } else {
        RASTER_GENERATION
    }
}
/// Whether the file at `path` is no longer the one a row was made from. An
/// offline original is not stale, so its previews stay usable.
fn stale(path: &Path, size: i64, modified: &str) -> bool {
    Stamp::read(path)
        .is_ok_and(|stamp| stamp.size as i64 != size || stamp.modified_ns.to_string() != modified)
}
fn encode_jpeg(image: &image::RgbImage, quality: u8) -> Result<EncodedPreview> {
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality).encode(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(EncodedPreview {
        width: image.width(),
        height: image.height(),
        jpeg,
    })
}
impl PreviewCache {
    pub fn path() -> PathBuf {
        crate::storage::data_dir().join("previews.sqlite3")
    }
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_millis(250))?;
        let app: i64 = db.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if app == 0 && version == 0 {
            let tables: i64 = db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )?;
            ensure!(
                tables == 0,
                "Preview cache path contains a different database"
            );
            db.execute_batch(&format!("PRAGMA auto_vacuum=INCREMENTAL; PRAGMA application_id={APP_ID}; PRAGMA user_version={VERSION};
                CREATE TABLE previews(source_path TEXT PRIMARY KEY,source_size INTEGER NOT NULL,modified_ns TEXT NOT NULL,prefix_hash TEXT NOT NULL,generation INTEGER NOT NULL,width INTEGER NOT NULL,height INTEGER NOT NULL,jpeg BLOB NOT NULL,last_used INTEGER NOT NULL);
                CREATE INDEX previews_last_used ON previews(last_used);"))?;
        } else {
            ensure!(
                app == APP_ID && version == VERSION,
                "Unsupported preview cache format"
            );
        }
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-32000; PRAGMA temp_store=MEMORY;",
        )?;
        db.execute_batch(SIZED_SCHEMA)?;
        let mut cache = Self {
            db,
            writes: 0,
            sized_writes: 0,
        };
        cache.prune(LIMIT)?;
        for kind in PreviewKind::ALL {
            cache.prune_sized(kind, kind.budget())?;
        }
        Ok(cache)
    }
    /// Encodes a thumbnail as JPEG at quality 88, enforcing the 1024 px side limit.
    pub fn encode_thumbnail(image: &image::RgbImage) -> Result<EncodedPreview> {
        ensure!(
            image.width() <= THUMBNAIL_MAX_EDGE && image.height() <= THUMBNAIL_MAX_EDGE,
            "Preview exceeds cache size limit"
        );
        encode_jpeg(image, THUMBNAIL_QUALITY)
    }
    /// Decodes a cached thumbnail JPEG off-lock, returning `None` if corrupt or
    /// larger than 1024 px on a side.
    pub fn decode_thumbnail(jpeg: &[u8]) -> Option<image::RgbImage> {
        image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
            .ok()
            .filter(|im| im.width() <= THUMBNAIL_MAX_EDGE && im.height() <= THUMBNAIL_MAX_EDGE)
            .map(|im| im.to_rgb8())
    }
    /// Encodes a sized preview as JPEG at [`SIZED_QUALITY`].
    pub fn encode_sized(image: &image::RgbImage) -> Result<EncodedPreview> {
        ensure!(
            image.width() > 0 && image.height() > 0,
            "Preview has no pixels"
        );
        encode_jpeg(image, SIZED_QUALITY)
    }
    /// Decodes a cached sized preview off-lock, returning `None` if corrupt or
    /// its dimensions differ from those recorded in the row.
    pub fn decode_sized(encoded: &EncodedPreview) -> Option<image::RgbImage> {
        image::load_from_memory_with_format(&encoded.jpeg, image::ImageFormat::Jpeg)
            .ok()
            .filter(|im| (im.width(), im.height()) == (encoded.width, encoded.height))
            .map(|im| im.to_rgb8())
    }
    pub fn load(&self, path: &Path) -> Result<Option<image::RgbImage>> {
        self.load_tagged(path, "")
    }
    /// Loads the cached JPEG bytes for `path` under `tag`, checking generation
    /// and source staleness (an offline original stays usable). Callers decode
    /// with [`PreviewCache::decode_thumbnail`] off-lock and call
    /// [`PreviewCache::delete_tagged`] if decoding fails.
    pub fn load_tagged_jpeg(&self, path: &Path, tag: &str) -> Result<Option<Vec<u8>>> {
        let key = tagged(path, tag);
        let row = self
            .db
            .prepare_cached(
                "SELECT source_size,modified_ns,generation,jpeg FROM previews WHERE source_path=?",
            )?
            .query_row([&key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })
            .optional()?;
        let Some((size, modified, generation, jpeg)) = row else {
            return Ok(None);
        };
        // An offline original keeps its preview, as in Lightroom.
        let stale = stale(path, size, &modified);
        // Maintenance is best effort, as another connection may hold the database, and
        // never changes the answer: a failed delete still misses, a failed touch hits.
        if generation != self::generation(path) || stale {
            let _ = self.delete_tagged(path, tag);
            return Ok(None);
        }
        let _ = self
            .db
            .prepare_cached("UPDATE previews SET last_used=? WHERE source_path=?")
            .and_then(|mut s| s.execute(params![now(), &key]));
        Ok(Some(jpeg))
    }
    /// Removes any cached thumbnail for `path` under `tag`.
    pub fn delete_tagged(&self, path: &Path, tag: &str) -> Result<()> {
        self.db
            .prepare_cached("DELETE FROM previews WHERE source_path=?")?
            .execute([tagged(path, tag)])?;
        Ok(())
    }
    /// A preview variant, e.g. rendered with an edit identified by `tag`.
    pub fn load_tagged(&self, path: &Path, tag: &str) -> Result<Option<image::RgbImage>> {
        let Some(jpeg) = self.load_tagged_jpeg(path, tag)? else {
            return Ok(None);
        };
        match Self::decode_thumbnail(&jpeg) {
            Some(image) => Ok(Some(image)),
            None => {
                let _ = self.delete_tagged(path, tag);
                Ok(None)
            }
        }
    }
    pub fn store(&mut self, path: &Path, stamp: &Stamp, image: &image::RgbImage) -> Result<()> {
        self.store_tagged(path, "", stamp, image)
    }
    /// Stores a JPEG-encoded thumbnail built from the file as it was at
    /// `stamp`, refusing it when the file has changed since.
    pub fn store_tagged_jpeg(
        &mut self,
        path: &Path,
        tag: &str,
        stamp: &Stamp,
        encoded: &EncodedPreview,
    ) -> Result<()> {
        ensure!(
            encoded.width <= THUMBNAIL_MAX_EDGE && encoded.height <= THUMBNAIL_MAX_EDGE,
            "Preview exceeds cache size limit"
        );
        ensure!(
            encoded.width > 0 && encoded.height > 0 && !encoded.jpeg.is_empty(),
            "Preview has no pixels"
        );
        ensure!(
            Stamp::read(path)? == *stamp,
            "Source changed while generating preview"
        );
        // prefix_hash, which needed the file's first 64 KB, is no longer checked.
        self.db
            .prepare_cached("INSERT INTO previews VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(source_path) DO UPDATE SET source_size=excluded.source_size,modified_ns=excluded.modified_ns,prefix_hash=excluded.prefix_hash,generation=excluded.generation,width=excluded.width,height=excluded.height,jpeg=excluded.jpeg,last_used=excluded.last_used")?
            .execute(params![
                tagged(path, tag),
                stamp.size as i64,
                stamp.modified_ns.to_string(),
                "",
                generation(path),
                encoded.width,
                encoded.height,
                encoded.jpeg.as_slice(),
                now()
            ])?;
        self.writes += 1;
        if self.writes.is_multiple_of(32) {
            self.prune(LIMIT)?;
        }
        Ok(())
    }
    pub fn store_tagged(
        &mut self,
        path: &Path,
        tag: &str,
        stamp: &Stamp,
        image: &image::RgbImage,
    ) -> Result<()> {
        ensure!(
            image.width() <= THUMBNAIL_MAX_EDGE && image.height() <= THUMBNAIL_MAX_EDGE,
            "Preview exceeds cache size limit"
        );
        ensure!(
            Stamp::read(path)? == *stamp,
            "Source changed while generating preview"
        );
        let encoded = Self::encode_thumbnail(image)?;
        self.store_tagged_jpeg(path, tag, stamp, &encoded)
    }
    /// Loads the cached sized preview JPEG and stored dimensions for `path` and
    /// `identity`, checking generation and source staleness (an offline original
    /// stays usable). Callers decode with [`PreviewCache::decode_sized`]
    /// off-lock and call [`PreviewCache::delete_sized`] if decoding fails.
    pub fn load_sized_jpeg(
        &self,
        path: &Path,
        identity: &str,
        kind: PreviewKind,
    ) -> Result<Option<EncodedPreview>> {
        let source = key(path);
        let row = self
            .db
            .prepare_cached(
                "SELECT source_size,modified_ns,generation,width,height,jpeg FROM sized_previews WHERE source_path=? AND identity=? AND kind=?",
            )?
            .query_row(params![&source, identity, kind.code()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                ))
            })
            .optional()?;
        let Some((size, modified, generation, width, height, jpeg)) = row else {
            return Ok(None);
        };
        // Best effort, as for thumbnails: a failed delete still misses, a failed touch hits.
        if generation != SIZED_GENERATION || stale(path, size, &modified) {
            let _ = self.delete_sized(path, identity, kind);
            return Ok(None);
        }
        let _ = self
            .db
            .prepare_cached(
                "UPDATE sized_previews SET last_used=? WHERE source_path=? AND identity=? AND kind=?",
            )
            .and_then(|mut s| s.execute(params![now(), &source, identity, kind.code()]));
        Ok(Some(EncodedPreview {
            width,
            height,
            jpeg,
        }))
    }
    /// Removes any cached sized preview of `kind` for `path` and `identity`.
    pub fn delete_sized(&self, path: &Path, identity: &str, kind: PreviewKind) -> Result<()> {
        self.db
            .prepare_cached(
                "DELETE FROM sized_previews WHERE source_path=? AND identity=? AND kind=?",
            )?
            .execute(params![key(path), identity, kind.code()])?;
        Ok(())
    }
    /// A built preview of `path` for the edit `identity`, with the thumbnails'
    /// offline and staleness rules: an offline original still hits, without
    /// being opened; a rewritten one misses and its row goes.
    pub fn load_sized(
        &self,
        path: &Path,
        identity: &str,
        kind: PreviewKind,
    ) -> Result<Option<image::RgbImage>> {
        let Some(encoded) = self.load_sized_jpeg(path, identity, kind)? else {
            return Ok(None);
        };
        match Self::decode_sized(&encoded) {
            Some(image) => Ok(Some(image)),
            None => {
                let _ = self.delete_sized(path, identity, kind);
                Ok(None)
            }
        }
    }
    /// Whether a usable preview exists without reading its pixels: the file is
    /// unchanged and the row was built for at least `requested_edge`. The size
    /// it reached is not compared, so a small original or a tight crop that
    /// cannot reach the edge is not rebuilt each time.
    pub fn sized_fresh(
        &self,
        path: &Path,
        identity: &str,
        kind: PreviewKind,
        requested_edge: u32,
    ) -> Result<bool> {
        let row = self
            .db
            .prepare_cached(
                "SELECT source_size,modified_ns,generation,requested_edge FROM sized_previews WHERE source_path=? AND identity=? AND kind=?",
            )?
            .query_row(params![key(path), identity, kind.code()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, u32>(3)?,
                ))
            })
            .optional()?;
        Ok(row.is_some_and(|(size, modified, generation, edge)| {
            generation == SIZED_GENERATION
                && edge >= requested_edge
                && !stale(path, size, &modified)
        }))
    }
    /// Stores a JPEG-encoded preview built from the file as it was at `stamp`,
    /// refusing it when the file has changed since.
    pub fn store_sized_jpeg(
        &mut self,
        path: &Path,
        identity: &str,
        kind: PreviewKind,
        stamp: &Stamp,
        requested_edge: u32,
        encoded: &EncodedPreview,
    ) -> Result<()> {
        ensure!(
            encoded.width > 0 && encoded.height > 0 && !encoded.jpeg.is_empty(),
            "Preview has no pixels"
        );
        ensure!(
            Stamp::read(path)? == *stamp,
            "Source changed while building preview"
        );
        self.db
            .prepare_cached("INSERT OR REPLACE INTO sized_previews VALUES(?,?,?,?,?,?,?,?,?,?,?)")?
            .execute(params![
                key(path),
                identity,
                kind.code(),
                stamp.size as i64,
                stamp.modified_ns.to_string(),
                SIZED_GENERATION,
                requested_edge,
                encoded.width,
                encoded.height,
                encoded.jpeg.as_slice(),
                now()
            ])?;
        self.sized_writes += 1;
        if self.sized_writes.is_multiple_of(8) {
            for kind in PreviewKind::ALL {
                self.prune_sized(kind, kind.budget())?;
            }
        }
        Ok(())
    }
    /// Stores a preview built from the file as it was at `stamp`, refusing it
    /// when the file has changed since.
    pub fn store_sized(
        &mut self,
        path: &Path,
        identity: &str,
        kind: PreviewKind,
        stamp: &Stamp,
        requested_edge: u32,
        image: &image::RgbImage,
    ) -> Result<()> {
        ensure!(
            image.width() > 0 && image.height() > 0,
            "Preview has no pixels"
        );
        ensure!(
            Stamp::read(path)? == *stamp,
            "Source changed while building preview"
        );
        let encoded = Self::encode_sized(image)?;
        self.store_sized_jpeg(path, identity, kind, stamp, requested_edge, &encoded)
    }
    /// Image bytes held by each kind.
    pub fn usage(&self) -> Result<PreviewUsage> {
        let thumbnails: i64 = self.db.query_row(
            "SELECT COALESCE(sum(length(jpeg)),0) FROM previews",
            [],
            |r| r.get(0),
        )?;
        Ok(PreviewUsage {
            thumbnails: thumbnails as u64,
            standard: self.sized_bytes(PreviewKind::Standard)? as u64,
            one_to_one: self.sized_bytes(PreviewKind::OneToOne)? as u64,
        })
    }
    /// Drops every built preview of `kind` for these files, whatever edit they
    /// were built for.
    pub fn discard(&mut self, paths: &[PathBuf], kind: PreviewKind) -> Result<()> {
        let tx = self.db.transaction()?;
        for path in paths {
            tx.execute(
                "DELETE FROM sized_previews WHERE source_path=? AND kind=?",
                params![key(path), kind.code()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Drops every built preview of `kind`, and every request to keep one.
    pub fn clear(&mut self, kind: PreviewKind) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM sized_previews WHERE kind=?", [kind.code()])?;
        tx.execute("DELETE FROM preview_intent WHERE kind=?", [kind.code()])?;
        tx.commit()?;
        self.db.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    }
    /// Counts a preview as used now, as when it is asked for again.
    pub fn touch_sized(&self, path: &Path, identity: &str, kind: PreviewKind) -> Result<()> {
        self.db.execute(
            "UPDATE sized_previews SET last_used=? WHERE source_path=? AND identity=? AND kind=?",
            params![now(), key(path), identity, kind.code()],
        )?;
        Ok(())
    }
    /// Lightroom's Automatically Discard 1:1 Previews: drops the previews of
    /// `kind` not shown or built for `unused`, and the requests for files left
    /// without one, so an edit does not build them again. Returns how many went.
    pub fn expire(&mut self, kind: PreviewKind, unused: Duration) -> Result<usize> {
        let before = now() - unused.as_secs() as i64;
        let tx = self.db.transaction()?;
        let gone = tx.execute(
            "DELETE FROM sized_previews WHERE kind=?1 AND last_used<?2",
            params![kind.code(), before],
        )?;
        tx.execute(
            "DELETE FROM preview_intent WHERE kind=?1 AND source_path NOT IN (SELECT source_path FROM sized_previews WHERE kind=?1)",
            [kind.code()],
        )?;
        tx.commit()?;
        if gone > 0 {
            self.db.execute_batch("PRAGMA incremental_vacuum;")?;
        }
        Ok(gone)
    }
    /// Records that the user asked for `kind` previews of this catalog photo,
    /// so they are rebuilt when its edit changes.
    pub fn record_intent(
        &mut self,
        catalog: &CatalogLocation,
        photo: PhotoId,
        path: &Path,
        kind: PreviewKind,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO preview_intent VALUES(?,?,?,?)",
            params![catalog_key(catalog), photo.0, key(path), kind.code()],
        )?;
        Ok(())
    }
    /// Whether this catalog photo, at this path, has `kind` previews requested.
    /// A different path means the id now names another photo.
    pub fn has_intent(
        &self,
        catalog: &CatalogLocation,
        photo: PhotoId,
        path: &Path,
        kind: PreviewKind,
    ) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT 1 FROM preview_intent WHERE catalog=? AND photo_id=? AND source_path=? AND kind=?",
                params![catalog_key(catalog), photo.0, key(path), kind.code()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
    /// Forgets the requests for these catalog photos: of `kind`, or of every
    /// kind when the photos left the catalog, so a later photo that reuses an
    /// id starts without any.
    /// Points `photo`'s requests at its file's new path, after the file was
    /// found moved, so its previews are built from there.
    pub fn move_intent(
        &mut self,
        catalog: &CatalogLocation,
        photo: PhotoId,
        path: &Path,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE preview_intent SET source_path=? WHERE catalog=? AND photo_id=?",
            params![key(path), catalog_key(catalog), photo.0],
        )?;
        Ok(())
    }
    pub fn forget_intent(
        &mut self,
        catalog: &CatalogLocation,
        photos: &[PhotoId],
        kind: Option<PreviewKind>,
    ) -> Result<()> {
        let catalog = catalog_key(catalog);
        let tx = self.db.transaction()?;
        for photo in photos {
            match kind {
                Some(kind) => tx.execute(
                    "DELETE FROM preview_intent WHERE catalog=? AND photo_id=? AND kind=?",
                    params![catalog, photo.0, kind.code()],
                )?,
                None => tx.execute(
                    "DELETE FROM preview_intent WHERE catalog=? AND photo_id=?",
                    params![catalog, photo.0],
                )?,
            };
        }
        tx.commit()?;
        Ok(())
    }
    fn sized_bytes(&self, kind: PreviewKind) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COALESCE(sum(length(jpeg)),0) FROM sized_previews WHERE kind=?",
            [kind.code()],
            |r| r.get(0),
        )?)
    }
    /// Evicts the least recently used rows of `kind` alone.
    fn prune_sized(&mut self, kind: PreviewKind, budget: i64) -> Result<()> {
        let total: i64 = self
            .db
            .prepare_cached(
                "SELECT COALESCE(sum(length(jpeg)),0) FROM sized_previews WHERE kind=?1",
            )?
            .query_row([kind.code()], |r| r.get(0))?;
        if total > budget {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let rows: Vec<(i64, i64)> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT rowid, length(jpeg) FROM sized_previews WHERE kind=?1 ORDER BY last_used ASC, source_path ASC",
                )?;
                stmt.query_map([kind.code()], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let to_delete = evict_prefix(rows, budget);
            if !to_delete.is_empty() {
                let mut del =
                    tx.prepare_cached("DELETE FROM sized_previews WHERE rowid=?1 AND kind=?2")?;
                for rowid in to_delete {
                    del.execute(params![rowid, kind.code()])?;
                }
            }
            tx.commit()?;
        }
        self.db.execute_batch("PRAGMA incremental_vacuum(256);")?;
        Ok(())
    }
    fn prune(&mut self, budget: i64) -> Result<()> {
        let total: i64 = self
            .db
            .prepare_cached("SELECT COALESCE(sum(length(jpeg)),0) FROM previews")?
            .query_row([], |r| r.get(0))?;
        if total > budget {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let rows: Vec<(i64, i64)> = {
                let mut stmt = tx.prepare_cached(
                    "SELECT rowid, length(jpeg) FROM previews ORDER BY last_used ASC, source_path ASC",
                )?;
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let to_delete = evict_prefix(rows, budget);
            if !to_delete.is_empty() {
                let mut del = tx.prepare_cached("DELETE FROM previews WHERE rowid=?")?;
                for rowid in to_delete {
                    del.execute([rowid])?;
                }
            }
            tx.commit()?;
        }
        self.db.execute_batch("PRAGMA incremental_vacuum(256);")?;
        Ok(())
    }
}

fn evict_prefix(rows: Vec<(i64, i64)>, budget: i64) -> Vec<i64> {
    let mut total: i64 = rows.iter().map(|(_, len)| *len).sum();
    if total <= budget {
        return Vec::new();
    }
    let mut to_delete = Vec::new();
    for (rowid, len) in rows {
        if total <= budget {
            break;
        }
        total -= len;
        to_delete.push(rowid);
    }
    to_delete
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_hits_offline_previews_and_source_invalidation() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let db = d.path().join("previews.sqlite3");
        let mut cache = PreviewCache::open(&db)?;
        let im = image::RgbImage::from_pixel(24, 16, image::Rgb([120, 70, 40]));
        cache.store(&raw, &Stamp::read(&raw)?, &im)?;
        drop(cache);
        let cache = PreviewCache::open(&db)?;
        assert_eq!(cache.load(&raw)?.unwrap().dimensions(), (24, 16));
        std::fs::remove_file(&raw)?;
        assert!(cache.load(&raw)?.is_some());
        std::fs::write(&raw, b"replacement raw")?;
        assert!(cache.load(&raw)?.is_none());
        Ok(())
    }
    #[test]
    fn same_size_rewrite_is_stale_by_its_modification_time() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let mut cache = PreviewCache::open(&d.path().join("previews.sqlite3"))?;
        let im = image::RgbImage::new(8, 8);
        cache.store(&raw, &Stamp::read(&raw)?, &im)?;
        let modified = std::fs::metadata(&raw)?.modified()?;
        std::fs::write(&raw, b"replaced raw")?;
        std::fs::File::options()
            .write(true)
            .open(&raw)?
            .set_modified(modified + Duration::from_secs(1))?;
        assert!(cache.load(&raw)?.is_none());
        Ok(())
    }
    /// On a network share every read of a photo is a round trip, so a cached
    /// preview must come back without opening its original.
    #[cfg(unix)]
    #[test]
    fn cached_previews_never_open_the_original() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let mut cache = PreviewCache::open(&d.path().join("previews.sqlite3"))?;
        let im = image::RgbImage::new(8, 8);
        cache.store(&raw, &Stamp::read(&raw)?, &im)?;
        cache.store_tagged(&raw, "edit-1", &Stamp::read(&raw)?, &im)?;
        std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o000))?;
        // Unless running as root, which ignores permissions.
        if std::fs::File::open(&raw).is_err() {
            assert!(cache.load(&raw)?.is_some());
            assert!(cache.load_tagged(&raw, "edit-1")?.is_some());
        }
        Ok(())
    }
    #[test]
    fn jpeg_thumbnails_made_before_colour_management_are_made_again() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        let jpeg = d.path().join("photo.jpg");
        std::fs::write(&raw, b"original raw")?;
        std::fs::write(&jpeg, b"original jpeg")?;
        let mut cache = PreviewCache::open(&d.path().join("previews.sqlite3"))?;
        let im = image::RgbImage::new(8, 8);
        for path in [&raw, &jpeg] {
            cache.store(path, &Stamp::read(path)?, &im)?;
        }
        // A JPEG's row as a build that ignored its ICC profile left it.
        cache.db.execute(
            "UPDATE previews SET generation=2 WHERE source_path=?1",
            [jpeg.to_string_lossy()],
        )?;
        assert!(cache.load(&jpeg)?.is_none());
        // A RAW's current row is kept.
        assert!(cache.load(&raw)?.is_some());
        // A RAW's row from before the one engine is made again.
        cache.db.execute("UPDATE previews SET generation=2", [])?;
        assert!(cache.load(&raw)?.is_none());
        cache.store(&jpeg, &Stamp::read(&jpeg)?, &im)?;
        assert!(cache.load(&jpeg)?.is_some());
        Ok(())
    }
    #[test]
    fn edited_previews_are_kept_apart_from_the_embedded_one() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let mut cache = PreviewCache::open(&d.path().join("previews.sqlite3"))?;
        let stamp = Stamp::read(&raw)?;
        let plain = image::RgbImage::from_pixel(24, 16, image::Rgb([120, 70, 40]));
        let edited = image::RgbImage::from_pixel(16, 16, image::Rgb([20, 70, 140]));
        cache.store(&raw, &stamp, &plain)?;
        cache.store_tagged(&raw, "edit-1", &stamp, &edited)?;
        assert_eq!(cache.load(&raw)?.unwrap().dimensions(), (24, 16));
        assert_eq!(
            cache.load_tagged(&raw, "edit-1")?.unwrap().dimensions(),
            (16, 16)
        );
        assert!(cache.load_tagged(&raw, "edit-2")?.is_none());
        Ok(())
    }
    #[test]
    fn reads_survive_failed_maintenance_writes() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let db = d.path().join("previews.sqlite3");
        let mut cache = PreviewCache::open(&db)?;
        let im = image::RgbImage::from_pixel(24, 16, image::Rgb([120, 70, 40]));
        cache.store(&raw, &Stamp::read(&raw)?, &im)?;
        // Another connection holds the write lock: no touch or delete can happen.
        let other = Connection::open(&db)?;
        other.execute_batch("BEGIN IMMEDIATE")?;
        assert!(cache.load(&raw)?.is_some());
        std::fs::write(&raw, b"replacement raw")?;
        assert!(cache.load(&raw)?.is_none());
        other.execute_batch("COMMIT")?;
        assert!(cache.load(&raw)?.is_none());
        Ok(())
    }
    #[test]
    fn broken_entries_budget_and_unrelated_database_are_safe() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.RAF");
        std::fs::write(&raw, b"fixture")?;
        let mut cache = PreviewCache::open(&d.path().join("cache.db"))?;
        let im = image::RgbImage::new(8, 8);
        cache.store(&raw, &Stamp::read(&raw)?, &im)?;
        cache.db.execute("UPDATE previews SET jpeg=X'001122'", [])?;
        assert!(cache.load(&raw)?.is_none());
        cache.store(&raw, &Stamp::read(&raw)?, &im)?;
        cache.prune(0)?;
        assert!(cache.load(&raw)?.is_none());
        let other = d.path().join("other.db");
        let db = Connection::open(&other)?;
        db.execute("CREATE TABLE precious(value TEXT)", [])?;
        drop(db);
        assert!(PreviewCache::open(&other).is_err());
        assert!(
            Connection::open(&other)?
                .prepare("SELECT * FROM precious")
                .is_ok()
        );
        Ok(())
    }
    fn photo(d: &tempfile::TempDir) -> Result<(PathBuf, PreviewCache)> {
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let cache = PreviewCache::open(&d.path().join("previews.sqlite3"))?;
        Ok((raw, cache))
    }
    #[test]
    fn sized_previews_coexist_with_thumbnails_and_each_other() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        cache.store_tagged(&raw, "edit-1", &stamp, &image::RgbImage::new(16, 8))?;
        let standard = image::RgbImage::from_pixel(64, 32, image::Rgb([200, 100, 50]));
        let one_to_one = image::RgbImage::from_pixel(128, 64, image::Rgb([50, 100, 200]));
        cache.store_sized(
            &raw,
            "edit-1",
            PreviewKind::Standard,
            &stamp,
            2048,
            &standard,
        )?;
        cache.store_sized(
            &raw,
            "edit-1",
            PreviewKind::OneToOne,
            &stamp,
            0,
            &one_to_one,
        )?;
        assert_eq!(
            cache.load_tagged(&raw, "edit-1")?.unwrap().dimensions(),
            (16, 8)
        );
        let loaded = cache
            .load_sized(&raw, "edit-1", PreviewKind::Standard)?
            .unwrap();
        assert_eq!(loaded.dimensions(), (64, 32));
        assert!(loaded.get_pixel(10, 10)[0] > 150);
        assert_eq!(
            cache
                .load_sized(&raw, "edit-1", PreviewKind::OneToOne)?
                .unwrap()
                .dimensions(),
            (128, 64)
        );
        assert!(
            cache
                .load_sized(&raw, "edit-2", PreviewKind::Standard)?
                .is_none()
        );
        let usage = cache.usage()?;
        assert!(usage.thumbnails > 0 && usage.standard > 0 && usage.one_to_one > 0);
        Ok(())
    }
    #[test]
    fn sized_previews_follow_the_file_and_survive_it_going_offline() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let im = image::RgbImage::new(32, 16);
        cache.store_sized(
            &raw,
            "a",
            PreviewKind::Standard,
            &Stamp::read(&raw)?,
            2048,
            &im,
        )?;
        std::fs::remove_file(&raw)?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::Standard)?
                .is_some()
        );
        assert!(cache.sized_fresh(&raw, "a", PreviewKind::Standard, 2048)?);
        std::fs::write(&raw, b"replacement raw")?;
        assert!(!cache.sized_fresh(&raw, "a", PreviewKind::Standard, 2048)?);
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::Standard)?
                .is_none()
        );
        // A file changed while its preview was built is refused.
        let old = Stamp::read(&raw)?;
        std::fs::write(&raw, b"third version of the raw")?;
        assert!(
            cache
                .store_sized(&raw, "a", PreviewKind::Standard, &old, 2048, &im)
                .is_err()
        );
        Ok(())
    }
    #[cfg(unix)]
    #[test]
    fn sized_previews_never_open_the_original() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let im = image::RgbImage::new(32, 16);
        cache.store_sized(
            &raw,
            "a",
            PreviewKind::Standard,
            &Stamp::read(&raw)?,
            2048,
            &im,
        )?;
        std::fs::set_permissions(&raw, std::fs::Permissions::from_mode(0o000))?;
        if std::fs::File::open(&raw).is_err() {
            assert!(
                cache
                    .load_sized(&raw, "a", PreviewKind::Standard)?
                    .is_some()
            );
            assert!(cache.sized_fresh(&raw, "a", PreviewKind::Standard, 2048)?);
        }
        Ok(())
    }
    #[test]
    fn freshness_compares_the_requested_edge_not_the_size_reached() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        // A tight crop built for 2048 px only reached 900 px.
        let im = image::RgbImage::new(900, 600);
        cache.store_sized(
            &raw,
            "a",
            PreviewKind::Standard,
            &Stamp::read(&raw)?,
            2048,
            &im,
        )?;
        assert!(cache.sized_fresh(&raw, "a", PreviewKind::Standard, 1440)?);
        assert!(cache.sized_fresh(&raw, "a", PreviewKind::Standard, 2048)?);
        assert!(!cache.sized_fresh(&raw, "a", PreviewKind::Standard, 2560)?);
        assert!(!cache.sized_fresh(&raw, "b", PreviewKind::Standard, 1440)?);
        assert!(!cache.sized_fresh(&raw, "a", PreviewKind::OneToOne, 0)?);
        Ok(())
    }
    #[test]
    fn pruning_one_kind_never_evicts_another() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        let im = image::RgbImage::new(32, 16);
        cache.store(&raw, &stamp, &im)?;
        cache.store_sized(&raw, "a", PreviewKind::Standard, &stamp, 2048, &im)?;
        cache.store_sized(&raw, "b", PreviewKind::Standard, &stamp, 2048, &im)?;
        cache.store_sized(&raw, "a", PreviewKind::OneToOne, &stamp, 0, &im)?;
        cache.prune_sized(PreviewKind::Standard, 0)?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::Standard)?
                .is_none()
        );
        assert!(
            cache
                .load_sized(&raw, "b", PreviewKind::Standard)?
                .is_none()
        );
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::OneToOne)?
                .is_some()
        );
        assert!(cache.load(&raw)?.is_some());
        cache.prune(0)?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::OneToOne)?
                .is_some()
        );
        Ok(())
    }
    #[test]
    fn corrupt_sized_previews_are_discarded() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let im = image::RgbImage::new(32, 16);
        cache.store_sized(
            &raw,
            "a",
            PreviewKind::Standard,
            &Stamp::read(&raw)?,
            2048,
            &im,
        )?;
        cache
            .db
            .execute("UPDATE sized_previews SET jpeg=X'001122'", [])?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::Standard)?
                .is_none()
        );
        assert_eq!(cache.usage()?.standard, 0);
        Ok(())
    }
    #[test]
    fn discarding_and_clearing_are_per_kind() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        let im = image::RgbImage::new(32, 16);
        let catalog = CatalogLocation::File(d.path().join("a.rawmakase"));
        cache.store_sized(&raw, "a", PreviewKind::Standard, &stamp, 2048, &im)?;
        cache.store_sized(&raw, "b", PreviewKind::Standard, &stamp, 2048, &im)?;
        cache.store_sized(&raw, "a", PreviewKind::OneToOne, &stamp, 0, &im)?;
        cache.record_intent(&catalog, PhotoId(1), &raw, PreviewKind::Standard)?;
        cache.record_intent(&catalog, PhotoId(1), &raw, PreviewKind::OneToOne)?;
        cache.discard(std::slice::from_ref(&raw), PreviewKind::Standard)?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::Standard)?
                .is_none()
        );
        assert!(
            cache
                .load_sized(&raw, "b", PreviewKind::Standard)?
                .is_none()
        );
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::OneToOne)?
                .is_some()
        );
        cache.clear(PreviewKind::OneToOne)?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::OneToOne)?
                .is_none()
        );
        assert!(!cache.has_intent(&catalog, PhotoId(1), &raw, PreviewKind::OneToOne)?);
        assert!(cache.has_intent(&catalog, PhotoId(1), &raw, PreviewKind::Standard)?);
        Ok(())
    }
    #[test]
    fn intent_belongs_to_one_photo_of_one_catalog() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let other_raw = d.path().join("other.ARW");
        let a = CatalogLocation::File(d.path().join("a.rawmakase"));
        let b = CatalogLocation::File(d.path().join("b.rawmakase"));
        let standard = PreviewKind::Standard;
        cache.record_intent(&a, PhotoId(1), &raw, standard)?;
        assert!(cache.has_intent(&a, PhotoId(1), &raw, standard)?);
        // A virtual copy of the same file, the same photo in another catalog,
        // or another kind has none.
        assert!(!cache.has_intent(&a, PhotoId(2), &raw, standard)?);
        assert!(!cache.has_intent(&b, PhotoId(1), &raw, standard)?);
        assert!(!cache.has_intent(&a, PhotoId(1), &raw, PreviewKind::OneToOne)?);
        // An id that now names another file.
        assert!(!cache.has_intent(&a, PhotoId(1), &other_raw, standard)?);
        cache.record_intent(&a, PhotoId(1), &raw, PreviewKind::OneToOne)?;
        cache.record_intent(&a, PhotoId(2), &raw, standard)?;
        cache.forget_intent(&a, &[PhotoId(1)], Some(standard))?;
        assert!(!cache.has_intent(&a, PhotoId(1), &raw, standard)?);
        assert!(cache.has_intent(&a, PhotoId(1), &raw, PreviewKind::OneToOne)?);
        // Removed from the catalog: every kind goes.
        cache.forget_intent(&a, &[PhotoId(1)], None)?;
        assert!(!cache.has_intent(&a, PhotoId(1), &raw, PreviewKind::OneToOne)?);
        assert!(cache.has_intent(&a, PhotoId(2), &raw, standard)?);
        Ok(())
    }
    #[test]
    fn a_version_one_cache_gains_the_new_tables_and_stays_version_one() -> Result<()> {
        let d = tempfile::tempdir()?;
        let path = d.path().join("previews.sqlite3");
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let old = Connection::open(&path)?;
        old.execute_batch(&format!("PRAGMA application_id={APP_ID}; PRAGMA user_version=1;
            CREATE TABLE previews(source_path TEXT PRIMARY KEY,source_size INTEGER NOT NULL,modified_ns TEXT NOT NULL,prefix_hash TEXT NOT NULL,generation INTEGER NOT NULL,width INTEGER NOT NULL,height INTEGER NOT NULL,jpeg BLOB NOT NULL,last_used INTEGER NOT NULL);
            CREATE INDEX previews_last_used ON previews(last_used);"))?;
        drop(old);
        let mut cache = PreviewCache::open(&path)?;
        let im = image::RgbImage::new(32, 16);
        cache.store_sized(
            &raw,
            "a",
            PreviewKind::Standard,
            &Stamp::read(&raw)?,
            2048,
            &im,
        )?;
        let version: i64 = cache
            .db
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(version, 1);
        drop(cache);
        // Opening again, as every build does, keeps the rows.
        let cache = PreviewCache::open(&path)?;
        assert!(
            cache
                .load_sized(&raw, "a", PreviewKind::Standard)?
                .is_some()
        );
        Ok(())
    }
    #[test]
    fn unused_previews_of_one_kind_expire() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        let im = image::RgbImage::new(32, 16);
        cache.store_sized(&raw, "old", PreviewKind::OneToOne, &stamp, 0, &im)?;
        cache.store_sized(&raw, "new", PreviewKind::OneToOne, &stamp, 0, &im)?;
        cache.store_sized(&raw, "old", PreviewKind::Standard, &stamp, 2048, &im)?;
        let long_ago = now() - 40 * 86400;
        cache.db.execute(
            "UPDATE sized_previews SET last_used=? WHERE identity='old'",
            [long_ago],
        )?;
        let month = Duration::from_secs(30 * 86400);
        assert_eq!(cache.expire(PreviewKind::OneToOne, month)?, 1);
        assert!(
            cache
                .load_sized(&raw, "old", PreviewKind::OneToOne)?
                .is_none()
        );
        assert!(
            cache
                .load_sized(&raw, "new", PreviewKind::OneToOne)?
                .is_some()
        );
        assert!(
            cache
                .load_sized(&raw, "old", PreviewKind::Standard)?
                .is_some()
        );
        // The request for a file whose previews of that kind all went goes too.
        let catalog = CatalogLocation::File(d.path().join("c.rawmakase"));
        let other = d.path().join("other.ARW");
        std::fs::write(&other, b"other raw")?;
        cache.record_intent(&catalog, PhotoId(1), &raw, PreviewKind::OneToOne)?;
        cache.record_intent(&catalog, PhotoId(2), &other, PreviewKind::OneToOne)?;
        cache.store_sized(
            &other,
            "x",
            PreviewKind::OneToOne,
            &Stamp::read(&other)?,
            0,
            &im,
        )?;
        cache.db.execute(
            "UPDATE sized_previews SET last_used=? WHERE source_path=?",
            params![long_ago, key(&other)],
        )?;
        cache.expire(PreviewKind::OneToOne, month)?;
        assert!(cache.has_intent(&catalog, PhotoId(1), &raw, PreviewKind::OneToOne)?);
        assert!(!cache.has_intent(&catalog, PhotoId(2), &other, PreviewKind::OneToOne)?);
        // Asked for again, a preview starts its time again.
        cache
            .db
            .execute("UPDATE sized_previews SET last_used=?", [long_ago])?;
        cache.touch_sized(&raw, "new", PreviewKind::OneToOne)?;
        assert_eq!(cache.expire(PreviewKind::OneToOne, month)?, 0);
        Ok(())
    }
    #[test]
    fn under_budget_pruning_and_multi_connection_sized_eviction_are_safe() -> Result<()> {
        let d = tempfile::tempdir()?;
        let raw = d.path().join("photo.ARW");
        std::fs::write(&raw, b"original raw")?;
        let db_path = d.path().join("previews.sqlite3");
        let mut cache_a = PreviewCache::open(&db_path)?;
        let cache_b = PreviewCache::open(&db_path)?;
        let stamp = Stamp::read(&raw)?;
        let im = image::RgbImage::from_pixel(32, 16, image::Rgb([100, 120, 140]));
        cache_a.store(&raw, &stamp, &im)?;
        cache_a.store_sized(&raw, "s1", PreviewKind::Standard, &stamp, 2048, &im)?;
        cache_a.store_sized(&raw, "o1", PreviewKind::OneToOne, &stamp, 0, &im)?;
        let before = cache_a.usage()?;
        // Under-budget prunes keep every row untouched.
        cache_a.prune(LIMIT)?;
        cache_a.prune_sized(PreviewKind::Standard, PreviewKind::Standard.budget())?;
        cache_a.prune_sized(PreviewKind::OneToOne, PreviewKind::OneToOne.budget())?;
        assert_eq!(cache_a.usage()?, before);

        // Even if another connection replaces the first rowid with a OneToOne entry,
        // pruning Standard never deletes a OneToOne preview.
        let std_rowid: i64 = cache_a.db.query_row(
            "SELECT rowid FROM sized_previews WHERE kind=?",
            [PreviewKind::Standard.code()],
            |r| r.get(0),
        )?;
        cache_b
            .db
            .execute("DELETE FROM sized_previews WHERE rowid=?", [std_rowid])?;
        cache_b.db.execute(
            "INSERT INTO sized_previews(rowid,source_path,identity,kind,source_size,modified_ns,generation,requested_edge,width,height,jpeg,last_used)
             VALUES(?1,?2,'o2',?3,1,'1',?4,0,32,16,X'FFD8FFD9',1)",
            params![
                std_rowid,
                key(&raw),
                PreviewKind::OneToOne.code(),
                SIZED_GENERATION,
            ],
        )?;
        cache_a.prune_sized(PreviewKind::Standard, 0)?;
        let remaining_one_to_one: i64 = cache_a.db.query_row(
            "SELECT count(*) FROM sized_previews WHERE kind=?",
            [PreviewKind::OneToOne.code()],
            |r| r.get(0),
        )?;
        assert_eq!(remaining_one_to_one, 2);
        Ok(())
    }

    #[test]
    fn under_budget_prune_reclaims_freelist_pages() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut cache = PreviewCache::open(&d.path().join("previews.sqlite3"))?;
        cache.db.execute(
            "INSERT INTO previews VALUES('big',1,'1','',?1,32,16,zeroblob(200000),1)",
            [GENERATION],
        )?;
        cache
            .db
            .execute("DELETE FROM previews WHERE source_path='big'", [])?;
        let freelist: i64 = cache
            .db
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        assert!(freelist > 0);
        // `execute_batch` steps `PRAGMA incremental_vacuum(256)` once per call, so an
        // under-budget prune strictly reduces `freelist_count` rather than skipping it.
        cache.prune(LIMIT)?;
        let after_thumb: i64 = cache
            .db
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        assert!(
            after_thumb < freelist,
            "expected under-budget prune to shrink freelist ({after_thumb} < {freelist})"
        );

        cache.db.execute(
            "INSERT INTO sized_previews VALUES('big','id',?1,1,'1',?2,2048,32,16,zeroblob(200000),1)",
            params![PreviewKind::Standard.code(), SIZED_GENERATION],
        )?;
        cache
            .db
            .execute("DELETE FROM sized_previews WHERE source_path='big'", [])?;
        let freelist: i64 = cache
            .db
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        assert!(freelist > 0);
        cache.prune_sized(PreviewKind::Standard, PreviewKind::Standard.budget())?;
        let after_sized: i64 = cache
            .db
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
        assert!(
            after_sized < freelist,
            "expected under-budget prune_sized to shrink freelist ({after_sized} < {freelist})"
        );
        Ok(())
    }
    #[test]
    fn off_lock_thumbnail_roundtrip_limits_and_offline_survival() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        let im = image::RgbImage::from_pixel(24, 16, image::Rgb([120, 70, 40]));
        let encoded = PreviewCache::encode_thumbnail(&im)?;
        assert_eq!((encoded.width(), encoded.height()), (24, 16));
        assert!(!encoded.jpeg().is_empty());
        cache.store_tagged_jpeg(&raw, "edit-1", &stamp, &encoded)?;
        let loaded_jpeg = cache.load_tagged_jpeg(&raw, "edit-1")?.unwrap();
        let decoded = PreviewCache::decode_thumbnail(&loaded_jpeg).unwrap();
        assert_eq!(decoded.dimensions(), (24, 16));
        assert!(decoded.get_pixel(4, 4)[0] > 100);

        assert!(PreviewCache::encode_thumbnail(&image::RgbImage::new(1025, 8)).is_err());
        assert!(PreviewCache::encode_thumbnail(&image::RgbImage::new(8, 1025)).is_err());
        let oversized = PreviewCache::encode_sized(&image::RgbImage::new(1025, 8))?;
        assert!(
            cache
                .store_tagged_jpeg(&raw, "too-wide", &stamp, &oversized)
                .is_err()
        );
        assert!(PreviewCache::decode_thumbnail(oversized.jpeg()).is_none());
        assert!(PreviewCache::decode_thumbnail(b"\x00\x11\x22").is_none());

        std::fs::remove_file(&raw)?;
        let offline_jpeg = cache.load_tagged_jpeg(&raw, "edit-1")?.unwrap();
        assert_eq!(
            PreviewCache::decode_thumbnail(&offline_jpeg)
                .unwrap()
                .dimensions(),
            (24, 16)
        );
        Ok(())
    }
    #[test]
    fn off_lock_sized_roundtrip_and_dimension_validation() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        assert!(PreviewCache::encode_sized(&image::RgbImage::new(0, 0)).is_err());

        let standard = image::RgbImage::from_pixel(64, 32, image::Rgb([200, 100, 50]));
        let one_to_one = image::RgbImage::from_pixel(128, 64, image::Rgb([50, 100, 200]));
        let encoded_standard = PreviewCache::encode_sized(&standard)?;
        let encoded_one_to_one = PreviewCache::encode_sized(&one_to_one)?;
        cache.store_sized_jpeg(
            &raw,
            "edit-1",
            PreviewKind::Standard,
            &stamp,
            2048,
            &encoded_standard,
        )?;
        cache.store_sized_jpeg(
            &raw,
            "edit-1",
            PreviewKind::OneToOne,
            &stamp,
            0,
            &encoded_one_to_one,
        )?;
        let loaded_standard = cache
            .load_sized_jpeg(&raw, "edit-1", PreviewKind::Standard)?
            .unwrap();
        assert_eq!(
            PreviewCache::decode_sized(&loaded_standard)
                .unwrap()
                .dimensions(),
            (64, 32)
        );
        let loaded_one_to_one = cache
            .load_sized_jpeg(&raw, "edit-1", PreviewKind::OneToOne)?
            .unwrap();
        assert_eq!(
            PreviewCache::decode_sized(&loaded_one_to_one)
                .unwrap()
                .dimensions(),
            (128, 64)
        );

        // Stored dimensions that disagree with the JPEG fail off-lock decoding.
        cache.db.execute(
            "UPDATE sized_previews SET width=99 WHERE source_path=? AND identity=? AND kind=?",
            params![key(&raw), "edit-1", PreviewKind::Standard.code()],
        )?;
        let mismatched = cache
            .load_sized_jpeg(&raw, "edit-1", PreviewKind::Standard)?
            .unwrap();
        assert_eq!(mismatched.width(), 99);
        assert!(PreviewCache::decode_sized(&mismatched).is_none());
        cache.delete_sized(&raw, "edit-1", PreviewKind::Standard)?;
        assert!(
            cache
                .load_sized_jpeg(&raw, "edit-1", PreviewKind::Standard)?
                .is_none()
        );

        std::fs::remove_file(&raw)?;
        let offline = cache
            .load_sized_jpeg(&raw, "edit-1", PreviewKind::OneToOne)?
            .unwrap();
        assert_eq!(
            PreviewCache::decode_sized(&offline).unwrap().dimensions(),
            (128, 64)
        );
        Ok(())
    }
    #[test]
    fn off_lock_stale_stamp_rejection_and_eviction() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let old_stamp = Stamp::read(&raw)?;
        let thumb = PreviewCache::encode_thumbnail(&image::RgbImage::new(16, 16))?;
        let sized = PreviewCache::encode_sized(&image::RgbImage::new(32, 16))?;

        std::fs::write(&raw, b"replaced while encoding")?;
        assert!(
            cache
                .store_tagged_jpeg(&raw, "edit", &old_stamp, &thumb)
                .is_err()
        );
        assert!(
            cache
                .store_sized_jpeg(
                    &raw,
                    "edit",
                    PreviewKind::Standard,
                    &old_stamp,
                    2048,
                    &sized
                )
                .is_err()
        );
        assert!(cache.load_tagged_jpeg(&raw, "edit")?.is_none());
        assert!(
            cache
                .load_sized_jpeg(&raw, "edit", PreviewKind::Standard)?
                .is_none()
        );

        let stamp = Stamp::read(&raw)?;
        cache.store_tagged_jpeg(&raw, "edit", &stamp, &thumb)?;
        cache.store_sized_jpeg(&raw, "edit", PreviewKind::Standard, &stamp, 2048, &sized)?;
        std::fs::write(&raw, b"rewritten later on disk")?;
        assert!(cache.load_tagged_jpeg(&raw, "edit")?.is_none());
        assert!(
            cache
                .load_sized_jpeg(&raw, "edit", PreviewKind::Standard)?
                .is_none()
        );
        assert_eq!(cache.usage()?, PreviewUsage::default());

        let stamp = Stamp::read(&raw)?;
        cache.store_tagged_jpeg(&raw, "edit", &stamp, &thumb)?;
        cache.store_sized_jpeg(&raw, "edit", PreviewKind::Standard, &stamp, 2048, &sized)?;
        cache.db.execute("UPDATE previews SET generation=1", [])?;
        cache
            .db
            .execute("UPDATE sized_previews SET generation=1", [])?;
        assert!(cache.load_tagged_jpeg(&raw, "edit")?.is_none());
        assert!(
            cache
                .load_sized_jpeg(&raw, "edit", PreviewKind::Standard)?
                .is_none()
        );
        assert_eq!(cache.usage()?, PreviewUsage::default());
        Ok(())
    }
    #[test]
    fn off_lock_corrupt_jpeg_cleanup() -> Result<()> {
        let d = tempfile::tempdir()?;
        let (raw, mut cache) = photo(&d)?;
        let stamp = Stamp::read(&raw)?;
        let thumb = PreviewCache::encode_thumbnail(&image::RgbImage::new(16, 16))?;
        let sized = PreviewCache::encode_sized(&image::RgbImage::new(32, 16))?;
        cache.store_tagged_jpeg(&raw, "edit", &stamp, &thumb)?;
        cache.store_sized_jpeg(&raw, "edit", PreviewKind::Standard, &stamp, 2048, &sized)?;

        cache.db.execute("UPDATE previews SET jpeg=X'001122'", [])?;
        cache
            .db
            .execute("UPDATE sized_previews SET jpeg=X'001122'", [])?;

        let corrupt_thumb = cache.load_tagged_jpeg(&raw, "edit")?.unwrap();
        assert!(PreviewCache::decode_thumbnail(&corrupt_thumb).is_none());
        cache.delete_tagged(&raw, "edit")?;
        assert_eq!(cache.usage()?.thumbnails, 0);
        assert!(cache.load_tagged_jpeg(&raw, "edit")?.is_none());

        let corrupt_sized = cache
            .load_sized_jpeg(&raw, "edit", PreviewKind::Standard)?
            .unwrap();
        assert!(PreviewCache::decode_sized(&corrupt_sized).is_none());
        cache.delete_sized(&raw, "edit", PreviewKind::Standard)?;
        assert_eq!(cache.usage()?.standard, 0);
        assert!(
            cache
                .load_sized_jpeg(&raw, "edit", PreviewKind::Standard)?
                .is_none()
        );
        Ok(())
    }
}
