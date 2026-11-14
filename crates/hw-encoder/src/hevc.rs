// SPDX-License-Identifier: MPL-2.0

//! HEVC on a GPU's VA-API encoder: the pictures coded by the GPU, the headers written here
//! (see [`crate::h265`]).
//!
//! Every picture is predicted from the one before it, and none is held back. A keyframe
//! (an IDR picture, with the parameter sets) comes first, and then only when asked. The
//! bitrate changes with the next picture, whatever it is.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::h265::{self, Coding, Colour, Picture, Profile, Sequence};
use crate::va::{self, Buffer, Config, Context, Display, Surface, sys};
use crate::{Packet, Settings};

/// The low-power entrypoint: the encoder of the GPU's fixed-function hardware, the only one
/// Intel GPUs have since Arc.
const ENTRYPOINT: sys::VAEntrypoint = sys::VAEntrypointEncSliceLP;

/// The picture order count starts again, with a keyframe, before it outgrows its type: after
/// more than a year at 60 pictures a second.
const MAX_ORDER: u32 = i32::MAX as u32;

/// What the encoder is told of the distance between keyframes, which only come when asked:
/// the rate control plans for pictures predicted from the one before.
const INTRA_PERIOD: u32 = 1024;

/// The rate control buffer: half a second of the bitrate. It bounds the size of a picture,
/// a keyframe's above all, and so the bursts on the network.
fn buffer_size(bitrate: u32) -> u32 {
    (bitrate / 2).max(1)
}

fn profile(profile: Profile) -> sys::VAProfile {
    match profile {
        Profile::Main => sys::VAProfileHEVCMain,
        Profile::Main10 => sys::VAProfileHEVCMain10,
        Profile::Main444 => sys::VAProfileHEVCMain444,
        Profile::Main444Ten => sys::VAProfileHEVCMain444_10,
    }
}

/// The surfaces a profile's pictures are in: their family and pixel format.
fn surface_format(profile: Profile) -> (u32, u32) {
    match profile {
        Profile::Main => (sys::VA_RT_FORMAT_YUV420, va::fourcc(b"NV12")),
        Profile::Main10 => (sys::VA_RT_FORMAT_YUV420_10, va::fourcc(b"P010")),
        Profile::Main444 => (sys::VA_RT_FORMAT_YUV444, va::fourcc(b"AYUV")),
        Profile::Main444Ten => (sys::VA_RT_FORMAT_YUV444_10, va::fourcc(b"Y410")),
    }
}

/// What the driver's encoder works with, from its attributes.
fn coding(display: &Display, profile: sys::VAProfile) -> Coding {
    let features = display
        .attribute(profile, ENTRYPOINT, sys::VAConfigAttribEncHEVCFeatures)
        .map(|value| sys::VAConfigAttribValEncHEVCFeatures { value });
    let sizes = display
        .attribute(profile, ENTRYPOINT, sys::VAConfigAttribEncHEVCBlockSizes)
        .map(|value| sys::VAConfigAttribValEncHEVCBlockSizes { value });
    let (Some(features), Some(sizes)) = (features, sizes) else {
        // UNTESTED: a driver that does not say (these attributes came with libva 2.12). What
        // every HEVC encoder is expected to code: 32x32 coding tree blocks, 16x16 coding
        // blocks, no optional tool.
        return Coding {
            log2_min_cb: 4,
            log2_ctb: 5,
            log2_min_tb: 2,
            log2_max_tb: 5,
            max_transform_depth_inter: 3,
            max_transform_depth_intra: 3,
            amp: false,
            sao: false,
            temporal_mvp: false,
            transform_skip: false,
            cu_qp_delta: true,
        };
    };
    // SAFETY: both views of these unions are plain integers.
    let (features, sizes) = unsafe { (features.bits, sizes.bits) };
    // A tool the driver supports, or requires, is on.
    let on = |feature: u32| feature != sys::VA_FEATURE_NOT_SUPPORTED;
    Coding {
        log2_min_cb: sizes.log2_min_luma_coding_block_size_minus3() + 3,
        log2_ctb: sizes.log2_max_coding_tree_block_size_minus3() + 3,
        log2_min_tb: sizes.log2_min_luma_transform_block_size_minus2() + 2,
        log2_max_tb: sizes.log2_max_luma_transform_block_size_minus2() + 2,
        max_transform_depth_inter: sizes.max_max_transform_hierarchy_depth_inter(),
        max_transform_depth_intra: sizes.max_max_transform_hierarchy_depth_intra(),
        amp: on(features.amp()),
        sao: on(features.sao()),
        temporal_mvp: on(features.temporal_mvp()),
        transform_skip: on(features.transform_skip()),
        // The rate control changes the quantizer within a picture.
        cu_qp_delta: true,
    }
}

/// Whether the GPU encodes this profile.
pub(crate) fn supported(display: &Display, full_chroma: bool, ten_bits: bool) -> bool {
    display.has(profile(Profile::new(full_chroma, ten_bits)), ENTRYPOINT)
}

/// An HEVC encoder for pictures of one size.
pub(crate) struct Hevc {
    context: Context,
    /// Kept as long as its context.
    _config: Config,
    sequence: Sequence,
    /// The picture to encode: the converter draws it.
    input: Surface,
    /// The pictures as a decoder will see them: the one being coded and the one before it,
    /// which it is predicted from.
    reconstructed: [Surface; 2],
    /// Which of the two the next picture is reconstructed in.
    current: usize,
    coded: Buffer,
    /// The picture order count of the next picture; `None` when it must be a keyframe.
    order: Option<u32>,
    /// The encoder has no P slices: pictures are predicted through both lists.
    both_lists: bool,
    quality: Option<u32>,
    framerate: u32,
    /// In bit/s.
    bitrate: u32,
    /// The encoder was not told of `bitrate` yet.
    bitrate_changed: bool,
}

impl Hevc {
    pub fn new(
        display: &Arc<Display>,
        settings: &Settings,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let stream_profile = Profile::new(settings.full_chroma, settings.ten_bits);
        let va_profile = profile(stream_profile);
        if !display.has(va_profile, ENTRYPOINT) {
            return Err(Error::Unsupported(format!("HEVC {stream_profile:?}")));
        }
        let attribute = |attribute| display.attribute(va_profile, ENTRYPOINT, attribute);
        let (rt_format, fourcc) = surface_format(stream_profile);
        if attribute(sys::VAConfigAttribRTFormat).is_none_or(|formats| formats & rt_format == 0) {
            return Err(Error::Unsupported(format!(
                "HEVC {stream_profile:?} pictures"
            )));
        }
        if attribute(sys::VAConfigAttribRateControl).is_none_or(|modes| modes & sys::VA_RC_CBR == 0)
        {
            return Err(Error::Unsupported("constant bitrate".into()));
        }
        // The headers written here: the parameter sets and the slice headers.
        let headers = sys::VA_ENC_PACKED_HEADER_SEQUENCE | sys::VA_ENC_PACKED_HEADER_SLICE;
        if attribute(sys::VAConfigAttribEncPackedHeaders)
            .is_none_or(|packed| packed & headers != headers)
        {
            return Err(Error::Unsupported("headers from the application".into()));
        }
        // An encoder that says its pictures predicted from the past use both lists has no P
        // slices (libva's VAConfigAttribPredictionDirection).
        let both_lists = attribute(sys::VAConfigAttribPredictionDirection)
            .is_some_and(|direction| direction & sys::VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0);
        // The middle of the driver's range of speed against quality, when it has one.
        let quality = attribute(sys::VAConfigAttribEncQualityRange)
            .filter(|&levels| levels > 1)
            .map(|levels| levels.div_ceil(2));

        let coding = coding(display, va_profile);
        let mut sequence = Sequence {
            profile: stream_profile,
            level_idc: 0,
            width,
            height,
            coding,
            // What the converter makes of the desktop's sRGB: BT.709 primaries (1), the sRGB
            // transfer (13), BT.601 matrix (6), limited range.
            colour: Colour {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: false,
            },
        };
        let (coded_width, coded_height) = (sequence.coded_width(), sequence.coded_height());
        sequence.level_idc = h265::level(
            coded_width,
            coded_height,
            settings.framerate,
            u64::from(settings.max_bitrate.max(settings.bitrate)),
        )
        .ok_or_else(|| {
            Error::Unsupported(format!("{width}x{height}: beyond HEVC's highest level"))
        })?;

        let config = Config::new(
            display,
            va_profile,
            ENTRYPOINT,
            &[
                (sys::VAConfigAttribRTFormat, rt_format),
                (sys::VAConfigAttribRateControl, sys::VA_RC_CBR),
                (sys::VAConfigAttribEncPackedHeaders, headers),
            ],
        )?;
        let context = Context::new(&config, coded_width, coded_height)?;
        let surface = || Surface::new(display, rt_format, fourcc, coded_width, coded_height);
        let input = surface()?;
        let reconstructed = [surface()?, surface()?];
        // More than a picture takes uncompressed, which no coded picture reaches.
        let raw = coded_width as usize * coded_height as usize * 3 * 2;
        let coded = Buffer::coded(&context, raw + (1 << 16))?;
        Ok(Self {
            context,
            _config: config,
            sequence,
            input,
            reconstructed,
            current: 0,
            coded,
            order: None,
            both_lists,
            quality,
            framerate: settings.framerate,
            bitrate: settings.bitrate,
            bitrate_changed: false,
        })
    }

    /// The size of the coded pictures, and of the input surface.
    pub fn coded_size(&self) -> (u32, u32) {
        (self.sequence.coded_width(), self.sequence.coded_height())
    }

    /// The surface to draw the next picture in.
    pub fn input(&self) -> &Surface {
        &self.input
    }

    /// Change the bitrate, in bit/s, from the next picture on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.bitrate_changed = true;
        }
    }

    /// Encode the picture drawn in the input surface; as a keyframe when asked.
    pub fn encode(&mut self, keyframe: bool) -> Result<Packet> {
        let order = match self.order {
            Some(order) if !keyframe && order < MAX_ORDER => order,
            _ => 0,
        };
        let picture = Picture {
            idr: order == 0,
            order,
            both_lists: self.both_lists,
        };
        // Whatever fails below, the next picture has nothing to be predicted from.
        self.order = None;
        let buffers = self.buffers(&picture)?;
        self.context.render(&self.input, &buffers)?;
        self.input.sync()?;
        let data = self.coded.read_coded()?;
        self.bitrate_changed = false;
        self.current ^= 1;
        self.order = Some(order + 1);
        Ok(Packet {
            data,
            keyframe: picture.idr,
        })
    }

    /// What the encoder is given for one picture, in the order it takes it.
    fn buffers(&self, picture: &Picture) -> Result<Vec<Buffer>> {
        let context = &self.context;
        let mut buffers = Vec::new();
        if picture.idr {
            // SAFETY: the structure of this buffer type.
            buffers.push(unsafe {
                Buffer::parameter(
                    context,
                    sys::VAEncSequenceParameterBufferType,
                    &self.sequence_parameters(),
                )
            }?);
            let framerate = sys::VAEncMiscParameterFrameRate {
                framerate: self.framerate,
                ..Default::default()
            };
            // SAFETY: the structure of this parameter type.
            buffers.push(unsafe {
                Buffer::miscellaneous(context, sys::VAEncMiscParameterTypeFrameRate, &framerate)
            }?);
            if let Some(quality_level) = self.quality {
                let quality = sys::VAEncMiscParameterBufferQualityLevel {
                    quality_level,
                    ..Default::default()
                };
                // SAFETY: the structure of this parameter type.
                buffers.push(unsafe {
                    Buffer::miscellaneous(
                        context,
                        sys::VAEncMiscParameterTypeQualityLevel,
                        &quality,
                    )
                }?);
            }
        }
        if picture.idr || self.bitrate_changed {
            let buffer_size = buffer_size(self.bitrate);
            let rate = sys::VAEncMiscParameterRateControl {
                bits_per_second: self.bitrate,
                // Constant bitrate: the target is the maximum.
                target_percentage: 100,
                // The time the buffer holds, in milliseconds.
                window_size: (u64::from(buffer_size) * 1000 / u64::from(self.bitrate.max(1)))
                    as u32,
                ..Default::default()
            };
            let hrd = sys::VAEncMiscParameterHRD {
                initial_buffer_fullness: buffer_size / 2,
                buffer_size,
                ..Default::default()
            };
            // SAFETY: the structures of these parameter types.
            unsafe {
                buffers.push(Buffer::miscellaneous(
                    context,
                    sys::VAEncMiscParameterTypeRateControl,
                    &rate,
                )?);
                buffers.push(Buffer::miscellaneous(
                    context,
                    sys::VAEncMiscParameterTypeHRD,
                    &hrd,
                )?);
            }
        }
        // SAFETY: the structure of this buffer type.
        buffers.push(unsafe {
            Buffer::parameter(
                context,
                sys::VAEncPictureParameterBufferType,
                &self.picture_parameters(picture),
            )
        }?);
        if picture.idr {
            let sets = [
                self.sequence.vps(),
                self.sequence.sps(),
                self.sequence.pps(),
            ]
            .concat();
            self.packed(&mut buffers, sys::VAEncPackedHeaderSequence, &sets)?;
        }
        self.packed(
            &mut buffers,
            sys::VAEncPackedHeaderSlice,
            &self.sequence.slice_header(picture),
        )?;
        // SAFETY: the structure of this buffer type.
        buffers.push(unsafe {
            Buffer::parameter(
                context,
                sys::VAEncSliceParameterBufferType,
                &self.slice_parameters(picture),
            )
        }?);
        Ok(buffers)
    }

    /// A header written here, for the encoder to put in the stream as it is: whole NAL
    /// units, with their emulation prevention bytes.
    fn packed(
        &self,
        buffers: &mut Vec<Buffer>,
        type_: sys::VAEncPackedHeaderType,
        header: &[u8],
    ) -> Result<()> {
        let parameters = sys::VAEncPackedHeaderParameterBuffer {
            type_,
            bit_length: header.len() as u32 * 8,
            has_emulation_bytes: 1,
            ..Default::default()
        };
        // SAFETY: the structure of this buffer type.
        buffers.push(unsafe {
            Buffer::parameter(
                &self.context,
                sys::VAEncPackedHeaderParameterBufferType,
                &parameters,
            )
        }?);
        buffers.push(Buffer::bytes(
            &self.context,
            sys::VAEncPackedHeaderDataBufferType,
            header,
        )?);
        Ok(())
    }

    /// The sequence, as the encoder is told it: what [`Sequence::sps`] writes.
    fn sequence_parameters(&self) -> sys::VAEncSequenceParameterBufferHEVC {
        let sequence = &self.sequence;
        let coding = &sequence.coding;
        let mut parameters = sys::VAEncSequenceParameterBufferHEVC {
            general_profile_idc: sequence.profile.idc(),
            general_level_idc: sequence.level_idc,
            general_tier_flag: 0,
            intra_period: INTRA_PERIOD,
            intra_idr_period: INTRA_PERIOD,
            ip_period: 1,
            bits_per_second: self.bitrate,
            pic_width_in_luma_samples: sequence.coded_width() as u16,
            pic_height_in_luma_samples: sequence.coded_height() as u16,
            log2_min_luma_coding_block_size_minus3: (coding.log2_min_cb - 3) as u8,
            log2_diff_max_min_luma_coding_block_size: (coding.log2_ctb - coding.log2_min_cb) as u8,
            log2_min_transform_block_size_minus2: (coding.log2_min_tb - 2) as u8,
            log2_diff_max_min_transform_block_size: (coding.log2_max_tb - coding.log2_min_tb) as u8,
            max_transform_hierarchy_depth_inter: coding.max_transform_depth_inter as u8,
            max_transform_hierarchy_depth_intra: coding.max_transform_depth_intra as u8,
            ..Default::default()
        };
        // SAFETY: both views of the union are plain integers.
        let fields = unsafe { &mut parameters.seq_fields.bits };
        fields.set_chroma_format_idc(if sequence.profile.full_chroma() { 3 } else { 1 });
        fields.set_bit_depth_luma_minus8(sequence.profile.bit_depth() - 8);
        fields.set_bit_depth_chroma_minus8(sequence.profile.bit_depth() - 8);
        fields.set_amp_enabled_flag(coding.amp.into());
        fields.set_sample_adaptive_offset_enabled_flag(coding.sao.into());
        fields.set_sps_temporal_mvp_enabled_flag(coding.temporal_mvp.into());
        // No picture refers to a later one.
        fields.set_low_delay_seq(1);
        parameters
    }

    /// A reference to a reconstructed picture.
    fn reference(&self, index: usize, order: u32, flags: u32) -> sys::VAPictureHEVC {
        sys::VAPictureHEVC {
            picture_id: self.reconstructed[index].id(),
            pic_order_cnt: order as i32,
            flags,
            ..Default::default()
        }
    }

    /// The picture, as the encoder is told it: what [`Sequence::pps`] writes, and the
    /// surfaces.
    fn picture_parameters(&self, picture: &Picture) -> sys::VAEncPictureParameterBufferHEVC {
        let coding = &self.sequence.coding;
        let none = sys::VAPictureHEVC {
            picture_id: sys::VA_INVALID_ID,
            flags: sys::VA_PICTURE_HEVC_INVALID,
            ..Default::default()
        };
        let mut parameters = sys::VAEncPictureParameterBufferHEVC {
            decoded_curr_pic: self.reference(self.current, picture.order, 0),
            reference_frames: [none; 15],
            coded_buf: self.coded.id(),
            // The reference picture whose motion vectors predict this picture's: the only
            // one, or none.
            collocated_ref_pic_index: if !picture.idr && coding.temporal_mvp {
                0
            } else {
                0xff
            },
            last_picture: 0,
            pic_init_qp: 26,
            diff_cu_qp_delta_depth: if coding.cu_qp_delta {
                coding.cu_qp_delta_depth() as u8
            } else {
                0
            },
            nal_unit_type: Sequence::nal_unit_type(picture),
            ..Default::default()
        };
        if !picture.idr {
            parameters.reference_frames[0] = self.reference(
                self.current ^ 1,
                picture.order - 1,
                sys::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            );
        }
        // SAFETY: both views of the union are plain integers.
        let fields = unsafe { &mut parameters.pic_fields.bits };
        fields.set_idr_pic_flag(picture.idr.into());
        // 1 for a picture predicted from nothing, 2 for one predicted from the past.
        fields.set_coding_type(if picture.idr { 1 } else { 2 });
        fields.set_reference_pic_flag(1);
        fields.set_transform_skip_enabled_flag(coding.transform_skip.into());
        fields.set_cu_qp_delta_enabled_flag(coding.cu_qp_delta.into());
        fields.set_pps_loop_filter_across_slices_enabled_flag(1);
        parameters
    }

    /// The picture's only slice, as the encoder is told it: what [`Sequence::slice_header`]
    /// writes.
    fn slice_parameters(&self, picture: &Picture) -> sys::VAEncSliceParameterBufferHEVC {
        let coding = &self.sequence.coding;
        let none = sys::VAPictureHEVC {
            picture_id: sys::VA_INVALID_ID,
            flags: sys::VA_PICTURE_HEVC_INVALID,
            ..Default::default()
        };
        let mut parameters = sys::VAEncSliceParameterBufferHEVC {
            slice_segment_address: 0,
            num_ctu_in_slice: self.sequence.ctus(),
            slice_type: Sequence::slice_type(picture) as u8,
            slice_pic_parameter_set_id: 0,
            ref_pic_list0: [none; 15],
            ref_pic_list1: [none; 15],
            max_num_merge_cand: 5,
            ..Default::default()
        };
        // SAFETY: both views of the union are plain integers.
        let fields = unsafe { &mut parameters.slice_fields.bits };
        fields.set_last_slice_of_pic_flag(1);
        fields.set_slice_sao_luma_flag(coding.sao.into());
        fields.set_slice_sao_chroma_flag(coding.sao.into());
        fields.set_slice_loop_filter_across_slices_enabled_flag(1);
        if !picture.idr {
            let reference = self.reference(
                self.current ^ 1,
                picture.order - 1,
                sys::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            );
            parameters.ref_pic_list0[0] = reference;
            fields.set_num_ref_idx_active_override_flag(1);
            fields.set_slice_temporal_mvp_enabled_flag(coding.temporal_mvp.into());
            if picture.both_lists {
                parameters.ref_pic_list1[0] = reference;
                fields.set_collocated_from_l0_flag(1);
            }
        }
        parameters
    }
}
