use super::{HEAD, Sparse, TAIL, checked, in_pieces, preview_from_file};
use crate::container::nef::tests::nef;
use crate::container::pef::tests::{note, pef};
use crate::container::test_tiff::{dir, long};
use crate::io::Order;
use std::io::Cursor;

const LE: Order = Order::Little;

/// A JPEG of `w` × `h` pixels as far as its frame header says, `body` bytes
/// long between that and its end.
fn jpeg(w: u16, h: u16, body: usize) -> Vec<u8> {
    let mut j = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8];
    j.extend(h.to_be_bytes());
    j.extend(w.to_be_bytes());
    j.extend([3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    j.extend((0..body).map(|i| (i % 251) as u8));
    j.extend([0xff, 0xd9]);
    j
}

/// A small NEF the readers open (a Nikon small RAW), its sensor data followed
/// by `gap` bytes standing in for a camera's, then JPEG previews of `sizes`,
/// then a directory for each, linked from IFD0: at the file's end, or before
/// `after` more bytes of filler.
fn nef_with_previews(gap: usize, sizes: &[([u16; 2], usize)], after: usize) -> Vec<u8> {
    let jpegs = sizes
        .iter()
        .map(|&([w, h], body)| jpeg(w, h, body))
        .collect();
    nef_with_jpegs(gap, jpegs, after)
}

/// [`nef_with_previews`] with these JPEGs as its previews.
fn nef_with_jpegs(gap: usize, jpegs: Vec<Vec<u8>>, after: usize) -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..8 {
        let x = 1000u64 | 1000 << 12 | 2048 << 24 | 2048 << 36;
        data.extend(&x.to_le_bytes()[..6]);
    }
    let mut f = nef("NIKON D810", 4, 4, 12, 34713, &[], &data);
    f.resize(f.len() + gap + f.len() % 2, 0x55);
    let mut previews = Vec::new();
    for j in jpegs {
        previews.push((f.len() as u32, j.len() as u32));
        f.extend(j);
        f.resize(f.len() + f.len() % 2, 0);
    }
    let mut links = Vec::new();
    for (at, len) in previews {
        let here = f.len() as u32;
        let d = dir(&[long(LE, 513, &[at]), long(LE, 514, &[len])], here, LE);
        links.push(here);
        f.extend(d);
    }
    // IFD0 links to the first, each to the next.
    let ifd0 = u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(f[ifd0..ifd0 + 2].try_into().unwrap()) as usize;
    let mut link_at = ifd0 + 2 + 12 * count;
    for here in links {
        f[link_at..link_at + 4].copy_from_slice(&here.to_le_bytes());
        link_at = here as usize + 2 + 12 * 2;
    }
    f.resize(f.len() + after, 0x55);
    f
}

/// [`jpeg`] with an APP1 segment of `app` bytes before its frame header, as
/// a camera's EXIF with its own thumbnail can be.
fn jpeg_after_app(w: u16, h: u16, app: u16, body: usize) -> Vec<u8> {
    let plain = jpeg(w, h, body);
    let mut j = vec![0xff, 0xd8, 0xff, 0xe1];
    j.extend(app.to_be_bytes());
    j.extend((2..app).map(|i| (i % 7) as u8));
    j.extend(&plain[2..]);
    j
}

fn sizes() -> Vec<([u16; 2], usize)> {
    // A Nikon's: a small JPEG, one of 640 pixels and a full-size one.
    vec![
        ([160, 120], 3_000),
        ([640, 424], 40_000),
        ([4928, 3264], 900_000),
    ]
}

#[test]
fn previews_past_the_head_are_read_without_the_rest() {
    let file = nef_with_previews(3 << 20, &sizes(), 0);
    let info = crate::open(&file).unwrap();
    for edge in [640, 4000] {
        let whole = crate::preview_reaching(&info, &file, edge).unwrap();
        let partial = preview_from_file(&mut Cursor::new(&file), edge).unwrap();
        assert_eq!(partial.preview, whole.as_ref(), "edge {edge}");
        assert_eq!(partial.info.flip, info.flip);
        assert_eq!(partial.info.variant, info.variant);
        // The head, the tail, each candidate's start and the preview.
        assert!(
            partial.read < HEAD + TAIL + 3 * (64 << 10) + whole.len() as u64,
            "read {} of {}",
            partial.read,
            file.len()
        );
    }
    // The 640-pixel preview is chosen for a 640-pixel thumbnail, not the full-size one.
    let small = preview_from_file(&mut Cursor::new(&file), 640).unwrap();
    assert!(small.preview.len() < 100_000);
    assert!(small.read < file.len() as u64 / 4);
}

#[test]
fn directories_beyond_the_parts_read_are_none_not_another_preview() {
    // The previews' directories are neither near the start nor at the end: the
    // parts read don't list them, and the caller reads the whole file.
    let file = nef_with_previews(3 << 20, &sizes(), 1 << 20);
    assert!(crate::preview(&crate::open(&file).unwrap(), &file).is_ok());
    assert!(preview_from_file(&mut Cursor::new(&file), 640).is_none());
}

#[test]
fn files_that_are_not_raws_are_none() {
    assert!(preview_from_file(&mut Cursor::new(Vec::<u8>::new()), 640).is_none());
    let garbage: Vec<u8> = (0..1_000_000u64).map(|i| (i * 7919 % 256) as u8).collect();
    assert!(preview_from_file(&mut Cursor::new(&garbage), 640).is_none());
    // A NEF without a JPEG preview has none to read.
    let file = nef_with_previews(0, &[], 0);
    assert!(preview_from_file(&mut Cursor::new(&file), 640).is_none());
}

#[test]
fn a_preview_smaller_than_asked_for_is_none() {
    // Read in pieces, a reader that misses a larger preview gives a smaller
    // one instead of an error. One smaller than the thumbnail is read whole,
    // where a larger one may be found.
    let file = nef_with_previews(3 << 20, &[([160, 120], 3_000)], 0);
    let info = crate::open(&file).unwrap();
    assert!(crate::preview_reaching(&info, &file, 640).is_ok());
    assert!(preview_from_file(&mut Cursor::new(&file), 640).is_none());
}

#[test]
fn formats_not_checked_against_whole_reads_are_none() {
    // A packed PEF, whose readers haven't been compared with the whole-file
    // read, with a preview well within the parts read.
    let preview = jpeg(640, 480, 10_000);
    let build = |at: u32| {
        pef(
            ["PENTAX", "PENTAX K-TEST"],
            [2, 2, 12, 32773],
            &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc],
            vec![long(LE, 513, &[at]), long(LE, 514, &[preview.len() as u32])],
            note(vec![]),
        )
    };
    let mut file = build(build(0).len() as u32);
    file.extend(&preview);
    let info = crate::open(&file).unwrap();
    assert_eq!(info.variant, crate::Variant::PefPacked);
    assert_eq!(
        crate::preview_reaching(&info, &file, 640).unwrap().as_ref(),
        preview
    );
    assert!(preview_from_file(&mut Cursor::new(&file), 640).is_none());
}

#[test]
fn a_frame_header_past_a_candidate_s_start_is_read() {
    // The full-size JPEG's EXIF fills the part of it read first, so its size
    // is only known once more of it is read. The 640-pixel one is chosen, as
    // from the whole file, not the full-size one.
    let jpegs = vec![
        jpeg(640, 424, 40_000),
        jpeg_after_app(4928, 3264, 65_533, 900_000),
    ];
    let file = nef_with_jpegs(3 << 20, jpegs, 0);
    let info = crate::open(&file).unwrap();
    let whole = crate::preview_reaching(&info, &file, 640).unwrap();
    let partial = preview_from_file(&mut Cursor::new(&file), 640).unwrap();
    assert_eq!(partial.preview, whole.as_ref());
    assert!(partial.read < file.len() as u64 / 4);
}

#[test]
fn more_candidates_than_are_read_are_none() {
    // Sixteen full-size JPEGs, then the 640-pixel one the whole file's read
    // chooses: not read before choosing, so the file is read whole.
    let mut sizes = vec![([4928, 3264], 1_000); 16];
    sizes.push(([640, 424], 1_000));
    let file = nef_with_previews(3 << 20, &sizes, 0);
    let info = crate::open(&file).unwrap();
    assert!(crate::preview_reaching(&info, &file, 640).unwrap().len() < 2_000);
    assert!(preview_from_file(&mut Cursor::new(&file), 640).is_none());
}

#[test]
fn a_preview_of_another_shape_is_not_taken_for_the_largest() {
    // The full-size JPEG starts past the head, so its size isn't known when
    // the file is first opened; a 4:3 preview within the head must not be
    // taken for the largest and chosen, where the whole file's read rejects
    // it for its shape.
    let file = nef_with_previews(0, &[([640, 480], 600_000), ([4928, 3264], 900_000)], 0);
    let info = crate::open(&file).unwrap();
    let whole = crate::preview_reaching(&info, &file, 640).unwrap();
    let partial = preview_from_file(&mut Cursor::new(&file), 640).unwrap();
    assert_eq!(partial.preview, whole.as_ref());
}

#[test]
fn a_candidate_read_whole_without_a_frame_header_is_left_out() {
    // A JPEG's start and an EXIF segment, without a frame: the whole file's
    // read leaves it out, and so does the read in pieces.
    let mut stub = vec![0xff, 0xd8, 0xff, 0xe1, 0, 8];
    stub.extend([0; 6]);
    stub.extend([0xff, 0xd9]);
    let jpegs = vec![stub, jpeg(640, 424, 40_000), jpeg(4928, 3264, 900_000)];
    let file = nef_with_jpegs(3 << 20, jpegs, 0);
    let info = crate::open(&file).unwrap();
    let whole = crate::preview_reaching(&info, &file, 640).unwrap();
    let partial = preview_from_file(&mut Cursor::new(&file), 640).unwrap();
    assert_eq!(partial.preview, whole.as_ref());
}

#[test]
fn each_byte_is_read_once() {
    let file: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
    let mut cursor = Cursor::new(&file);
    let mut s = Sparse::new(&mut cursor).unwrap();
    s.fill(100, 100).unwrap();
    s.fill(150, 100).unwrap();
    s.fill(50, 300).unwrap();
    s.fill(9_990, 100).unwrap();
    assert_eq!(s.read(), 300 + 10);
    assert_eq!(s.data.filled_ranges(), [(50, 350), (9_990, 10_000)]);
    assert_eq!(s.data.allocated_bytes(), 300 + 10);
    assert_eq!(s.source().get(50, 300).unwrap().as_ref(), &file[50..350]);
    assert!(
        s.source()
            .get(350, 9_990 - 350)
            .unwrap()
            .iter()
            .all(|b| *b == 0)
    );
}

#[test]
fn large_sparse_file_reads_preview_without_allocating_file_size() {
    use std::io::{Read, Seek, SeekFrom};

    struct SparseReader {
        len: u64,
        pos: u64,
        regions: Vec<(u64, Vec<u8>)>,
    }
    impl Read for SparseReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.len || buf.is_empty() {
                return Ok(0);
            }
            let n = (self.len - self.pos).min(buf.len() as u64) as usize;
            buf[..n].fill(0);
            let start = self.pos;
            let end = start + n as u64;
            for (r_start, r_data) in &self.regions {
                let r_end = *r_start + r_data.len() as u64;
                let copy_start = start.max(*r_start);
                let copy_end = end.min(r_end);
                if copy_start < copy_end {
                    let count = (copy_end - copy_start) as usize;
                    let dst_off = (copy_start - start) as usize;
                    let src_off = (copy_start - *r_start) as usize;
                    buf[dst_off..dst_off + count]
                        .copy_from_slice(&r_data[src_off..src_off + count]);
                }
            }
            self.pos = end;
            Ok(n)
        }
    }
    impl Seek for SparseReader {
        fn seek(&mut self, style: SeekFrom) -> std::io::Result<u64> {
            let next = match style {
                SeekFrom::Start(at) => Some(at),
                SeekFrom::End(off) => self.len.checked_add_signed(off),
                SeekFrom::Current(off) => self.pos.checked_add_signed(off),
            }
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "negative seek")
            })?;
            self.pos = next;
            Ok(next)
        }
    }

    let mut raw_data = Vec::new();
    for _ in 0..8 {
        let x = 1000u64 | 1000 << 12 | 2048 << 24 | 2048 << 36;
        raw_data.extend(&x.to_le_bytes()[..6]);
    }
    let mut head = nef("NIKON D810", 4, 4, 12, 34713, &[], &raw_data);
    let logical_len = 2u64 << 30;
    let small = jpeg(640, 424, 40_000);
    let full = jpeg(4928, 3264, 900_000);
    let small_at = 512u32 << 20;
    let full_at = 1u32 << 30;
    let ifd1_at = (logical_len as u32) - 4096;

    let mut tail = dir(
        &[
            long(LE, 513, &[small_at]),
            long(LE, 514, &[small.len() as u32]),
        ],
        ifd1_at,
        LE,
    );
    let ifd2_at = ifd1_at + tail.len() as u32;
    let d2 = dir(
        &[
            long(LE, 513, &[full_at]),
            long(LE, 514, &[full.len() as u32]),
        ],
        ifd2_at,
        LE,
    );
    let tail_link = tail.len() - 4;
    tail[tail_link..].copy_from_slice(&ifd2_at.to_le_bytes());
    tail.extend(d2);

    let ifd0 = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(head[ifd0..ifd0 + 2].try_into().unwrap()) as usize;
    let link_at = ifd0 + 2 + 12 * count;
    head[link_at..link_at + 4].copy_from_slice(&ifd1_at.to_le_bytes());

    let regions = vec![
        (0, head),
        (u64::from(small_at), small.clone()),
        (u64::from(full_at), full),
        (u64::from(ifd1_at), tail),
    ];

    let mut probe = SparseReader {
        len: logical_len,
        pos: 0,
        regions: regions.clone(),
    };
    let s = Sparse::new(&mut probe).unwrap();
    assert_eq!(s.len(), logical_len);
    assert_eq!(s.data.allocated_bytes(), 0);

    let mut reader = SparseReader {
        len: logical_len,
        pos: 0,
        regions,
    };
    let partial = preview_from_file(&mut reader, 640).unwrap();
    assert_eq!(partial.preview, small);
    assert!(
        partial.read < 2 << 20,
        "expected sparse read < 2 MiB on 2 GiB file, read {}",
        partial.read
    );
}

#[test]
fn sparse_source_refuses_giant_unfilled_materialization_and_handles_straddling_reads() {
    use crate::io::{MAX_PIECE, Source, SparseData};
    use std::io::{Read, Seek, SeekFrom};

    let logical_len = 1u64 << 30;
    let mut data = SparseData::new(logical_len);
    assert_eq!(data.allocated_bytes(), 0);
    {
        let source = Source::Sparse {
            data: &data,
            base: 0,
            len: logical_len,
        };
        assert!(source.get(100 << 20, MAX_PIECE + 1).is_none());
        assert_eq!(source.byte(100 << 20), Some(0));
    }

    let mut backing = vec![0u8; 200];
    for (i, b) in backing[100..120].iter_mut().enumerate() {
        *b = (i as u8) + 1;
    }
    backing[130..150].fill(0xaa);
    backing[135] = 0xff;
    let mut cur = Cursor::new(backing);
    data.fill(&mut cur, 100, 20).unwrap();
    data.fill(&mut cur, 130, 20).unwrap();
    assert_eq!(data.allocated_bytes(), 40);

    let source = Source::Sparse {
        data: &data,
        base: 0,
        len: logical_len,
    };
    let mut expected = vec![0u8; 60];
    for (i, b) in expected[5..25].iter_mut().enumerate() {
        *b = (i as u8) + 1;
    }
    expected[35..55].fill(0xaa);
    expected[40] = 0xff;

    assert_eq!(
        source.array::<60>(95).as_ref().map(<[u8; 60]>::as_slice),
        Some(expected.as_slice())
    );
    assert_eq!(source.get(95, 60).as_deref(), Some(expected.as_slice()));
    assert!(source.starts_with_at(95, &[0, 0, 0, 0, 0, 1, 2, 3]));
    assert!(!source.starts_with_at(125, &[1]));
    assert_eq!(source.find_byte(0, logical_len, 0xff), Some(135));

    let mut r = source.reader();
    r.seek(SeekFrom::Start(95)).unwrap();
    let mut buf = [0u8; 60];
    r.read_exact(&mut buf).unwrap();
    assert_eq!(buf.as_slice(), expected.as_slice());
    assert_eq!(r.seek(SeekFrom::Current(-5)).unwrap(), 150);
    assert_eq!(r.seek(SeekFrom::End(0)).unwrap(), logical_len);
    assert_eq!(r.read(&mut buf[..1]).unwrap(), 0);
    assert!(r.seek(SeekFrom::Start(0)).is_ok());
    assert!(r.seek(SeekFrom::Current(-1)).is_err());
}

#[test]
fn checked_formats_match_whole_file_previews_read_in_pieces() {
    use crate::container::arw::tests::{Value, tiff};

    let small = jpeg(640, 424, 40_000);
    let large = jpeg(1600, 1060, 80_000);
    let gap = vec![0x44u8; 2 << 20];

    // DNG with SubIFDs holding JPEG previews past a 2 MiB gap.
    let dng = tiff(
        vec![
            vec![
                (254, Value::Long(vec![0])),
                (256, Value::Long(vec![4])),
                (257, Value::Long(vec![4])),
                (258, Value::Short(vec![16])),
                (259, Value::Short(vec![1])),
                (262, Value::Short(vec![32803])),
                (273, Value::Blob(0)),
                (277, Value::Short(vec![1])),
                (278, Value::Long(vec![4])),
                (279, Value::Long(vec![32])),
                (330, Value::Ifd(1)),
                (33421, Value::Short(vec![2, 2])),
                (33422, Value::Bytes(1, vec![0, 1, 1, 2])),
                (50706, Value::Bytes(1, vec![1, 4, 0, 0])),
            ],
            vec![
                (254, Value::Long(vec![1])),
                (256, Value::Long(vec![640])),
                (257, Value::Long(vec![424])),
                (259, Value::Short(vec![7])),
                (262, Value::Short(vec![6])),
                (273, Value::Blob(2)),
                (279, Value::Long(vec![small.len() as u32])),
                (330, Value::Ifd(2)),
            ],
            vec![
                (254, Value::Long(vec![1])),
                (256, Value::Long(vec![1600])),
                (257, Value::Long(vec![1060])),
                (259, Value::Short(vec![7])),
                (262, Value::Short(vec![6])),
                (273, Value::Blob(3)),
                (279, Value::Long(vec![large.len() as u32])),
            ],
        ],
        vec![vec![0; 32], gap.clone(), small.clone(), large.clone()],
    );

    // Sony ARW with a SubIFD holding JPEG previews past a 2 MiB gap.
    let arw = tiff(
        vec![
            vec![
                (271, Value::Ascii("SONY")),
                (272, Value::Ascii("ILCE-7RM4")),
                (274, Value::Short(vec![6])),
                (330, Value::Ifd(1)),
                (513, Value::Blob(2)),
                (514, Value::Long(vec![small.len() as u32])),
            ],
            vec![
                (254, Value::Long(vec![0])),
                (256, Value::Long(vec![4])),
                (257, Value::Long(vec![4])),
                (258, Value::Short(vec![14])),
                (259, Value::Short(vec![1])),
                (262, Value::Short(vec![32803])),
                (273, Value::Blob(0)),
                (279, Value::Long(vec![32])),
                (33421, Value::Short(vec![2, 2])),
                (33422, Value::Bytes(1, vec![0, 1, 1, 2])),
                (330, Value::Ifd(2)),
            ],
            vec![
                (254, Value::Long(vec![1])),
                (256, Value::Long(vec![1600])),
                (257, Value::Long(vec![1060])),
                (513, Value::Blob(3)),
                (514, Value::Long(vec![large.len() as u32])),
            ],
        ],
        vec![vec![0; 32], gap, small, large],
    );

    for file in [dng, arw] {
        let info = crate::open(&file).unwrap();
        assert!(checked(info.variant));
        for edge in [640, 1200] {
            let whole = crate::preview_reaching(&info, &file, edge).unwrap();
            let partial = preview_from_file(&mut Cursor::new(&file), edge).unwrap();
            assert_eq!(partial.preview, whole.as_ref());
            assert_eq!(partial.info.variant, info.variant);
            assert_eq!(partial.info.flip, info.flip);
        }
    }
}

/// Every RAW in `RAWMAKASE_PARTIAL_FIXTURES` (searched recursively), of any
/// format, read in pieces and whole for a 640-pixel thumbnail: prints for each
/// variant how many gave the same preview and orientation, how many another
/// and how many only read whole. Fails if a format [`checked`] accepts gives
/// another; a format may be added there once none of its files does.
#[test]
#[ignore = "Set RAWMAKASE_PARTIAL_FIXTURES to a folder of RAWs"]
fn real_files_give_the_same_preview_read_in_pieces() {
    use std::collections::BTreeMap;
    let dir = std::env::var_os("RAWMAKASE_PARTIAL_FIXTURES").expect("RAWMAKASE_PARTIAL_FIXTURES");
    let mut todo = vec![std::path::PathBuf::from(dir)];
    // Per variant: the same, another, read whole.
    let mut tally: BTreeMap<String, [u32; 3]> = BTreeMap::new();
    let mut differ = Vec::new();
    let (mut total, mut read) = (0u64, 0u64);
    while let Some(path) = todo.pop() {
        if path.is_dir() {
            todo.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .flatten()
                    .map(|e| e.path()),
            );
            continue;
        }
        let Ok(file) = std::fs::read(&path) else {
            continue;
        };
        let Ok(info) = crate::open(&file) else {
            continue;
        };
        let Ok(whole) = crate::preview_reaching(&info, &file, 640) else {
            continue;
        };
        let counts = tally.entry(format!("{:?}", info.variant)).or_default();
        match in_pieces(&mut Cursor::new(&file), 640, |_| true) {
            Some(p) if p.preview == whole.as_ref() && p.info.flip == info.flip => {
                counts[0] += 1;
                total += file.len() as u64;
                read += p.read;
            }
            Some(p) => {
                counts[1] += 1;
                eprintln!("another: {} ({:?})", path.display(), info.variant);
                if checked(info.variant) || checked(p.info.variant) {
                    differ.push(path.display().to_string());
                }
            }
            None => counts[2] += 1,
        }
    }
    for (variant, [same, other, whole]) in &tally {
        eprintln!("{variant}: same {same}, another {other}, read whole {whole}");
    }
    eprintln!(
        "read {read} of {total} bytes ({:.1} %) for the same previews",
        read as f64 * 100. / total.max(1) as f64
    );
    assert!(
        differ.is_empty(),
        "another preview in a checked format: {differ:?}"
    );
}
