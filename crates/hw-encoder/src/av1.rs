// SPDX-License-Identifier: MPL-2.0

//! The headers of an AV1 stream, written as the AV1 Bitstream & Decoding Process
//! Specification (version 1.0.0 with Errata 1) specifies them: the sequence header, the frame
//! header and the temporal delimiter, each in its OBU. The GPU codes the tiles; with VA-API,
//! the headers are the application's to write.
//!
//! The stream is the simplest a desktop needs: profile 0 (4:2:0), one layer, every frame shown
//! and predicted from the one before it only, nothing held back. Every frame refreshes every
//! reference slot, so that the previous frame is in all of them. Section numbers are those of
//! the specification.

use crate::bits::BitWriter;
use crate::h265::Colour;

/// OBU types (6.2.2).
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_FRAME_HEADER: u8 = 3;

/// Frame types (6.8.2).
const KEY_FRAME: u64 = 0;
const INTER_FRAME: u64 = 1;

/// The number of reference frames a frame names (3: REFS_PER_FRAME).
const REFS_PER_FRAME: usize = 7;

/// The order hint is sent modulo two to this power (5.5.1: OrderHintBits, at most 8). Each
/// frame refers to the one before it, so any size does.
pub const ORDER_HINT_BITS: u32 = 8;

/// The tile limits (3: MAX_TILE_WIDTH, MAX_TILE_AREA, MAX_TILE_ROWS, MAX_TILE_COLS).
const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_ROWS: u32 = 64;
const MAX_TILE_COLS: u32 = 64;

/// Superblocks are 64x64 (`use_128x128_superblock` is 0): 16 mode info units of 4x4 each
/// way.
const SB_SHIFT: u32 = 4;
const SB_SIZE_LOG2: u32 = 6;

/// The bytes of the frame header's `obu_size`, written with leading zeros (4.10.5 allows
/// it): the encoder fills it in once it knows the frame's size, and the field must not change
/// length when it does.
pub const FRAME_HEADER_SIZE_BYTES: usize = 4;

/// The interpolation filters (6.8.9).
pub const SWITCHABLE: u8 = 4;

/// `leb128()` (4.10.5): `value` in `bytes` bytes, 7 bits each from the least significant,
/// the top bit set on all but the last.
fn leb128(value: u32, bytes: usize) -> Vec<u8> {
    debug_assert!((1..=5).contains(&bytes) && (bytes == 5 || u64::from(value) >> (7 * bytes) == 0));
    (0..bytes)
        .map(|i| {
            let more = if i + 1 < bytes { 0x80 } else { 0 };
            ((value >> (7 * i)) & 0x7f) as u8 | more
        })
        .collect()
}

/// The fewest bytes of a `leb128()` of `value`.
fn leb128_length(value: u32) -> usize {
    (32 - value.leading_zeros()).div_ceil(7).max(1) as usize
}

/// An OBU's header (5.3.2), with no extension and with its size, which `leb128()` follows.
fn obu_header(obu_type: u8) -> u8 {
    // obu_forbidden_bit, obu_type (4), obu_extension_flag, obu_has_size_field,
    // obu_reserved_1bit.
    (obu_type << 3) | (1 << 1)
}

/// An OBU: its header, the size of its payload in `size_bytes` (the fewest when `None`),
/// then the payload.
fn obu(obu_type: u8, payload: &[u8], size_bytes: Option<usize>) -> Vec<u8> {
    let size = payload.len() as u32;
    let mut unit = vec![obu_header(obu_type)];
    unit.extend(leb128(
        size,
        size_bytes.unwrap_or_else(|| leb128_length(size)),
    ));
    unit.extend_from_slice(payload);
    unit
}

/// The temporal delimiter (5.6), which starts every temporal unit: an OBU with no payload.
pub fn temporal_delimiter() -> Vec<u8> {
    obu(OBU_TEMPORAL_DELIMITER, &[], None)
}

/// `tile_log2()` (5.9.16): the smallest `k` such that `block << k` reaches `target`.
fn tile_log2(block: u32, target: u32) -> u32 {
    let mut k = 0;
    while (block << k) < target {
        k += 1;
    }
    k
}

/// How a frame is cut in tiles: uniformly, in as few as the standard allows (5.9.15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiles {
    pub cols_log2: u32,
    pub rows_log2: u32,
    /// The width of each column and the height of each row, in superblocks.
    pub widths: Vec<u32>,
    pub heights: Vec<u32>,
    /// What the frame header says only when there is more than one tile: the bytes of each
    /// tile's size, less one.
    pub size_bytes_minus_1: u32,
    /// The largest values `increment_tile_cols_log2` and `increment_tile_rows_log2` can
    /// reach: the frame header writes no flag when the count is there already.
    max_cols_log2: u32,
    max_rows_log2: u32,
}

impl Tiles {
    fn new(sb_cols: u32, sb_rows: u32, size_bytes_minus_1: u32) -> Self {
        let max_tile_width_sb = MAX_TILE_WIDTH >> SB_SIZE_LOG2;
        let max_tile_area_sb = MAX_TILE_AREA >> (2 * SB_SIZE_LOG2);
        let min_cols_log2 = tile_log2(max_tile_width_sb, sb_cols);
        let max_cols_log2 = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_rows_log2 = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_tiles_log2 = min_cols_log2.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        let cols_log2 = min_cols_log2;
        let rows_log2 = min_tiles_log2.saturating_sub(cols_log2);
        // Equal tiles, the last one with what remains.
        let split = |count: u32, log2: u32| -> Vec<u32> {
            let size = (count + (1 << log2) - 1) >> log2;
            (0..count)
                .step_by(size as usize)
                .map(|start| size.min(count - start))
                .collect()
        };
        Self {
            cols_log2,
            rows_log2,
            widths: split(sb_cols, cols_log2),
            heights: split(sb_rows, rows_log2),
            size_bytes_minus_1,
            max_cols_log2,
            max_rows_log2,
        }
    }

    pub fn count(&self) -> usize {
        self.widths.len() * self.heights.len()
    }

    /// `tile_info()` (5.9.15), uniform spacing.
    fn write(&self, w: &mut BitWriter) {
        w.flag(true); // uniform_tile_spacing_flag
        // increment_tile_cols_log2: the minimum is kept.
        if self.cols_log2 < self.max_cols_log2 {
            w.flag(false);
        }
        // increment_tile_rows_log2.
        if self.rows_log2 < self.max_rows_log2 {
            w.flag(false);
        }
        if self.cols_log2 > 0 || self.rows_log2 > 0 {
            // context_update_tile_id: the first tile.
            w.bits(0, self.cols_log2 + self.rows_log2);
            w.bits(u64::from(self.size_bytes_minus_1), 2); // tile_size_bytes_minus_1
        }
    }
}

/// The coding of a frame that the encoder may change after the header is written: the
/// rate control sets the quantizer and the filters of each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filters {
    pub base_q_idx: u8,
    /// `loop_filter_level[ 0..4 ]`: luma vertical and horizontal, then each chroma plane.
    pub loop_filter_level: [u8; 4],
    /// CDEF: `cdef_damping_minus_3`, `cdef_bits`, and the strengths, the primary in the top
    /// four bits and the secondary in the bottom two of each, the same for luma and chroma.
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    pub cdef_strengths: [u8; 8],
}

impl Filters {
    /// What a frame starts with, before the rate control's own: a middle quantizer, and
    /// filters of that strength. Every filter level is above 0, so that every field is in
    /// the header for the encoder to overwrite.
    pub const DEFAULT: Self = Self {
        base_q_idx: 128,
        loop_filter_level: [16, 16, 8, 8],
        cdef_damping_minus_3: 2,
        cdef_bits: 0,
        cdef_strengths: [9, 0, 0, 0, 0, 0, 0, 0],
    };
}

/// A coded video sequence: what its sequence header says, and what every frame header
/// follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    /// 10 bits per sample instead of 8.
    pub ten_bits: bool,
    /// The size of the frames.
    pub width: u32,
    pub height: u32,
    /// `seq_level_idx`: see [`level_of`].
    pub level_idx: u8,
    pub colour: Colour,
    pub tiles: Tiles,
}

/// A frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// A key frame, shown: it starts the sequence again, and refers to nothing.
    pub key: bool,
    /// Its `order_hint`: one more for each frame, modulo 2^[`ORDER_HINT_BITS`].
    pub order_hint: u8,
    /// `interpolation_filter` (6.8.9): [`SWITCHABLE`] lets the encoder pick it per block.
    pub interpolation_filter: u8,
    /// `TxMode` is `TX_MODE_SELECT` rather than `TX_MODE_LARGEST` (6.8.21).
    pub tx_mode_select: bool,
    pub filters: Filters,
}

/// A frame header OBU, and where the encoder finds the fields it rewrites: offsets in bits
/// from the start of the OBU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    pub bytes: Vec<u8>,
    pub qindex_offset: u32,
    pub loop_filter_offset: u32,
    pub cdef_offset: u32,
    pub cdef_size: u32,
    /// Where the `obu_size` field is, in bytes from the start of the OBU.
    pub size_field_offset: u32,
}

impl Sequence {
    /// A sequence of `width`x`height` frames, cut in tiles of `tile_size_bytes_minus_1`
    /// size bytes; its level is still to set.
    pub fn new(
        ten_bits: bool,
        width: u32,
        height: u32,
        colour: Colour,
        tile_size_bytes_minus_1: u32,
    ) -> Self {
        // MiCols and MiRows (5.9.9), then superblocks (5.9.15).
        let mi_cols = 2 * width.div_ceil(8);
        let mi_rows = 2 * height.div_ceil(8);
        let sb_cols = (mi_cols + (1 << SB_SHIFT) - 1) >> SB_SHIFT;
        let sb_rows = (mi_rows + (1 << SB_SHIFT) - 1) >> SB_SHIFT;
        Self {
            ten_bits,
            width,
            height,
            level_idx: 0,
            colour,
            tiles: Tiles::new(sb_cols, sb_rows, tile_size_bytes_minus_1),
        }
    }

    /// The bits that hold `frame_width_minus_1` and `frame_height_minus_1`.
    fn size_bits(size: u32) -> u32 {
        (32 - (size - 1).leading_zeros()).max(1)
    }

    /// The sequence header OBU (5.5).
    pub fn header(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(0, 3); // seq_profile: Main, 4:2:0
        w.flag(false); // still_picture
        w.flag(false); // reduced_still_picture_header
        w.flag(false); // timing_info_present_flag
        w.flag(false); // initial_display_delay_present_flag
        w.bits(0, 5); // operating_points_cnt_minus_1
        w.bits(0, 12); // operating_point_idc[ 0 ]
        w.bits(u64::from(self.level_idx), 5); // seq_level_idx[ 0 ]
        if self.level_idx > 7 {
            w.flag(false); // seq_tier[ 0 ]: Main
        }
        let (width_bits, height_bits) = (Self::size_bits(self.width), Self::size_bits(self.height));
        w.bits(u64::from(width_bits - 1), 4); // frame_width_bits_minus_1
        w.bits(u64::from(height_bits - 1), 4); // frame_height_bits_minus_1
        w.bits(u64::from(self.width - 1), width_bits); // max_frame_width_minus_1
        w.bits(u64::from(self.height - 1), height_bits); // max_frame_height_minus_1
        w.flag(false); // frame_id_numbers_present_flag
        w.flag(false); // use_128x128_superblock
        w.flag(false); // enable_filter_intra
        w.flag(false); // enable_intra_edge_filter
        w.flag(false); // enable_interintra_compound
        w.flag(false); // enable_masked_compound
        w.flag(false); // enable_warped_motion
        w.flag(false); // enable_dual_filter
        w.flag(true); // enable_order_hint
        w.flag(false); // enable_jnt_comp
        w.flag(false); // enable_ref_frame_mvs
        w.flag(false); // seq_choose_screen_content_tools
        w.flag(false); // seq_force_screen_content_tools: no palette, no intra block copy
        w.bits(u64::from(ORDER_HINT_BITS - 1), 3); // order_hint_bits_minus_1
        w.flag(false); // enable_superres
        w.flag(true); // enable_cdef
        w.flag(false); // enable_restoration
        self.color_config(&mut w);
        w.flag(false); // film_grain_params_present
        w.trailing_bits();
        obu(OBU_SEQUENCE_HEADER, &w.into_bytes(), None)
    }

    /// `color_config()` (5.5.2), profile 0.
    fn color_config(&self, w: &mut BitWriter) {
        w.flag(self.ten_bits); // high_bitdepth
        w.flag(false); // mono_chrome
        w.flag(true); // color_description_present_flag
        w.bits(u64::from(self.colour.primaries), 8); // color_primaries
        w.bits(u64::from(self.colour.transfer), 8); // transfer_characteristics
        w.bits(u64::from(self.colour.matrix), 8); // matrix_coefficients
        // Not the identity matrix: the range, then, in 4:2:0, where the chroma samples are.
        w.flag(self.colour.full_range); // color_range
        w.bits(0, 2); // chroma_sample_position: CSP_UNKNOWN
        w.flag(false); // separate_uv_delta_q
    }

    /// The frame header OBU (5.9), with its trailing bits (5.9.1: an OBU of its own, not
    /// inside a frame OBU).
    pub fn frame_header(&self, frame: &Frame) -> FrameHeader {
        let mut w = BitWriter::new();
        w.flag(false); // show_existing_frame
        w.bits(if frame.key { KEY_FRAME } else { INTER_FRAME }, 2); // frame_type
        w.flag(true); // show_frame
        // showable_frame follows from show_frame; a shown key frame is error resilient.
        if !frame.key {
            w.flag(false); // error_resilient_mode
        }
        w.flag(false); // disable_cdf_update
        // No screen content tools in the sequence: allow_screen_content_tools is 0.
        w.flag(false); // frame_size_override_flag
        w.bits(u64::from(frame.order_hint), ORDER_HINT_BITS); // order_hint
        if !frame.key {
            // The context comes from the frame before (LAST_FRAME - LAST_FRAME).
            w.bits(0, 3); // primary_ref_frame
            w.bits(0xff, 8); // refresh_frame_flags: every slot
        }
        if frame.key {
            // frame_size(): the sequence's (no override, no superres).
            w.flag(false); // render_and_frame_size_different
        } else {
            w.flag(false); // frame_refs_short_signaling
            for _ in 0..REFS_PER_FRAME {
                w.bits(0, 3); // ref_frame_idx[ i ]: the previous frame is in every slot
            }
            // frame_size() and render_size(), as for a key frame.
            w.flag(false); // render_and_frame_size_different
            w.flag(true); // allow_high_precision_mv
            // read_interpolation_filter()
            if frame.interpolation_filter == SWITCHABLE {
                w.flag(true); // is_filter_switchable
            } else {
                w.flag(false); // is_filter_switchable
                w.bits(u64::from(frame.interpolation_filter), 2); // interpolation_filter
            }
            w.flag(false); // is_motion_mode_switchable
            // No reference motion vectors in the sequence: use_ref_frame_mvs is 0.
        }
        w.flag(false); // disable_frame_end_update_cdf
        self.tiles.write(&mut w);

        // quantization_params() (5.9.12)
        let qindex_offset = w.len();
        w.bits(u64::from(frame.filters.base_q_idx), 8); // base_q_idx
        w.flag(false); // DeltaQYDc: delta_coded
        w.flag(false); // DeltaQUDc: delta_coded
        w.flag(false); // DeltaQUAc: delta_coded
        w.flag(false); // using_qmatrix
        w.flag(false); // segmentation_enabled (5.9.14)
        // delta_q_params() (5.9.17): base_q_idx is above 0.
        debug_assert!(frame.filters.base_q_idx > 0);
        w.flag(false); // delta_q_present
        // delta_lf_params(): nothing without delta_q_present.

        // loop_filter_params() (5.9.11): not lossless, no intra block copy.
        let loop_filter_offset = w.len();
        let levels = frame.filters.loop_filter_level;
        debug_assert!(levels[0] > 0 || levels[1] > 0);
        for level in levels {
            w.bits(u64::from(level.min(63)), 6); // loop_filter_level[ i ]
        }
        w.bits(0, 3); // loop_filter_sharpness
        w.flag(false); // loop_filter_delta_enabled

        // cdef_params() (5.9.19)
        let cdef_offset = w.len();
        w.bits(u64::from(frame.filters.cdef_damping_minus_3), 2);
        w.bits(u64::from(frame.filters.cdef_bits), 2);
        for &strength in &frame.filters.cdef_strengths[..1 << frame.filters.cdef_bits] {
            for _plane in ["luma", "chroma"] {
                w.bits(u64::from(strength >> 2), 4); // cdef_*_pri_strength[ i ]
                w.bits(u64::from(strength & 3), 2); // cdef_*_sec_strength[ i ]
            }
        }
        let cdef_size = w.len() - cdef_offset;
        // lr_params(): no loop restoration in the sequence.

        w.flag(frame.tx_mode_select); // tx_mode_select (5.9.21)
        if !frame.key {
            w.flag(false); // reference_select (5.9.23): single reference
        }
        // skip_mode_params(): not without reference_select. No warped motion in the sequence.
        w.flag(false); // reduced_tx_set
        if !frame.key {
            // global_motion_params() (5.9.24)
            for _ in 0..REFS_PER_FRAME {
                w.flag(false); // is_global
            }
        }
        // film_grain_params(): none in the sequence.
        w.trailing_bits();

        let unit = obu(
            OBU_FRAME_HEADER,
            &w.into_bytes(),
            Some(FRAME_HEADER_SIZE_BYTES),
        );
        // The payload starts after the OBU header and the size field.
        let start = 8 * (1 + FRAME_HEADER_SIZE_BYTES) as u32;
        FrameHeader {
            bytes: unit,
            qindex_offset: start + qindex_offset as u32,
            loop_filter_offset: start + loop_filter_offset as u32,
            cdef_offset: start + cdef_offset as u32,
            cdef_size: cdef_size as u32,
            size_field_offset: 1,
        }
    }
}

/// The limits of a level (A.3).
struct Level {
    seq_level_idx: u8,
    /// MaxPicSize, MaxHSize, MaxVSize: samples.
    picture: u64,
    width: u32,
    height: u32,
    /// MaxDisplayRate, MaxDecodeRate: samples per second.
    shown: u64,
    decoded: u64,
    /// MainMbps, in units of 100 kbit/s.
    bitrate: u64,
    /// MaxTiles, MaxTileCols.
    tiles: usize,
    tile_cols: usize,
}

/// A row of A.3's tables, in their order.
#[allow(clippy::too_many_arguments)]
const fn level(
    seq_level_idx: u8,
    picture: u64,
    width: u32,
    height: u32,
    shown: u64,
    decoded: u64,
    bitrate: u64,
    tiles: usize,
    tile_cols: usize,
) -> Level {
    Level {
        seq_level_idx,
        picture,
        width,
        height,
        shown,
        decoded,
        bitrate,
        tiles,
        tile_cols,
    }
}

/// The levels A.3 defines, the lowest first.
const LEVELS: [Level; 14] = [
    level(0, 147_456, 2048, 1152, 4_423_680, 5_529_600, 15, 8, 4),
    level(1, 278_784, 2816, 1584, 8_363_520, 10_454_400, 30, 8, 4),
    level(4, 665_856, 4352, 2448, 19_975_680, 24_969_600, 60, 16, 6),
    level(5, 1_065_024, 5504, 3096, 31_950_720, 39_938_400, 100, 16, 6),
    level(8, 2_359_296, 6144, 3456, 70_778_880, 77_856_768, 120, 32, 8),
    level(
        9,
        2_359_296,
        6144,
        3456,
        141_557_760,
        155_713_536,
        200,
        32,
        8,
    ),
    level(
        12,
        8_912_896,
        8192,
        4352,
        267_386_880,
        273_715_200,
        300,
        64,
        8,
    ),
    level(
        13,
        8_912_896,
        8192,
        4352,
        534_773_760,
        547_430_400,
        400,
        64,
        8,
    ),
    level(
        14,
        8_912_896,
        8192,
        4352,
        1_069_547_520,
        1_094_860_800,
        600,
        64,
        8,
    ),
    level(
        15,
        8_912_896,
        8192,
        4352,
        1_069_547_520,
        1_176_502_272,
        600,
        64,
        8,
    ),
    level(
        16,
        35_651_584,
        16384,
        8704,
        1_069_547_520,
        1_176_502_272,
        600,
        128,
        16,
    ),
    level(
        17,
        35_651_584,
        16384,
        8704,
        2_139_095_040,
        2_189_721_600,
        1000,
        128,
        16,
    ),
    level(
        18,
        35_651_584,
        16384,
        8704,
        4_278_190_080,
        4_379_443_200,
        1600,
        128,
        16,
    ),
    level(
        19,
        35_651_584,
        16384,
        8704,
        4_278_190_080,
        4_706_009_088,
        1600,
        128,
        16,
    ),
];

/// The lowest level (`seq_level_idx`) of the Main tier that allows the frames of `sequence`,
/// this many a second, at up to `bitrate` bit/s (A.3, profile 0); none when even the highest
/// does not.
pub fn level_of(sequence: &Sequence, framerate: u32, bitrate: u64) -> Option<u8> {
    let (width, height) = (sequence.width, sequence.height);
    let samples = u64::from(width) * u64::from(height);
    let rate = samples * u64::from(framerate);
    LEVELS
        .iter()
        .find(|l| {
            samples <= l.picture
                && width <= l.width
                && height <= l.height
                && rate <= l.shown
                && rate <= l.decoded
                && bitrate <= l.bitrate * 100_000
                && sequence.tiles.count() <= l.tiles
                && sequence.tiles.widths.len() <= l.tile_cols
        })
        .map(|l| l.seq_level_idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLOUR: Colour = Colour {
        primaries: 1,
        transfer: 13,
        matrix: 6,
        full_range: false,
    };

    /// `n` bits of `bytes` from bit `offset`, most significant first.
    fn read(bytes: &[u8], offset: u32, n: u32) -> u64 {
        (offset..offset + n).fold(0, |value, bit| {
            (value << 1) | u64::from((bytes[(bit / 8) as usize] >> (7 - bit % 8)) & 1)
        })
    }

    #[test]
    fn leb128_codes() {
        assert_eq!(leb128(0, 1), [0]);
        assert_eq!(leb128(127, 1), [0x7f]);
        assert_eq!(leb128(128, 2), [0x80, 0x01]);
        // With leading zeros, as the frame header's size is written.
        assert_eq!(leb128(5, 4), [0x85, 0x80, 0x80, 0x00]);
        assert_eq!(leb128_length(0), 1);
        assert_eq!(leb128_length(127), 1);
        assert_eq!(leb128_length(128), 2);
        assert_eq!(leb128_length(u32::MAX), 5);
    }

    #[test]
    fn a_temporal_delimiter_is_two_bytes() {
        assert_eq!(temporal_delimiter(), [0x12, 0x00]);
    }

    #[test]
    fn tiles_follow_the_limits() {
        // 1080p: 30x17 superblocks, one tile.
        let one = Sequence::new(false, 1920, 1080, COLOUR, 3).tiles;
        assert_eq!((one.widths.len(), one.heights.len()), (1, 1));
        assert_eq!((one.widths[0], one.heights[0]), (30, 17));
        // 8192x4320: 128x68 superblocks, wider than a tile may be (64 superblocks) and
        // larger than its area (2304): two columns, two rows.
        let four = Sequence::new(false, 8192, 4320, COLOUR, 3).tiles;
        assert_eq!((four.cols_log2, four.rows_log2), (1, 1));
        assert_eq!(four.widths, [64, 64]);
        assert_eq!(four.heights, [34, 34]);
        // 3440x1440: 54x23 superblocks, within both limits.
        let wide = Sequence::new(false, 3440, 1440, COLOUR, 3).tiles;
        assert_eq!(wide.count(), 1);
    }

    #[test]
    fn levels() {
        let level_of = |width, height, framerate, bitrate| {
            super::level_of(
                &Sequence::new(false, width, height, COLOUR, 3),
                framerate,
                bitrate,
            )
        };
        // A.3's examples: 1080p at 30 frames a second is level 4.0, at 60 level 4.1; 2160p at
        // 30 level 5.0, at 60 level 5.1.
        assert_eq!(level_of(1920, 1080, 30, 10_000_000), Some(8));
        assert_eq!(level_of(1920, 1080, 60, 10_000_000), Some(9));
        assert_eq!(level_of(3840, 2160, 30, 10_000_000), Some(12));
        assert_eq!(level_of(3840, 2160, 60, 10_000_000), Some(13));
        // Above 20 Mbit/s, 1080p at 60 needs level 5.0.
        assert_eq!(level_of(1920, 1080, 60, 25_000_000), Some(12));
        assert_eq!(level_of(3440, 1440, 30, 10_000_000), Some(12));
        assert_eq!(level_of(20000, 100, 1, 0), None);
    }

    /// The sequence header's fields, read back at their place.
    #[test]
    fn sequence_header() {
        let mut sequence = Sequence::new(true, 1920, 1080, COLOUR, 3);
        sequence.level_idx = 9;
        let unit = sequence.header();
        // OBU_SEQUENCE_HEADER with its size, which is the rest.
        assert_eq!(unit[0], 0x0a);
        assert_eq!(unit[1] as usize, unit.len() - 2);
        let payload = &unit[2..];
        // seq_profile, still_picture, reduced_still_picture_header, timing_info_present_flag,
        // initial_display_delay_present_flag, operating_points_cnt_minus_1, operating_point_idc.
        assert_eq!(read(payload, 0, 3 + 1 + 1 + 1 + 1 + 5 + 12), 0);
        assert_eq!(read(payload, 24, 5), 9); // seq_level_idx
        assert_eq!(read(payload, 29, 1), 0); // seq_tier
        // 11 bits each for 1919 and 1079.
        assert_eq!(read(payload, 30, 4), 10);
        assert_eq!(read(payload, 34, 4), 10);
        assert_eq!(read(payload, 38, 11), 1919);
        assert_eq!(read(payload, 49, 11), 1079);
        // Then the tools: 8 flags off, enable_order_hint, 2 flags off, the 2 screen content
        // flags off; order_hint_bits_minus_1; enable_superres, enable_cdef, enable_restoration.
        assert_eq!(read(payload, 60, 13), 1 << 4);
        assert_eq!(read(payload, 73, 3), 7);
        assert_eq!(read(payload, 76, 3), 0b010);
        // color_config: high_bitdepth, mono_chrome, color_description_present_flag, then 1,
        // 13, 6; color_range, chroma_sample_position, separate_uv_delta_q; then
        // film_grain_params_present, and the trailing one bit, which ends a byte.
        assert_eq!(read(payload, 79, 3), 0b101);
        assert_eq!(read(payload, 82, 24), (1 << 16) | (13 << 8) | 6);
        assert_eq!(read(payload, 106, 5), 0);
        assert_eq!(read(payload, 111, 1), 1);
        assert_eq!(payload.len(), 14);
    }

    /// The offsets point at the fields they name, and the size field holds the rest.
    #[test]
    fn frame_header_offsets() {
        let sequence = Sequence::new(false, 1920, 1080, COLOUR, 3);
        for key in [true, false] {
            let frame = Frame {
                key,
                order_hint: 3,
                interpolation_filter: SWITCHABLE,
                tx_mode_select: true,
                filters: Filters::DEFAULT,
            };
            let header = sequence.frame_header(&frame);
            let bytes = &header.bytes;
            assert_eq!(bytes[0], 0x1a, "OBU_FRAME_HEADER with its size");
            let size = (0..4).fold(0, |size, i| {
                size | u32::from(bytes[1 + i] & 0x7f) << (7 * i)
            });
            assert_eq!(size as usize, bytes.len() - 5);
            assert_eq!(header.size_field_offset, 1);
            assert_eq!(read(bytes, header.qindex_offset, 8), 128);
            assert_eq!(read(bytes, header.loop_filter_offset, 24), 0x410208);
            // damping 2, cdef_bits 0, then the first strength for luma and chroma.
            assert_eq!(header.cdef_size, 2 + 2 + 2 * 6);
            assert_eq!(read(bytes, header.cdef_offset, 4), 0b1000);
            assert_eq!(read(bytes, header.cdef_offset + 4, 12), 0b0010_0100_1001);
            // show_existing_frame, frame_type, show_frame.
            let start = 8 * 5;
            assert_eq!(read(bytes, start, 4), if key { 0b0001 } else { 0b0011 });
        }
    }
}
