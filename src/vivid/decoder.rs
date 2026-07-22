//! FFmpeg-backed elementary-packet decoder used by Vivid video media channels.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::ptr;

use vivid_protocol::media::ParsedVideoPacket;
use vivid_protocol::messages::ParsedVideoSourceConfig;

const AVMEDIA_TYPE_VIDEO: c_int = 0;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;
const AV_PKT_FLAG_KEY: c_int = 1;
const AVERROR_EOF: c_int = -541_478_725;
const SWS_BILINEAR: c_int = 2;
const PACKET_TIME_BASE: AVRational = AVRational { num: 1, den: 1_000_000 };

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AVRational {
    num: c_int,
    den: c_int,
}

#[repr(C)]
struct AVCodecPrefix {
    name: *const c_char,
    long_name: *const c_char,
    media_type: c_int,
    id: c_int,
}

#[repr(C)]
struct AVCodecParametersPrefix {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
    coded_side_data: *mut c_void,
    nb_coded_side_data: c_int,
    format: c_int,
    bit_rate: i64,
    bits_per_coded_sample: c_int,
    bits_per_raw_sample: c_int,
    profile: c_int,
    level: c_int,
    width: c_int,
    height: c_int,
}

#[repr(C)]
struct AVPacketPrefix {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: c_int,
    stream_index: c_int,
    flags: c_int,
    side_data: *mut c_void,
    side_data_elems: c_int,
    duration: i64,
    pos: i64,
    opaque: *mut c_void,
    opaque_ref: *mut c_void,
    time_base: AVRational,
}

#[repr(C)]
struct AVFramePrefix {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    pict_type: c_int,
    sample_aspect_ratio: AVRational,
    pts: i64,
}

#[derive(Debug)]
pub struct DecodedFrame {
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub struct Decoder {
    context: *mut c_void,
    packet: *mut c_void,
    frame: *mut c_void,
    scale: *mut c_void,
    scale_format: c_int,
    scale_size: (c_int, c_int),
    rgba_format: c_int,
    sws_colorspace: c_int,
    source_full_range: c_int,
}

impl Decoder {
    pub fn new(config: &ParsedVideoSourceConfig) -> io::Result<Self> {
        // Homebrew FFmpeg's native `av1` decoder advertises VideoToolbox and can open even on
        // Macs which cannot actually decode AV1 in hardware, only to fail on the first packet.
        // Prefer the bounded software implementation in that case. Other codecs retain FFmpeg's
        // primary decoder selection and can use their supported hardware paths.
        let decoder_name = if config.codec == "av1" { "libdav1d" } else { config.codec.as_str() };
        let codec_name =
            CString::new(decoder_name).map_err(|_| invalid("video codec contains NUL"))?;
        let codec = unsafe { avcodec_find_decoder_by_name(codec_name.as_ptr()) };
        if codec.is_null() {
            return Err(invalid_owned(format!("FFmpeg decoder {decoder_name:?} is unavailable")));
        }
        let mut context = unsafe { avcodec_alloc_context3(codec) };
        if context.is_null() {
            return Err(io::Error::other("FFmpeg could not allocate a decoder context"));
        }

        let mut parameters = unsafe { avcodec_parameters_alloc() };
        if parameters.is_null() {
            unsafe { avcodec_free_context(&mut context) };
            return Err(io::Error::other("FFmpeg could not allocate codec parameters"));
        }

        let result = (|| {
            let codec_prefix = unsafe { &*(codec as *const AVCodecPrefix) };
            let parameters_prefix = unsafe { &mut *(parameters as *mut AVCodecParametersPrefix) };
            parameters_prefix.codec_type = AVMEDIA_TYPE_VIDEO;
            parameters_prefix.codec_id = codec_prefix.id;
            parameters_prefix.profile = config.profile;
            parameters_prefix.level = config.level;
            parameters_prefix.width = c_int::try_from(config.width)
                .map_err(|_| invalid("video width exceeds FFmpeg limits"))?;
            parameters_prefix.height = c_int::try_from(config.height)
                .map_err(|_| invalid("video height exceeds FFmpeg limits"))?;
            parameters_prefix.bit_rate = i64::try_from(config.bitrate).unwrap_or(i64::MAX);

            if !config.extradata.is_empty() {
                let allocation = config
                    .extradata
                    .len()
                    .checked_add(AV_INPUT_BUFFER_PADDING_SIZE)
                    .ok_or_else(|| invalid("codec extradata size overflows"))?;
                let extradata = unsafe { av_mallocz(allocation) } as *mut u8;
                if extradata.is_null() {
                    return Err(io::Error::other("FFmpeg could not allocate codec extradata"));
                }
                unsafe {
                    ptr::copy_nonoverlapping(
                        config.extradata.as_ptr(),
                        extradata,
                        config.extradata.len(),
                    );
                }
                parameters_prefix.extradata = extradata;
                parameters_prefix.extradata_size = c_int::try_from(config.extradata.len())
                    .map_err(|_| invalid("codec extradata exceeds i32"))?;
            }

            check_ffmpeg("could not configure decoder", unsafe {
                avcodec_parameters_to_context(context, parameters)
            })?;
            check_ffmpeg("could not set decoder packet time base", unsafe {
                av_opt_set_q(context, c"pkt_timebase".as_ptr(), PACKET_TIME_BASE, 0)
            })?;
            check_ffmpeg("could not open decoder", unsafe {
                avcodec_open2(context, codec, ptr::null_mut())
            })?;
            Ok(())
        })();
        unsafe { avcodec_parameters_free(&mut parameters) };
        if let Err(error) = result {
            let mut context = context;
            unsafe { avcodec_free_context(&mut context) };
            return Err(error);
        }

        let packet = unsafe { av_packet_alloc() };
        let frame = unsafe { av_frame_alloc() };
        if packet.is_null() || frame.is_null() {
            let mut packet = packet;
            let mut frame = frame;
            let mut context = context;
            unsafe {
                av_packet_free(&mut packet);
                av_frame_free(&mut frame);
                avcodec_free_context(&mut context);
            }
            return Err(io::Error::other("FFmpeg could not allocate decode buffers"));
        }
        let rgba_name = c"rgba";
        let rgba_format = unsafe { av_get_pix_fmt(rgba_name.as_ptr()) };
        if rgba_format < 0 {
            let mut packet = packet;
            let mut frame = frame;
            let mut context = context;
            unsafe {
                av_packet_free(&mut packet);
                av_frame_free(&mut frame);
                avcodec_free_context(&mut context);
            }
            return Err(io::Error::other("FFmpeg RGBA pixel format is unavailable"));
        }

        Ok(Self {
            context,
            packet,
            frame,
            scale: ptr::null_mut(),
            scale_format: -1,
            scale_size: (0, 0),
            rgba_format,
            sws_colorspace: match config.matrix {
                1 => 1, // ITU-R BT.709
                2 => 5, // ITU-R BT.601 / SMPTE 170M
                3 => 9, // ITU-R BT.2020
                _ => 1, // RGB/identity input; coefficients are unused by RGB paths
            },
            source_full_range: c_int::from(config.range == 2),
        })
    }

    pub fn push(&mut self, packet: ParsedVideoPacket<'_>) -> io::Result<Vec<DecodedFrame>> {
        if packet.data.is_empty() {
            return Ok(Vec::new());
        }
        let size = c_int::try_from(packet.data.len())
            .map_err(|_| invalid("encoded packet exceeds FFmpeg i32 size"))?;
        check_ffmpeg("could not allocate encoded packet", unsafe {
            av_new_packet(self.packet, size)
        })?;
        let av_packet = unsafe { &mut *(self.packet as *mut AVPacketPrefix) };
        unsafe {
            ptr::copy_nonoverlapping(packet.data.as_ptr(), av_packet.data, packet.data.len())
        };
        av_packet.pts = packet.pts_us;
        av_packet.dts = packet.dts_us;
        av_packet.duration = i64::try_from(packet.duration_us).unwrap_or(i64::MAX);
        av_packet.flags = if packet.flags & vivid_protocol::media::VIDEO_PACKET_KEY != 0 {
            AV_PKT_FLAG_KEY
        } else {
            0
        };
        av_packet.time_base = PACKET_TIME_BASE;

        let send_result = unsafe { avcodec_send_packet(self.context, self.packet) };
        unsafe { av_packet_unref(self.packet) };
        check_ffmpeg("decoder rejected encoded packet", send_result)?;
        self.receive_frames(false)
    }

    pub fn finish(&mut self) -> io::Result<Vec<DecodedFrame>> {
        let result = unsafe { avcodec_send_packet(self.context, ptr::null()) };
        if result < 0 && result != AVERROR_EOF {
            return Err(ffmpeg_error("could not drain decoder", result));
        }
        self.receive_frames(true)
    }

    fn receive_frames(&mut self, draining: bool) -> io::Result<Vec<DecodedFrame>> {
        let mut output = Vec::new();
        loop {
            let result = unsafe { avcodec_receive_frame(self.context, self.frame) };
            if result == -libc::EAGAIN || result == AVERROR_EOF {
                break;
            }
            check_ffmpeg("could not receive decoded frame", result)?;
            output.push(self.convert_frame()?);
            unsafe { av_frame_unref(self.frame) };
        }
        if draining && output.is_empty() {
            log::debug!("Vivid decoder drained without an additional frame");
        }
        Ok(output)
    }

    fn convert_frame(&mut self) -> io::Result<DecodedFrame> {
        let frame = unsafe { &*(self.frame as *const AVFramePrefix) };
        if frame.width <= 0 || frame.height <= 0 || frame.width > 8192 || frame.height > 8192 {
            return Err(invalid("decoder produced invalid frame dimensions"));
        }
        if self.scale.is_null()
            || self.scale_format != frame.format
            || self.scale_size != (frame.width, frame.height)
        {
            if !self.scale.is_null() {
                unsafe { sws_freeContext(self.scale) };
            }
            self.scale = unsafe {
                sws_getContext(
                    frame.width,
                    frame.height,
                    frame.format,
                    frame.width,
                    frame.height,
                    self.rgba_format,
                    SWS_BILINEAR,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                )
            };
            if self.scale.is_null() {
                return Err(io::Error::other("FFmpeg could not create RGBA converter"));
            }
            let source_coefficients = unsafe { sws_getCoefficients(self.sws_colorspace) };
            let destination_coefficients = unsafe { sws_getCoefficients(1) };
            if source_coefficients.is_null()
                || destination_coefficients.is_null()
                || unsafe {
                    sws_setColorspaceDetails(
                        self.scale,
                        source_coefficients,
                        self.source_full_range,
                        destination_coefficients,
                        1,
                        0,
                        1 << 16,
                        1 << 16,
                    )
                } < 0
            {
                return Err(io::Error::other("FFmpeg could not apply declared video colorimetry"));
            }
            self.scale_format = frame.format;
            self.scale_size = (frame.width, frame.height);
        }

        let width = u32::try_from(frame.width).unwrap();
        let height = u32::try_from(frame.height).unwrap();
        let length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid("decoded frame allocation overflows"))?;
        let mut rgba = vec![0_u8; length];
        let mut destination = [ptr::null_mut(); 4];
        destination[0] = rgba.as_mut_ptr();
        let destination_lines = [frame.width * 4, 0, 0, 0];
        let converted = unsafe {
            sws_scale(
                self.scale,
                frame.data.as_ptr() as *const *const u8,
                frame.linesize.as_ptr(),
                0,
                frame.height,
                destination.as_mut_ptr(),
                destination_lines.as_ptr(),
            )
        };
        if converted != frame.height {
            return Err(io::Error::other("FFmpeg returned a partial RGBA frame"));
        }
        Ok(DecodedFrame { pts_us: frame.pts, width, height, rgba })
    }

    #[cfg(test)]
    fn packet_time_base(&self) -> io::Result<AVRational> {
        let mut time_base = AVRational { num: 0, den: 0 };
        check_ffmpeg("could not read decoder packet time base", unsafe {
            av_opt_get_q(self.context, c"pkt_timebase".as_ptr(), 0, &mut time_base)
        })?;
        Ok(time_base)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.scale.is_null() {
                sws_freeContext(self.scale);
            }
            av_frame_free(&mut self.frame);
            av_packet_free(&mut self.packet);
            avcodec_free_context(&mut self.context);
        }
    }
}

fn check_ffmpeg(context: &str, result: c_int) -> io::Result<()> {
    if result < 0 { Err(ffmpeg_error(context, result)) } else { Ok(()) }
}

fn ffmpeg_error(context: &str, code: c_int) -> io::Error {
    let mut buffer = [0_i8; 256];
    let description = if unsafe { av_strerror(code, buffer.as_mut_ptr(), buffer.len()) } == 0 {
        unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned()
    } else {
        format!("FFmpeg error {code}")
    };
    io::Error::other(format!("{context}: {description}"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_owned(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[link(name = "avcodec")]
unsafe extern "C" {}

#[link(name = "avutil")]
unsafe extern "C" {}

#[link(name = "swscale")]
unsafe extern "C" {}

unsafe extern "C" {
    fn avcodec_find_decoder_by_name(name: *const c_char) -> *const c_void;
    fn avcodec_alloc_context3(codec: *const c_void) -> *mut c_void;
    fn avcodec_free_context(context: *mut *mut c_void);
    fn avcodec_parameters_alloc() -> *mut c_void;
    fn avcodec_parameters_free(parameters: *mut *mut c_void);
    fn avcodec_parameters_to_context(context: *mut c_void, parameters: *const c_void) -> c_int;
    fn avcodec_open2(
        context: *mut c_void,
        codec: *const c_void,
        options: *mut *mut c_void,
    ) -> c_int;
    fn avcodec_send_packet(context: *mut c_void, packet: *const c_void) -> c_int;
    fn avcodec_receive_frame(context: *mut c_void, frame: *mut c_void) -> c_int;
    fn av_packet_alloc() -> *mut c_void;
    fn av_packet_free(packet: *mut *mut c_void);
    fn av_packet_unref(packet: *mut c_void);
    fn av_new_packet(packet: *mut c_void, size: c_int) -> c_int;
    fn av_frame_alloc() -> *mut c_void;
    fn av_frame_free(frame: *mut *mut c_void);
    fn av_frame_unref(frame: *mut c_void);
    fn av_mallocz(size: usize) -> *mut c_void;
    fn av_opt_set_q(
        object: *mut c_void,
        name: *const c_char,
        value: AVRational,
        flags: c_int,
    ) -> c_int;
    #[cfg(test)]
    fn av_opt_get_q(
        object: *mut c_void,
        name: *const c_char,
        flags: c_int,
        output: *mut AVRational,
    ) -> c_int;
    fn av_get_pix_fmt(name: *const c_char) -> c_int;
    fn av_strerror(error: c_int, buffer: *mut c_char, buffer_size: usize) -> c_int;
    fn sws_getContext(
        source_width: c_int,
        source_height: c_int,
        source_format: c_int,
        destination_width: c_int,
        destination_height: c_int,
        destination_format: c_int,
        flags: c_int,
        source_filter: *mut c_void,
        destination_filter: *mut c_void,
        parameters: *const f64,
    ) -> *mut c_void;
    fn sws_scale(
        context: *mut c_void,
        source: *const *const u8,
        source_stride: *const c_int,
        source_slice_y: c_int,
        source_slice_height: c_int,
        destination: *mut *mut u8,
        destination_stride: *const c_int,
    ) -> c_int;
    fn sws_getCoefficients(colorspace: c_int) -> *const c_int;
    fn sws_setColorspaceDetails(
        context: *mut c_void,
        inverse_table: *const c_int,
        source_range: c_int,
        table: *const c_int,
        destination_range: c_int,
        brightness: c_int,
        contrast: c_int,
        saturation: c_int,
    ) -> c_int;
    fn sws_freeContext(context: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_context_uses_protocol_packet_time_base() {
        let decoder = Decoder::new(&ParsedVideoSourceConfig {
            source_id: 1,
            codec: "h264".into(),
            packetization: "h264-annexb-au-v1".into(),
            extradata: Vec::new(),
            width: 16,
            height: 16,
            profile: 0,
            level: 0,
            bitrate: 0,
            color_primaries: 1,
            transfer: 1,
            matrix: 1,
            range: 1,
            sar_num: 1,
            sar_den: 1,
            max_access_unit_bytes: 1024,
            codec_string: None,
            decoder_config: None,
        })
        .unwrap();

        assert_eq!(decoder.packet_time_base().unwrap(), PACKET_TIME_BASE);
    }
}
