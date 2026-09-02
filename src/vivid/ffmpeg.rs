//! Runtime-verified FFmpeg ABI layouts.
//!
//! Vivido reads FFmpeg's public structures directly rather than through a binding crate, so the
//! field offsets it uses have to match the library it was actually linked against. FFmpeg freezes
//! those offsets within a major version but not across them, and the three supported platforms do
//! not agree on which major they ship: Homebrew, a distribution package, and vcpkg routinely
//! differ. Two of the structures Vivido depends on changed shape in that window:
//!
//! * `AVCodecParameters` gained `coded_side_data`/`nb_coded_side_data` after `extradata_size` in
//!   FFmpeg 7, moving everything past it by sixteen bytes. Guessing wrong writes `width`,
//!   `height`, `profile`, and `level` into the wrong fields — the divergence that actually bites.
//! * `AVFrame` carried `int key_frame` between `format` and `pict_type` through FFmpeg 7 and
//!   dropped it in 8. On a 64-bit target the padding before an eight-byte-aligned `pts` absorbs
//!   that `int`, so both shapes put every field Vivido reads in the same place. That is a property
//!   of the target's alignment rules rather than of the declaration, so it is probed rather than
//!   assumed.
//!
//! Rather than trust a version table, this module *probes*. Every structure FFmpeg allocates is
//! reset to documented defaults — `AV_NOPTS_VALUE` for timestamps, `-1` for `pos` and `format`,
//! `AV_PROFILE_UNKNOWN` for `profile` and `level` — and those sentinels sit exactly at the offsets
//! that move. Reading them back through each candidate layout identifies the real one, and an
//! FFmpeg whose layout matches none of them is refused with a typed error instead of being
//! misread. Fields ahead of every divergence (`data`, `linesize`, `extended_data`, `width`,
//! `height`, `nb_samples`, `format`) share one head struct, because they have never moved.

use std::ffi::{c_char, c_int, c_void};
use std::io;
use std::ptr;
use std::sync::OnceLock;

/// `AV_NOPTS_VALUE`.
const NO_PTS: i64 = i64::MIN;
/// `AV_PROFILE_UNKNOWN` / `AV_LEVEL_UNKNOWN`.
const UNKNOWN_PROFILE: c_int = -99;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

/// The part of `AVFrame` that has never moved.
#[repr(C)]
pub struct AVFrameHead {
    pub data: [*mut u8; 8],
    pub linesize: [c_int; 8],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
}

/// `AVFrame` through `pts`, for a library that still declares `key_frame` (FFmpeg 7 and earlier).
///
/// On every 64-bit target this lands `pts` at the same offset as [`AVFrameCurrent`], because the
/// padding before an eight-byte-aligned `pts` absorbs the extra `int`. It is kept as a distinct
/// candidate anyway: the coincidence is a property of the target's alignment rules, not of the
/// declaration, and the probe below is what establishes which one this build actually needs.
#[repr(C)]
struct AVFrameLegacy {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    key_frame: c_int,
    pict_type: c_int,
    sample_aspect_ratio: AVRational,
    pts: i64,
}

/// `AVFrame` through `pts`, for a library that has dropped `key_frame` (FFmpeg 8 and later).
#[repr(C)]
struct AVFrameCurrent {
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

/// The part of `AVCodecParameters` that has never moved.
///
/// Every variant below repeats these fields rather than nesting this struct: a nested `#[repr(C)]`
/// struct carries its own alignment, which would place `bit_rate` at the wrong offset. The
/// layouts have to be flat to match C's.
#[repr(C)]
struct AVCodecParametersHead {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
}

/// `AVCodecParameters` before `coded_side_data` was introduced (FFmpeg 6 and earlier).
#[repr(C)]
struct AVCodecParametersLegacy {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
    format: c_int,
    bit_rate: i64,
    bits_per_coded_sample: c_int,
    bits_per_raw_sample: c_int,
    profile: c_int,
    level: c_int,
    width: c_int,
    height: c_int,
}

/// `AVCodecParameters` with `coded_side_data` (FFmpeg 7 and later), which shifts everything after
/// `extradata_size` by sixteen bytes.
#[repr(C)]
struct AVCodecParametersCurrent {
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

/// The fields of `AVCodecParameters` Vivido writes, gathered behind the layout that owns them.
struct Tail<'a> {
    format: &'a mut c_int,
    bit_rate: &'a mut i64,
    profile: &'a mut c_int,
    level: &'a mut c_int,
    width: &'a mut c_int,
    height: &'a mut c_int,
}

/// `AVPacket`, which has kept one shape across every supported major. Probed all the same.
#[repr(C)]
pub struct AVPacket {
    buf: *mut c_void,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
    stream_index: c_int,
    pub flags: c_int,
    side_data: *mut c_void,
    side_data_elems: c_int,
    pub duration: i64,
    pos: i64,
    opaque: *mut c_void,
    opaque_ref: *mut c_void,
    pub time_base: AVRational,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameLayout {
    /// FFmpeg 7 and earlier: `key_frame` is still declared.
    Legacy,
    /// FFmpeg 8 and later.
    Current,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParametersLayout {
    /// FFmpeg 6 and earlier: no `coded_side_data`.
    Legacy,
    /// FFmpeg 7 and later.
    Current,
}

/// The verified layout of the FFmpeg this process is linked against.
pub struct Abi {
    frame: FrameLayout,
    parameters: ParametersLayout,
}

static ABI: OnceLock<Result<Abi, String>> = OnceLock::new();

/// The verified FFmpeg ABI, probing once per process.
///
/// Every FFmpeg-backed entry point calls this before allocating a decoder or an audio device, so
/// an unrecognized library becomes one typed error on the affected track rather than a misread
/// structure somewhere later.
pub fn abi() -> io::Result<&'static Abi> {
    match ABI.get_or_init(probe) {
        Ok(abi) => Ok(abi),
        Err(message) => Err(io::Error::new(io::ErrorKind::Unsupported, message.clone())),
    }
}

fn probe() -> Result<Abi, String> {
    let frame = probe_frame().ok_or_else(|| unsupported("AVFrame"))?;
    let parameters = probe_parameters().ok_or_else(|| unsupported("AVCodecParameters"))?;
    probe_packet().ok_or_else(|| unsupported("AVPacket"))?;
    log::debug!(
        "FFmpeg ABI verified: libavcodec {}, libavutil {}, {frame:?} frame, {parameters:?} \
         codec parameters",
        version_text(unsafe { avcodec_version() }),
        version_text(unsafe { avutil_version() }),
    );
    Ok(Abi { frame, parameters })
}

fn unsupported(structure: &str) -> String {
    format!(
        "this FFmpeg does not match any {structure} layout Vivido knows (libavcodec {}, libavutil \
         {}, libswscale {}, libswresample {}); Vivid media is unavailable",
        version_text(unsafe { avcodec_version() }),
        version_text(unsafe { avutil_version() }),
        version_text(unsafe { swscale_version() }),
        version_text(unsafe { swresample_version() }),
    )
}

fn version_text(version: u32) -> String {
    format!("{}.{}.{}", version >> 16, (version >> 8) & 0xff, version & 0xff)
}

/// `av_frame_unref` resets a frame to its documented defaults, which put `AV_NOPTS_VALUE` exactly
/// at the offset the two candidate layouts disagree about.
fn probe_frame() -> Option<FrameLayout> {
    let mut frame = unsafe { av_frame_alloc() };
    if frame.is_null() {
        return None;
    }
    unsafe { av_frame_unref(frame) };
    let legacy = unsafe { (*(frame as *const AVFrameLegacy)).pts };
    let current = unsafe { (*(frame as *const AVFrameCurrent)).pts };
    let layout = match (legacy == NO_PTS, current == NO_PTS) {
        // Both agreeing means the target's padding puts `pts` in the same place either way, so
        // either candidate reads the right field. Neither agreeing means this is a layout Vivido
        // has never seen, and reading it would be a guess.
        (_, true) => Some(FrameLayout::Current),
        (true, false) => Some(FrameLayout::Legacy),
        (false, false) => None,
    };
    unsafe { av_frame_free(&mut frame) };
    layout
}

/// `avcodec_parameters_alloc` resets `format` to -1 and both `profile` and `level` to
/// `AV_PROFILE_UNKNOWN`, which straddle the point `coded_side_data` was inserted at.
fn probe_parameters() -> Option<ParametersLayout> {
    let mut parameters = unsafe { avcodec_parameters_alloc() };
    if parameters.is_null() {
        return None;
    }
    let legacy = unsafe { &*(parameters as *const AVCodecParametersLegacy) };
    let current = unsafe { &*(parameters as *const AVCodecParametersCurrent) };
    let legacy_matches =
        legacy.format == -1 && legacy.profile == UNKNOWN_PROFILE && legacy.level == UNKNOWN_PROFILE;
    let current_matches = current.format == -1
        && current.profile == UNKNOWN_PROFILE
        && current.level == UNKNOWN_PROFILE;
    // These two genuinely differ — `coded_side_data` moves everything after `extradata_size` by
    // sixteen bytes — so exactly one may claim the sentinels.
    let layout = match (legacy_matches, current_matches) {
        (true, false) => Some(ParametersLayout::Legacy),
        (false, true) => Some(ParametersLayout::Current),
        _ => None,
    };
    unsafe { avcodec_parameters_free(&mut parameters) };
    layout
}

/// `av_packet_alloc` resets timestamps to `AV_NOPTS_VALUE`, `pos` to -1, and `time_base` to 0/1.
/// `time_base` is the last field, so agreeing on it confirms the whole structure.
fn probe_packet() -> Option<()> {
    let mut packet = unsafe { av_packet_alloc() };
    if packet.is_null() {
        return None;
    }
    let fields = unsafe { &*(packet as *const AVPacket) };
    let valid = fields.pts == NO_PTS
        && fields.dts == NO_PTS
        && fields.pos == -1
        && fields.time_base == AVRational { num: 0, den: 1 };
    unsafe { av_packet_free(&mut packet) };
    valid.then_some(())
}

impl Abi {
    /// The always-stable head of an `AVFrame`.
    ///
    /// Returned by value. A `&AVFrameHead` would take its lifetime from `&self`, and [`abi`] hands
    /// out `&'static Abi`, so the reference would outlive the frame as far as the borrow checker
    /// could tell. Every field here is `Copy`, and callers only read, so returning the head requires
    /// no allocation or ownership transfer. The plane pointers inside it stay valid exactly as long
    /// as the frame does — copying the head does not copy the pixel data.
    ///
    /// # Safety
    /// `frame` must be a live `AVFrame` allocated by FFmpeg.
    pub unsafe fn frame(&self, frame: *const c_void) -> AVFrameHead {
        // SAFETY: the caller guarantees a live `AVFrame`, and this head covers only fields that
        // have never moved, so it needs no layout selection.
        unsafe { (frame as *const AVFrameHead).read() }
    }

    /// An `AVFrame`'s presentation timestamp, read at the offset this library actually uses.
    ///
    /// # Safety
    /// `frame` must be a live `AVFrame` allocated by FFmpeg.
    pub unsafe fn frame_pts(&self, frame: *const c_void) -> i64 {
        match self.frame {
            FrameLayout::Legacy => unsafe { (*(frame as *const AVFrameLegacy)).pts },
            FrameLayout::Current => unsafe { (*(frame as *const AVFrameCurrent)).pts },
        }
    }

    /// Fill `parameters` for a decoder Vivido is about to open.
    ///
    /// `codec_type` and `codec_id` are left as `avcodec_parameters_from_context` set them from the
    /// context `avcodec_alloc_context3` already tagged with the chosen codec.
    ///
    /// # Safety
    /// `parameters` must be a live `AVCodecParameters` allocated by FFmpeg.
    pub unsafe fn set_parameters(
        &self,
        parameters: *mut c_void,
        media_type: c_int,
        codec_id: c_int,
        description: ParameterValues,
    ) {
        let head = unsafe { &mut *(parameters as *mut AVCodecParametersHead) };
        head.codec_type = media_type;
        head.codec_id = codec_id;
        if let Some((extradata, extradata_size)) = description.extradata {
            head.extradata = extradata;
            head.extradata_size = extradata_size;
        }
        // SAFETY: same live allocation, still borrowed only for the duration of this call.
        unsafe {
            self.with_parameters_tail(parameters, |tail| {
                *tail.bit_rate = 0;
                if let Some(profile) = description.profile {
                    *tail.profile = profile;
                }
                if let Some(level) = description.level {
                    *tail.level = level;
                }
                if let Some((width, height)) = description.dimensions {
                    *tail.width = width;
                    *tail.height = height;
                }
                if let Some(format) = description.format {
                    *tail.format = format;
                }
            });
        }
    }

    /// Run `write` against the layout-selected tail of `parameters`.
    ///
    /// The tail is handed to a closure rather than returned. A returned `Tail` would take its
    /// lifetime from `&self`, and [`abi`] hands out `&'static Abi`, so the borrow checker would
    /// happily let one outlive the allocation it points into — the reference would say nothing
    /// about how long FFmpeg keeps that memory. Passing it to a closure keeps the borrow inside a
    /// scope the caller cannot escape, which is the only span the pointer is known to be live for.
    ///
    /// # Safety
    /// `parameters` must be a live `AVCodecParameters` allocated by FFmpeg, and must stay live for
    /// the duration of the call.
    unsafe fn with_parameters_tail<R>(
        &self,
        parameters: *mut c_void,
        write: impl FnOnce(Tail<'_>) -> R,
    ) -> R {
        match self.parameters {
            ParametersLayout::Legacy => {
                // SAFETY: the caller guarantees a live allocation, and the probe established that
                // this library uses the legacy layout.
                let fields = unsafe { &mut *(parameters as *mut AVCodecParametersLegacy) };
                write(Tail {
                    format: &mut fields.format,
                    bit_rate: &mut fields.bit_rate,
                    profile: &mut fields.profile,
                    level: &mut fields.level,
                    width: &mut fields.width,
                    height: &mut fields.height,
                })
            },
            ParametersLayout::Current => {
                // SAFETY: as above, for the layout carrying `coded_side_data`.
                let fields = unsafe { &mut *(parameters as *mut AVCodecParametersCurrent) };
                write(Tail {
                    format: &mut fields.format,
                    bit_rate: &mut fields.bit_rate,
                    profile: &mut fields.profile,
                    level: &mut fields.level,
                    width: &mut fields.width,
                    height: &mut fields.height,
                })
            },
        }
    }
}

/// What a caller wants written into an `AVCodecParameters`. Absent fields keep FFmpeg's default.
#[derive(Default)]
pub struct ParameterValues {
    pub extradata: Option<(*mut u8, c_int)>,
    pub profile: Option<c_int>,
    pub level: Option<c_int>,
    pub dimensions: Option<(c_int, c_int)>,
    pub format: Option<c_int>,
}

/// The `AVCodec` fields Vivido reads: only its ID, and only to tag the parameters it hands back.
///
/// # Safety
/// `codec` must be a live `AVCodec` returned by FFmpeg.
pub unsafe fn codec_id(codec: *const c_void) -> c_int {
    #[repr(C)]
    struct AVCodecHead {
        name: *const c_char,
        long_name: *const c_char,
        media_type: c_int,
        id: c_int,
    }
    unsafe { (*(codec as *const AVCodecHead)).id }
}

/// Allocate a zeroed `AVCodecParameters`, or fail.
pub fn allocate_parameters() -> io::Result<*mut c_void> {
    let parameters = unsafe { avcodec_parameters_alloc() };
    if parameters.is_null() {
        return Err(io::Error::other("FFmpeg could not allocate codec parameters"));
    }
    Ok(parameters)
}

/// Release an `AVCodecParameters` allocated by [`allocate_parameters`].
pub fn free_parameters(parameters: &mut *mut c_void) {
    if !parameters.is_null() {
        unsafe { avcodec_parameters_free(parameters) };
        *parameters = ptr::null_mut();
    }
}

#[link(name = "avcodec")]
unsafe extern "C" {}

#[link(name = "avutil")]
unsafe extern "C" {}

#[link(name = "swscale")]
unsafe extern "C" {}

#[link(name = "swresample")]
unsafe extern "C" {}

unsafe extern "C" {
    fn avcodec_version() -> u32;
    fn avutil_version() -> u32;
    fn swscale_version() -> u32;
    fn swresample_version() -> u32;
    fn avcodec_parameters_alloc() -> *mut c_void;
    fn avcodec_parameters_free(parameters: *mut *mut c_void);
    fn av_frame_alloc() -> *mut c_void;
    fn av_frame_free(frame: *mut *mut c_void);
    fn av_frame_unref(frame: *mut c_void);
    fn av_packet_alloc() -> *mut c_void;
    fn av_packet_free(packet: *mut *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the module: whatever FFmpeg this build linked against, its layout is
    /// identified rather than assumed. A failure here means the probe needs a new candidate
    /// layout, not that the test is wrong.
    #[test]
    fn the_linked_ffmpeg_abi_is_identified() {
        let abi = abi().expect("the linked FFmpeg matches a known layout");
        // Both probes must agree with the library's own reported majors, which is an independent
        // check on the sentinel probe rather than a restatement of it.
        let avcodec_major = unsafe { avcodec_version() } >> 16;
        // `coded_side_data` arrived in FFmpeg 7, which is libavcodec 61. This is an independent
        // check on the sentinel probe rather than a restatement of it: the probe reads memory, the
        // major comes from the library's own version call.
        let expected_parameters =
            if avcodec_major >= 61 { ParametersLayout::Current } else { ParametersLayout::Legacy };
        assert_eq!(abi.parameters, expected_parameters, "libavcodec {avcodec_major}");
    }

    #[test]
    fn a_frame_reports_the_unset_timestamp_sentinel_through_the_chosen_layout() {
        let abi = abi().expect("known FFmpeg layout");
        let mut frame = unsafe { av_frame_alloc() };
        assert!(!frame.is_null());
        unsafe { av_frame_unref(frame) };
        assert_eq!(unsafe { abi.frame_pts(frame) }, NO_PTS);
        // The stable head must read back the defaults too, which proves the head and the
        // layout-selected tail are consistent with one another.
        let head = unsafe { abi.frame(frame) };
        assert_eq!((head.width, head.height, head.nb_samples), (0, 0, 0));
        assert_eq!(head.format, -1);
        unsafe { av_frame_free(&mut frame) };
    }

    #[test]
    fn codec_parameters_round_trip_through_the_chosen_layout() {
        let abi = abi().expect("known FFmpeg layout");
        let mut parameters = allocate_parameters().expect("parameters");
        unsafe {
            abi.set_parameters(
                parameters,
                0,
                27,
                ParameterValues {
                    profile: Some(77),
                    level: Some(31),
                    dimensions: Some((1920, 1080)),
                    ..Default::default()
                },
            );
        }
        unsafe {
            abi.with_parameters_tail(parameters, |tail| {
                assert_eq!((*tail.width, *tail.height), (1920, 1080));
                assert_eq!((*tail.profile, *tail.level), (77, 31));
                assert_eq!(*tail.bit_rate, 0);
            });
        }
        let head = unsafe { &*(parameters as *const AVCodecParametersHead) };
        assert_eq!((head.codec_type, head.codec_id), (0, 27));
        free_parameters(&mut parameters);
    }
}
