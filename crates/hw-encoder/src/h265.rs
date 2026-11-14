// SPDX-License-Identifier: MPL-2.0

//! The headers of an HEVC stream, written as Rec. ITU-T H.265 (08/2021) specifies them: the
//! parameter sets and the slice segment header. The GPU codes the pictures; with VA-API, the
//! headers are the application's to write.
//!
//! The stream is the simplest a desktop needs: one layer, one slice per picture, each picture
//! predicted from the one before it and nothing else, no picture held back. Clause numbers
//! are those of the Recommendation.

use crate::bits::{BitWriter, nal_unit};

/// NAL unit types (table 7-1).
const TRAIL_R: u8 = 1;
const IDR_W_RADL: u8 = 19;
const VPS_NUT: u8 = 32;
const SPS_NUT: u8 = 33;
const PPS_NUT: u8 = 34;

/// Slice types (table 7-7).
const SLICE_B: u32 = 0;
const SLICE_P: u32 = 1;
const SLICE_I: u32 = 2;

/// The picture order count is sent modulo two to this power (7.4.3.2.1: 4 to 16). Each
/// picture refers to the one before it, so any size does.
pub const LOG2_MAX_POC_LSB: u32 = 8;

/// The decoded picture buffer holds the reference picture and the current one.
pub const DEC_PIC_BUFFERING: u32 = 2;

/// The profiles (annex A.3), by chroma format and depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 4:2:0, 8 bits.
    Main,
    /// 4:2:0, 10 bits.
    Main10,
    /// 4:4:4, 8 bits: a format range extensions profile.
    Main444,
    /// 4:4:4, 10 bits: a format range extensions profile.
    Main444Ten,
}

impl Profile {
    pub fn new(full_chroma: bool, ten_bits: bool) -> Self {
        match (full_chroma, ten_bits) {
            (false, false) => Self::Main,
            (false, true) => Self::Main10,
            (true, false) => Self::Main444,
            (true, true) => Self::Main444Ten,
        }
    }

    /// `general_profile_idc` (A.3.2, A.3.3, A.3.5).
    pub fn idc(self) -> u8 {
        match self {
            Self::Main => 1,
            Self::Main10 => 2,
            Self::Main444 | Self::Main444Ten => 4,
        }
    }

    /// The `general_profile_compatibility_flag`s, the flag of profile `j` in bit `31 - j`: as
    /// they are in the bitstream, and in the `profile-compatibility-indicator` of RFC 7798.
    /// A Main stream is a Main 10 stream too (A.3.2, note).
    pub fn compatibility(self) -> u32 {
        let flag = |profile: u32| 1 << (31 - profile);
        match self {
            Self::Main => flag(1) | flag(2),
            Self::Main10 => flag(2),
            Self::Main444 | Self::Main444Ten => flag(4),
        }
    }

    pub fn full_chroma(self) -> bool {
        matches!(self, Self::Main444 | Self::Main444Ten)
    }

    pub fn bit_depth(self) -> u32 {
        match self {
            Self::Main | Self::Main444 => 8,
            Self::Main10 | Self::Main444Ten => 10,
        }
    }

    /// `chroma_format_idc` (table 6-1).
    fn chroma_format_idc(self) -> u32 {
        if self.full_chroma() { 3 } else { 1 }
    }

    /// How many luma samples a chroma sample spans, each way (table 6-1): the unit of the
    /// conformance window.
    fn chroma_step(self) -> u32 {
        if self.full_chroma() { 1 } else { 2 }
    }
}

/// The coding tools and block sizes the GPU's encoder works with: the parameter sets must
/// say what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coding {
    /// Base 2 logarithms of the smallest coding block, the coding tree block, and the
    /// smallest and largest transform blocks.
    pub log2_min_cb: u32,
    pub log2_ctb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub max_transform_depth_inter: u32,
    pub max_transform_depth_intra: u32,
    /// Asymmetric motion partitions.
    pub amp: bool,
    /// Sample adaptive offset.
    pub sao: bool,
    /// Motion vectors predicted from the reference picture's.
    pub temporal_mvp: bool,
    pub transform_skip: bool,
    /// The quantizer changes within a picture: what a rate control needs.
    pub cu_qp_delta: bool,
}

impl Coding {
    /// `diff_cu_qp_delta_depth` (7.4.3.3.1): the quantizer may change down to the smallest
    /// coding block.
    pub fn cu_qp_delta_depth(&self) -> u32 {
        self.log2_ctb - self.log2_min_cb
    }
}

/// The colours of the pictures (annex E.3.1, tables E.3 to E.5), written in the stream so
/// that a decoder does not guess them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

/// A coded video sequence: what its parameter sets say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sequence {
    pub profile: Profile,
    /// `general_level_idc`: see [`level`].
    pub level_idc: u8,
    /// The size of the pictures shown.
    pub width: u32,
    pub height: u32,
    pub coding: Coding,
    pub colour: Colour,
}

/// A picture's place in the sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    /// An IDR picture: the first of a coded video sequence, predicted from nothing.
    pub idr: bool,
    /// Its picture order count: 0 for an IDR picture, one more for each picture after it.
    pub order: u32,
    /// Predicted from the previous picture through both reference lists (a B slice whose two
    /// lists are the same): what encoders without P slices code instead.
    pub both_lists: bool,
}

/// The header of a NAL unit of the base layer and the lowest temporal sub-layer (7.3.1.2).
fn nal_header(nal_unit_type: u8) -> [u8; 2] {
    // forbidden_zero_bit, nal_unit_type (6), nuh_layer_id (6), nuh_temporal_id_plus1 (3).
    [nal_unit_type << 1, 1]
}

impl Sequence {
    /// The size of the coded pictures: whole smallest coding blocks (7.4.3.2.1).
    pub fn coded_width(&self) -> u32 {
        self.width.next_multiple_of(1 << self.coding.log2_min_cb)
    }

    pub fn coded_height(&self) -> u32 {
        self.height.next_multiple_of(1 << self.coding.log2_min_cb)
    }

    /// The coding tree units of a picture.
    pub fn ctus(&self) -> u32 {
        let ctb = 1 << self.coding.log2_ctb;
        self.coded_width().div_ceil(ctb) * self.coded_height().div_ceil(ctb)
    }

    /// `profile_tier_level( 1, 0 )` (7.3.3), Main tier.
    fn profile_tier_level(&self, w: &mut BitWriter) {
        w.bits(0, 2); // general_profile_space
        w.flag(false); // general_tier_flag
        w.bits(u64::from(self.profile.idc()), 5);
        w.bits(u64::from(self.profile.compatibility()), 32);
        w.flag(true); // general_progressive_source_flag
        w.flag(false); // general_interlaced_source_flag
        w.flag(false); // general_non_packed_constraint_flag
        w.flag(true); // general_frame_only_constraint_flag
        if self.profile.full_chroma() {
            // The format range extensions profiles, by table A.2.
            w.flag(true); // general_max_12bit_constraint_flag
            w.flag(true); // general_max_10bit_constraint_flag
            w.flag(self.profile.bit_depth() == 8); // general_max_8bit_constraint_flag
            w.flag(false); // general_max_422chroma_constraint_flag
            w.flag(false); // general_max_420chroma_constraint_flag
            w.flag(false); // general_max_monochrome_constraint_flag
            w.flag(false); // general_intra_constraint_flag
            w.flag(false); // general_one_picture_only_constraint_flag
            w.flag(true); // general_lower_bit_rate_constraint_flag
            w.bits(0, 34); // general_reserved_zero_34bits
        } else {
            // general_reserved_zero_7bits, general_one_picture_only_constraint_flag,
            // general_reserved_zero_35bits.
            w.bits(0, 43);
        }
        w.flag(false); // general_inbld_flag
        w.bits(u64::from(self.level_idc), 8);
    }

    /// The video parameter set (7.3.2.1).
    pub fn vps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(0, 4); // vps_video_parameter_set_id
        w.flag(true); // vps_base_layer_internal_flag
        w.flag(true); // vps_base_layer_available_flag
        w.bits(0, 6); // vps_max_layers_minus1
        w.bits(0, 3); // vps_max_sub_layers_minus1
        w.flag(true); // vps_temporal_id_nesting_flag
        w.bits(0xffff, 16); // vps_reserved_0xffff_16bits
        self.profile_tier_level(&mut w);
        w.flag(true); // vps_sub_layer_ordering_info_present_flag
        w.ue(DEC_PIC_BUFFERING - 1); // vps_max_dec_pic_buffering_minus1[ 0 ]
        w.ue(0); // vps_max_num_reorder_pics[ 0 ]
        w.ue(0); // vps_max_latency_increase_plus1[ 0 ]
        w.bits(0, 6); // vps_max_layer_id
        w.ue(0); // vps_num_layer_sets_minus1
        w.flag(false); // vps_timing_info_present_flag
        w.flag(false); // vps_extension_flag
        w.trailing_bits();
        nal_unit(&nal_header(VPS_NUT), &w.into_bytes())
    }

    /// The sequence parameter set (7.3.2.2.1).
    pub fn sps(&self) -> Vec<u8> {
        let coding = &self.coding;
        let mut w = BitWriter::new();
        w.bits(0, 4); // sps_video_parameter_set_id
        w.bits(0, 3); // sps_max_sub_layers_minus1
        w.flag(true); // sps_temporal_id_nesting_flag
        self.profile_tier_level(&mut w);
        w.ue(0); // sps_seq_parameter_set_id
        w.ue(self.profile.chroma_format_idc());
        if self.profile.full_chroma() {
            w.flag(false); // separate_colour_plane_flag
        }
        w.ue(self.coded_width()); // pic_width_in_luma_samples
        w.ue(self.coded_height()); // pic_height_in_luma_samples
        // The conformance window crops the coded pictures to the ones shown, in units of
        // chroma samples (7.4.3.2.1): in 4:2:0, an odd size shows one more column or row,
        // which the encoder fills with the last one.
        let step = self.profile.chroma_step();
        let right = (self.coded_width() - self.width) / step;
        let bottom = (self.coded_height() - self.height) / step;
        w.flag(right != 0 || bottom != 0); // conformance_window_flag
        if right != 0 || bottom != 0 {
            w.ue(0); // conf_win_left_offset
            w.ue(right); // conf_win_right_offset
            w.ue(0); // conf_win_top_offset
            w.ue(bottom); // conf_win_bottom_offset
        }
        w.ue(self.profile.bit_depth() - 8); // bit_depth_luma_minus8
        w.ue(self.profile.bit_depth() - 8); // bit_depth_chroma_minus8
        w.ue(LOG2_MAX_POC_LSB - 4); // log2_max_pic_order_cnt_lsb_minus4
        w.flag(true); // sps_sub_layer_ordering_info_present_flag
        w.ue(DEC_PIC_BUFFERING - 1); // sps_max_dec_pic_buffering_minus1[ 0 ]
        w.ue(0); // sps_max_num_reorder_pics[ 0 ]
        w.ue(0); // sps_max_latency_increase_plus1[ 0 ]
        w.ue(coding.log2_min_cb - 3); // log2_min_luma_coding_block_size_minus3
        w.ue(coding.log2_ctb - coding.log2_min_cb); // log2_diff_max_min_luma_coding_block_size
        w.ue(coding.log2_min_tb - 2); // log2_min_luma_transform_block_size_minus2
        w.ue(coding.log2_max_tb - coding.log2_min_tb); // log2_diff_max_min_luma_transform_block_size
        w.ue(coding.max_transform_depth_inter); // max_transform_hierarchy_depth_inter
        w.ue(coding.max_transform_depth_intra); // max_transform_hierarchy_depth_intra
        w.flag(false); // scaling_list_enabled_flag
        w.flag(coding.amp); // amp_enabled_flag
        w.flag(coding.sao); // sample_adaptive_offset_enabled_flag
        w.flag(false); // pcm_enabled_flag
        // The reference picture set goes in each slice header.
        w.ue(0); // num_short_term_ref_pic_sets
        w.flag(false); // long_term_ref_pics_present_flag
        w.flag(coding.temporal_mvp); // sps_temporal_mvp_enabled_flag
        w.flag(false); // strong_intra_smoothing_enabled_flag
        w.flag(true); // vui_parameters_present_flag
        self.vui(&mut w);
        w.flag(false); // sps_extension_present_flag
        w.trailing_bits();
        nal_unit(&nal_header(SPS_NUT), &w.into_bytes())
    }

    /// The video usability information (E.2.1): the colours, and nothing else. No timing:
    /// a desktop's pictures come when it changes.
    fn vui(&self, w: &mut BitWriter) {
        w.flag(false); // aspect_ratio_info_present_flag
        w.flag(false); // overscan_info_present_flag
        w.flag(true); // video_signal_type_present_flag
        w.bits(5, 3); // video_format: unspecified (table E.2)
        w.flag(self.colour.full_range); // video_full_range_flag
        w.flag(true); // colour_description_present_flag
        w.bits(u64::from(self.colour.primaries), 8); // colour_primaries
        w.bits(u64::from(self.colour.transfer), 8); // transfer_characteristics
        w.bits(u64::from(self.colour.matrix), 8); // matrix_coeffs
        w.flag(false); // chroma_loc_info_present_flag
        w.flag(false); // neutral_chroma_indication_flag
        w.flag(false); // field_seq_flag
        w.flag(false); // frame_field_info_present_flag
        w.flag(false); // default_display_window_flag
        w.flag(false); // vui_timing_info_present_flag
        w.flag(false); // bitstream_restriction_flag
    }

    /// The picture parameter set (7.3.2.3.1).
    pub fn pps(&self) -> Vec<u8> {
        let coding = &self.coding;
        let mut w = BitWriter::new();
        w.ue(0); // pps_pic_parameter_set_id
        w.ue(0); // pps_seq_parameter_set_id
        w.flag(false); // dependent_slice_segments_enabled_flag
        w.flag(false); // output_flag_present_flag
        w.bits(0, 3); // num_extra_slice_header_bits
        w.flag(false); // sign_data_hiding_enabled_flag
        w.flag(false); // cabac_init_present_flag
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.se(0); // init_qp_minus26
        w.flag(false); // constrained_intra_pred_flag
        w.flag(coding.transform_skip); // transform_skip_enabled_flag
        w.flag(coding.cu_qp_delta); // cu_qp_delta_enabled_flag
        if coding.cu_qp_delta {
            w.ue(coding.cu_qp_delta_depth()); // diff_cu_qp_delta_depth
        }
        w.se(0); // pps_cb_qp_offset
        w.se(0); // pps_cr_qp_offset
        w.flag(false); // pps_slice_chroma_qp_offsets_present_flag
        w.flag(false); // weighted_pred_flag
        w.flag(false); // weighted_bipred_flag
        w.flag(false); // transquant_bypass_enabled_flag
        w.flag(false); // tiles_enabled_flag
        w.flag(false); // entropy_coding_sync_enabled_flag
        w.flag(true); // pps_loop_filter_across_slices_enabled_flag
        w.flag(false); // deblocking_filter_control_present_flag
        w.flag(false); // pps_scaling_list_data_present_flag
        w.flag(false); // lists_modification_present_flag
        w.ue(0); // log2_parallel_merge_level_minus2
        w.flag(false); // slice_segment_header_extension_present_flag
        w.flag(false); // pps_extension_present_flag
        w.trailing_bits();
        nal_unit(&nal_header(PPS_NUT), &w.into_bytes())
    }

    /// `nal_unit_type` of a picture's slice.
    pub fn nal_unit_type(picture: &Picture) -> u8 {
        if picture.idr { IDR_W_RADL } else { TRAIL_R }
    }

    /// `slice_type` of a picture's slice, as VA-API and the bitstream number it.
    pub fn slice_type(picture: &Picture) -> u32 {
        match (picture.idr, picture.both_lists) {
            (true, _) => SLICE_I,
            (false, true) => SLICE_B,
            (false, false) => SLICE_P,
        }
    }

    /// The NAL unit of a picture's only slice segment, up to its data: the slice segment
    /// header (7.3.6.1), ended on a byte. The encoder puts the coded slice after it.
    pub fn slice_header(&self, picture: &Picture) -> Vec<u8> {
        let coding = &self.coding;
        let slice_type = Self::slice_type(picture);
        let mut w = BitWriter::new();
        w.flag(true); // first_slice_segment_in_pic_flag
        if picture.idr {
            w.flag(false); // no_output_of_prior_pics_flag
        }
        w.ue(0); // slice_pic_parameter_set_id
        w.ue(slice_type);
        let temporal_mvp = !picture.idr && coding.temporal_mvp;
        if !picture.idr {
            let lsb = picture.order & ((1 << LOG2_MAX_POC_LSB) - 1);
            w.bits(u64::from(lsb), LOG2_MAX_POC_LSB); // slice_pic_order_cnt_lsb
            w.flag(false); // short_term_ref_pic_set_sps_flag
            // st_ref_pic_set( 0 ) (7.3.7): the picture before this one, and used by it.
            w.ue(1); // num_negative_pics
            w.ue(0); // num_positive_pics
            w.ue(0); // delta_poc_s0_minus1[ 0 ]
            w.flag(true); // used_by_curr_pic_s0_flag[ 0 ]
            if coding.temporal_mvp {
                w.flag(true); // slice_temporal_mvp_enabled_flag
            }
        }
        if coding.sao {
            w.flag(true); // slice_sao_luma_flag
            w.flag(true); // slice_sao_chroma_flag
        }
        if slice_type != SLICE_I {
            w.flag(true); // num_ref_idx_active_override_flag
            w.ue(0); // num_ref_idx_l0_active_minus1
            if slice_type == SLICE_B {
                w.ue(0); // num_ref_idx_l1_active_minus1
                w.flag(false); // mvd_l1_zero_flag
            }
            if temporal_mvp && slice_type == SLICE_B {
                w.flag(true); // collocated_from_l0_flag
            }
            // One reference picture in the list: no collocated_ref_idx.
            w.ue(0); // five_minus_max_num_merge_cand
        }
        w.se(0); // slice_qp_delta
        // The deblocking filter is on (no control in the PPS), so the flag is present.
        w.flag(true); // slice_loop_filter_across_slices_enabled_flag
        w.trailing_bits(); // byte_alignment( )
        nal_unit(&nal_header(Self::nal_unit_type(picture)), &w.into_bytes())
    }
}

/// The limits of a level (tables A.8 and A.9, Main tier): `general_level_idc` (30 times the
/// level number), the luma samples of a picture, the luma samples per second, and the
/// bitrate in units of 1000 bit/s.
const LEVELS: [(u8, u64, u64, u64); 13] = [
    (30, 36_864, 552_960, 128),
    (60, 122_880, 3_686_400, 1_500),
    (63, 245_760, 7_372_800, 3_000),
    (90, 552_960, 16_588_800, 6_000),
    (93, 983_040, 33_177_600, 10_000),
    (120, 2_228_224, 66_846_720, 12_000),
    (123, 2_228_224, 133_693_440, 20_000),
    (150, 8_912_896, 267_386_880, 25_000),
    (153, 8_912_896, 534_773_760, 40_000),
    (156, 8_912_896, 1_069_547_520, 60_000),
    (180, 35_651_584, 1_069_547_520, 60_000),
    (183, 35_651_584, 2_139_095_040, 120_000),
    (186, 35_651_584, 4_278_190_080, 240_000),
];

/// The lowest level (`general_level_idc`) of the Main tier that allows coded pictures of
/// this size, this many a second, at up to `bitrate` bit/s (A.4.1, A.4.2); none when even
/// the highest does not. The bitrate limit taken is the Main profile's, the lowest of the
/// profiles'.
pub fn level(width: u32, height: u32, framerate: u32, bitrate: u64) -> Option<u8> {
    let samples = u64::from(width) * u64::from(height);
    let side = u64::from(width.max(height));
    LEVELS
        .iter()
        .find(|&&(_, max_picture, max_rate, max_bitrate)| {
            samples <= max_picture
                // Neither side longer than Sqrt( MaxLumaPs * 8 ).
                && side * side <= max_picture * 8
                && samples * u64::from(framerate) <= max_rate
                && bitrate <= max_bitrate * 1000
        })
        .map(|&(idc, ..)| idc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the Intel Arc A380's encoder works with.
    pub(crate) const A380: Coding = Coding {
        log2_min_cb: 3,
        log2_ctb: 6,
        log2_min_tb: 2,
        log2_max_tb: 5,
        max_transform_depth_inter: 2,
        max_transform_depth_intra: 2,
        amp: true,
        sao: true,
        temporal_mvp: true,
        transform_skip: true,
        cu_qp_delta: true,
    };

    fn sequence(profile: Profile, width: u32, height: u32) -> Sequence {
        Sequence {
            profile,
            level_idc: 123,
            width,
            height,
            coding: A380,
            colour: Colour {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: false,
            },
        }
    }

    /// A NAL unit without its emulation prevention bytes (7.3.1.1).
    fn payload(unit: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut zeros = 0;
        for &byte in &unit[4..] {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            bytes.push(byte);
        }
        [&unit[..4], &bytes].concat()
    }

    /// The profile, tier and level as RFC 7798 gives them in an offer: profile-id, then the
    /// compatibility flags and the 48 bits from general_progressive_source_flag in base16.
    fn offered(profile: Profile) -> (u8, String, String) {
        let sps = payload(&sequence(profile, 1920, 1080).sps());
        // Start code (4), NAL header (2), one byte up to sps_temporal_id_nesting_flag, then
        // the profile_tier_level.
        let ptl = &sps[7..];
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
        (ptl[0] & 0x1f, hex(&ptl[1..5]), hex(&ptl[5..11]))
    }

    /// The values measured in the streams of a reference encoder (GStreamer's on an Intel
    /// Arc A380), which are those of annex A.3 and table A.2.
    #[test]
    fn profiles_are_written_as_the_standard_says() {
        assert_eq!(
            offered(Profile::Main),
            (1, "60000000".into(), "900000000000".into())
        );
        assert_eq!(
            offered(Profile::Main10),
            (2, "20000000".into(), "900000000000".into())
        );
        assert_eq!(
            offered(Profile::Main444),
            (4, "08000000".into(), "9E0800000000".into())
        );
        assert_eq!(
            offered(Profile::Main444Ten),
            (4, "08000000".into(), "9C0800000000".into())
        );
        // general_level_idc follows.
        assert_eq!(payload(&sequence(Profile::Main, 1920, 1080).sps())[18], 123);
    }

    #[test]
    fn pictures_are_coded_in_whole_blocks() {
        let s = sequence(Profile::Main, 1920, 1080);
        assert_eq!((s.coded_width(), s.coded_height()), (1920, 1080));
        assert_eq!(s.ctus(), 30 * 17);
        let s = sequence(Profile::Main, 322, 242);
        assert_eq!((s.coded_width(), s.coded_height()), (328, 248));
        assert_eq!(s.ctus(), 6 * 4);
    }

    #[test]
    fn levels() {
        // 1080p at 60 frames a second: level 4.1; above 20 Mbit/s, level 5.
        assert_eq!(level(1920, 1080, 60, 10_000_000), Some(123));
        assert_eq!(level(1920, 1080, 30, 10_000_000), Some(120));
        assert_eq!(level(1920, 1080, 60, 30_000_000), Some(153));
        // 3440x1440 and 3840x2160: level 5 at 30 frames a second, 5.1 at 60.
        assert_eq!(level(3440, 1440, 30, 10_000_000), Some(150));
        assert_eq!(level(3840, 2160, 60, 30_000_000), Some(153));
        // A picture small enough for level 1, but too wide for it and for level 2.
        assert_eq!(level(1024, 32, 1, 0), Some(63));
        assert_eq!(level(8192, 4320, 120, 0), Some(186));
        assert_eq!(level(16384, 8640, 30, 0), None);
    }

    #[test]
    fn slices() {
        let s = sequence(Profile::Main, 1920, 1080);
        let idr = Picture {
            idr: true,
            order: 0,
            both_lists: true,
        };
        // IDR_W_RADL, then: first slice, no_output_of_prior_pics 0, PPS 0 (1), slice type I
        // (011), SAO luma and chroma, slice_qp_delta 0 (1), loop filter across slices, and
        // the alignment: 1 0 1 011 1 1 | 1 1 1 00000.
        assert_eq!(s.slice_header(&idr), [0, 0, 0, 1, 19 << 1, 1, 0xaf, 0xe0]);
        let next = Picture {
            idr: false,
            order: 257,
            both_lists: true,
        };
        // TRAIL_R, then: first slice, PPS 0 (1), slice type B (1), order 257 modulo 256
        // (00000001), no SPS reference picture set, one negative picture (010), no positive
        // one (1), the one before (1), used, temporal MVP, SAO luma and chroma, override,
        // one reference in each list (1, 1), mvd_l1_zero 0, collocated from list 0, five
        // merge candidates (1), slice_qp_delta 0 (1), loop filter across slices, alignment.
        assert_eq!(
            s.slice_header(&next),
            [
                0,
                0,
                0,
                1,
                1 << 1,
                1,
                0b1110_0000,
                0b0010_0101,
                0b1111_1111,
                0b0111_1100
            ]
        );
    }
}
