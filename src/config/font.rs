use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use vivido_config_derive::{ConfigDeserialize, SerdeReplace};

use crate::config::ui_config::Delta;

/// Font config.
///
/// Defaults are provided at the level of this struct per platform, but not per
/// field in this struct. It might be nice in the future to have defaults for
/// each value independently. Alternatively, maybe erroring when the user
/// doesn't provide complete config is Ok.
#[derive(ConfigDeserialize, Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Font {
    /// Extra spacing per character.
    pub offset: Delta<i8>,

    /// Glyph offset within character cell.
    pub glyph_offset: Delta<i8>,

    #[config(removed = "set the AppleFontSmoothing user default instead")]
    pub use_thin_strokes: bool,

    /// Normal font face.
    normal: FontDescription,

    /// Bold font face.
    bold: SecondaryFontDescription,

    /// Italic font face.
    italic: SecondaryFontDescription,

    /// Bold italic font face.
    bold_italic: SecondaryFontDescription,

    /// Font size in points.
    size: Size,

    /// Removed built-in box drawing compatibility key.
    #[config(alias = "builtin_box_drawing", removed = "use a font with box-drawing glyphs")]
    #[serde(skip_serializing)]
    builtin_box_drawing_removed: bool,
}

impl Font {
    /// Get a font clone with a size modification.
    pub fn with_size(self, size: FontSize) -> Font {
        Font { size: Size(size), ..self }
    }

    #[inline]
    pub fn size(&self) -> FontSize {
        self.size.0
    }

    /// Get normal font description.
    pub fn normal(&self) -> &FontDescription {
        &self.normal
    }

    /// Get bold font description.
    pub fn bold(&self) -> FontDescription {
        self.bold.desc(&self.normal)
    }

    /// Get italic font description.
    pub fn italic(&self) -> FontDescription {
        self.italic.desc(&self.normal)
    }

    /// Get bold italic font description.
    pub fn bold_italic(&self) -> FontDescription {
        self.bold_italic.desc(&self.normal)
    }
}

/// Description of the normal font.
#[derive(ConfigDeserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct FontDescription {
    pub family: String,
    pub style: Option<String>,
}

impl FontDescription {
    pub fn style(&self) -> Option<&str> {
        self.style.as_deref()
    }
}

impl Default for FontDescription {
    fn default() -> FontDescription {
        FontDescription {
            #[cfg(not(any(target_os = "macos", windows)))]
            family: "monospace".into(),
            #[cfg(target_os = "macos")]
            family: "Menlo".into(),
            #[cfg(windows)]
            family: "Consolas".into(),
            style: None,
        }
    }
}

/// Description of the italic and bold font.
#[derive(ConfigDeserialize, Serialize, Debug, Default, Clone, PartialEq, Eq)]
pub struct SecondaryFontDescription {
    family: Option<String>,
    style: Option<String>,
}

impl SecondaryFontDescription {
    pub fn desc(&self, fallback: &FontDescription) -> FontDescription {
        FontDescription {
            family: self.family.clone().unwrap_or_else(|| fallback.family.clone()),
            style: self.style.clone(),
        }
    }
}

#[derive(SerdeReplace, Debug, Clone, PartialEq, Eq)]
struct Size(FontSize);

/// Font size stored in configuration points.
#[derive(Debug, Clone, Copy, PartialOrd, PartialEq)]
pub struct FontSize(f32);

impl Eq for FontSize {}

impl FontSize {
    pub const fn new(size: f32) -> Self {
        Self(size)
    }

    pub fn from_px(size: f32) -> Self {
        #[cfg(target_os = "macos")]
        let points = size;
        #[cfg(not(target_os = "macos"))]
        let points = size * 72. / 96.;
        Self(points)
    }

    pub fn as_px(self) -> f32 {
        #[cfg(target_os = "macos")]
        return self.0;
        #[cfg(not(target_os = "macos"))]
        return self.0 * 96. / 72.;
    }

    pub const fn as_pt(self) -> f32 {
        self.0
    }

    pub const fn scale(self, factor: f32) -> Self {
        Self(self.0 * factor)
    }
}

impl Default for Size {
    fn default() -> Self {
        Self(FontSize::new(11.25))
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NumVisitor;
        impl Visitor<'_> for NumVisitor {
            type Value = Size;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("f64 or i64")
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(Size(FontSize::new(value as f32)))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(Size(FontSize::new(value as f32)))
            }
        }

        deserializer.deserialize_any(NumVisitor)
    }
}

impl Serialize for Size {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.0.as_pt())
    }
}

#[cfg(test)]
mod tests {
    use super::FontSize;

    #[test]
    fn point_size_roundtrips_through_platform_pixels() {
        let points = FontSize::new(12.0);
        let pixels = points.as_px();

        #[cfg(target_os = "macos")]
        assert_eq!(pixels, 12.0);
        #[cfg(not(target_os = "macos"))]
        assert_eq!(pixels, 16.0);

        assert!((FontSize::from_px(pixels).as_pt() - 12.0).abs() < f32::EPSILON);
    }
}
