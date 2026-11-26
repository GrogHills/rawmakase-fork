//! Canonical Huffman codes as JPEG defines them (ITU T.81 annex C): code
//! lengths 1 to 16 with the number of codes of each length, then the symbols
//! in code order. Decoded with a 10-bit lookup table and canonical bounds for
//! longer codes.
use super::bits::Msb;
use crate::error::{Result, corrupt};

const FAST_BITS: u32 = 10;
const FAST_SIZE: usize = 1 << FAST_BITS;
const FAST_SHIFT: u32 = 16 - FAST_BITS;
const SLOW_LENS: usize = 16 - FAST_BITS as usize;

#[derive(PartialEq)]
pub(crate) struct Huffman {
    /// By the top [`FAST_BITS`] bits: the code's length (`1..=FAST_BITS`, or 0
    /// for longer or invalid codes) and its symbol.
    fast: [(u8, u8); FAST_SIZE],
    /// For lengths 11 to 16, one past the largest 16-bit left-aligned code of
    /// that length (0 when no codes of that length exist).
    end16: [u32; SLOW_LENS],
    /// `first_symbol_index - first_code` for lengths 11 to 16.
    val_offset: [i32; SLOW_LENS],
    /// Symbols of lengths 11 to 16 in canonical code order.
    symbols: Vec<u8>,
}

impl Huffman {
    /// From the number of codes of each length 1 to 16 and the symbols.
    pub(crate) fn new(counts: &[u8; 16], symbols: &[u8]) -> Result<Self> {
        let total: usize = counts.iter().map(|&n| usize::from(n)).sum();
        if total > symbols.len() || total == 0 {
            return Err(corrupt("Huffman table"));
        }
        let mut fast = [(0u8, 0u8); FAST_SIZE];
        let mut end16 = [0u32; SLOW_LENS];
        let mut val_offset = [0i32; SLOW_LENS];
        let slow_count: usize = counts[FAST_BITS as usize..]
            .iter()
            .map(|&n| usize::from(n))
            .sum();
        let mut slow_symbols = Vec::with_capacity(slow_count);
        let (mut code, mut k) = (0u32, 0usize);
        for (i, &n) in counts.iter().enumerate() {
            let len = i as u32 + 1;
            if len <= FAST_BITS {
                let shift = FAST_BITS - len;
                let span = 1usize << shift;
                for _ in 0..n {
                    if code >= 1 << len {
                        return Err(corrupt("Huffman table overfull"));
                    }
                    let start = (code as usize) << shift;
                    fast[start..start + span].fill((len as u8, symbols[k]));
                    code += 1;
                    k += 1;
                }
            } else {
                let first_code = code;
                let first_sym = slow_symbols.len();
                for _ in 0..n {
                    if code >= 1 << len {
                        return Err(corrupt("Huffman table overfull"));
                    }
                    slow_symbols.push(symbols[k]);
                    code += 1;
                    k += 1;
                }
                if n > 0 {
                    let s = i - FAST_BITS as usize;
                    end16[s] = code << (16 - len);
                    val_offset[s] = first_sym as i32 - first_code as i32;
                }
            }
            code <<= 1;
        }
        Ok(Self {
            fast,
            end16,
            val_offset,
            symbols: slow_symbols,
        })
    }
    #[inline]
    fn lookup_slow(&self, next16: u32) -> Result<(u32, u8)> {
        for i in 0..SLOW_LENS {
            if next16 < self.end16[i] {
                let len = FAST_BITS + 1 + i as u32;
                let code = (next16 >> (16 - len)) as i32;
                let idx = (code + self.val_offset[i]) as usize;
                return Ok((len, self.symbols[idx]));
            }
        }
        Err(corrupt("invalid Huffman code"))
    }
    /// The code the next 16 bits start with: its length and symbol, for
    /// pumps other than [`Msb`].
    #[inline]
    pub(crate) fn lookup(&self, next16: u32) -> Result<(u32, u8)> {
        if next16 >= 1 << 16 {
            return Err(corrupt("invalid Huffman code"));
        }
        let (len, symbol) = self.fast[(next16 >> FAST_SHIFT) as usize];
        if len > 0 {
            return Ok((u32::from(len), symbol));
        }
        self.lookup_slow(next16)
    }
    /// The next symbol.
    #[inline]
    pub(crate) fn symbol(&self, bits: &mut Msb) -> Result<u8> {
        let next16 = bits.peek(16);
        let (len, symbol) = self.fast[(next16 >> FAST_SHIFT) as usize];
        if len > 0 {
            bits.skip(u32::from(len));
            return Ok(symbol);
        }
        let (len, symbol) = self.lookup_slow(next16)?;
        bits.skip(len);
        Ok(symbol)
    }
    /// The next difference: a length symbol then that many bits, as JPEG's
    /// lossless and DC differences are coded (F.2.2.1). Length 16 is 32768
    /// with no bits following; with `sixteen`, the 16 bits some encoders
    /// write after it hold the difference.
    #[inline]
    pub(crate) fn difference(&self, bits: &mut Msb, sixteen: bool) -> Result<i32> {
        let len = u32::from(self.symbol(bits)?);
        Ok(match len {
            0 => 0,
            16 => {
                if sixteen {
                    return Ok(extend(bits.get(16), 16));
                }
                32768
            }
            1..=15 => extend(bits.get(len), len),
            _ => return Err(corrupt("difference length")),
        })
    }
}

/// A `len`-bit difference read as JPEG codes it: values below half the range
/// are negative.
#[inline]
pub(crate) fn extend(v: u32, len: u32) -> i32 {
    if len == 0 {
        0
    } else if v < 1 << (len - 1) {
        v as i32 - (1 << len) + 1
    } else {
        v as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_canonical_codes() {
        // Lengths: one code of 2 bits (00 → 5), two of 3 bits (010 → 6, 011 → 7).
        let mut counts = [0u8; 16];
        counts[1] = 1;
        counts[2] = 2;
        let h = Huffman::new(&counts, &[5, 6, 7]).unwrap();
        let mut bits = Msb::new(&[0b00_010_011, 0b1100_0000]);
        assert_eq!(h.symbol(&mut bits).unwrap(), 5);
        assert_eq!(h.symbol(&mut bits).unwrap(), 6);
        assert_eq!(h.symbol(&mut bits).unwrap(), 7);
        assert!(h.symbol(&mut bits).is_err());
    }
    #[test]
    fn extends_differences() {
        assert_eq!(extend(0, 1), -1);
        assert_eq!(extend(1, 1), 1);
        assert_eq!(extend(0b010, 3), -5);
        assert_eq!(extend(0b110, 3), 6);
    }
}
