// SPDX-License-Identifier: MPL-2.0

//! Writing a bitstream: bits from the most significant, the Exp-Golomb codes of the ITU-T
//! video standards, and their NAL units.

/// Bits written one after the other, the first in the most significant bit of the first byte.
#[derive(Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte: 0 when the writer is byte-aligned.
    used: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `count` low bits of `value`, most significant first: `u(n)`.
    pub fn bits(&mut self, value: u64, count: u32) {
        debug_assert!(count <= 64 && (count == 64 || value >> count == 0));
        for i in (0..count).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }

    /// One bit: `u(1)`.
    pub fn flag(&mut self, value: bool) {
        self.bits(u64::from(value), 1);
    }

    /// An unsigned Exp-Golomb code, `ue(v)` (H.265 9.2): as many zeros as the bits after the
    /// first one of `value + 1`, then `value + 1`.
    pub fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let length = 64 - code.leading_zeros();
        self.bits(0, length - 1);
        self.bits(code, length);
    }

    /// A signed Exp-Golomb code, `se(v)` (H.265 9.2.2): positive values are the odd codes,
    /// the others the even ones. Every value of the standards' syntax elements fits; the
    /// lowest `i32` does not.
    pub fn se(&mut self, value: i32) {
        debug_assert!(value > i32::MIN);
        let code = if value > 0 {
            2 * value as u32 - 1
        } else {
            2 * value.unsigned_abs()
        };
        self.ue(code);
    }

    /// A one, then zeros up to the next byte: `rbsp_trailing_bits()` and `byte_alignment()`
    /// (H.265 7.3.2.11, 7.3.2.12).
    pub fn trailing_bits(&mut self) {
        self.flag(true);
        while self.used != 0 {
            self.flag(false);
        }
    }

    /// How many bits were written.
    pub fn len(&self) -> usize {
        match self.used {
            0 => self.bytes.len() * 8,
            used => (self.bytes.len() - 1) * 8 + used as usize,
        }
    }

    /// The bytes written; the last one is padded with zeros.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// A NAL unit in the byte stream format (H.265 annex B): a four-byte start code, `header`,
/// then `payload` with an emulation prevention byte (0x03) wherever it would otherwise show
/// two zero bytes followed by a byte of 0 to 3 (H.265 7.4.2), which is how a start code, or
/// such a byte itself, would read.
pub fn nal_unit(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut unit = Vec::with_capacity(4 + header.len() + payload.len() + payload.len() / 64);
    unit.extend_from_slice(&[0, 0, 0, 1]);
    unit.extend_from_slice(header);
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte <= 3 {
            unit.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        unit.push(byte);
    }
    unit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(write: impl FnOnce(&mut BitWriter)) -> Vec<u8> {
        let mut writer = BitWriter::new();
        write(&mut writer);
        writer.into_bytes()
    }

    #[test]
    fn bits_fill_bytes_from_the_top() {
        assert_eq!(written(|w| w.bits(0b101, 3)), [0b1010_0000]);
        assert_eq!(
            written(|w| {
                w.bits(0x3, 2);
                w.bits(0xabc, 12);
            }),
            [0b1110_1010, 0b1111_0000]
        );
        assert_eq!(written(|w| w.bits(0xffff_ffff, 32)), [0xff; 4]);
    }

    /// The codes of H.265 table 9-2, and the mapping of table 9-3.
    #[test]
    fn exp_golomb_codes() {
        assert_eq!(written(|w| w.ue(0)), [0b1000_0000]);
        assert_eq!(written(|w| w.ue(1)), [0b0100_0000]);
        assert_eq!(written(|w| w.ue(2)), [0b0110_0000]);
        assert_eq!(written(|w| w.ue(3)), [0b0010_0000]);
        assert_eq!(written(|w| w.ue(6)), [0b0011_1000]);
        assert_eq!(written(|w| w.ue(7)), [0b0001_0000]);
        assert_eq!(written(|w| w.ue(u32::MAX)), [0, 0, 0, 0, 0x80, 0, 0, 0, 0]);
        // 0, 1, -1, 2, -2 are the codes 0 to 4.
        assert_eq!(written(|w| w.se(0)), written(|w| w.ue(0)));
        assert_eq!(written(|w| w.se(1)), written(|w| w.ue(1)));
        assert_eq!(written(|w| w.se(-1)), written(|w| w.ue(2)));
        assert_eq!(written(|w| w.se(2)), written(|w| w.ue(3)));
        assert_eq!(written(|w| w.se(-2)), written(|w| w.ue(4)));
    }

    #[test]
    fn trailing_bits_align() {
        assert_eq!(written(|w| w.trailing_bits()), [0x80]);
        assert_eq!(
            written(|w| {
                w.bits(0b11, 2);
                w.trailing_bits();
            }),
            [0b1110_0000]
        );
        assert_eq!(
            written(|w| {
                w.bits(0xff, 8);
                w.trailing_bits();
            }),
            [0xff, 0x80]
        );
    }

    #[test]
    fn emulation_is_prevented() {
        // The sequences H.265 7.4.2 forbids in a NAL unit, and the one it reserves.
        for last in 0..=3 {
            assert_eq!(nal_unit(&[], &[0, 0, last]), [0, 0, 0, 1, 0, 0, 3, last]);
        }
        assert_eq!(nal_unit(&[], &[0, 0, 4]), [0, 0, 0, 1, 0, 0, 4]);
        // Zeros in a row: the count starts again after each prevention byte.
        assert_eq!(
            nal_unit(&[0x40, 1], &[0, 0, 0, 0, 0, 1]),
            [0, 0, 0, 1, 0x40, 1, 0, 0, 3, 0, 0, 3, 0, 1]
        );
        assert_eq!(nal_unit(&[], &[1, 0, 0]), [0, 0, 0, 1, 1, 0, 0]);
    }
}
