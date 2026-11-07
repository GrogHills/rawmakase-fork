//! The catalog's record of smart previews (`smart_previews`): which photos
//! have one, its file, and the original it was built from.
//!
//! A smart preview is a smaller copy of a photo that RAWmakase built while the
//! original was online, so the photo can be edited while the original is
//! offline. It is not a cache: once the original is offline, it can't be built
//! again, so the catalog keeps track of it for as long as its photo exists. A
//! row is removed only with its photo (Remove from Catalog, Move to Trash),
//! when the user discards it, or when a newer build replaces it. Each of those
//! deletes the file only after the transaction that stops naming it commits
//! ([`Write::delete_after_commit`]).
//!
//! Rows are keyed by master photo, so virtual copies share their master's,
//! and moving or renaming the photo changes nothing here. The files are in the
//! catalog's store folder ([`Catalog::smart_preview_folder`]). The catalog
//! only records their names and what their headers say. It never reads or
//! writes their contents: the format belongs to `rawmakase-decode`, which the
//! catalog can't reach.
//!
//! Smart preview files are RAWmakase's own derived files, not a user's photos,
//! so they are deleted outright, as the preview cache's are.
use super::db::{Reads, Write, sql};
use super::value::row;
use super::{Catalog, CatalogLocation, PhotoId};
use crate::storage::Identity;
use anyhow::{Context, Result, ensure};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// The extension of a smart preview file.
const EXTENSION: &str = "rmsp";

/// A smart preview just built, as its file's header describes it, to record
/// with [`Catalog::record_smart_preview`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltSmartPreview {
    /// Its name in the store folder ([`smart_preview_file_name`]).
    pub file: String,
    /// The version of the file's format.
    pub format_version: u32,
    /// The original it was built from, as read when the build began.
    pub original: Identity,
    /// The original's width and height, in pixels.
    pub original_size: (u32, u32),
    /// The smart preview's width and height, in pixels.
    pub size: (u32, u32),
}

/// A photo's smart preview, as the catalog records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmartPreview {
    /// The master photo it is of.
    pub photo: PhotoId,
    /// Its file, in the store folder.
    pub path: PathBuf,
    pub format_version: u32,
    /// The original it was built from.
    pub original: Identity,
    /// The original's width and height, in pixels.
    pub original_size: (u32, u32),
    /// Its width and height, in pixels.
    pub size: (u32, u32),
    /// The file's length when it was recorded.
    pub bytes: u64,
    /// When it was recorded, in UTC, as catalogs store times
    /// ([`rawmakase_model::time::utc_text`]).
    pub built_at: String,
}

impl SmartPreview {
    /// Whether the original, seen online as `seen`, is no longer the photo
    /// this was built from ([`Identity::same_photo`]). A stale smart preview
    /// is kept: the caller decides what to do with it.
    pub fn is_stale(&self, seen: &Identity) -> bool {
        !self.original.same_photo(seen)
    }
}

/// The name of the file of a smart preview of `master` built from
/// `original`: `<photo id>-<identity key>.rmsp`, unique per photo and version
/// of its original. The builder writes it under a temporary name, then
/// renames it to this without replacing anything.
pub fn smart_preview_file_name(master: PhotoId, original: &Identity) -> String {
    format!("{}-{}.{EXTENSION}", master.0, original.key())
}

/// The folder a catalog at `location` keeps its smart previews in: beside a
/// catalog file, named after it (`Photos.rawmakase` keeps them in
/// `Photos.rawmakase-smart-previews`). A catalog that isn't a file would keep
/// them in RAWmakase's data folder (`storage::data_dir`).
pub(super) fn folder(location: &CatalogLocation) -> PathBuf {
    match location {
        CatalogLocation::File(path) => {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push("-smart-previews");
            path.with_file_name(name)
        }
    }
}

/// Whether `file` is a smart preview's name: a single plain file name with
/// the extension, nothing that leads out of the store folder.
fn is_file_name(file: &str) -> bool {
    let path = Path::new(file);
    let mut parts = path.components();
    matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
        && !file.starts_with('.')
        && path.extension().is_some_and(|e| e == EXTENSION)
}

row! {
    struct Stored {
        photo: PhotoId,
        file: String,
        identity: String,
        original_width: i64,
        original_height: i64,
        width: i64,
        height: i64,
        bytes: i64,
        version: i64,
        built_at: String,
    }
}

row! {
    /// A row's photo and file name.
    struct Named {
        photo: PhotoId,
        file: String,
    }
}

impl Stored {
    fn read(self, folder: &Path) -> Result<SmartPreview> {
        Ok(SmartPreview {
            photo: self.photo,
            path: folder.join(&self.file),
            format_version: u32::try_from(self.version)?,
            original: serde_json::from_str(&self.identity)
                .context("A smart preview's original can't be read")?,
            original_size: (
                u32::try_from(self.original_width)?,
                u32::try_from(self.original_height)?,
            ),
            size: (u32::try_from(self.width)?, u32::try_from(self.height)?),
            bytes: u64::try_from(self.bytes)?,
            built_at: self.built_at,
        })
    }
}

/// The master of `photo`, if it is in the catalog.
fn master_of(db: &impl Reads, photo: PhotoId) -> Result<Option<PhotoId>> {
    db.read_optional(
        sql!("SELECT COALESCE(master_id, id) FROM photos WHERE id=?"),
        &[&photo],
    )
}

/// Removes the rows of `master`'s family, keyed by the master or by one of
/// its copies (a release before this one may have made a master a copy),
/// and deletes their files after the commit, except `keep`. Returns whether
/// there were any.
fn forget_family(w: &mut Write<'_>, master: PhotoId, keep: Option<&str>) -> Result<bool> {
    let rows: Vec<Named> = w.read(
        sql!(
            "SELECT s.photo, s.file FROM smart_previews s JOIN photos p ON p.id = s.photo
             WHERE p.id = ? OR p.master_id = ?"
        ),
        &[&master, &master],
    )?;
    let folder = folder(w.location());
    for row in &rows {
        w.execute(
            sql!("DELETE FROM smart_previews WHERE photo=?"),
            &[&row.photo],
        )?;
        if keep != Some(row.file.as_str()) {
            w.delete_after_commit(folder.join(&row.file));
        }
    }
    Ok(!rows.is_empty())
}

/// Removes `photo`'s own row, as the photo leaves the catalog, and deletes
/// its file after the commit. A virtual copy normally has none.
pub(super) fn forget_photo(w: &mut Write<'_>, photo: PhotoId) -> Result<()> {
    let file: Option<String> = w.read_optional(
        sql!("SELECT file FROM smart_previews WHERE photo=?"),
        &[&photo],
    )?;
    if let Some(file) = file {
        w.execute(sql!("DELETE FROM smart_previews WHERE photo=?"), &[&photo])?;
        w.delete_after_commit(folder(w.location()).join(file));
    }
    Ok(())
}

/// Gives the smart preview of `master` to `now`, its copy that becomes the
/// master (Set Copy as Master). The file keeps its name.
pub(super) fn pass_to(w: &mut Write<'_>, master: PhotoId, now: PhotoId) -> Result<()> {
    // A row of the copy's own, which nothing records, would be its family's
    // second; the master's is the one kept.
    let had: Option<i64> = w.read_optional(
        sql!("SELECT 1 FROM smart_previews WHERE photo=?"),
        &[&master],
    )?;
    if had.is_some() {
        forget_photo(w, now)?;
    }
    w.execute(
        sql!("UPDATE smart_previews SET photo=? WHERE photo=?"),
        &[&now, &master],
    )?;
    Ok(())
}

/// The file names of every smart preview recorded in `db`.
pub(super) fn files(db: &impl Reads) -> Result<Vec<(PhotoId, String)>> {
    let rows: Vec<Named> = db.read(
        sql!("SELECT photo, file FROM smart_previews ORDER BY photo"),
        &[],
    )?;
    Ok(rows.into_iter().map(|r| (r.photo, r.file)).collect())
}

impl Catalog {
    /// The folder this catalog keeps its smart preview files in.
    pub fn smart_preview_folder(&self) -> PathBuf {
        folder(&self.location)
    }

    /// Records the smart preview of `photo` (its master's, for a virtual
    /// copy) just built in [`file`](BuiltSmartPreview::file) of the store
    /// folder. In one transaction, it replaces the row of the photo's
    /// previous one, whose file is deleted only after that commits (unless
    /// it is the same file, built again from the same original).
    ///
    /// Refused, recording nothing, when the name isn't a smart preview's,
    /// the file isn't there, or the photo has left the catalog. The new file
    /// then stays where it is, for the caller to delete or record again.
    pub fn record_smart_preview(
        &mut self,
        photo: PhotoId,
        built: &BuiltSmartPreview,
    ) -> Result<SmartPreview> {
        ensure!(
            is_file_name(&built.file),
            "Not a smart preview's file name: {}",
            built.file
        );
        let folder = self.smart_preview_folder();
        let path = folder.join(&built.file);
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("The smart preview {} is not there", path.display()))?;
        ensure!(metadata.is_file(), "Not a file: {}", path.display());
        let identity = serde_json::to_string(&built.original)?;
        let bytes = i64::try_from(metadata.len())?;
        let built_at = rawmakase_model::time::now_text();
        let master = self.db.write(|w| {
            let master = master_of(w, photo)?.context("The photo is no longer in the catalog")?;
            forget_family(w, master, Some(&built.file))?;
            w.execute(
                sql!(
                    "INSERT INTO smart_previews
                     (photo, file, identity, original_width, original_height, width, height,
                      bytes, version, built_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ),
                &[
                    &master,
                    &built.file,
                    &identity,
                    &i64::from(built.original_size.0),
                    &i64::from(built.original_size.1),
                    &i64::from(built.size.0),
                    &i64::from(built.size.1),
                    &bytes,
                    &i64::from(built.format_version),
                    &built_at,
                ],
            )?;
            Ok(master)
        })?;
        Ok(SmartPreview {
            photo: master,
            path,
            format_version: built.format_version,
            original: built.original.clone(),
            original_size: built.original_size,
            size: built.size,
            bytes: metadata.len(),
            built_at,
        })
    }

    /// The smart preview of `photo`, its master's for a virtual copy. Its
    /// file may be missing (deleted outside RAWmakase, or the catalog moved
    /// without its store folder): the caller checks when it opens it.
    pub fn smart_preview(&self, photo: PhotoId) -> Result<Option<SmartPreview>> {
        let stored: Option<Stored> = self.db.read_optional(
            sql!(
                "SELECT s.photo, s.file, s.identity, s.original_width, s.original_height,
                    s.width, s.height, s.bytes, s.version, s.built_at
                 FROM smart_previews s JOIN photos p ON p.id = s.photo
                 WHERE COALESCE(p.master_id, p.id) =
                     (SELECT COALESCE(q.master_id, q.id) FROM photos q WHERE q.id = ?)
                 ORDER BY CASE WHEN p.master_id IS NULL THEN 0 ELSE 1 END, s.built_at DESC
                 LIMIT 1"
            ),
            &[&photo],
        )?;
        stored
            .map(|s| s.read(&self.smart_preview_folder()))
            .transpose()
    }

    /// Every photo with a smart preview: the masters with one, and their
    /// virtual copies.
    pub fn photos_with_smart_previews(&self) -> Result<BTreeSet<PhotoId>> {
        let photos: Vec<PhotoId> = self.db.read(
            sql!(
                "SELECT p.id FROM photos p
                 WHERE COALESCE(p.master_id, p.id) IN
                     (SELECT COALESCE(q.master_id, q.id)
                      FROM smart_previews s JOIN photos q ON q.id = s.photo)"
            ),
            &[],
        )?;
        Ok(photos.into_iter().collect())
    }

    /// Discards the smart previews of `photos` (their masters', for virtual
    /// copies), in one transaction; their files are deleted after it
    /// commits. Returns the masters whose smart previews were discarded.
    pub fn discard_smart_previews(&mut self, photos: &[PhotoId]) -> Result<Vec<PhotoId>> {
        self.db.write(|w| {
            let mut masters = BTreeSet::new();
            for &photo in photos {
                if let Some(master) = master_of(w, photo)? {
                    masters.insert(master);
                }
            }
            let mut discarded = Vec::new();
            for master in masters {
                if forget_family(w, master, None)? {
                    discarded.push(master);
                }
            }
            Ok(discarded)
        })
    }

    /// The bytes every smart preview's file took when it was recorded.
    pub fn smart_previews_size(&self) -> Result<u64> {
        row! {
            struct Total {
                bytes: i64,
            }
        }
        let total: Total = self.db.read_one(
            sql!("SELECT COALESCE(sum(bytes), 0) FROM smart_previews"),
            &[],
        )?;
        Ok(u64::try_from(total.bytes)?)
    }

    /// The smart preview files in the store folder that no row names: left by
    /// a deletion that failed, a release before smart previews that removed
    /// their photos, or a build whose photo left the catalog. Only RAWmakase's
    /// own files are listed, so their owner may delete them; a file a build
    /// has written but not yet recorded is listed too, so the caller asks only
    /// while no build is running.
    pub fn smart_preview_orphans(&self) -> Result<Vec<PathBuf>> {
        let folder = self.smart_preview_folder();
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let named: BTreeSet<String> = files(&self.db)?.into_iter().map(|(_, f)| f).collect();
        let mut orphans = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if is_file_name(&name) && !named.contains(&name) && entry.file_type()?.is_file() {
                orphans.push(entry.path());
            }
        }
        orphans.sort();
        Ok(orphans)
    }
}
