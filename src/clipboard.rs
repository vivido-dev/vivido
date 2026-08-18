use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use image::ImageEncoder;
use log::{debug, warn};
use winit::raw_window_handle::RawDisplayHandle;

use crate::terminal::term::ClipboardType;

#[cfg(any(target_os = "macos", windows))]
use copypasta::ClipboardContext;
use copypasta::ClipboardProvider;
use copypasta::nop_clipboard::NopClipboardContext;
#[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
use copypasta::wayland_clipboard;

/// An upper bound on a pasted image, applied to the decoded RGBA buffer before any allocation.
///
/// The remote binding enforces its own ceiling on the encoded transfer. This one only keeps a
/// hostile or corrupt clipboard advertisement from reserving unbounded local memory.
const MAXIMUM_CLIPBOARD_IMAGE_BYTES: usize = 512 * 1024 * 1024;

/// Non-text clipboard content that can be copied to a file-drop binding instead of pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardMedia {
    /// Paths a file manager placed on the clipboard as `text/uri-list`.
    Files(Vec<PathBuf>),
    /// Raw pixels re-encoded as PNG, under a presenter-chosen name.
    Image { name: String, png: Vec<u8> },
}

pub struct Clipboard {
    clipboard: Box<dyn ClipboardProvider>,
    selection: Option<Box<dyn ClipboardProvider>>,
    media: MediaClipboard,
}

/// Lazily opened media clipboard.
///
/// `copypasta` is text-only, so images and file lists need a second provider. Opening it is
/// deferred until a paste actually asks for media, and a platform that cannot provide one is
/// remembered as unavailable so every later paste stays a plain text paste.
enum MediaClipboard {
    Unopened,
    Open(Box<arboard::Clipboard>),
    Unavailable,
}

impl MediaClipboard {
    fn provider(&mut self) -> Option<&mut arboard::Clipboard> {
        if matches!(self, Self::Unopened) {
            *self = match arboard::Clipboard::new() {
                Ok(clipboard) => Self::Open(Box::new(clipboard)),
                Err(err) => {
                    debug!("Unable to open the media clipboard: {err}");
                    Self::Unavailable
                },
            };
        }
        match self {
            Self::Open(clipboard) => Some(clipboard),
            Self::Unopened | Self::Unavailable => None,
        }
    }
}

/// The boxed-error result copypasta's `ClipboardProvider` uses; its alias is private to that
/// crate, so implementations spell the concrete type.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),))]
type ProviderResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

/// A copypasta-compatible text provider backed by arboard.
///
/// copypasta is not built with an X11 backend, so under X11 arboard carries text as well as
/// images. `primary` addresses the X11 primary selection used by middle-click paste.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),))]
struct ArboardText {
    clipboard: arboard::Clipboard,
    primary: bool,
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),))]
impl ArboardText {
    fn clipboard(clipboard: arboard::Clipboard) -> Self {
        Self { clipboard, primary: false }
    }

    fn primary(clipboard: arboard::Clipboard) -> Self {
        Self { clipboard, primary: true }
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),))]
impl ClipboardProvider for ArboardText {
    fn set_contents(&mut self, text: String) -> ProviderResult<()> {
        use arboard::SetExtLinux;

        let mut operation = self.clipboard.set();
        if self.primary {
            operation = operation.clipboard(arboard::LinuxClipboardKind::Primary);
        }
        operation.text(text).map_err(|err| Box::new(err) as _)
    }

    fn get_contents(&mut self) -> ProviderResult<String> {
        use arboard::GetExtLinux;

        let mut operation = self.clipboard.get();
        if self.primary {
            operation = operation.clipboard(arboard::LinuxClipboardKind::Primary);
        }
        operation.text().map_err(|err| Box::new(err) as _)
    }
}

impl Clipboard {
    pub unsafe fn new(display: RawDisplayHandle) -> Self {
        match display {
            #[cfg(all(feature = "wayland", not(any(target_os = "macos", windows))))]
            RawDisplayHandle::Wayland(display) => {
                let (selection, clipboard) = unsafe {
                    wayland_clipboard::create_clipboards_from_external(display.display.as_ptr())
                };
                Self {
                    clipboard: Box::new(clipboard),
                    selection: Some(Box::new(selection)),
                    media: MediaClipboard::Unopened,
                }
            },
            // copypasta has no X11 backend, so arboard carries text and the primary selection.
            #[cfg(all(
                unix,
                not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),
            ))]
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => Self::new_arboard_text(),
            _ => Self::default(),
        }
    }

    /// Open the text and primary-selection clipboards through arboard.
    ///
    /// A session without any usable clipboard server keeps the nop behavior: no text, and media
    /// permanently unavailable.
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),
    ))]
    fn new_arboard_text() -> Self {
        let Ok(clipboard) = arboard::Clipboard::new() else {
            warn!("Unable to open the X11 clipboard; text clipboard stays disabled");
            return Self::new_nop();
        };
        let selection = arboard::Clipboard::new().map(ArboardText::primary).map_err(|err| {
            warn!("Unable to open the X11 primary selection: {err}");
            err
        });
        Self {
            clipboard: Box::new(ArboardText::clipboard(clipboard)),
            selection: selection
                .ok()
                .map(|adapter| Box::new(adapter) as Box<dyn ClipboardProvider>),
            media: MediaClipboard::Unopened,
        }
    }

    /// Used for tests and as the default before a platform clipboard is connected.
    ///
    /// Media stays permanently unavailable so a headless run never opens a display connection.
    pub fn new_nop() -> Self {
        Self {
            clipboard: Box::new(NopClipboardContext::new().unwrap()),
            selection: None,
            media: MediaClipboard::Unavailable,
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        #[cfg(any(target_os = "macos", windows))]
        return Self {
            clipboard: Box::new(ClipboardContext::new().unwrap()),
            selection: None,
            media: MediaClipboard::Unopened,
        };

        #[cfg(not(any(target_os = "macos", windows)))]
        return Self::new_nop();
    }
}

impl Clipboard {
    pub fn store(&mut self, ty: ClipboardType, text: impl Into<String>) {
        let clipboard = match (ty, &mut self.selection) {
            (ClipboardType::Selection, Some(provider)) => provider,
            (ClipboardType::Selection, None) => return,
            _ => &mut self.clipboard,
        };

        clipboard.set_contents(text.into()).unwrap_or_else(|err| {
            warn!("Unable to store text in clipboard: {err}");
        });
    }

    pub fn load(&mut self, ty: ClipboardType) -> String {
        let clipboard = match (ty, &mut self.selection) {
            (ClipboardType::Selection, Some(provider)) => provider,
            _ => &mut self.clipboard,
        };

        match clipboard.get_contents() {
            Err(err) => {
                debug!("Unable to load text from clipboard: {err}");
                String::new()
            },
            Ok(text) => text,
        }
    }

    /// Load non-text clipboard content, preferring a file list over raw pixels.
    ///
    /// A file manager's copy advertises both `text/uri-list` and, for an image, sometimes a
    /// thumbnail. The paths are the better answer because they carry the real filename. `None`
    /// means the caller should fall back to an ordinary text paste.
    pub fn load_media(&mut self) -> Option<ClipboardMedia> {
        let clipboard = self.media.provider()?;
        match clipboard.get().file_list() {
            Ok(paths) if !paths.is_empty() => return Some(ClipboardMedia::Files(paths)),
            Ok(_) => {},
            Err(err) => debug!("Unable to load a file list from the clipboard: {err}"),
        }
        match clipboard.get().image() {
            Ok(image) => match encode_png(&image) {
                Ok(png) => Some(ClipboardMedia::Image { name: pasted_image_name(), png }),
                Err(err) => {
                    warn!("Unable to encode the pasted image: {err}");
                    None
                },
            },
            Err(err) => {
                debug!("Unable to load an image from the clipboard: {err}");
                None
            },
        }
    }
}

/// Re-encode a clipboard RGBA image as PNG after validating its declared geometry.
fn encode_png(image: &arboard::ImageData<'_>) -> Result<Vec<u8>, String> {
    let width = u32::try_from(image.width).map_err(|_| "image width is out of range".to_owned())?;
    let height =
        u32::try_from(image.height).map_err(|_| "image height is out of range".to_owned())?;
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "image dimensions overflow".to_owned())?;
    if expected > MAXIMUM_CLIPBOARD_IMAGE_BYTES {
        return Err("image exceeds the clipboard ceiling".into());
    }
    if expected != image.bytes.len() {
        return Err("image dimensions disagree with its buffer".into());
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&image.bytes, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|err| err.to_string())?;
    Ok(png)
}

/// Name a pasted image. The receiver resolves collisions, so this only needs to be plausible.
fn pasted_image_name() -> String {
    let seconds =
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
    format!("pasted-image-{seconds}.png")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: usize, height: usize, bytes: Vec<u8>) -> arboard::ImageData<'static> {
        arboard::ImageData { width, height, bytes: bytes.into() }
    }

    #[test]
    fn an_rgba_buffer_round_trips_its_geometry_through_png() {
        let png = encode_png(&image(2, 3, vec![7; 2 * 3 * 4])).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
    }

    #[test]
    fn a_buffer_that_disagrees_with_its_geometry_is_rejected() {
        assert!(encode_png(&image(2, 3, vec![7; 8])).is_err());
        assert!(encode_png(&image(usize::MAX, 2, Vec::new())).is_err());
        assert!(encode_png(&image(1 << 15, 1 << 15, Vec::new())).is_err());
    }

    #[test]
    fn a_pasted_image_name_is_accepted_by_the_wire_validator() {
        vivid_protocol::file_drop::validate_suggested_name(&pasted_image_name()).unwrap();
    }

    /// Runs only when opted in (`VIVIDO_CLIPBOARD_TEST=1`) because it round-trips through the
    /// real session clipboard and would otherwise clobber the user's copy buffer mid-build.
    #[cfg(all(
        unix,
        not(any(target_os = "macos", target_os = "android", target_os = "emscripten")),
    ))]
    #[test]
    fn an_arboard_text_provider_round_trips_when_opted_in() {
        if std::env::var_os("VIVIDO_CLIPBOARD_TEST").is_none() {
            return;
        }
        let Ok(clipboard) = arboard::Clipboard::new() else { return };
        let mut provider = ArboardText::clipboard(clipboard);
        provider.set_contents("vivido".into()).unwrap();
        assert_eq!(provider.get_contents().unwrap(), "vivido");
    }
}
