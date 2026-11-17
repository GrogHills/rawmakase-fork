//! Editable previews in the catalog (docs/editable-previews.md): a row per
//! master that virtual copies share, which goes with its photo whichever
//! release removes it, follows Set Copy as Master (an older release's
//! promotion too, when the catalog opens), goes out of date when the original
//! comes back as another file, and travels with its photo in Export as
//! Catalog.
use super::editable_previews::{carry, folder};
use super::*;
use crate::storage::Identity;
use rusqlite::Connection;
use std::path::PathBuf;

/// A catalog of folder `photos` holding `files`, with each photo's id.
struct Fx {
    _dir: tempfile::TempDir,
    dir: PathBuf,
    path: PathBuf,
    cat: Catalog,
    photos: Vec<(PhotoId, PathBuf)>,
}

fn fixture(files: &[&str]) -> Result<Fx> {
    let d = tempfile::tempdir()?;
    let dir = d.path().to_path_buf();
    let photos = dir.join("photos");
    std::fs::create_dir_all(&photos)?;
    for name in files {
        std::fs::write(photos.join(name), name.as_bytes())?;
    }
    let path = dir.join("Photos.rawmakase");
    let mut cat = Catalog::create(&path)?;
    cat.add_folder(&photos)?;
    let photos = files
        .iter()
        .map(|name| {
            let p = cat.photos()?;
            let photo = p.iter().find(|p| p.filename == *name).unwrap();
            Ok((photo.id, photo.path.clone()))
        })
        .collect::<Result<_>>()?;
    Ok(Fx {
        _dir: d,
        dir,
        path,
        cat,
        photos,
    })
}

/// Records a copy of photo `id`, whose original is at `original`, with a
/// file standing for it; returns the file.
fn record(cat: &mut Catalog, id: PhotoId, original: &Path) -> Result<PathBuf> {
    let identity = Identity::read(original)?;
    let master = cat.editable_preview_master(id)?;
    let file = super::editable_preview_file_name(master, &identity);
    let folder = cat.editable_previews_folder();
    std::fs::create_dir_all(&folder)?;
    std::fs::write(folder.join(&file), b"editable preview")?;
    assert_eq!(
        cat.record_editable_preview(id, &file, RAWMAKASE_DNG, &identity)?,
        None
    );
    Ok(folder.join(file))
}

impl Catalog {
    /// The master whose row photo `id`'s copy is.
    fn editable_preview_master(&self, id: PhotoId) -> Result<PhotoId> {
        Ok(self
            .db_for_tests()
            .query_row(
                "SELECT COALESCE(master_id, id) FROM photos WHERE id=?",
                [id.0],
                |r| r.get(0),
            )
            .map(PhotoId)?)
    }
}

/// Which photos have rows, by id.
fn rows(cat: &Catalog) -> Result<Vec<i64>> {
    let mut statement = cat
        .db_for_tests()
        .prepare("SELECT photo FROM editable_previews ORDER BY photo")?;
    let rows = statement
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Set Copy as Master as a release that doesn't know the table does it.
fn promote_as_an_older_release(db: &Connection, copy: PhotoId, master: PhotoId) -> Result<()> {
    db.execute(
        "UPDATE photos SET master_id=?1 WHERE master_id=?2 AND id<>?1",
        [copy.0, master.0],
    )?;
    db.execute(
        "UPDATE photos SET master_id=?, copy_name='Copy 1' WHERE id=?",
        [copy.0, master.0],
    )?;
    db.execute(
        "UPDATE photos SET master_id=NULL, copy_name='' WHERE id=?",
        [copy.0],
    )?;
    Ok(())
}

#[test]
fn virtual_copies_open_from_their_masters_copy() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let copy = fx.cat.create_virtual_copy(a)?;
    record(&mut fx.cat, copy, &path)?;
    // Recorded for the master, whichever photo of the family built it.
    assert_eq!(rows(&fx.cat)?, [a.0]);
    let preview = fx.cat.editable_preview(copy)?.unwrap();
    assert_eq!(fx.cat.editable_preview(a)?.unwrap(), preview);
    assert_eq!(preview.master, a);
    assert_eq!(preview.format, RAWMAKASE_DNG);
    assert_eq!(preview.original, Identity::read(&path)?);
    assert!(!preview.out_of_date);
    assert_eq!(fx.cat.editable_previews()?, [preview]);
    Ok(())
}

#[test]
fn building_again_replaces_the_copy_and_hands_back_the_old_file() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let first = record(&mut fx.cat, a, &path)?;
    fx.cat.mark_editable_preview_out_of_date(a)?;
    let identity = Identity {
        modified_ns: 1,
        ..Identity::read(&path)?
    };
    let file = super::editable_preview_file_name(a, &identity);
    let old = fx
        .cat
        .record_editable_preview(a, &file, RAWMAKASE_DNG, &identity)?;
    assert_eq!(
        old.map(|f| fx.cat.editable_previews_folder().join(f)),
        Some(first)
    );
    let preview = fx.cat.editable_preview(a)?.unwrap();
    assert_eq!(
        (preview.file.as_str(), preview.out_of_date),
        (file.as_str(), false)
    );
    // The same name again hands nothing back: it is the file just written.
    assert_eq!(
        fx.cat
            .record_editable_preview(a, &file, RAWMAKASE_DNG, &identity)?,
        None
    );
    Ok(())
}

#[test]
fn a_name_that_is_not_a_plain_file_name_is_refused_and_never_followed() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let identity = Identity::read(&path)?;
    for name in [
        "",
        ".",
        "..",
        "../a.dng",
        "sub/a.dng",
        "a\\b.dng",
        ".hidden.dng",
    ] {
        assert!(
            fx.cat
                .record_editable_preview(a, name, RAWMAKASE_DNG, &identity)
                .is_err(),
            "{name:?}"
        );
    }
    // A row written another way that names a file elsewhere names nothing.
    fx.cat.db_for_tests().execute(
        "INSERT INTO editable_previews(photo, file, format, original, built_at)
         VALUES (?, '../Photos.rawmakase', 'rawmakase-dng-1', ?, '2026-11-17 00:00:00')",
        rusqlite::params![a.0, serde_json::to_string(&identity)?],
    )?;
    let preview = fx.cat.editable_preview(a)?.unwrap();
    let folder = fx.cat.editable_previews_folder();
    assert_eq!(preview.path_in(&folder), None);
    assert_eq!(preview.usable(&folder, |_| Ok(())), None);
    // Nor is it handed back for deletion, whether replaced or discarded.
    assert_eq!(
        fx.cat
            .record_editable_preview(a, "a.dng", RAWMAKASE_DNG, &identity)?,
        None
    );
    fx.cat.db_for_tests().execute(
        "UPDATE editable_previews SET file='../victim.NEF' WHERE photo=?",
        rusqlite::params![a.0],
    )?;
    assert_eq!(
        fx.cat.discard_editable_previews(&[a])?,
        Vec::<String>::new()
    );
    assert!(
        fx.cat.editable_preview(a)?.is_none(),
        "the row is forgotten"
    );
    Ok(())
}

#[test]
fn the_copies_folder_is_beside_the_catalog() {
    let location = CatalogLocation::File(PathBuf::from("shoots").join("Photos.rawmakase"));
    assert_eq!(
        folder(&location),
        PathBuf::from("shoots").join("Photos.rawmakase-editable-previews")
    );
    let original = Identity {
        size: 12,
        modified_ns: 34,
        prefix_hash: "00ff".into(),
    };
    assert_eq!(
        super::editable_preview_file_name(PhotoId(7), &original),
        "7-00ff-12-34.dng"
    );
}

#[test]
fn removing_a_virtual_copy_keeps_its_masters_copy() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let copy = fx.cat.create_virtual_copy(a)?;
    let file = record(&mut fx.cat, a, &path)?;
    fx.cat.remove_virtual_copy(copy)?;
    assert_eq!(rows(&fx.cat)?, [a.0]);
    assert!(fx.cat.editable_preview_orphans()?.is_empty());
    assert!(file.is_file());
    Ok(())
}

#[test]
fn removing_the_folder_removes_the_row_and_leaves_its_file_to_delete() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let file = record(&mut fx.cat, a, &path)?;
    let folder = fx.cat.folders()?[0].id;
    fx.cat.remove_folders(&[folder])?;
    assert!(rows(&fx.cat)?.is_empty());
    assert_eq!(fx.cat.editable_preview_orphans()?, [file]);
    Ok(())
}

/// Set Copy as Master moves the row in its transaction: the family keeps
/// its copy, and removing the former master, now a virtual copy, keeps it.
#[test]
fn set_copy_as_master_moves_the_row() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let copy = fx.cat.create_virtual_copy(a)?;
    let file = record(&mut fx.cat, a, &path)?;
    let before = fx.cat.editable_preview(a)?.unwrap();
    fx.cat.set_copy_as_master(copy)?;
    assert_eq!(rows(&fx.cat)?, [copy.0]);
    assert_eq!(
        fx.cat.editable_preview(a)?.unwrap(),
        EditablePreview {
            master: copy,
            ..before.clone()
        }
    );
    fx.cat.remove_virtual_copy(a)?;
    let after = fx.cat.editable_preview(copy)?.unwrap();
    assert_eq!(after.file, before.file);
    assert!(fx.cat.editable_preview_orphans()?.is_empty());
    assert!(file.is_file());
    Ok(())
}

/// An older release promotes a virtual copy without knowing the table, so
/// the row stays on what is now a virtual copy; the next open moves it.
#[test]
fn a_row_an_older_release_left_on_a_virtual_copy_moves_to_the_master_on_open() -> Result<()> {
    let mut fx = fixture(&["a.NEF", "b.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let (b, b_path) = fx.photos[1].clone();
    let copy = fx.cat.create_virtual_copy(a)?;
    let b_copy = fx.cat.create_virtual_copy(b)?;
    record(&mut fx.cat, a, &path)?;
    record(&mut fx.cat, b, &b_path)?;
    let Fx { path: at, cat, .. } = &mut fx;
    promote_as_an_older_release(cat.db_for_tests(), copy, a)?;
    // A stray row on a virtual copy whose master has its own: the master's
    // stays.
    promote_as_an_older_release(cat.db_for_tests(), b_copy, b)?;
    cat.db_for_tests().execute(
        "INSERT INTO editable_previews(photo, file, format, original, built_at)
         SELECT ?, 'newer.dng', format, original, built_at FROM editable_previews WHERE photo=?",
        [b_copy.0, b.0],
    )?;
    assert_eq!(rows(cat)?, [a.0, b.0, b_copy.0]);
    let reopened = Catalog::open(at.as_path())?;
    assert_eq!(rows(&reopened)?, [copy.0, b_copy.0]);
    assert_eq!(reopened.editable_preview(a)?.unwrap().master, copy);
    assert_eq!(reopened.editable_preview(b)?.unwrap().file, "newer.dng");
    Ok(())
}

/// The one sequence the repair can't undo: an older release promotes a copy
/// and then removes the former master before this release opens the catalog.
/// The cascade takes the family's row with that photo; its file is left for
/// the next Build or Discard, and the photo has no copy.
#[test]
fn an_older_release_promoting_then_removing_the_former_master_leaves_no_row() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let copy = fx.cat.create_virtual_copy(a)?;
    let file = record(&mut fx.cat, a, &path)?;
    let db = fx.cat.db_for_tests();
    promote_as_an_older_release(db, copy, a)?;
    db.execute("DELETE FROM photos WHERE id=?", [a.0])?;
    let reopened = Catalog::open(fx.path.as_path())?;
    assert!(rows(&reopened)?.is_empty());
    assert_eq!(reopened.editable_preview(copy)?, None);
    assert_eq!(reopened.editable_preview_orphans()?, [file]);
    Ok(())
}

/// A release that doesn't know the table removes a photo with SQL alone:
/// the cascade drops the row, and the file is left for the next Build.
#[test]
fn removing_a_photo_with_sql_alone_drops_the_row_through_the_cascade() -> Result<()> {
    let mut fx = fixture(&["a.NEF", "b.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let (b, b_path) = fx.photos[1].clone();
    let file = record(&mut fx.cat, a, &path)?;
    let kept = record(&mut fx.cat, b, &b_path)?;
    fx.cat
        .db_for_tests()
        .execute("DELETE FROM photos WHERE id=?", [a.0])?;
    assert_eq!(rows(&fx.cat)?, [b.0]);
    assert_eq!(fx.cat.editable_preview_orphans()?, [file]);
    assert!(kept.is_file());
    Ok(())
}

#[test]
fn discarding_forgets_the_copies_and_hands_back_their_files() -> Result<()> {
    let mut fx = fixture(&["a.NEF", "b.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let (b, b_path) = fx.photos[1].clone();
    let copy = fx.cat.create_virtual_copy(a)?;
    record(&mut fx.cat, a, &path)?;
    record(&mut fx.cat, b, &b_path)?;
    // A virtual copy discards its master's; a photo without one is skipped.
    let files = fx
        .cat
        .discard_editable_previews(&[copy, copy, PhotoId(999)])?;
    assert_eq!(files.len(), 1);
    assert!(files[0].starts_with(&format!("{}-", a.0)));
    assert_eq!(rows(&fx.cat)?, [b.0]);
    // Hidden files, a write in progress and folders are never orphans.
    let folder = fx.cat.editable_previews_folder();
    std::fs::write(folder.join(".DS_Store"), b"")?;
    std::fs::write(folder.join("rawmakase-a1b2c3.partial"), b"")?;
    std::fs::create_dir(folder.join("sub"))?;
    assert_eq!(fx.cat.editable_preview_orphans()?, [folder.join(&files[0])]);
    Ok(())
}

/// Replacing the file, relinking its folder or finding it elsewhere: the
/// same file keeps the copy up to date; another file makes it out of date,
/// and it is never opened again.
#[test]
fn the_copy_goes_out_of_date_only_when_the_original_comes_back_as_another_file() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    record(&mut fx.cat, a, &path)?;
    let folder = fx.cat.editable_previews_folder();
    let opens = |p: &Path| -> Result<()> { Ok(std::fs::metadata(p).map(|_| ())?) };
    // The same file in another folder, relinked: a new modification time
    // alone is no change.
    let elsewhere = fx.dir.join("elsewhere");
    std::fs::create_dir(&elsewhere)?;
    std::fs::copy(&path, elsewhere.join("a.NEF"))?;
    let photos_folder = fx.cat.folders()?[0].id;
    fx.cat.relink_folder(photos_folder, &elsewhere)?;
    let moved = fx.cat.photos()?[0].path.clone();
    assert_eq!(moved, elsewhere.join("a.NEF"));
    let preview = fx.cat.note_original(a, &Identity::read(&moved)?)?.unwrap();
    assert!(!preview.out_of_date);
    assert!(preview.usable(&folder, opens).is_some());
    // Another file at the path.
    std::fs::write(&moved, b"another photo")?;
    let preview = fx.cat.note_original(a, &Identity::read(&moved)?)?.unwrap();
    assert!(preview.out_of_date);
    assert!(fx.cat.editable_preview(a)?.unwrap().out_of_date);
    assert_eq!(preview.usable(&folder, opens), None);
    // The first file back doesn't bring it back: Build makes a new one.
    std::fs::copy(&path, &moved)?;
    let preview = fx.cat.note_original(a, &Identity::read(&moved)?)?.unwrap();
    assert!(preview.out_of_date);
    Ok(())
}

/// The catalog records files and never decodes them: whatever the reader
/// refuses (a record version it doesn't know, a missing or truncated file)
/// makes the copy out of date, and nothing panics.
#[test]
fn a_copy_the_reader_refuses_is_out_of_date() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let file = record(&mut fx.cat, a, &path)?;
    let folder = fx.cat.editable_previews_folder();
    let preview = fx.cat.editable_preview(a)?.unwrap();
    assert_eq!(preview.usable(&folder, |_| Ok(())), Some(file.clone()));
    for refusal in ["unknown record version 2", "truncated"] {
        assert_eq!(
            preview.usable(&folder, |_| Err(anyhow::anyhow!(refusal))),
            None
        );
    }
    std::fs::remove_file(&file)?;
    assert_eq!(
        preview.usable(&folder, |p| Ok(std::fs::metadata(p).map(|_| ())?)),
        None
    );
    Ok(())
}

#[test]
fn export_as_catalog_carries_the_copies_of_the_photos_it_exports_and_only_those() -> Result<()> {
    let mut fx = fixture(&["a.NEF", "b.NEF", "c.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    let (b, b_path) = fx.photos[1].clone();
    let (c, c_path) = fx.photos[2].clone();
    let a_file = record(&mut fx.cat, a, &path)?;
    let b_file = record(&mut fx.cat, b, &b_path)?;
    let c_file = record(&mut fx.cat, c, &c_path)?;
    // A copy whose file is gone is left out, row and all.
    std::fs::remove_file(&c_file)?;
    let destination = fx.dir.join("out").join("Picked.rawmakase");
    let export = fx
        .cat
        .export_catalog(&ExportScope::Photos(vec![a, c]), &destination)?;
    assert_eq!(export.editable_previews, 1);
    let exported = Catalog::open(destination.as_path())?;
    assert_eq!(rows(&exported)?, [a.0]);
    let carried = exported.editable_previews_folder();
    assert_eq!(
        carried,
        fx.dir.join("out/Picked.rawmakase-editable-previews")
    );
    let names: Vec<_> = std::fs::read_dir(&carried)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()?;
    assert_eq!(names, [a_file.file_name().unwrap()]);
    assert_eq!(
        std::fs::read(carried.join(names[0].clone()))?,
        std::fs::read(&a_file)?
    );
    // This catalog is as it was.
    assert_eq!(rows(&fx.cat)?, [a.0, b.0, c.0]);
    assert!(b_file.is_file());
    Ok(())
}

#[test]
fn export_as_catalog_never_writes_into_a_copies_folder_already_there() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let (a, path) = fx.photos[0].clone();
    record(&mut fx.cat, a, &path)?;
    let destination = fx.dir.join("Picked.rawmakase");
    let there = fx.dir.join("Picked.rawmakase-editable-previews");
    std::fs::create_dir(&there)?;
    std::fs::write(there.join("someone's.dng"), b"left")?;
    assert!(
        fx.cat
            .export_catalog(&ExportScope::Photos(vec![a]), &destination)
            .is_err()
    );
    assert!(!destination.exists());
    assert_eq!(std::fs::read_dir(&there)?.count(), 1);
    Ok(())
}

#[test]
fn carrying_nothing_makes_no_folder() -> Result<()> {
    let mut fx = fixture(&["a.NEF"])?;
    let to = fx.dir.join("staged");
    assert_eq!(carry(&fx.dir.join("none"), &mut fx.cat, &to)?, 0);
    assert!(!to.exists());
    Ok(())
}
