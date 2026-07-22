//! Screenshot pixel conversion and private PNG persistence.

use std::fs;
use std::io::{Error as IoError, ErrorKind, Result as IoResult, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use image::ImageEncoder;

use crate::display::ScreenshotPixels;

/// Convert a padded premultiplied GPU readback and persist it as a private PNG.
pub fn save(mut pixels: ScreenshotPixels) -> IoResult<PathBuf> {
    compact_and_unpremultiply(&mut pixels)?;

    let temp_dir = std::env::temp_dir();
    let temp_dir =
        if temp_dir.is_absolute() { temp_dir } else { std::env::current_dir()?.join(temp_dir) };
    let mut file = tempfile::Builder::new()
        .prefix("vivido-screenshot-")
        .suffix(".png")
        .tempfile_in(temp_dir)?;
    fs::set_permissions(file.path(), fs::Permissions::from_mode(0o600))?;
    image::codecs::png::PngEncoder::new(file.as_file_mut())
        .write_image(&pixels.bytes, pixels.width, pixels.height, image::ExtendedColorType::Rgba8)
        .map_err(IoError::other)?;
    file.as_file_mut().flush()?;

    let (persisted, path) = file.keep().map_err(|err| err.error)?;
    drop(persisted);
    Ok(path)
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

    for pixel in pixels.bytes.chunks_exact_mut(4) {
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
    use std::os::unix::fs::PermissionsExt;

    use image::GenericImageView;

    use super::{compact_and_unpremultiply, save};
    use crate::display::ScreenshotPixels;

    #[test]
    fn compacts_rows_and_converts_straight_alpha() {
        let mut bytes = vec![0; 512];
        bytes[..4].copy_from_slice(&[64, 0, 128, 192]);
        bytes[256..260].copy_from_slice(&[10, 20, 30, 0]);
        let mut pixels = ScreenshotPixels { bytes, width: 1, height: 2, padded_bytes_per_row: 256 };

        compact_and_unpremultiply(&mut pixels).unwrap();

        assert_eq!(pixels.bytes, [85, 0, 170, 192, 0, 0, 0, 0]);
    }

    #[test]
    fn saves_private_persistent_png() {
        let pixels = ScreenshotPixels {
            bytes: vec![255, 64, 32, 255],
            width: 1,
            height: 1,
            padded_bytes_per_row: 4,
        };

        let path = save(pixels).unwrap();
        let image = image::open(&path).unwrap();
        let mode = path.metadata().unwrap().permissions().mode();

        assert!(path.is_absolute());
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.to_rgba8().as_raw(), &[255, 64, 32, 255]);
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_file(path).unwrap();
    }
}
