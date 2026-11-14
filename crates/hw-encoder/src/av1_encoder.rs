// SPDX-License-Identifier: MPL-2.0

//! AV1 on a GPU's VA-API encoder: the tiles coded by the GPU, the headers written here (see
//! [`crate::av1`]).
//!
//! Every frame is predicted from the one before it, and none is held back. A key frame comes
//! first, and then only when asked. The bitrate changes with the next frame, whatever it is.
//!
//! The driver rewrites parts of the frame header the rate control decides (the quantizer, the
//! loop filter, CDEF) and the header's size, at the offsets it is given; the temporal
//! delimiter that starts each frame is added here, after the encoder.

use std::sync::Arc;

use crate::av1::{self, FRAME_HEADER_SIZE_BYTES, Filters, Frame, SWITCHABLE, Sequence};
use crate::common::{self, ENTRYPOINT, INTRA_PERIOD, RateControl};
use crate::error::{Error, Result};
use crate::h265::Colour;
use crate::va::{self, Buffer, Config, Context, Display, Surface, sys};
use crate::{Packet, Settings};

/// Profile 0 (Main): 4:2:0, 8 and 10 bits.
const PROFILE: sys::VAProfile = sys::VAProfileAV1Profile0;

/// The lowest quantizer the rate control may pick: 0 would make a frame lossless, which
/// changes the frame header's layout, and which a desktop never needs.
const MIN_QINDEX: u8 = 1;
const MAX_QINDEX: u8 = 255;

/// The interpolation filters (6.8.9), as libva's bit mask of them numbers them.
const EIGHTTAP: u8 = 0;

/// `TxMode` (6.8.21), as libva numbers it.
const TX_MODE_LARGEST: u32 = 1;
const TX_MODE_SELECT: u32 = 2;

/// The surfaces of a depth: their family and pixel format.
fn surface_format(ten_bits: bool) -> (u32, u32) {
    if ten_bits {
        (sys::VA_RT_FORMAT_YUV420_10, va::fourcc(b"P010"))
    } else {
        (sys::VA_RT_FORMAT_YUV420, va::fourcc(b"NV12"))
    }
}

/// Whether the GPU encodes AV1 with this chroma and depth.
pub(crate) fn supported(display: &Display, full_chroma: bool, ten_bits: bool) -> bool {
    !full_chroma
        && display.has(PROFILE, ENTRYPOINT)
        && display
            .attribute(PROFILE, ENTRYPOINT, sys::VAConfigAttribRTFormat)
            .is_some_and(|formats| formats & surface_format(ten_bits).0 != 0)
}

/// What the driver's encoder does, from its attributes.
struct Features {
    interpolation_filter: u8,
    tx_mode_select: bool,
    tile_size_bytes_minus_1: u32,
    max_tiles: usize,
}

impl Features {
    fn new(display: &Display) -> Self {
        let attribute = |attribute| display.attribute(PROFILE, ENTRYPOINT, attribute);
        let ext1 = attribute(sys::VAConfigAttribEncAV1Ext1)
            .map(|value| sys::VAConfigAttribValEncAV1Ext1 { value });
        let ext2 = attribute(sys::VAConfigAttribEncAV1Ext2)
            .map(|value| sys::VAConfigAttribValEncAV1Ext2 { value });
        // SAFETY: both views of these unions are plain integers.
        let (filters, ext2) = unsafe {
            (
                ext1.map(|ext1| ext1.bits.interpolation_filter()),
                ext2.map(|ext2| ext2.bits),
            )
        };
        // The encoder picks the filter of each block when it may; otherwise the first it has.
        let interpolation_filter = match filters {
            Some(filters) if filters & (1 << SWITCHABLE) != 0 => SWITCHABLE,
            Some(filters) if filters != 0 => filters.trailing_zeros() as u8,
            // UNTESTED: a driver that does not say: the regular filter, which every encoder
            // has.
            _ => EIGHTTAP,
        };
        match ext2 {
            Some(ext2) => Self {
                interpolation_filter,
                tx_mode_select: ext2.tx_mode_support() & (1 << TX_MODE_SELECT) != 0,
                tile_size_bytes_minus_1: ext2.tile_size_bytes_minus1(),
                max_tiles: ext2.max_tile_num_minus1() as usize + 1,
            },
            // UNTESTED: a driver that does not say. Four bytes of tile size are always
            // enough.
            None => Self {
                interpolation_filter,
                tx_mode_select: false,
                tile_size_bytes_minus_1: 3,
                max_tiles: 1,
            },
        }
    }
}

/// An AV1 encoder for frames of one size.
pub(crate) struct Av1 {
    context: Context,
    /// Kept as long as its context.
    _config: Config,
    sequence: Sequence,
    features: Features,
    /// The frame to encode: the converter draws it.
    input: Surface,
    /// The frames as a decoder will see them: the one being coded and the one before it,
    /// which it is predicted from.
    reconstructed: [Surface; 2],
    /// Which of the two the next frame is reconstructed in.
    current: usize,
    coded: Buffer,
    /// The frames since the key frame, the next one's included; `None` when it must be a
    /// key frame.
    count: Option<u32>,
    rate: RateControl,
}

impl Av1 {
    pub fn new(
        display: &Arc<Display>,
        settings: &Settings,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let depth = if settings.ten_bits { 10 } else { 8 };
        if settings.full_chroma {
            return Err(Error::Unsupported("AV1 in 4:4:4 (profile 1)".into()));
        }
        let (rt_format, fourcc) = surface_format(settings.ten_bits);
        // The headers written here: the sequence header and the frame headers.
        let headers = sys::VA_ENC_PACKED_HEADER_SEQUENCE | sys::VA_ENC_PACKED_HEADER_PICTURE;
        common::check(
            display,
            PROFILE,
            rt_format,
            headers,
            &format!("AV1 4:2:0 {depth} bits"),
        )?;
        let features = Features::new(display);

        let mut sequence = Sequence::new(
            settings.ten_bits,
            width,
            height,
            // What the converter makes of the desktop's sRGB: BT.709 primaries (1), the sRGB
            // transfer (13), BT.601 matrix (6), limited range.
            Colour {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: false,
            },
            features.tile_size_bytes_minus_1,
        );
        if sequence.tiles.count() > features.max_tiles {
            return Err(Error::Unsupported(format!(
                "{width}x{height}: {} tiles",
                sequence.tiles.count()
            )));
        }
        sequence.level_idx = av1::level_of(
            &sequence,
            settings.framerate,
            u64::from(settings.max_bitrate.max(settings.bitrate)),
        )
        .ok_or_else(|| {
            Error::Unsupported(format!("{width}x{height}: beyond AV1's highest level"))
        })?;

        let config = Config::new(
            display,
            PROFILE,
            ENTRYPOINT,
            &[
                (sys::VAConfigAttribRTFormat, rt_format),
                (sys::VAConfigAttribRateControl, sys::VA_RC_CBR),
                (sys::VAConfigAttribEncPackedHeaders, headers),
            ],
        )?;
        // Whole mode info units of 8x8 samples (5.9.9): the converter fills them.
        let (coded_width, coded_height) = (width.next_multiple_of(8), height.next_multiple_of(8));
        let context = Context::new(&config, coded_width, coded_height)?;
        let surface = || Surface::new(display, rt_format, fourcc, coded_width, coded_height);
        let input = surface()?;
        let reconstructed = [surface()?, surface()?];
        let coded = Buffer::coded(&context, common::coded_size(coded_width, coded_height))?;
        Ok(Self {
            context,
            _config: config,
            sequence,
            features,
            input,
            reconstructed,
            current: 0,
            coded,
            count: None,
            rate: RateControl::new(display, PROFILE, settings.framerate, settings.bitrate),
        })
    }

    /// The size of the input surface.
    pub fn coded_size(&self) -> (u32, u32) {
        (
            self.sequence.width.next_multiple_of(8),
            self.sequence.height.next_multiple_of(8),
        )
    }

    /// The surface to draw the next frame in.
    pub fn input(&self) -> &Surface {
        &self.input
    }

    /// Change the bitrate, in bit/s, from the next frame on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.rate.set_bitrate(bitrate);
    }

    /// Encode the frame drawn in the input surface; as a key frame when asked.
    pub fn encode(&mut self, keyframe: bool) -> Result<Packet> {
        let count = match self.count {
            Some(count) if !keyframe => count,
            _ => 0,
        };
        let frame = Frame {
            key: count == 0,
            // Modulo 2^ORDER_HINT_BITS (8).
            order_hint: count as u8,
            interpolation_filter: self.features.interpolation_filter,
            tx_mode_select: self.features.tx_mode_select,
            filters: Filters::DEFAULT,
        };
        // Whatever fails below, the next frame has nothing to be predicted from.
        self.count = None;
        let buffers = self.buffers(&frame)?;
        self.context.render(&self.input, &buffers)?;
        self.input.sync()?;
        let coded = self.coded.read_coded()?;
        self.rate.told();
        self.current ^= 1;
        self.count = Some(count.wrapping_add(1).max(1));
        // A temporal unit: the delimiter, then what the encoder made of the headers and the
        // frame.
        let mut data = av1::temporal_delimiter();
        data.extend_from_slice(&coded);
        Ok(Packet {
            data,
            keyframe: frame.key,
        })
    }

    /// What the encoder is given for one frame, in the order it takes it.
    fn buffers(&self, frame: &Frame) -> Result<Vec<Buffer>> {
        let context = &self.context;
        let mut buffers = Vec::new();
        // The headers before the frame header, which the encoder puts in the stream first:
        // the sequence header, with a key frame.
        let sequence_header = if frame.key {
            self.sequence.header()
        } else {
            Vec::new()
        };
        if frame.key {
            // SAFETY: the structure of this buffer type.
            buffers.push(unsafe {
                Buffer::parameter(
                    context,
                    sys::VAEncSequenceParameterBufferType,
                    &self.sequence_parameters(),
                )
            }?);
        }
        buffers.extend(self.rate.buffers(context, frame.key)?);
        if frame.key {
            common::packed(
                context,
                &mut buffers,
                sys::VAEncPackedHeaderSequence,
                &sequence_header,
            )?;
        }
        let header = self.sequence.frame_header(frame);
        let mut parameters = self.picture_parameters(frame);
        parameters.bit_offset_qindex = header.qindex_offset;
        parameters.bit_offset_loopfilter_params = header.loop_filter_offset;
        parameters.bit_offset_cdef_params = header.cdef_offset;
        parameters.size_in_bits_cdef_params = header.cdef_size;
        // From the start of the headers given with the frame.
        parameters.byte_offset_frame_hdr_obu_size =
            sequence_header.len() as u32 + header.size_field_offset;
        parameters.size_in_bits_frame_hdr_obu = header.bytes.len() as u32 * 8;
        // SAFETY: the structure of this buffer type.
        buffers.push(unsafe {
            Buffer::parameter(context, sys::VAEncPictureParameterBufferType, &parameters)
        }?);
        common::packed(
            context,
            &mut buffers,
            sys::VAEncPackedHeaderPicture,
            &header.bytes,
        )?;
        // One tile group: every tile.
        let group = sys::VAEncTileGroupBufferAV1 {
            tg_start: 0,
            tg_end: (self.sequence.tiles.count() - 1) as u8,
            ..Default::default()
        };
        // SAFETY: the structure of this buffer type.
        buffers.push(unsafe {
            Buffer::parameter(context, sys::VAEncSliceParameterBufferType, &group)
        }?);
        Ok(buffers)
    }

    /// The sequence, as the encoder is told it: what [`Sequence::header`] writes.
    fn sequence_parameters(&self) -> sys::VAEncSequenceParameterBufferAV1 {
        let mut parameters = sys::VAEncSequenceParameterBufferAV1 {
            seq_profile: 0,
            seq_level_idx: self.sequence.level_idx,
            seq_tier: 0,
            hierarchical_flag: 0,
            intra_period: INTRA_PERIOD,
            ip_period: 1,
            bits_per_second: self.rate.bitrate(),
            order_hint_bits_minus_1: (av1::ORDER_HINT_BITS - 1) as u8,
            ..Default::default()
        };
        // SAFETY: both views of the union are plain integers.
        let fields = unsafe { &mut parameters.seq_fields.bits };
        fields.set_enable_order_hint(1);
        fields.set_enable_cdef(1);
        fields.set_bit_depth_minus8(if self.sequence.ten_bits { 2 } else { 0 });
        fields.set_subsampling_x(1);
        fields.set_subsampling_y(1);
        parameters
    }

    /// The frame, as the encoder is told it: what [`Sequence::frame_header`] writes, and the
    /// surfaces.
    fn picture_parameters(&self, frame: &Frame) -> sys::VAEncPictureParameterBufferAV1 {
        let filters = &frame.filters;
        let tiles = &self.sequence.tiles;
        let mut parameters = sys::VAEncPictureParameterBufferAV1 {
            frame_width_minus_1: (self.sequence.width - 1) as u16,
            frame_height_minus_1: (self.sequence.height - 1) as u16,
            reconstructed_frame: self.reconstructed[self.current].id(),
            coded_buf: self.coded.id(),
            reference_frames: [sys::VA_INVALID_SURFACE; 8],
            ref_frame_idx: [0; 7],
            // PRIMARY_REF_NONE for a key frame; LAST_FRAME's otherwise.
            primary_ref_frame: if frame.key { 7 } else { 0 },
            order_hint: frame.order_hint,
            refresh_frame_flags: 0xff,
            interpolation_filter: frame.interpolation_filter,
            filter_level: [filters.loop_filter_level[0], filters.loop_filter_level[1]],
            filter_level_u: filters.loop_filter_level[2],
            filter_level_v: filters.loop_filter_level[3],
            // What a frame with no past starts with (setup_past_independence(), 7.20); the
            // header does not change them.
            ref_deltas: [1, 0, 0, 0, -1, 0, -1, -1],
            base_qindex: filters.base_q_idx,
            min_base_qindex: MIN_QINDEX,
            max_base_qindex: MAX_QINDEX,
            tile_cols: tiles.widths.len() as u8,
            tile_rows: tiles.heights.len() as u8,
            cdef_damping_minus_3: filters.cdef_damping_minus_3,
            cdef_bits: filters.cdef_bits,
            cdef_y_strengths: filters.cdef_strengths,
            cdef_uv_strengths: filters.cdef_strengths,
            ..Default::default()
        };
        for (i, &width) in tiles.widths.iter().enumerate() {
            parameters.width_in_sbs_minus_1[i] = (width - 1) as u16;
        }
        for (i, &height) in tiles.heights.iter().enumerate() {
            parameters.height_in_sbs_minus_1[i] = (height - 1) as u16;
        }
        if !frame.key {
            // The previous frame is in every slot, and LAST_FRAME names it.
            parameters.reference_frames = [self.reconstructed[self.current ^ 1].id(); 8];
            // SAFETY: both views of the union are plain integers.
            unsafe { parameters.ref_frame_ctrl_l0.fields.set_search_idx0(1) };
        }
        // SAFETY: both views of these unions are plain integers.
        unsafe {
            let flags = &mut parameters.picture_flags.bits;
            flags.set_frame_type(if frame.key { 0 } else { 1 });
            // A shown key frame is error resilient (5.9.2).
            flags.set_error_resilient_mode(frame.key.into());
            flags.set_allow_high_precision_mv((!frame.key).into());
            let mode = &mut parameters.mode_control_flags.bits;
            mode.set_tx_mode(if frame.tx_mode_select {
                TX_MODE_SELECT
            } else {
                TX_MODE_LARGEST
            });
            // SINGLE_REFERENCE.
            mode.set_reference_mode(0);
            parameters
                .tile_group_obu_hdr_info
                .bits
                .set_obu_has_size_field(1);
        }
        debug_assert_eq!(FRAME_HEADER_SIZE_BYTES, 4);
        parameters
    }
}
