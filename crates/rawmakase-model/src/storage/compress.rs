//! Fast zlib compression helpers shared across persistence crates.

use flate2::{Compression, bufread::ZlibDecoder, write::ZlibEncoder};
use std::io::{self, Read, Write};

/// Compresses `input` with [`Compression::fast`] and a zlib wrapper.
pub fn zlib_compress_fast(input: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(input)?;
    encoder.finish()
}

/// Decompresses `compressed` into `out`, rejecting size mismatches, corrupt
/// checksums and trailing bytes.
pub fn zlib_decompress_exact(compressed: &[u8], out: &mut [u8]) -> io::Result<()> {
    let mut decoder = ZlibDecoder::new(compressed);
    decoder.read_exact(out)?;
    // One extra read forces flate2 to consume the Adler-32 trailer and confirm EOF.
    let mut extra = [0u8; 1];
    if decoder.read(&mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Compressed stream exceeds expected size",
        ));
    }
    if decoder.total_in() != compressed.len() as u64 || !decoder.into_inner().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Trailing bytes after compressed stream",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty_and_non_empty() -> io::Result<()> {
        for input in [&[][..], b"hello rawmakase compression".as_slice()] {
            let compressed = zlib_compress_fast(input)?;
            let mut restored = vec![0u8; input.len()];
            zlib_decompress_exact(&compressed, &mut restored)?;
            assert_eq!(restored, input);
        }
        Ok(())
    }

    #[test]
    fn rejects_truncation_corruption_and_size_mismatch() -> io::Result<()> {
        let input = vec![42u8; 1024];
        let compressed = zlib_compress_fast(&input)?;

        let mut truncated = vec![0u8; input.len()];
        assert!(
            zlib_decompress_exact(&compressed[..compressed.len() - 1], &mut truncated).is_err()
        );

        let mut flipped = compressed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        assert!(zlib_decompress_exact(&flipped, &mut vec![0u8; input.len()]).is_err());

        let mut too_big = vec![0u8; input.len() + 1];
        assert!(zlib_decompress_exact(&compressed, &mut too_big).is_err());

        let mut too_small = vec![0u8; input.len() - 1];
        assert!(zlib_decompress_exact(&compressed, &mut too_small).is_err());

        let mut with_trailing = compressed;
        with_trailing.push(0);
        assert!(zlib_decompress_exact(&with_trailing, &mut vec![0u8; input.len()]).is_err());
        Ok(())
    }
}
