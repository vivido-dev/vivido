//! Receiving the pixels of a transmitted image.
//!
//! A transmission arrives as one or more commands, each carrying a base64 chunk that may itself be
//! split across PTY reads at any byte. [`Load`] accepts those fragments as they come and never
//! holds more than the image's own decoded size.
//!
//! Uncompressed pixels are decoded straight into the buffer that becomes the image, a few
//! kilobytes at a time, so receiving them costs the terminal lock nothing it would notice.
//! Compressed pixels and PNG files are different: turning them into pixels is one indivisible
//! piece of work that can take tens of milliseconds. Those are handed back as a [`DecodeJob`] for
//! the caller to run wherever it can afford to.

// Rust guideline compliant 2026-10-07

use std::io::Cursor;

use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use flate2::{Decompress, FlushDecompress, Status};

use super::command::Command;

/// Largest width or height of one image, in pixels.
///
/// This is wgpu's default `max_texture_dimension_2d`, and also what the media compositor clamps
/// its own targets to. An image that could not become a texture is refused when it is
/// transmitted, where the application can still be told.
pub(crate) const MAX_DIMENSION: u32 = 8192;

/// Largest compressed payload (zlib stream or PNG file) one transmission may carry.
///
/// Kitty accepts 400 MB. Sixty-four is several times a PNG of [`MAX_DIMENSION`] squared
/// photographic pixels, and it is also the most a decode job is ever asked to chew through.
pub(crate) const MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;

/// Bytes a transmission may run past its declared size before it is refused.
///
/// Kitty tolerates ten trailing bytes. Twelve is that rounded up to whole base64 quads, and it
/// also absorbs the at most two bytes by which a padded final quad is over-estimated.
const SLACK: usize = 12;

/// Extra bytes a zlib stream may occupy beyond the pixels it expands to, as Kitty allows.
const ZLIB_OVERHEAD: usize = 1024;

/// Bytes of a PNG file needed to read its dimensions: signature, chunk header, width, height.
const PNG_HEADER_BYTES: usize = 24;

/// Base64 as the protocol uses it: standard alphabet, padding optional on every chunk.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Why a transmission was refused, in the protocol's `CODE:message` reply form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Failure {
    /// Errno-style code the protocol puts before the colon.
    pub code: &'static str,
    /// Human-readable detail. Never contains anything the application did not already know.
    pub message: &'static str,
}

impl Failure {
    pub(crate) const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}

const TOO_MUCH_DATA: Failure = Failure::new("EFBIG", "Too much data");
const NOT_ENOUGH_DATA: Failure = Failure::new("ENODATA", "Insufficient image data");
const BAD_BASE64: Failure = Failure::new("EINVAL", "Invalid base64 data");
const BAD_ZLIB: Failure = Failure::new("EINVAL", "Failed to inflate image data");
const BAD_PNG: Failure = Failure::new("EBADPNG", "Failed to decode PNG data");
const OUT_OF_MEMORY: Failure = Failure::new("ENOMEM", "Out of memory");

/// Decoded 32-bit straight-alpha pixels of one image.
#[derive(PartialEq, Eq)]
pub(crate) struct Pixels {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, rows top to bottom.
    pub rgba: Vec<u8>,
    /// Whether every pixel has full alpha.
    pub opaque: bool,
}

// The `Debug` impls in this module leave image data out: it is megabytes of what the application
// drew, which a debug dump has no use for and no business holding.
impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pixels")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("opaque", &self.opaque)
            .finish_non_exhaustive()
    }
}

/// What a completed transmission produced.
#[derive(Debug)]
pub(crate) enum Received {
    /// Pixels that needed no further work.
    Pixels(Pixels),
    /// Compressed data that still has to be decoded.
    Job(DecodeJob),
}

/// Incremental base64 decoder for input split at arbitrary bytes.
#[derive(Debug, Default)]
struct Base64Stream {
    /// Characters of a quad whose remainder has not arrived yet.
    pending: [u8; 4],
    pending_len: usize,
}

impl Base64Stream {
    /// Upper bound on the bytes [`Self::push`] would append for `input`.
    fn bound(&self, input: usize) -> usize {
        (self.pending_len + input) / 4 * 3
    }

    /// Decode every whole quad of `input` onto the end of `out`, keeping the remainder.
    fn push(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<(), Failure> {
        if self.pending_len > 0 {
            let take = (4 - self.pending_len).min(input.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&input[..take]);
            self.pending_len += take;
            input = &input[take..];
            if self.pending_len < 4 {
                return Ok(());
            }
            self.pending_len = 0;
            BASE64.decode_vec(self.pending, out).map_err(|_| BAD_BASE64)?;
        }
        let whole = input.len() & !3;
        BASE64.decode_vec(&input[..whole], out).map_err(|_| BAD_BASE64)?;
        let rest = &input[whole..];
        self.pending[..rest.len()].copy_from_slice(rest);
        self.pending_len = rest.len();
        Ok(())
    }

    /// Decode the unpadded tail left by the last [`Self::push`] of a chunk.
    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), Failure> {
        let len = std::mem::take(&mut self.pending_len);
        BASE64.decode_vec(&self.pending[..len], out).map_err(|_| BAD_BASE64)
    }
}

/// How the bytes of a transmission become pixels.
#[derive(Debug)]
enum Target {
    /// 32-bit pixels, decoded in place.
    Rgba,
    /// 24-bit pixels, widened to 32 bits as they arrive.
    Rgb,
    /// A zlib stream or PNG file, kept as received until the transmission ends.
    Encoded(Encoding),
    /// A refused transmission; its remaining chunks are swallowed.
    Rejected(Failure),
}

/// Compressed forms a transmission can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// zlib-compressed 24-bit pixels.
    ZlibRgb,
    /// zlib-compressed 32-bit pixels.
    ZlibRgba,
    /// A PNG file.
    Png,
    /// A zlib-compressed PNG file.
    ZlibPng,
}

/// A transmission in progress.
pub(crate) struct Load {
    /// Control data of the transmission's first command, which governs all of it.
    pub start: Command,
    target: Target,
    /// Pixels for the uncompressed targets, raw received bytes for the encoded ones.
    buf: Vec<u8>,
    /// Bytes `buf` holds when the transmission is complete, for the uncompressed targets.
    expected: usize,
    /// Most bytes `buf` may ever hold.
    limit: usize,
    base64: Base64Stream,
    /// Staging for 24-bit pixels between base64 decoding and widening. Reused across chunks.
    rgb: Vec<u8>,
    /// Bytes of `buf` whose alpha has been inspected; always a multiple of four.
    scanned: usize,
    opaque: bool,
}

impl std::fmt::Debug for Load {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Load")
            .field("start", &self.start)
            .field("target", &self.target)
            .field("received", &self.buf.len())
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

impl Load {
    /// Start a transmission described by the control data of its first command.
    ///
    /// A transmission that cannot be accepted still returns a `Load`, one that discards whatever
    /// follows and reports the reason from [`Self::finish`]. The remaining chunks of a refused
    /// transmission have to be consumed somewhere, and treating them as new commands would
    /// produce one bogus reply per chunk.
    pub(crate) fn begin(start: Command) -> Self {
        let mut load = Self {
            start,
            target: Target::Rgba,
            buf: Vec::new(),
            expected: 0,
            limit: 0,
            base64: Base64Stream::default(),
            rgb: Vec::new(),
            scanned: 0,
            opaque: true,
        };
        match load.plan() {
            Ok(target) => load.target = target,
            Err(failure) => load.target = Target::Rejected(failure),
        }
        load
    }

    /// Work out the target and reserve its buffer.
    fn plan(&mut self) -> Result<Target, Failure> {
        let start = &self.start;
        if !matches!(start.medium, 0 | b'd') {
            return Err(Failure::new("EINVAL", "Unsupported transmission medium"));
        }
        let compressed = start.compression == b'z';
        let bytes_per_pixel = match start.format {
            0 | 32 => 4,
            24 => 3,
            100 => {
                self.limit = MAX_ENCODED_BYTES;
                return Ok(Target::Encoded(if compressed {
                    Encoding::ZlibPng
                } else {
                    Encoding::Png
                }));
            },
            _ => return Err(Failure::new("EINVAL", "Unknown image format")),
        };
        if start.data_width > MAX_DIMENSION || start.data_height > MAX_DIMENSION {
            return Err(Failure::new("EINVAL", "Image too large"));
        }
        // Both factors are at most `MAX_DIMENSION`, so neither product can overflow `usize`.
        let pixels = start.data_width as usize * start.data_height as usize;
        if pixels == 0 {
            return Err(Failure::new("EINVAL", "Zero width/height not allowed"));
        }
        self.expected = pixels * 4;
        if compressed {
            self.limit = (pixels * bytes_per_pixel + ZLIB_OVERHEAD).min(MAX_ENCODED_BYTES);
            return Ok(Target::Encoded(if bytes_per_pixel == 3 {
                Encoding::ZlibRgb
            } else {
                Encoding::ZlibRgba
            }));
        }
        self.limit = self.expected + SLACK;
        // Reserving does not touch the pages, so a transmission that declares a large image and
        // then sends nothing costs address space only.
        self.buf.try_reserve_exact(self.limit).map_err(|_| OUT_OF_MEMORY)?;
        Ok(if bytes_per_pixel == 3 { Target::Rgb } else { Target::Rgba })
    }

    /// Whether the transmission was refused and is only draining its remaining chunks.
    pub(crate) fn rejected(&self) -> bool {
        matches!(self.target, Target::Rejected(_))
    }

    /// Refuse the transmission, releasing what it had received.
    pub(crate) fn reject(&mut self, failure: Failure) {
        if !self.rejected() {
            self.target = Target::Rejected(failure);
            self.buf = Vec::new();
            self.rgb = Vec::new();
        }
    }

    /// Accept the next fragment of the current chunk's base64 payload.
    pub(crate) fn push(&mut self, payload: &[u8]) {
        if let Err(failure) = self.decode(payload) {
            self.reject(failure);
        }
    }

    /// Mark the end of one chunk, whose base64 is complete in itself.
    pub(crate) fn end_chunk(&mut self) {
        if let Err(failure) = self.decode_tail() {
            self.reject(failure);
        }
    }

    fn decode(&mut self, payload: &[u8]) -> Result<(), Failure> {
        match self.target {
            Target::Rejected(_) => Ok(()),
            Target::Rgb => {
                // Three received bytes become four stored ones.
                let stored = self.buf.len() / 4 * 3 + self.rgb.len();
                if stored + self.base64.bound(payload.len()) > self.limit / 4 * 3 {
                    return Err(TOO_MUCH_DATA);
                }
                self.base64.push(payload, &mut self.rgb)?;
                self.widen();
                Ok(())
            },
            Target::Rgba | Target::Encoded(_) => {
                if self.buf.len() + self.base64.bound(payload.len()) > self.limit {
                    return Err(TOO_MUCH_DATA);
                }
                self.base64.push(payload, &mut self.buf)?;
                self.scan();
                Ok(())
            },
        }
    }

    fn decode_tail(&mut self) -> Result<(), Failure> {
        match self.target {
            Target::Rejected(_) => {
                self.base64 = Base64Stream::default();
                Ok(())
            },
            Target::Rgb => {
                self.base64.finish(&mut self.rgb)?;
                self.widen();
                Ok(())
            },
            Target::Rgba | Target::Encoded(_) => {
                self.base64.finish(&mut self.buf)?;
                self.scan();
                Ok(())
            },
        }
    }

    /// Move every whole 24-bit pixel from the staging buffer into the image.
    fn widen(&mut self) {
        let whole = self.rgb.len() / 3 * 3;
        self.buf.extend(
            self.rgb[..whole].as_chunks::<3>().0.iter().flat_map(|px| [px[0], px[1], px[2], 255]),
        );
        self.rgb.drain(..whole);
    }

    /// Fold the alpha of newly decoded 32-bit pixels into the opacity verdict.
    ///
    /// Done as the pixels arrive, while they are still in cache, so that an image is known to be
    /// opaque the moment it is complete without a second pass over all of it.
    fn scan(&mut self) {
        if !matches!(self.target, Target::Rgba) {
            return;
        }
        let end = (self.buf.len() & !3).min(self.expected);
        if self.opaque && end > self.scanned {
            self.opaque = all_opaque(&self.buf[self.scanned..end]);
        }
        self.scanned = end.max(self.scanned);
    }

    /// Complete the transmission.
    ///
    /// # Errors
    ///
    /// Returns the reason the transmission was refused, or `ENODATA` when it ended short of the
    /// size its dimensions require.
    pub(crate) fn finish(mut self) -> Result<Received, Failure> {
        let (width, height) = (self.start.data_width, self.start.data_height);
        match self.target {
            Target::Rejected(failure) => Err(failure),
            Target::Rgb | Target::Rgba => {
                if self.buf.len() < self.expected {
                    return Err(NOT_ENOUGH_DATA);
                }
                self.buf.truncate(self.expected);
                Ok(Received::Pixels(Pixels { width, height, rgba: self.buf, opaque: self.opaque }))
            },
            Target::Encoded(encoding) => {
                Ok(Received::Job(DecodeJob { encoding, width, height, data: self.buf }))
            },
        }
    }
}

/// Compressed image data waiting to be turned into pixels.
///
/// Running a job is the only expensive step in receiving an image, and it touches nothing but
/// its own data, so it can run without the terminal lock.
pub(crate) struct DecodeJob {
    encoding: Encoding,
    /// Declared dimensions. Meaningless for PNG, which carries its own.
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl std::fmt::Debug for DecodeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodeJob")
            .field("encoding", &self.encoding)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl DecodeJob {
    /// Dimensions the decoded image will have, without decoding it.
    ///
    /// # Errors
    ///
    /// Returns a failure when a PNG's header is unreadable or names a size outside
    /// [`MAX_DIMENSION`].
    pub(crate) fn dimensions(&self) -> Result<(u32, u32), Failure> {
        let header = match self.encoding {
            Encoding::ZlibRgb | Encoding::ZlibRgba => return Ok((self.width, self.height)),
            Encoding::Png => self.data.get(..PNG_HEADER_BYTES).map(<[u8]>::to_vec),
            Encoding::ZlibPng => {
                let mut header = vec![0; PNG_HEADER_BYTES];
                let mut inflater = Decompress::new(true);
                inflater
                    .decompress(&self.data, &mut header, FlushDecompress::None)
                    .ok()
                    .filter(|_| inflater.total_out() == PNG_HEADER_BYTES as u64)
                    .map(|_| header)
            },
        };
        let (width, height) = header.as_deref().and_then(png_dimensions).ok_or(BAD_PNG)?;
        if width == 0 || height == 0 {
            return Err(BAD_PNG);
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Failure::new("EINVAL", "Image too large"));
        }
        Ok((width, height))
    }

    /// Decode the image.
    ///
    /// # Errors
    ///
    /// Returns a failure when the data is corrupt, expands to something other than the declared
    /// size, or exceeds the decoder's limits.
    pub(crate) fn run(self) -> Result<Pixels, Failure> {
        let (width, height) = (self.width, self.height);
        let pixels = width as usize * height as usize;
        match self.encoding {
            Encoding::ZlibRgba => {
                let rgba = inflate_exact(&self.data, pixels * 4)?;
                let opaque = all_opaque(&rgba);
                Ok(Pixels { width, height, rgba, opaque })
            },
            Encoding::ZlibRgb => {
                let rgb = inflate_exact(&self.data, pixels * 3)?;
                let mut rgba = Vec::new();
                rgba.try_reserve_exact(pixels * 4).map_err(|_| OUT_OF_MEMORY)?;
                rgba.extend(
                    rgb.as_chunks::<3>().0.iter().flat_map(|px| [px[0], px[1], px[2], 255]),
                );
                Ok(Pixels { width, height, rgba, opaque: true })
            },
            Encoding::Png => decode_png(&self.data),
            Encoding::ZlibPng => decode_png(&inflate_bounded(&self.data, MAX_ENCODED_BYTES)?),
        }
    }
}

/// Whether every pixel of an RGBA buffer has full alpha.
///
/// Written without an early exit so the loop vectorizes; the buffers this runs over are a few
/// kilobytes of freshly decoded pixels, where a branch per pixel costs more than it saves.
fn all_opaque(rgba: &[u8]) -> bool {
    rgba.as_chunks::<4>().0.iter().fold(0xFF, |alpha, px| alpha & px[3]) == 0xFF
}

/// Width and height from the first [`PNG_HEADER_BYTES`] of a PNG file.
fn png_dimensions(header: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let header: &[u8; PNG_HEADER_BYTES] = header.try_into().ok()?;
    if &header[..8] != SIGNATURE || &header[12..16] != b"IHDR" {
        return None;
    }
    let field = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    Some((field(16), field(20)))
}

/// Inflate a zlib stream that must expand to exactly `expected` bytes.
fn inflate_exact(data: &[u8], expected: usize) -> Result<Vec<u8>, Failure> {
    let mut out = Vec::new();
    out.try_reserve_exact(expected).map_err(|_| OUT_OF_MEMORY)?;
    // `decompress_vec` writes into spare capacity only, so a stream that would expand past the
    // declared size stops at it instead of growing the buffer.
    let status = Decompress::new(true)
        .decompress_vec(data, &mut out, FlushDecompress::Finish)
        .map_err(|_| BAD_ZLIB)?;
    match status {
        Status::StreamEnd if out.len() == expected => Ok(out),
        Status::StreamEnd => Err(NOT_ENOUGH_DATA),
        Status::Ok | Status::BufError => Err(BAD_ZLIB),
    }
}

/// Inflate a zlib stream of unknown size, refusing to produce more than `limit` bytes.
fn inflate_bounded(data: &[u8], limit: usize) -> Result<Vec<u8>, Failure> {
    /// Output is grown in steps of this size; it bounds the over-allocation for small files.
    const STEP: usize = 256 * 1024;

    let mut inflater = Decompress::new(true);
    let mut out = Vec::new();
    loop {
        if out.len() == out.capacity() {
            if out.len() >= limit {
                return Err(TOO_MUCH_DATA);
            }
            let step = STEP.min(limit - out.len());
            out.try_reserve_exact(step).map_err(|_| OUT_OF_MEMORY)?;
        }
        // Both totals are bounded by the lengths of slices already held in memory.
        let consumed = usize::try_from(inflater.total_in()).map_err(|_| BAD_ZLIB)?;
        let before = out.len();
        // Not `Finish`: that promises the whole output fits the buffer it is given, and the
        // stream cannot be resumed once that turns out to be false.
        let status = inflater
            .decompress_vec(&data[consumed..], &mut out, FlushDecompress::None)
            .map_err(|_| BAD_ZLIB)?;
        match status {
            Status::StreamEnd => return Ok(out),
            // No progress with room to write means the stream is truncated.
            Status::Ok | Status::BufError if out.len() == before && out.len() < out.capacity() => {
                return Err(BAD_ZLIB);
            },
            Status::Ok | Status::BufError => {},
        }
    }
}

/// Decode a PNG file into straight-alpha RGBA.
fn decode_png(data: &[u8]) -> Result<Pixels, Failure> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    // A 16-bit image of the largest size is twice its 8-bit form before it is narrowed.
    limits.max_alloc = Some(u64::from(MAX_DIMENSION) * u64::from(MAX_DIMENSION) * 8);
    let mut reader = image::ImageReader::with_format(Cursor::new(data), image::ImageFormat::Png);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| BAD_PNG)?.into_rgba8();
    let (width, height) = image.dimensions();
    let rgba = image.into_raw();
    let opaque = all_opaque(&rgba);
    Ok(Pixels { width, height, rgba, opaque })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    use super::super::command::parse;
    use super::*;

    /// Run a transmission whose chunks are each delivered in the given fragments.
    fn receive(control: &str, chunks: &[&[&[u8]]]) -> Result<Received, Failure> {
        let mut load = Load::begin(parse(control.as_bytes()).unwrap());
        for chunk in chunks {
            for fragment in *chunk {
                load.push(fragment);
            }
            load.end_chunk();
        }
        load.finish()
    }

    fn pixels(received: Result<Received, Failure>) -> Pixels {
        match received.unwrap() {
            Received::Pixels(pixels) => pixels,
            Received::Job(job) => job.run().unwrap(),
        }
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        use image::ImageEncoder;
        let mut file = Vec::new();
        image::codecs::png::PngEncoder::new(&mut file)
            .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        file
    }

    #[test]
    fn rgba_survives_every_split_of_its_base64() {
        let rgba: Vec<u8> = (0..32).collect();
        let encoded = STANDARD.encode(&rgba);
        for split in 0..=encoded.len() {
            let (head, tail) = encoded.as_bytes().split_at(split);
            let received = pixels(receive("s=4,v=2", &[&[head, tail]]));
            assert_eq!(received.rgba, rgba, "split at {split}");
            assert!(!received.opaque);
        }
    }

    #[test]
    fn chunks_are_independent_base64_with_optional_padding() {
        let rgba = [1, 2, 3, 255, 4, 5, 6, 255];
        // Five bytes then three: neither chunk is a multiple of three, so both need padding.
        for engine in [&STANDARD, &STANDARD_NO_PAD] {
            let (first, second) = (engine.encode(&rgba[..5]), engine.encode(&rgba[5..]));
            let received = pixels(receive("s=2,v=1", &[&[first.as_bytes()], &[second.as_bytes()]]));
            assert_eq!(received.rgba, rgba);
            assert!(received.opaque);
        }
    }

    #[test]
    fn opacity_is_decided_by_alpha_alone() {
        let opaque = STANDARD.encode([0, 0, 0, 255, 9, 9, 9, 255]);
        assert!(pixels(receive("s=2,v=1", &[&[opaque.as_bytes()]])).opaque);
        let translucent = STANDARD.encode([255, 255, 255, 255, 255, 255, 255, 254]);
        assert!(!pixels(receive("s=2,v=1", &[&[translucent.as_bytes()]])).opaque);
    }

    #[test]
    fn rgb_is_widened_across_fragment_and_chunk_boundaries() {
        let rgb: Vec<u8> = (1..=18).collect();
        let expected: Vec<u8> = rgb.chunks(3).flat_map(|px| [px[0], px[1], px[2], 255]).collect();
        for split in 0..=rgb.len() {
            let (first, second) = (STANDARD.encode(&rgb[..split]), STANDARD.encode(&rgb[split..]));
            let received =
                pixels(receive("f=24,s=3,v=2", &[&[first.as_bytes()], &[second.as_bytes()]]));
            assert_eq!(received.rgba, expected, "chunked at {split}");
            assert!(received.opaque);
        }
    }

    #[test]
    fn a_short_transmission_is_refused() {
        let encoded = STANDARD.encode([0; 7]);
        assert_eq!(receive("s=2,v=1", &[&[encoded.as_bytes()]]).unwrap_err().code, "ENODATA");
        assert_eq!(receive("f=24,s=2,v=1", &[&[b"AAAA"]]).unwrap_err().code, "ENODATA");
    }

    #[test]
    fn a_transmission_past_its_declared_size_is_refused_and_released() {
        let chunk = STANDARD.encode([0; 3072]);
        let mut load = Load::begin(parse(b"s=2,v=1").unwrap());
        load.push(chunk.as_bytes());
        assert!(load.rejected());
        assert_eq!(load.buf.capacity(), 0, "the partial image is freed at once");
        // Later chunks of the refused transmission are swallowed.
        load.end_chunk();
        load.push(chunk.as_bytes());
        load.end_chunk();
        assert_eq!(load.finish().unwrap_err().code, "EFBIG");
    }

    #[test]
    fn a_few_trailing_bytes_are_tolerated_as_kitty_does() {
        let encoded = STANDARD.encode([7; 8 + 9]);
        assert_eq!(pixels(receive("s=2,v=1", &[&[encoded.as_bytes()]])).rgba, [7; 8]);
    }

    #[test]
    fn invalid_base64_is_refused() {
        for payload in [&b"AA*A"[..], b"A", b"AAAAA", b"AA==AAAA"] {
            let failure = receive("s=1,v=1", &[&[payload]]).unwrap_err();
            assert_eq!(failure.code, "EINVAL", "{:?}", std::str::from_utf8(payload));
        }
    }

    #[test]
    fn impossible_dimensions_and_formats_are_refused_before_any_data() {
        for (control, code) in [
            ("s=0,v=1", "EINVAL"),
            ("s=1,v=0", "EINVAL"),
            ("s=8193,v=1", "EINVAL"),
            ("s=1,v=8193", "EINVAL"),
            ("f=8,s=1,v=1", "EINVAL"),
            ("t=f,s=1,v=1", "EINVAL"),
            ("t=t,s=1,v=1", "EINVAL"),
            ("t=s,s=1,v=1", "EINVAL"),
        ] {
            let load = Load::begin(parse(control.as_bytes()).unwrap());
            assert!(load.rejected(), "{control}");
            assert_eq!(load.finish().unwrap_err().code, code, "{control}");
        }
    }

    #[test]
    fn the_largest_image_reserves_without_committing() {
        let load = Load::begin(parse(b"s=8192,v=8192").unwrap());
        assert!(!load.rejected());
        assert_eq!(load.expected, 8192 * 8192 * 4);
        assert!(load.buf.is_empty());
    }

    #[test]
    fn zlib_pixels_inflate_to_exactly_their_declared_size() {
        let rgba: Vec<u8> = (0..16).map(|value| value | 0xF0).collect();
        let encoded = STANDARD.encode(zlib(&rgba));
        assert_eq!(pixels(receive("o=z,s=2,v=2", &[&[encoded.as_bytes()]])).rgba, rgba);

        let rgb = [1, 2, 3, 4, 5, 6];
        let encoded = STANDARD.encode(zlib(&rgb));
        let received = pixels(receive("o=z,f=24,s=2,v=1", &[&[encoded.as_bytes()]]));
        assert_eq!(received.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
        assert!(received.opaque);
    }

    #[test]
    fn zlib_that_expands_wrongly_is_refused() {
        let job = |control: &str, data: &[u8]| {
            let encoded = STANDARD.encode(data);
            match receive(control, &[&[encoded.as_bytes()]]).unwrap() {
                Received::Job(job) => job.run().unwrap_err().code,
                Received::Pixels(_) => panic!("compressed data must be a job"),
            }
        };
        assert_eq!(job("o=z,s=2,v=2", &zlib(&[0; 15])), "ENODATA", "one byte short");
        assert_eq!(job("o=z,s=2,v=2", &zlib(&[0; 17])), "EINVAL", "one byte over");
        assert_eq!(job("o=z,s=2,v=2", b"not zlib"), "EINVAL");
        let truncated = zlib(&[9; 16]);
        assert_eq!(job("o=z,s=2,v=2", &truncated[..truncated.len() - 3]), "EINVAL");
    }

    /// Zeros compress a thousandfold, so the compressed size says nothing about the work or
    /// memory a stream demands. The declared size has to bound both.
    #[test]
    fn a_zlib_bomb_stops_at_the_declared_size() {
        // Half a megabyte of zeros is about 550 bytes compressed: small enough to be accepted
        // for a single pixel, and then stopped four bytes into inflating it.
        let bomb = zlib(&vec![0; 512 * 1024]);
        assert!(bomb.len() < ZLIB_OVERHEAD);
        let encoded = STANDARD.encode(&bomb);
        let Received::Job(job) = receive("o=z,s=1,v=1", &[&[encoded.as_bytes()]]).unwrap() else {
            panic!("compressed data must be a job");
        };
        assert_eq!(job.run().unwrap_err().code, "EINVAL");

        // A larger one does not even get that far.
        let bomb = zlib(&vec![0; 16 * 1024 * 1024]);
        let encoded = STANDARD.encode(&bomb);
        assert_eq!(receive("o=z,s=1,v=1", &[&[encoded.as_bytes()]]).unwrap_err().code, "EFBIG");

        // A compressed PNG has no declared size, so its inflation is capped outright.
        assert_eq!(inflate_bounded(&bomb, 1024 * 1024).unwrap_err().code, "EFBIG");
        assert_eq!(inflate_bounded(&bomb, 16 * 1024 * 1024).unwrap().len(), 16 * 1024 * 1024);
        assert_eq!(
            inflate_bounded(&bomb[..bomb.len() / 2], 64 * 1024 * 1024).unwrap_err().code,
            "EINVAL"
        );
    }

    #[test]
    fn png_reports_its_own_dimensions_before_and_after_decoding() {
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 9, 9, 9, 255, 1, 1, 1, 255, 2, 2, 2,
            255,
        ];
        let file = png(3, 2, &rgba);
        for (control, data) in [("f=100", file.clone()), ("f=100,o=z", zlib(&file))] {
            let encoded = STANDARD.encode(&data);
            let Received::Job(job) = receive(control, &[&[encoded.as_bytes()]]).unwrap() else {
                panic!("PNG must be a job");
            };
            assert_eq!(job.dimensions(), Ok((3, 2)), "{control}");
            let decoded = job.run().unwrap();
            assert_eq!((decoded.width, decoded.height), (3, 2));
            assert_eq!(decoded.rgba, rgba);
            assert!(!decoded.opaque);
        }
    }

    #[test]
    fn corrupt_png_is_refused_at_the_header_or_at_decode() {
        let job = |data: &[u8]| {
            let encoded = STANDARD.encode(data);
            match receive("f=100", &[&[encoded.as_bytes()]]).unwrap() {
                Received::Job(job) => job,
                Received::Pixels(_) => panic!("PNG must be a job"),
            }
        };
        assert_eq!(job(b"").dimensions().unwrap_err().code, "EBADPNG");
        assert_eq!(
            job(b"GIF89a, definitely not a PNG file").dimensions().unwrap_err().code,
            "EBADPNG"
        );

        let mut file = png(2, 2, &[255; 16]);
        // Past the header, so the dimensions still read; the pixel data no longer decodes.
        let middle = file.len() / 2;
        file.truncate(middle);
        assert_eq!(job(&file).dimensions(), Ok((2, 2)));
        assert_eq!(job(&file).run().unwrap_err().code, "EBADPNG");
    }

    #[test]
    fn a_png_claiming_impossible_dimensions_is_refused_from_its_header() {
        let mut file = png(1, 1, &[0; 4]);
        file[16..20].copy_from_slice(&8193_u32.to_be_bytes());
        let encoded = STANDARD.encode(&file);
        let Received::Job(job) = receive("f=100", &[&[encoded.as_bytes()]]).unwrap() else {
            panic!("PNG must be a job");
        };
        assert_eq!(job.dimensions().unwrap_err().code, "EINVAL");

        file[16..20].copy_from_slice(&0_u32.to_be_bytes());
        let encoded = STANDARD.encode(&file);
        let Received::Job(job) = receive("f=100", &[&[encoded.as_bytes()]]).unwrap() else {
            panic!("PNG must be a job");
        };
        assert_eq!(job.dimensions().unwrap_err().code, "EBADPNG");
    }

    #[test]
    fn debug_output_never_contains_image_data() {
        let payload = STANDARD.encode([0xAB; 3000]);
        let mut load = Load::begin(parse(b"s=100,v=100").unwrap());
        load.push(payload.as_bytes());
        let shown = format!("{load:?}");
        assert!(shown.len() < 1500 && shown.contains("received: 3000"), "{shown}");

        let file = png(40, 40, &[0xCD; 6400]);
        let encoded = STANDARD.encode(&file);
        let Received::Job(job) = receive("f=100", &[&[encoded.as_bytes()]]).unwrap() else {
            panic!("PNG must be a job");
        };
        assert!(format!("{job:?}").len() < 200);
        assert!(format!("{:?}", job.run().unwrap()).len() < 200);
    }

    #[test]
    fn compressed_data_is_bounded_by_what_its_pixels_could_need() {
        let mut load = Load::begin(parse(b"o=z,s=1,v=1").unwrap());
        assert_eq!(load.limit, 4 + ZLIB_OVERHEAD);
        load.push(STANDARD.encode(vec![0; 2048]).as_bytes());
        assert!(load.rejected());
    }
}
