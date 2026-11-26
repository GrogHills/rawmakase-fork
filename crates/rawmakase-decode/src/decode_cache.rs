//! Disk cache of developed camera images, so reopening a photo, or opening one that
//! was prefetched, skips decoding. An entry holds the image exactly as `raw::Raw::develop`
//! produced it, plus the pixels highlight recovery changed (usually under 1%), so the
//! recovered image is restored without recomputing it.
//!
//! Entries are keyed by the RAW file's identity (size, modification time and a hash of
//! its first 64 KB), the demosaic setting and the running executable's size and
//! modification time, so a rebuilt or updated app never reads another build's output.
//! The directory is capped; the least recently used entries are removed first. Only
//! derived pixels are stored: RAW files, sidecars and catalogs are never touched.
//! The user can move the directory to a folder of their choice, such as another
//! drive.
use crate::camera_data::{CameraImage, Demosaic, Metadata};
use crate::storage::{zlib_compress_fast, zlib_decompress_exact};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

const MAGIC: &[u8; 8] = b"RMKDEC\0\x02";
/// Pixels per independently compressed stripe (768 KiB raw RGB `f32`).
const STRIPE_PIXELS: usize = 65_536;
const MAX_STRIPE_PIXELS: u32 = 1 << 20;
const MAX_PIXELS: u64 = 150_000_000;
const MAX_STRIPES: u64 = 65_536;
/// Default size cap of the cache directory.
pub const DEFAULT_LIMIT: u64 = 4 << 30;
/// The subfolder of a chosen folder that holds the entries.
pub const CHOSEN_SUBFOLDER: &str = "RAWmakase Cache";

/// The per-user cache directory: ~/Library/Caches/RAWmakase on macOS,
/// %LOCALAPPDATA%\RAWmakase\Cache on Windows and $XDG_CACHE_HOME/rawmakase (default
/// ~/.cache/rawmakase) elsewhere. RAWMAKASE_CACHE_DIR overrides all of them. A
/// folder chosen with [`DecodeCache::set_folder`] overrides this for decoded images.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RAWMAKASE_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA")
    {
        return PathBuf::from(local).join("RAWmakase").join("Cache");
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches/RAWmakase");
    }
    std::env::var_os("XDG_CACHE_HOME")
        .map(|p| PathBuf::from(p).join("rawmakase"))
        .unwrap_or_else(|| home.join(".cache/rawmakase"))
}

fn build_identity_str() -> &'static str {
    static IDENTITY: OnceLock<String> = OnceLock::new();
    IDENTITY.get_or_init(|| {
        std::env::current_exe()
            .and_then(fs::metadata)
            .map(|m| {
                let modified = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                format!("{}-{modified}", m.len())
            })
            .unwrap_or_default()
    })
}

/// The running executable's size and modification time: what a cache entry made
/// by this build is keyed by, so another build never reads it.
pub fn build_identity() -> String {
    build_identity_str().to_string()
}

/// The key of a result this build made from file version `id`: the file's identity
/// ([`Identity::key`](crate::storage::Identity::key)), then `own`, what the cache's
/// result depends on beyond the file (a decode's demosaic, AI Denoise's model,
/// assembly and provider, a Camera Look fit's version), then [`build_identity`].
/// Each cache keeps its own part and reads the file's identity once, when its job
/// is captured; Library previews keep their cheap stamps instead.
pub fn source_key(id: &crate::storage::Identity, own: impl std::fmt::Display) -> String {
    format!("{}-{own}-{}", id.key(), build_identity_str())
}

#[derive(Clone, Debug)]
pub struct DecodeCache {
    dir: Arc<RwLock<Dir>>,
    limit: Arc<AtomicU64>,
}
/// Where one kind of cache entry lives: its subfolder of the default directory and of a
/// chosen folder, and its file extension. Decoded images and AI Denoise results each
/// have their own, so relocating, purging or trimming one never touches the other's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Namespace {
    pub default: &'static str,
    pub chosen: &'static str,
    pub extension: &'static str,
}
/// Decoded images.
pub(crate) const DECODED: Namespace = Namespace {
    default: "decoded",
    chosen: CHOSEN_SUBFOLDER,
    extension: "decoded",
};
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Dir {
    pub path: PathBuf,
    /// The folder the user chose, if any. Its subfolder is created only while
    /// it exists, so an unplugged drive's mount point is never recreated on
    /// the system disk; the default directory is created with its parents.
    pub chosen: Option<PathBuf>,
}
impl Dir {
    fn of(folder: Option<&Path>) -> Self {
        Self::of_in(DECODED, folder)
    }
    pub(crate) fn of_in(namespace: Namespace, folder: Option<&Path>) -> Self {
        match folder {
            None => Self {
                path: cache_dir().join(namespace.default),
                chosen: None,
            },
            Some(folder) => Self {
                path: folder.join(namespace.chosen),
                chosen: Some(folder.to_path_buf()),
            },
        }
    }
    pub(crate) fn create(&self) -> std::io::Result<()> {
        if self.chosen.is_none() {
            return fs::create_dir_all(&self.path);
        }
        match fs::create_dir(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && self.path.is_dir() => Ok(()),
            result => result,
        }
    }
}
#[derive(Serialize, Deserialize, PartialEq)]
struct Header {
    key: String,
    width: u32,
    height: u32,
    fast: bool,
    scale_factor: f32,
    scale_clipped: u32,
    stripe_pixels: u32,
    /// Pixels changed by highlight recovery, stored after the image stripes.
    recovered: u64,
    /// Compressed byte length of the recovered-pixel delta block.
    recovered_compressed_len: u64,
}
impl Default for DecodeCache {
    fn default() -> Self {
        Self::new(cache_dir().join("decoded"), DEFAULT_LIMIT)
    }
}
impl DecodeCache {
    pub fn new(dir: PathBuf, limit: u64) -> Self {
        Self {
            dir: Arc::new(RwLock::new(Dir {
                path: dir,
                chosen: None,
            })),
            limit: Arc::new(AtomicU64::new(limit)),
        }
    }
    fn location(&self) -> Dir {
        self.dir.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// The directory holding the entries, shared with clones.
    pub fn dir(&self) -> PathBuf {
        self.location().path
    }
    /// The folder chosen with [`set_folder`](Self::set_folder); `None` for the
    /// default directory.
    pub fn folder(&self) -> Option<PathBuf> {
        self.location().chosen
    }
    /// Keeps entries in [`CHOSEN_SUBFOLDER`] of `folder`, or in the default
    /// directory for `None`, for this cache and its clones. Entries already in
    /// the previous directory stay there.
    pub fn set_folder(&self, folder: Option<&Path>) {
        *self.dir.write().unwrap_or_else(|e| e.into_inner()) = Dir::of(folder);
    }
    /// [`set_folder`](Self::set_folder), then removes the previous directory's
    /// entries, which nothing reads any more. Returns how many were removed.
    /// The new directory is created and written to first: if either fails,
    /// the cache and its entries stay where they are.
    pub fn relocate(&self, folder: Option<&Path>) -> std::io::Result<usize> {
        let next = Dir::of(folder);
        next.create()?;
        tempfile::tempfile_in(&next.path)?;
        let previous = self.dir();
        *self.dir.write().unwrap_or_else(|e| e.into_inner()) = next;
        if previous == self.dir() {
            return Ok(0);
        }
        Ok(purge(&previous, DECODED.extension))
    }
    /// Removes every entry, and nothing else in the directory. Returns how
    /// many were removed.
    pub fn purge(&self) -> usize {
        purge(&self.dir(), DECODED.extension)
    }
    /// The size cap in bytes, shared with clones used by existing workers.
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }
    /// Changes the cap for this cache and its clones. The next store trims to
    /// the new cap; changing it performs no disk I/O on the caller's thread.
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }
    /// The key of `path` as the current build develops it at full size with `demosaic`.
    pub fn key(path: &Path, demosaic: Demosaic) -> Result<String> {
        Ok(Self::key_of(
            &crate::storage::Identity::read(path)?,
            demosaic,
        ))
    }
    /// The key of the file version `id`, as [`key`](Self::key) names it.
    pub fn key_of(id: &crate::storage::Identity, demosaic: Demosaic) -> String {
        source_key(id, format_args!("{demosaic:?}"))
    }
    fn file(&self, key: &str) -> PathBuf {
        file_in(&self.dir(), key)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.file(key).is_file()
    }
    /// The cached image for `key`, with its recovered image set; `metadata` comes
    /// from opening the RAW, which is cheap. `None` on a miss or an unusable entry.
    pub fn load(&self, key: &str, metadata: &Metadata) -> Option<CameraImage> {
        let path = self.file(key);
        let image = Self::read(&path, key, metadata).ok()?;
        // Mark as recently used for eviction.
        let _ = fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(SystemTime::now()));
        Some(image)
    }
    fn read(path: &Path, key: &str, metadata: &Metadata) -> Result<CameraImage> {
        let file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        let mut reader = std::io::BufReader::new(file);
        let mut magic = [0; 8];
        reader.read_exact(&mut magic)?;
        ensure!(&magic == MAGIC, "Not a decode cache entry");
        let mut len = [0; 4];
        reader.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        ensure!(len < 1 << 16, "Invalid decode cache header");
        let mut header = vec![0; len];
        reader.read_exact(&mut header)?;
        let header: Header = serde_json::from_slice(&header)?;
        ensure!(header.key == key, "Decode cache key collision");
        let n = header.width as u64 * header.height as u64;
        ensure!(
            n > 0 && n <= MAX_PIXELS && header.recovered <= n,
            "Invalid decode cache size"
        );
        ensure!(
            header.stripe_pixels > 0 && header.stripe_pixels <= MAX_STRIPE_PIXELS,
            "Invalid decode cache stripe size"
        );
        let stripe_pixels = header.stripe_pixels as usize;
        let stripe_count = n.div_ceil(header.stripe_pixels as u64);
        ensure!(stripe_count <= MAX_STRIPES, "Too many decode cache stripes");
        let table_start = 12 + len as u64;
        let table_bytes = stripe_count * 4;
        let stripes_start = table_start
            .checked_add(table_bytes)
            .context("Decode cache table overflow")?;
        ensure!(size >= stripes_start, "Decode cache entry is truncated");
        let mut table_buf = vec![0u8; table_bytes as usize];
        reader.read_exact(&mut table_buf)?;
        let mut stripes = Vec::with_capacity(stripe_count as usize);
        let mut cursor = stripes_start;
        let mut remaining = n as usize;
        for &chunk in table_buf.as_chunks::<4>().0 {
            let comp_len = u32::from_le_bytes(chunk) as u64;
            let raw_pixels = remaining.min(stripe_pixels);
            remaining -= raw_pixels;
            let raw_bytes = (raw_pixels as u64) * 12;
            ensure!(
                comp_len > 0 && comp_len <= max_compressed_len(raw_bytes),
                "Invalid decode cache stripe length"
            );
            let next = cursor
                .checked_add(comp_len)
                .context("Decode cache stripe offset overflow")?;
            stripes.push((cursor, comp_len as usize, raw_pixels));
            cursor = next;
        }
        if header.recovered == 0 {
            ensure!(
                header.recovered_compressed_len == 0,
                "Unexpected recovered block length"
            );
        } else {
            ensure!(
                header.recovered_compressed_len > 0
                    && header.recovered_compressed_len <= max_compressed_len(header.recovered * 16),
                "Invalid recovered block length"
            );
        }
        let expected_size = cursor
            .checked_add(header.recovered_compressed_len)
            .context("Decode cache file size overflow")?;
        ensure!(
            size == expected_size,
            "Decode cache entry has the wrong size"
        );
        let file = reader.into_inner();
        let mut pixels = vec![[0f32; 3]; n as usize];
        pixels
            .par_chunks_mut(stripe_pixels)
            .zip(stripes.par_iter())
            .try_for_each(|(dst, &(offset, comp_len, raw_pixels))| -> Result<()> {
                ensure!(dst.len() == raw_pixels, "Stripe length mismatch");
                let mut compressed = vec![0u8; comp_len];
                read_exact_at_file(&file, offset, &mut compressed)?;
                let mut raw = vec![0u8; raw_pixels * 12];
                zlib_decompress_exact(&compressed, &mut raw)?;
                unshuffle_pixels(&raw, dst)
            })?;
        let changes = if header.recovered == 0 {
            Vec::new()
        } else {
            let count = header.recovered as usize;
            let mut compressed = vec![0u8; header.recovered_compressed_len as usize];
            read_exact_at_file(&file, cursor, &mut compressed)?;
            let mut raw = vec![0u8; count * 16];
            zlib_decompress_exact(&compressed, &mut raw)?;
            unshuffle_changes(&raw, count)?
        };
        // Like a decode of the file, the loaded image measures its own Auto CA
        // rather than sharing one measured on the image `metadata` came from.
        let metadata = Metadata {
            lateral_ca: Default::default(),
            ..metadata.clone()
        };
        let image = CameraImage {
            recovered: Default::default(),
            width: header.width,
            height: header.height,
            pixels: Arc::new(pixels),
            metadata,
            fast: header.fast,
            scale_factor: header.scale_factor,
            scale_clipped: header.scale_clipped,
            hdr: Default::default(),
        };
        let recovered_pixels = if changes.is_empty() {
            image.pixels.clone()
        } else {
            let mut recovered_pixels = vec![[0.; 3]; image.pixels.len()];
            recovered_pixels
                .par_chunks_mut(1 << 20)
                .zip(image.pixels.par_chunks(1 << 20))
                .for_each(|(a, b)| a.copy_from_slice(b));
            for change in changes {
                let p = recovered_pixels
                    .get_mut(change.index as usize)
                    .context("Invalid recovered pixel")?;
                *p = change.value;
            }
            Arc::new(recovered_pixels)
        };
        let recovered = CameraImage {
            recovered: Default::default(),
            pixels: recovered_pixels,
            metadata: image.metadata.clone(),
            width: image.width,
            height: image.height,
            fast: image.fast,
            scale_factor: image.scale_factor,
            scale_clipped: image.scale_clipped,
            hdr: Default::default(),
        };
        let _ = image.recovered.set(Arc::new(recovered));
        Ok(image)
    }
    /// Stores `image` and its recovered image (which must be set) under `key`, then
    /// trims the directory to the size cap. Written to a temporary file and renamed,
    /// so a concurrent reader never sees a partial entry.
    pub fn store(&self, key: &str, image: &CameraImage) -> Result<()> {
        // RAWs only: a rendered file opens fast enough, and an HDR file's light isn't
        // in the cache's format (docs/records/hdr-stage4-design.md).
        ensure!(
            !image.metadata.is_rendered(),
            "The decode cache keeps RAWs only"
        );
        let recovered = image.recovered.get().context("Image not recovered")?;
        ensure!(
            recovered.pixels.len() == image.pixels.len(),
            "Recovered size mismatch"
        );
        let changes = if Arc::ptr_eq(&image.pixels, &recovered.pixels) {
            Vec::new()
        } else {
            const CHUNK: usize = 65_536;
            let chunks: Vec<Vec<Change>> = image
                .pixels
                .par_chunks(CHUNK)
                .zip(recovered.pixels.par_chunks(CHUNK))
                .enumerate()
                .map(|(chunk_idx, (a, b))| {
                    let base = chunk_idx * CHUNK;
                    let mut local = Vec::new();
                    for (offset, (pa, pb)) in a.iter().zip(b).enumerate() {
                        // Compare bit patterns so -0.0 and +0.0 remain distinguishable.
                        if pa[0].to_bits() != pb[0].to_bits()
                            || pa[1].to_bits() != pb[1].to_bits()
                            || pa[2].to_bits() != pb[2].to_bits()
                        {
                            local.push(Change {
                                index: (base + offset) as u32,
                                value: *pb,
                            });
                        }
                    }
                    local
                })
                .collect();
            let mut changes = Vec::with_capacity(chunks.iter().map(Vec::len).sum());
            for c in chunks {
                changes.extend(c);
            }
            changes
        };
        let compressed_stripes: Vec<Vec<u8>> = image
            .pixels
            .par_chunks(STRIPE_PIXELS)
            .map(|stripe| zlib_compress_fast(&shuffle_pixels(stripe)))
            .collect::<std::io::Result<Vec<_>>>()?;
        let compressed_changes = if changes.is_empty() {
            Vec::new()
        } else {
            zlib_compress_fast(&shuffle_changes(&changes))?
        };
        let header = serde_json::to_vec(&Header {
            key: key.into(),
            width: image.width,
            height: image.height,
            fast: image.fast,
            scale_factor: image.scale_factor,
            scale_clipped: image.scale_clipped,
            stripe_pixels: STRIPE_PIXELS as u32,
            recovered: changes.len() as u64,
            recovered_compressed_len: compressed_changes.len() as u64,
        })?;
        let dir = self.location();
        dir.create()?;
        let mut file = tempfile::NamedTempFile::new_in(&dir.path)?;
        {
            let mut out = std::io::BufWriter::new(file.as_file_mut());
            out.write_all(MAGIC)?;
            out.write_all(&(header.len() as u32).to_le_bytes())?;
            out.write_all(&header)?;
            for stripe in &compressed_stripes {
                let len = u32::try_from(stripe.len()).context("Compressed stripe exceeds u32")?;
                out.write_all(&len.to_le_bytes())?;
            }
            for stripe in &compressed_stripes {
                out.write_all(stripe)?;
            }
            if !compressed_changes.is_empty() {
                out.write_all(&compressed_changes)?;
            }
            out.flush()?;
        }
        let path = file_in(&dir.path, key);
        file.persist(&path)?;
        // Moved meanwhile: the old directory's entries were removed, or are
        // about to be, and this one goes too, so none outlives the move.
        if self.location() != dir {
            let _ = fs::remove_file(&path);
            return Ok(());
        }
        self.trim(&dir.path)
    }
    /// Removes the least recently used entries until `dir` fits the cap.
    fn trim(&self, dir: &Path) -> Result<()> {
        trim(dir, DECODED.extension, self.limit())
    }
}
/// Removes the least recently used `extension` entries of `dir` until they fit `limit`.
pub(crate) fn trim(dir: &Path, extension: &str, limit: u64) -> Result<()> {
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = fs::read_dir(dir)?
        .filter_map(|e| {
            let e = e.ok()?;
            let path = e.path();
            (path.extension()? == extension).then_some(())?;
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), path))
        })
        .collect();
    let mut total: u64 = entries.iter().map(|e| e.1).sum();
    entries.sort();
    for (_, size, path) in entries {
        if total <= limit {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
    Ok(())
}
fn file_in(dir: &Path, key: &str) -> PathBuf {
    entry_in(dir, key, DECODED.extension)
}
/// The file of `key` in `dir`.
pub(crate) fn entry_in(dir: &Path, key: &str, extension: &str) -> PathBuf {
    let hash = crate::storage::fnv1a(crate::storage::FNV_OFFSET, key.as_bytes());
    dir.join(format!("{hash:016x}.{extension}"))
}
/// Removes every `extension` entry of `dir`, and nothing else. Returns how many.
pub(crate) fn purge(dir: &Path, extension: &str) -> usize {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == extension))
        .filter(|e| fs::remove_file(e.path()).is_ok())
        .count()
}
/// Fills `buf` from `offset`, in parallel chunks: one reader leaves most of an SSD's
/// bandwidth unused on a 700 MB image.
fn max_compressed_len(raw_bytes: u64) -> u64 {
    raw_bytes.saturating_add(raw_bytes / 64).saturating_add(128)
}

/// Transposes each channel's 4 little-endian `f32` bytes into contiguous byte
/// planes across a stripe so exponent and sign bytes group for deflate.
fn shuffle_pixels(pixels: &[[f32; 3]]) -> Vec<u8> {
    let n = pixels.len();
    let mut out = vec![0u8; n * 12];
    for (i, px) in pixels.iter().enumerate() {
        for (c, &sample) in px.iter().enumerate() {
            let bytes = sample.to_bits().to_le_bytes();
            let plane = c * 4 * n + i;
            out[plane] = bytes[0];
            out[plane + n] = bytes[1];
            out[plane + 2 * n] = bytes[2];
            out[plane + 3 * n] = bytes[3];
        }
    }
    out
}

fn unshuffle_pixels(shuffled: &[u8], out: &mut [[f32; 3]]) -> Result<()> {
    let n = out.len();
    ensure!(shuffled.len() == n * 12, "Unexpected stripe payload length");
    for (i, px) in out.iter_mut().enumerate() {
        for (c, sample) in px.iter_mut().enumerate() {
            let plane = c * 4 * n + i;
            let bytes = [
                shuffled[plane],
                shuffled[plane + n],
                shuffled[plane + 2 * n],
                shuffled[plane + 3 * n],
            ];
            *sample = f32::from_bits(u32::from_le_bytes(bytes));
        }
    }
    Ok(())
}

fn shuffle_changes(changes: &[Change]) -> Vec<u8> {
    let n = changes.len();
    let mut out = vec![0u8; n * 16];
    for (i, change) in changes.iter().enumerate() {
        let idx = change.index.to_le_bytes();
        out[i] = idx[0];
        out[n + i] = idx[1];
        out[2 * n + i] = idx[2];
        out[3 * n + i] = idx[3];
        for (c, &sample) in change.value.iter().enumerate() {
            let bytes = sample.to_bits().to_le_bytes();
            let plane = (c + 1) * 4 * n + i;
            out[plane] = bytes[0];
            out[plane + n] = bytes[1];
            out[plane + 2 * n] = bytes[2];
            out[plane + 3 * n] = bytes[3];
        }
    }
    out
}

fn unshuffle_changes(shuffled: &[u8], count: usize) -> Result<Vec<Change>> {
    ensure!(
        shuffled.len() == count * 16,
        "Unexpected recovered block length"
    );
    let mut changes = Vec::with_capacity(count);
    for i in 0..count {
        let index = u32::from_le_bytes([
            shuffled[i],
            shuffled[count + i],
            shuffled[2 * count + i],
            shuffled[3 * count + i],
        ]);
        let mut value = [0f32; 3];
        for (c, sample) in value.iter_mut().enumerate() {
            let plane = (c + 1) * 4 * count + i;
            let bytes = [
                shuffled[plane],
                shuffled[plane + count],
                shuffled[plane + 2 * count],
                shuffled[plane + 3 * count],
            ];
            *sample = f32::from_bits(u32::from_le_bytes(bytes));
        }
        changes.push(Change { index, value });
    }
    Ok(changes)
}

fn read_exact_at_file(file: &fs::File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            let n = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buf[done..],
                offset + done as u64,
            )?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
}

pub(crate) fn read_at(file: &fs::File, offset: u64, buf: &mut [u8]) -> Result<()> {
    const CHUNK: usize = 16 << 20;
    buf.par_chunks_mut(CHUNK)
        .enumerate()
        .try_for_each(|(i, chunk)| -> std::io::Result<()> {
            read_exact_at_file(file, offset + (i * CHUNK) as u64, chunk)
        })?;
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Change {
    index: u32,
    value: [f32; 3],
}

#[cfg(test)]
mod tests {
    use super::*;
    /// A decode is filed under the file's identity, the demosaic and the build, in
    /// the form the entries already on disk have.
    #[test]
    fn a_decode_key_keeps_its_form() {
        let id = crate::storage::Identity {
            size: 12,
            modified_ns: 34,
            prefix_hash: "ab".into(),
        };
        assert_eq!(
            DecodeCache::key_of(&id, Demosaic::Rawmakase),
            format!("ab-12-34-Rawmakase-{}", build_identity())
        );
    }
    #[test]
    fn changed_limit_reaches_existing_workers_and_controls_eviction() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut rendered = image(4, 3, 0.2);
        rendered.metadata.source = rawmakase_model::camera_data::Source::Rendered;
        assert!(
            DecodeCache::new(dir.path().join("rendered"), u64::MAX)
                .store("r", &rendered)
                .is_err(),
            "the decode cache keeps RAWs only"
        );
        let cache = DecodeCache::new(dir.path().to_path_buf(), 1);
        let worker = cache.clone();
        cache.set_limit(1 << 20);
        worker.store("a", &image(40, 30, 0.2))?;
        assert!(cache.contains("a"));
        cache.set_limit(1);
        worker.store("b", &image(40, 30, 0.4))?;
        assert!(!cache.contains("a"));
        assert!(!cache.contains("b"));
        Ok(())
    }

    #[test]
    fn a_chosen_folder_reaches_existing_workers_and_relocating_removes_old_entries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("default"), u64::MAX);
        let worker = cache.clone();
        cache.store("a", &image(40, 30, 0.2))?;
        fs::write(dir.path().join("default/notes.txt"), "kept")?;
        let chosen = dir.path().join("other drive");
        fs::create_dir(&chosen)?;
        assert_eq!(cache.relocate(Some(&chosen))?, 1);
        assert_eq!(worker.dir(), chosen.join(CHOSEN_SUBFOLDER));
        assert_eq!(worker.folder(), Some(chosen.clone()));
        assert!(!worker.contains("a"));
        assert!(dir.path().join("default/notes.txt").is_file());
        worker.store("b", &image(40, 30, 0.4))?;
        assert!(cache.contains("b"));
        assert!(fs::read_dir(chosen.join(CHOSEN_SUBFOLDER))?.count() == 1);
        // Choosing the same folder again removes nothing.
        assert_eq!(cache.relocate(Some(&chosen))?, 0);
        assert!(cache.contains("b"));
        Ok(())
    }

    /// A chosen folder whose cache subfolder cannot be made or written keeps
    /// the cache, and its entries, where they are.
    #[test]
    fn relocating_to_an_unusable_folder_changes_nothing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("default"), u64::MAX);
        cache.store("a", &image(40, 30, 0.2))?;
        let blocked = dir.path().join("blocked");
        fs::create_dir(&blocked)?;
        fs::write(blocked.join(CHOSEN_SUBFOLDER), "a file, not a folder")?;
        for folder in [blocked, dir.path().join("missing")] {
            assert!(cache.relocate(Some(&folder)).is_err());
            assert_eq!(cache.dir(), dir.path().join("default"));
            assert!(cache.contains("a"));
        }
        Ok(())
    }

    /// An unplugged drive's folder is not recreated on the system disk: storing
    /// fails and nothing is written.
    #[test]
    fn a_missing_chosen_folder_is_not_created() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("default"), u64::MAX);
        let unplugged = dir.path().join("Volumes/Photos SSD");
        cache.set_folder(Some(&unplugged));
        assert!(cache.store("a", &image(40, 30, 0.2)).is_err());
        assert!(!dir.path().join("Volumes").exists());
        assert!(!cache.contains("a"));
        Ok(())
    }

    fn image(width: u32, height: u32, seed: f32) -> CameraImage {
        let im = CameraImage {
            recovered: Default::default(),
            width,
            height,
            pixels: Arc::new(
                (0..width * height)
                    .map(|i| [(i as f32 * 0.1 + seed).sin().abs(), 0.5, seed])
                    .collect(),
            ),
            metadata: Metadata::default(),
            fast: false,
            scale_factor: 1.5,
            scale_clipped: 7,
            hdr: Default::default(),
        };
        let mut recovered = im.clone();
        Arc::make_mut(&mut recovered.pixels)[3] = [2., 3., 4.];
        let _ = im.recovered.set(Arc::new(recovered));
        im
    }
    #[test]
    fn empty_recovery_changes_share_pixels_with_independent_recovered_image() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("decoded"), 1 << 20);
        let metadata = Metadata {
            make: "Test".into(),
            ..Default::default()
        };
        let im = CameraImage {
            recovered: Default::default(),
            width: 16,
            height: 12,
            pixels: Arc::new(vec![[0.1, 0.2, 0.3]; 16 * 12]),
            metadata: metadata.clone(),
            fast: false,
            scale_factor: 1.,
            scale_clipped: 0,
            hdr: Default::default(),
        };
        let _ = im.recovered.set(Arc::new(im.clone()));
        cache.store("no_changes", &im)?;
        let mut loaded = cache.load("no_changes", &metadata).unwrap();
        assert!(Arc::ptr_eq(
            &loaded.pixels,
            &loaded.recovered.get().unwrap().pixels
        ));
        loaded.metadata.make = "Modified".into();
        assert_eq!(loaded.recovered.get().unwrap().metadata.make, "Test");

        let changed = image(16, 12, 0.2);
        cache.store("with_changes", &changed)?;
        let loaded_changed = cache.load("with_changes", &metadata).unwrap();
        assert!(!Arc::ptr_eq(
            &loaded_changed.pixels,
            &loaded_changed.recovered.get().unwrap().pixels
        ));
        assert_eq!(
            loaded_changed.recovered.get().unwrap().pixels[3],
            [2., 3., 4.]
        );
        Ok(())
    }
    #[test]
    fn entries_round_trip_and_stay_within_the_cap() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let a = image(40, 30, 0.2);
        let b = image(40, 30, 0.4);
        let probe = DecodeCache::new(dir.path().join("probe"), u64::MAX);
        probe.store("a", &a)?;
        probe.store("b", &b)?;
        let size_a = fs::metadata(probe.file("a"))?.len();
        let size_b = fs::metadata(probe.file("b"))?.len();
        let cache = DecodeCache::new(
            dir.path().join("decoded"),
            size_a + size_b + (size_a.min(size_b) / 2),
        );
        let metadata = Metadata {
            make: "Test".into(),
            ..Default::default()
        };
        assert!(cache.load("a", &metadata).is_none());
        cache.store("a", &a)?;
        assert!(cache.contains("a"));
        let loaded = cache.load("a", &metadata).unwrap();
        assert_eq!(loaded.pixels, a.pixels);
        assert_eq!(loaded.metadata.make, "Test");
        assert_eq!((loaded.scale_factor, loaded.scale_clipped), (1.5, 7));
        assert_eq!(
            loaded.recovered.get().unwrap().pixels,
            a.recovered.get().unwrap().pixels
        );
        // Another key never reads this entry, even on a file-name collision.
        assert!(DecodeCache::read(&cache.file("a"), "b", &metadata).is_err());
        // A truncated entry is a miss.
        let bytes_on_disk = fs::read(cache.file("a"))?;
        fs::write(cache.file("a"), &bytes_on_disk[..bytes_on_disk.len() - 4])?;
        assert!(cache.load("a", &metadata).is_none());
        cache.store("a", &a)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        cache.store("b", &b)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(cache.load("a", &metadata).is_some());
        std::thread::sleep(std::time::Duration::from_millis(20));
        // The least recently used entry goes first.
        cache.store("c", &image(40, 30, 0.6))?;
        assert!(cache.contains("a") && cache.contains("c") && !cache.contains("b"));
        Ok(())
    }

    #[test]
    fn bit_exact_f32_multi_stripe_roundtrip_with_recovered_diffs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("decoded"), u64::MAX);
        // 300x300 spans two 65,536-pixel stripes and mixes special f32 bit patterns.
        let width = 300u32;
        let height = 300u32;
        let total = (width * height) as usize;
        let specials = [
            [0.0f32, -0.0, f32::from_bits(1)],
            [f32::MIN_POSITIVE, -1.25, 16.5],
            [f32::MAX, f32::INFINITY, f32::NEG_INFINITY],
        ];
        let pixels: Vec<[f32; 3]> = (0..total)
            .map(|i| {
                if i < specials.len() {
                    specials[i]
                } else {
                    let t = i as f32 / total as f32;
                    [t, (t * 7.0).sin() * 0.5 + 0.5, t * t]
                }
            })
            .collect();
        let im = CameraImage {
            recovered: Default::default(),
            width,
            height,
            pixels: Arc::new(pixels),
            metadata: Metadata::default(),
            fast: true,
            scale_factor: 2.25,
            scale_clipped: 12,
            hdr: Default::default(),
        };
        let mut recovered = im.clone();
        // Flip -0.0 -> +0.0 on pixel 0 and touch indices in both stripes.
        let rec_pixels = Arc::make_mut(&mut recovered.pixels);
        rec_pixels[0] = [0.0, 0.0, f32::from_bits(1)];
        rec_pixels[5] = [1.5, 2.5, 3.5];
        rec_pixels[STRIPE_PIXELS + 123] = [4.25, 0.0, -0.5];
        rec_pixels[total - 1] = [9.0, 8.0, 7.0];
        let _ = im.recovered.set(Arc::new(recovered));

        cache.store("multi", &im)?;
        let loaded = cache
            .load("multi", &Metadata::default())
            .context("Cache miss")?;
        for (a, b) in loaded.pixels.iter().zip(im.pixels.iter()) {
            assert_eq!(
                [a[0].to_bits(), a[1].to_bits(), a[2].to_bits()],
                [b[0].to_bits(), b[1].to_bits(), b[2].to_bits()]
            );
        }
        let orig_rec = im.recovered.get().unwrap();
        let load_rec = loaded.recovered.get().unwrap();
        for (a, b) in load_rec.pixels.iter().zip(orig_rec.pixels.iter()) {
            assert_eq!(
                [a[0].to_bits(), a[1].to_bits(), a[2].to_bits()],
                [b[0].to_bits(), b[1].to_bits(), b[2].to_bits()]
            );
        }
        Ok(())
    }

    #[test]
    fn truncated_and_corrupted_stripes_are_misses() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("decoded"), u64::MAX);
        let im = image(300, 300, 0.3);
        cache.store("k", &im)?;
        let raw = fs::read(cache.file("k"))?;
        let metadata = Metadata::default();
        assert!(cache.load("k", &metadata).is_some());

        let header_len = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
        let table_start = 12 + header_len;
        let stripe0_len =
            u32::from_le_bytes(raw[table_start..table_start + 4].try_into().unwrap()) as usize;
        let stripe0_start = table_start + 8;

        // Truncate inside the first stripe.
        fs::write(cache.file("k"), &raw[..stripe0_start + stripe0_len / 2])?;
        assert!(cache.load("k", &metadata).is_none());

        // Corrupt a payload byte inside the first stripe.
        let mut bad_stripe = raw.clone();
        bad_stripe[stripe0_start + stripe0_len / 2] ^= 0xff;
        fs::write(cache.file("k"), &bad_stripe)?;
        assert!(cache.load("k", &metadata).is_none());

        // Corrupt a payload byte inside the recovered-changes block.
        let mut bad_recovered = raw.clone();
        let tail = bad_recovered.len() - 2;
        bad_recovered[tail] ^= 0xff;
        fs::write(cache.file("k"), &bad_recovered)?;
        assert!(cache.load("k", &metadata).is_none());

        // Corrupt stripe length in the index table.
        let mut bad_table = raw;
        bad_table[table_start] ^= 0x01;
        fs::write(cache.file("k"), &bad_table)?;
        assert!(cache.load("k", &metadata).is_none());
        Ok(())
    }

    #[test]
    fn v1_magic_entry_is_treated_as_miss() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("decoded"), u64::MAX);
        fs::create_dir_all(cache.dir())?;
        let mut legacy = Vec::from(*b"RMKDEC\0\x01");
        let header = serde_json::to_vec(&serde_json::json!({
            "key": "legacy",
            "width": 2,
            "height": 2,
            "fast": false,
            "scale_factor": 1.0,
            "scale_clipped": 0,
            "recovered": 0
        }))?;
        legacy.extend_from_slice(&(header.len() as u32).to_le_bytes());
        legacy.extend_from_slice(&header);
        legacy.extend_from_slice(&[0u8; 2 * 2 * 12]);
        fs::write(cache.file("legacy"), &legacy)?;
        assert!(cache.load("legacy", &Metadata::default()).is_none());
        Ok(())
    }

    #[test]
    fn smooth_ramp_compresses_substantially() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = DecodeCache::new(dir.path().join("decoded"), u64::MAX);
        let width = 512u32;
        let height = 512u32;
        let mut pixels = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = x as f32 / (width - 1) as f32;
                let g = y as f32 / (height - 1) as f32;
                let b = (r + g) * 0.5;
                pixels.push([r, g, b]);
            }
        }
        let im = CameraImage {
            recovered: Default::default(),
            width,
            height,
            pixels: Arc::new(pixels),
            metadata: Metadata::default(),
            fast: false,
            scale_factor: 1.0,
            scale_clipped: 0,
            hdr: Default::default(),
        };
        let _ = im.recovered.set(Arc::new(im.clone()));
        cache.store("ramp", &im)?;
        let on_disk = fs::metadata(cache.file("ramp"))?.len();
        let raw_bytes = (width as u64) * (height as u64) * 12;
        assert!(
            on_disk < raw_bytes / 3,
            "expected smooth ramp ({on_disk} B) to compress below 1/3 of raw ({raw_bytes} B)"
        );
        let loaded = cache
            .load("ramp", &Metadata::default())
            .context("Cache miss")?;
        for (a, b) in loaded.pixels.iter().zip(im.pixels.iter()) {
            assert_eq!(
                [a[0].to_bits(), a[1].to_bits(), a[2].to_bits()],
                [b[0].to_bits(), b[1].to_bits(), b[2].to_bits()]
            );
        }
        Ok(())
    }
}
