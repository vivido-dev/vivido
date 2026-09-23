//! Screenshot pixel conversion and private PNG persistence.

#[cfg(unix)]
use std::fs;
use std::io::{Error as IoError, ErrorKind, Result as IoResult, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use image::ImageEncoder;

use crate::display::ScreenshotPixels;

/// Convert a padded premultiplied GPU readback and persist it as a private PNG.
pub fn save(mut pixels: ScreenshotPixels) -> IoResult<PathBuf> {
    compact_and_unpremultiply(&mut pixels)?;
    apply_capture_redactions(&mut pixels)?;

    let temp_dir = std::env::temp_dir();
    let temp_dir =
        if temp_dir.is_absolute() { temp_dir } else { std::env::current_dir()?.join(temp_dir) };
    let mut file = tempfile::Builder::new()
        .prefix("vivido-screenshot-")
        .suffix(".png")
        .tempfile_in(temp_dir)?;
    #[cfg(unix)]
    fs::set_permissions(file.path(), fs::Permissions::from_mode(0o600))?;
    // Windows creates this inside the current user's temp directory. The PNG contains no
    // authority, while the IPC endpoint that requests it independently enforces owner-only ACLs.
    image::codecs::png::PngEncoder::new(file.as_file_mut())
        .write_image(&pixels.bytes, pixels.width, pixels.height, image::ExtendedColorType::Rgba8)
        .map_err(IoError::other)?;
    file.as_file_mut().flush()?;

    let (persisted, path) = file.keep().map_err(|err| err.error)?;
    drop(persisted);
    Ok(path)
}

fn apply_capture_redactions(pixels: &mut ScreenshotPixels) -> IoResult<()> {
    let width = usize::try_from(pixels.width)
        .map_err(|_| IoError::new(ErrorKind::InvalidData, "invalid screenshot width"))?;
    let height = usize::try_from(pixels.height)
        .map_err(|_| IoError::new(ErrorKind::InvalidData, "invalid screenshot height"))?;
    for redaction in &pixels.redactions {
        let left = usize::try_from(redaction.left).unwrap_or(usize::MAX).min(width);
        let top = usize::try_from(redaction.top).unwrap_or(usize::MAX).min(height);
        let right = usize::try_from(redaction.right).unwrap_or(usize::MAX).min(width);
        let bottom = usize::try_from(redaction.bottom).unwrap_or(usize::MAX).min(height);
        for row in top..bottom {
            for column in left..right {
                let offset = row
                    .checked_mul(width)
                    .and_then(|value| value.checked_add(column))
                    .and_then(|value| value.checked_mul(4))
                    .ok_or_else(|| {
                        IoError::new(ErrorKind::InvalidData, "invalid screenshot redaction")
                    })?;
                pixels.bytes[offset..offset + 4].copy_from_slice(&[36, 40, 48, 255]);
            }
        }
    }
    Ok(())
}

/// Remove WebGPU row padding and convert premultiplied RGBA to straight alpha in place.
fn compact_and_unpremultiply(pixels: &mut ScreenshotPixels) -> IoResult<()> {
    let row_bytes = usize::try_from(pixels.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "invalid screenshot width"))?;
    let padded_bytes_per_row = usize::try_from(pixels.padded_bytes_per_row)
        .map_err(|_| IoError::new(ErrorKind::InvalidData, "invalid screenshot row stride"))?;
    if padded_bytes_per_row < row_bytes {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            "screenshot row stride is smaller than its pixel width",
        ));
    }

    let height = usize::try_from(pixels.height)
        .map_err(|_| IoError::new(ErrorKind::InvalidData, "invalid screenshot height"))?;
    let padded_length = padded_bytes_per_row
        .checked_mul(height)
        .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "invalid screenshot allocation"))?;
    let compact_length = row_bytes
        .checked_mul(height)
        .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "invalid screenshot allocation"))?;
    if pixels.bytes.len() != padded_length {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            "screenshot readback has an unexpected length",
        ));
    }

    for row in 0..height {
        let source = row * padded_bytes_per_row;
        let destination = row * row_bytes;
        pixels.bytes.copy_within(source..source + row_bytes, destination);
    }
    pixels.bytes.truncate(compact_length);

    for pixel in pixels.bytes.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }

        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = u8::try_from(straight.min(255)).unwrap();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use image::GenericImageView;

    use super::{apply_capture_redactions, compact_and_unpremultiply, save};
    use crate::display::CaptureRedaction;
    use crate::display::ScreenshotPixels;

    #[test]
    fn compacts_rows_and_converts_straight_alpha() {
        let mut bytes = vec![0; 512];
        bytes[..4].copy_from_slice(&[64, 0, 128, 192]);
        bytes[256..260].copy_from_slice(&[10, 20, 30, 0]);
        let mut pixels = ScreenshotPixels {
            bytes,
            width: 1,
            height: 2,
            padded_bytes_per_row: 256,
            redactions: Vec::new(),
        };

        compact_and_unpremultiply(&mut pixels).unwrap();

        assert_eq!(pixels.bytes, [85, 0, 170, 192, 0, 0, 0, 0]);
    }

    #[test]
    fn capture_policy_replaces_pixels_with_an_opaque_neutral_color() {
        let mut pixels = ScreenshotPixels {
            bytes: vec![255; 3 * 2 * 4],
            width: 3,
            height: 2,
            padded_bytes_per_row: 12,
            redactions: vec![CaptureRedaction { left: 1, top: 0, right: 3, bottom: 1 }],
        };
        apply_capture_redactions(&mut pixels).unwrap();
        assert_eq!(&pixels.bytes[4..12], &[36, 40, 48, 255, 36, 40, 48, 255]);
        assert_eq!(&pixels.bytes[12..], &[255; 12]);
    }

    #[test]
    fn saves_private_persistent_png() {
        let pixels = ScreenshotPixels {
            bytes: vec![255, 64, 32, 255],
            width: 1,
            height: 1,
            padded_bytes_per_row: 4,
            redactions: Vec::new(),
        };

        let path = save(pixels).unwrap();
        let image = image::open(&path).unwrap();
        #[cfg(unix)]
        let mode = path.metadata().unwrap().permissions().mode();

        assert!(path.is_absolute());
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.to_rgba8().as_raw(), &[255, 64, 32, 255]);
        #[cfg(unix)]
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_file(path).unwrap();
    }
}
