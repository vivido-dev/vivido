//! FFmpeg-backed elementary-packet decoder used by Vivid video media channels.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::ptr;
use std::sync::{Arc, Mutex};

use vivid_protocol::media::ParsedVideoPacket;
use vivid_protocol::track::{TrackMode, VideoConfiguration};

use crate::vivid::ffmpeg::{self, AVPacket, AVRational, ParameterValues};
use crate::vivid::scene::RgbaBuffer;

// Bound one decoded RGBA frame to 256 MiB (8192² × 4). This is a local resource ceiling,
// applied to decoder output as well as configuration: codecs may report changed dimensions.
const MAX_DECODED_DIMENSION: u32 = 8192;

const AVMEDIA_TYPE_VIDEO: c_int = 0;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;
const AV_PKT_FLAG_KEY: c_int = 1;
const AVERROR_EOF: c_int = -541_478_725;
const SWS_BILINEAR: c_int = 2;
const PACKET_TIME_BASE: AVRational = AVRational { num: 1, den: 1_000_000 };

#[derive(Debug)]
pub struct DecodedFrame {
    pub pts_us: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: RgbaBuffer,
}

pub struct Decoder {
    /// The verified layout of the FFmpeg this decoder reads structures from.
    abi: &'static ffmpeg::Abi,
    context: *mut c_void,
    packet: *mut c_void,
    frame: *mut c_void,
    scale: *mut c_void,
    scale_format: c_int,
    scale_size: (c_int, c_int),
    rgba_format: c_int,
    sws_colorspace: c_int,
    source_full_range: c_int,
    free_rgba: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Decoder {
    pub fn new(config: &VideoConfiguration, mode: TrackMode) -> io::Result<Self> {
        if !(1..=MAX_DECODED_DIMENSION).contains(&config.coded_width)
            || !(1..=MAX_DECODED_DIMENSION).contains(&config.coded_height)
        {
            return Err(invalid("coded video dimensions exceed decoder limits"));
        }
        // FFmpeg's native `av1` decoder can open when only a hardware path is available, then fail
        // on the first packet on systems without supported AV1 hardware. Require the bounded
        // software implementation for predictable decoding on every supported platform.
        let decoder_name = if config.codec == "av1" { "libdav1d" } else { config.codec.as_str() };
        let codec_name =
            CString::new(decoder_name).map_err(|_| invalid("video codec contains NUL"))?;
        // Verify the linked FFmpeg's structure layout before touching any of it.
        let abi = ffmpeg::abi()?;
        // SAFETY: the CString is NUL-terminated and live through the call; the returned descriptor is library-owned.
        let codec = unsafe { avcodec_find_decoder_by_name(codec_name.as_ptr()) };
        if codec.is_null() {
            return Err(invalid_owned(format!("FFmpeg decoder {decoder_name:?} is unavailable")));
        }
        // SAFETY: codec is a checked live library descriptor; this allocation is owned here and null is handled.
        let mut context = unsafe { avcodec_alloc_context3(codec) };
        if context.is_null() {
            return Err(io::Error::other("FFmpeg could not allocate a decoder context"));
        }

        let mut parameters = match ffmpeg::allocate_parameters() {
            Ok(parameters) => parameters,
            Err(error) => {
                // SAFETY: these are uniquely owned FFmpeg allocations; each matching free function accepts null and clears its pointer.
                unsafe { avcodec_free_context(&mut context) };
                return Err(error);
            },
        };

        let result = (|| {
            let dimensions = (
                c_int::try_from(config.coded_width)
                    .map_err(|_| invalid("video width exceeds FFmpeg limits"))?,
                c_int::try_from(config.coded_height)
                    .map_err(|_| invalid("video height exceeds FFmpeg limits"))?,
            );

            let extradata = if config.extradata.is_empty() {
                None
            } else {
                let length = c_int::try_from(config.extradata.len())
                    .map_err(|_| invalid("codec extradata exceeds i32"))?;
                let allocation = config
                    .extradata
                    .len()
                    .checked_add(AV_INPUT_BUFFER_PADDING_SIZE)
                    .ok_or_else(|| invalid("codec extradata size overflows"))?;
                // SAFETY: the checked size includes FFmpeg padding; allocation failure is handled before writing.
                let extradata = unsafe { av_mallocz(allocation) } as *mut u8;
                if extradata.is_null() {
                    return Err(io::Error::other("FFmpeg could not allocate codec extradata"));
                }
                // SAFETY: the source slice and freshly allocated destination are disjoint and both contain the checked packet/extradata length.
                unsafe {
                    ptr::copy_nonoverlapping(
                        config.extradata.as_ptr(),
                        extradata,
                        config.extradata.len(),
                    );
                }
                Some((extradata, length))
            };
            // SAFETY: parameters is uniquely owned and the verified ABI matches; padded av_malloc extradata transfers to it.
            unsafe {
                abi.set_parameters(
                    parameters,
                    AVMEDIA_TYPE_VIDEO,
                    ffmpeg::codec_id(codec),
                    ParameterValues {
                        extradata,
                        profile: Some(config.profile),
                        level: Some(config.level),
                        dimensions: Some(dimensions),
                        format: None,
                    },
                );
            }

            // SAFETY: context and parameters are live owned allocations; FFmpeg copies parameters without retaining this borrow.
            check_ffmpeg("could not configure decoder", unsafe {
                avcodec_parameters_to_context(context, parameters)
            })?;
            // SAFETY: the decoder context is live, option names are NUL-terminated, and output arguments are initialized writable locals.
            check_ffmpeg("could not set decoder packet time base", unsafe {
                av_opt_set_q(context, c"pkt_timebase".as_ptr(), PACKET_TIME_BASE, 0)
            })?;
            // SAFETY: the decoder context is live, option names are NUL-terminated, and output arguments are initialized writable locals.
            check_ffmpeg("could not configure automatic decoder threading", unsafe {
                av_opt_set_int(context, c"threads".as_ptr(), 0, 0)
            })?;
            if decoder_name == "libdav1d" {
                if mode == TrackMode::Live {
                    // SAFETY: the decoder context is live, option names are NUL-terminated, and output arguments are initialized writable locals.
                    check_ffmpeg("could not bound live AV1 frame delay", unsafe {
                        av_opt_set_int(context, c"max_frame_delay".as_ptr(), 1, 1)
                    })?;
                }
            } else {
                let thread_type = if mode == TrackMode::Live { 2 } else { 2 | 1 };
                // SAFETY: the decoder context is live, option names are NUL-terminated, and output arguments are initialized writable locals.
                check_ffmpeg("could not configure decoder thread type", unsafe {
                    av_opt_set_int(context, c"thread_type".as_ptr(), thread_type, 0)
                })?;
            }
            // SAFETY: context is owned and configured, codec is library-owned, and null requests default options.
            check_ffmpeg("could not open decoder", unsafe {
                avcodec_open2(context, codec, ptr::null_mut())
            })?;
            Ok(())
        })();
        // SAFETY: parameters is uniquely owned here and is no longer used after release.
        unsafe { ffmpeg::free_parameters(&mut parameters) };
        if let Err(error) = result {
            let mut context = context;
            // SAFETY: these are uniquely owned FFmpeg allocations; each matching free function accepts null and clears its pointer.
            unsafe { avcodec_free_context(&mut context) };
            return Err(error);
        }

        // SAFETY: the native allocator returns an owned packet pointer; null is checked before use.
        let packet = unsafe { av_packet_alloc() };
        // SAFETY: the native allocator returns an owned frame pointer; null is checked before use.
        let frame = unsafe { av_frame_alloc() };
        if packet.is_null() || frame.is_null() {
            let mut packet = packet;
            let mut frame = frame;
            let mut context = context;
            // SAFETY: these are uniquely owned FFmpeg allocations; each matching free function accepts null and clears its pointer.
            unsafe {
                av_packet_free(&mut packet);
                av_frame_free(&mut frame);
                avcodec_free_context(&mut context);
            }
            return Err(io::Error::other("FFmpeg could not allocate decode buffers"));
        }
        let rgba_name = c"rgba";
        // SAFETY: rgba_name is a live NUL-terminated CString; the result is a scalar pixel-format identifier.
        let rgba_format = unsafe { av_get_pix_fmt(rgba_name.as_ptr()) };
        if rgba_format < 0 {
            let mut packet = packet;
            let mut frame = frame;
            let mut context = context;
            // SAFETY: these are uniquely owned FFmpeg allocations; each matching free function accepts null and clears its pointer.
            unsafe {
                av_packet_free(&mut packet);
                av_frame_free(&mut frame);
                avcodec_free_context(&mut context);
            }
            return Err(io::Error::other("FFmpeg RGBA pixel format is unavailable"));
        }

        Ok(Self {
            abi,
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
            source_full_range: c_int::from(config.signal_range == 2),
            free_rgba: Arc::new(Mutex::new(Vec::with_capacity(2))),
        })
    }

    /// Decode every access unit needed to preserve reference state, but avoid the expensive
    /// YUV-to-RGBA conversion and allocation for output that is already outside the live latency
    /// window.
    pub fn push_discarding_before(
        &mut self,
        packet: ParsedVideoPacket<'_>,
        discard_before_pts_us: Option<i64>,
    ) -> io::Result<(Vec<DecodedFrame>, u64)> {
        if packet.data.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let size = c_int::try_from(packet.data.len())
            .map_err(|_| invalid("encoded packet exceeds FFmpeg i32 size"))?;
        // SAFETY: self owns the initialized packet; the requested length was checked to fit c_int.
        check_ffmpeg("could not allocate encoded packet", unsafe {
            av_new_packet(self.packet, size)
        })?;
        // SAFETY: the ABI probe verified AVPacket layout; &mut self gives exclusive access to this live packet.
        let av_packet = unsafe { &mut *(self.packet as *mut AVPacket) };
        // SAFETY: the source slice and freshly allocated destination are disjoint and both contain the checked packet/extradata length.
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

        // SAFETY: context and any non-null packet are live; FFmpeg references packet data before the caller unreferences it.
        let send_result = unsafe { avcodec_send_packet(self.context, self.packet) };
        // SAFETY: this initialized packet is uniquely owned; the decoder retains its own references to submitted data.
        unsafe { av_packet_unref(self.packet) };
        check_ffmpeg("decoder rejected encoded packet", send_result)?;
        self.receive_frames(false, discard_before_pts_us)
    }

    pub fn finish(&mut self) -> io::Result<Vec<DecodedFrame>> {
        // SAFETY: context and any non-null packet are live; FFmpeg references packet data before the caller unreferences it.
        let result = unsafe { avcodec_send_packet(self.context, ptr::null()) };
        if result < 0 && result != AVERROR_EOF {
            return Err(ffmpeg_error("could not drain decoder", result));
        }
        self.receive_frames(true, None).map(|(frames, _)| frames)
    }

    fn receive_frames(
        &mut self,
        draining: bool,
        discard_before_pts_us: Option<i64>,
    ) -> io::Result<(Vec<DecodedFrame>, u64)> {
        let mut output = Vec::new();
        let mut discarded = 0_u64;
        loop {
            // SAFETY: context and frame are live uniquely owned allocations; a successful result initializes the frame.
            let result = unsafe { avcodec_receive_frame(self.context, self.frame) };
            if result == -libc::EAGAIN || result == AVERROR_EOF {
                break;
            }
            check_ffmpeg("could not receive decoded frame", result)?;
            // SAFETY: self owns a successfully decoded live AVFrame and the verified ABI selects its field offsets.
            let pts_us = unsafe { self.abi.frame_pts(self.frame) };
            if decoded_frame_is_late(pts_us, discard_before_pts_us) {
                discarded = discarded.saturating_add(1);
            } else {
                output.push(self.convert_frame()?);
            }
            // SAFETY: self owns this initialized frame and no borrowed pixel/sample data is used after unref.
            unsafe { av_frame_unref(self.frame) };
        }
        if draining && output.is_empty() {
            log::debug!("Vivid decoder drained without an additional frame");
        }
        Ok((output, discarded))
    }

    fn convert_frame(&mut self) -> io::Result<DecodedFrame> {
        // SAFETY: self owns a successfully decoded live AVFrame and the verified ABI selects its field offsets.
        let pts_us = unsafe { self.abi.frame_pts(self.frame) };
        // SAFETY: self owns a successfully decoded live AVFrame and the verified ABI selects its field offsets.
        let frame = unsafe { self.abi.frame(self.frame) };
        let (width, height) = match (u32::try_from(frame.width), u32::try_from(frame.height)) {
            (Ok(width @ 1..=MAX_DECODED_DIMENSION), Ok(height @ 1..=MAX_DECODED_DIMENSION)) => {
                (width, height)
            },
            _ => return Err(invalid("decoder produced invalid frame dimensions")),
        };
        if self.scale.is_null()
            || self.scale_format != frame.format
            || self.scale_size != (frame.width, frame.height)
        {
            if !self.scale.is_null() {
                // SAFETY: scale is this decoder's uniquely owned converter and is not used after it is freed.
                unsafe { sws_freeContext(self.scale) };
            }
            // SAFETY: dimensions and pixel formats were validated; optional filter and parameter pointers are null.
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
            // SAFETY: the function returns a library-owned coefficient table used only while the linked library is live.
            let source_coefficients = unsafe { sws_getCoefficients(self.sws_colorspace) };
            // SAFETY: the function returns a library-owned coefficient table used only while the linked library is live.
            let destination_coefficients = unsafe { sws_getCoefficients(1) };
            if source_coefficients.is_null()
                || destination_coefficients.is_null()
                // SAFETY: scale is live and both non-null coefficient tables are library-owned arrays of the required length.
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

        let length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid("decoded frame allocation overflows"))?;
        let mut rgba = self
            .free_rgba
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .filter(|buffer| buffer.len() == length)
            .unwrap_or_else(|| vec![0_u8; length]);
        let mut destination = [ptr::null_mut(); 4];
        destination[0] = rgba.as_mut_ptr();
        let destination_lines = [frame.width * 4, 0, 0, 0];
        // SAFETY: the decoded frame owns its input planes; destination storage was checked for width × height × four bytes and remains live.
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
        Ok(DecodedFrame {
            pts_us,
            width,
            height,
            rgba: RgbaBuffer::pooled(rgba, Arc::downgrade(&self.free_rgba)),
        })
    }

    #[cfg(test)]
    fn packet_time_base(&self) -> io::Result<AVRational> {
        let mut time_base = AVRational { num: 0, den: 0 };
        // SAFETY: the live decoder writes a rational into this initialized local through a valid pointer.
        check_ffmpeg("could not read decoder packet time base", unsafe {
            av_opt_get_q(self.context, c"pkt_timebase".as_ptr(), 0, &mut time_base)
        })?;
        Ok(time_base)
    }

    #[cfg(test)]
    fn integer_option(&self, name: &CStr, flags: c_int) -> io::Result<i64> {
        let mut value = 0;
        // SAFETY: name is NUL-terminated and the live decoder writes to this initialized integer local.
        check_ffmpeg("could not read decoder option", unsafe {
            av_opt_get_int(self.context, name.as_ptr(), flags, &mut value)
        })?;
        Ok(value)
    }
}

fn decoded_frame_is_late(pts_us: i64, discard_before_pts_us: Option<i64>) -> bool {
    discard_before_pts_us.is_some_and(|deadline| pts_us < deadline)
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: these are uniquely owned FFmpeg allocations; each matching free function accepts null and clears its pointer.
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
    let mut buffer: [c_char; 256] = [0; 256];
    // SAFETY: buffer is writable for its declared length; successful av_strerror writes a NUL-terminated diagnostic.
    let description = if unsafe { av_strerror(code, buffer.as_mut_ptr(), buffer.len()) } == 0 {
        // SAFETY: successful av_strerror wrote a terminated string into the still-live buffer.
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
    fn av_opt_set_int(object: *mut c_void, name: *const c_char, value: i64, flags: c_int) -> c_int;
    #[cfg(test)]
    fn av_opt_get_q(
        object: *mut c_void,
        name: *const c_char,
        flags: c_int,
        output: *mut AVRational,
    ) -> c_int;
    #[cfg(test)]
    fn av_opt_get_int(
        object: *mut c_void,
        name: *const c_char,
        flags: c_int,
        output: *mut i64,
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
    fn oversized_coded_dimensions_are_rejected_before_native_allocation() {
        for dimensions in [(0, 1), (1, 0), (MAX_DECODED_DIMENSION + 1, 1), (1, u32::MAX)] {
            let mut config = video_config("h264", Vec::new());
            config.coded_width = dimensions.0;
            config.coded_height = dimensions.1;
            assert!(Decoder::new(&config, TrackMode::Live).is_err());
        }
    }

    fn video_config(codec: &str, extradata: Vec<u8>) -> VideoConfiguration {
        VideoConfiguration {
            codec: codec.into(),
            packetization: format!("{codec}-test"),
            extradata,
            coded_width: 640,
            coded_height: 480,
            profile: 0,
            level: 4,
            maximum_reorder_depth: 0,
            color_primaries: 1,
            transfer: 1,
            matrix: 1,
            signal_range: 1,
            aspect_numerator: 1,
            aspect_denominator: 1,
            maximum_access_unit_bytes: 1024 * 1024,
            codec_string: None,
            decoder_configuration: None,
        }
    }

    #[test]
    fn decoder_context_uses_protocol_packet_time_base() {
        let decoder = Decoder::new(&video_config("h264", Vec::new()), TrackMode::Live).unwrap();

        assert_eq!(decoder.packet_time_base().unwrap(), PACKET_TIME_BASE);
    }

    #[test]
    fn decoder_threading_is_mode_aware() {
        let config = video_config("h264", Vec::new());
        let live = Decoder::new(&config, TrackMode::Live).unwrap();
        let timed = Decoder::new(&config, TrackMode::Timed).unwrap();

        assert!(live.integer_option(c"threads", 0).unwrap() > 1);
        assert!(timed.integer_option(c"threads", 0).unwrap() > 1);
        assert_eq!(live.integer_option(c"thread_type", 0).unwrap(), 2);
        assert_eq!(timed.integer_option(c"thread_type", 0).unwrap(), 3);
    }

    #[test]
    fn live_discard_deadline_keeps_the_boundary_and_drops_only_older_frames() {
        assert!(decoded_frame_is_late(99_999, Some(100_000)));
        assert!(!decoded_frame_is_late(100_000, Some(100_000)));
        assert!(!decoded_frame_is_late(0, None));
    }

    #[test]
    #[cfg(windows)]
    fn windows_decodes_av1_with_software_decoder() {
        // Canonical sequence-header OBU and first temporal unit from medias/under_attack.webm.
        let extradata = [
            0x0a, 0x0e, 0x00, 0x00, 0x00, 0x24, 0xc4, 0xff, 0xdf, 0x30, 0xbf, 0x44, 0x04, 0x04,
            0x04, 0x10,
        ];
        let access_unit = [
            0x0a, 0x0e, 0x00, 0x00, 0x00, 0x24, 0xc4, 0xff, 0xdf, 0x30, 0xbf, 0x44, 0x04, 0x04,
            0x04, 0x10, 0x32, 0x2e, 0x10, 0x00, 0x46, 0x71, 0x8a, 0x60, 0xc3, 0x0c, 0x30, 0x90,
            0x41, 0x20, 0x07, 0xee, 0x43, 0xbd, 0x31, 0x18, 0x9b, 0xb8, 0x10, 0xf4, 0x72, 0x82,
            0xe5, 0x32, 0xe7, 0xc7, 0x11, 0xa7, 0x0f, 0x82, 0x7f, 0x25, 0x17, 0x49, 0x75, 0x9e,
            0x87, 0x13, 0x6f, 0xca, 0xa7, 0x25, 0x3f, 0x80,
        ];
        let mut decoder =
            Decoder::new(&video_config("av1", extradata.to_vec()), TrackMode::Live).unwrap();
        let (frames, discarded) = decoder
            .push_discarding_before(
                ParsedVideoPacket {
                    epoch: 1,
                    flags: vivid_protocol::media::VIDEO_PACKET_KEY,
                    packet_id: 1,
                    pts_us: 0,
                    dts_us: 0,
                    duration_us: 40_000,
                    side_data: &[],
                    data: &access_unit,
                },
                None,
            )
            .unwrap();
        assert_eq!(discarded, 0);
        assert_eq!(frames.len(), 1);
        assert_eq!((frames[0].width, frames[0].height), (640, 480));
    }
}
