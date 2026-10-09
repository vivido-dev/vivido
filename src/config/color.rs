use serde::de::Error as SerdeError;
use serde::{Deserialize, Deserializer, Serialize};

use crate::display::color::{CellRgb, Rgb};

#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
/// Terminal palette and special-purpose cell colors.
pub struct Colors {
    /// Default terminal colors.
    pub primary: PrimaryColors,
    /// Cursor appearance or current cursor state.
    pub cursor: InvertedCellColors,
    /// Current selection or its configured appearance.
    pub selection: InvertedCellColors,
    /// Normal ANSI palette colors.
    pub normal: NormalColors,
    /// Bright ANSI palette colors.
    pub bright: BrightColors,
    /// Optional dim ANSI palette overrides.
    pub dim: Option<DimColors>,
    /// Explicit overrides for indexed palette entries.
    pub indexed_colors: Vec<IndexedColor>,
    /// Appearance of terminal search matches.
    pub search: SearchColors,
    /// Line-indicator color overrides.
    pub line_indicator: LineIndicatorColors,
    /// Keyboard hint-label appearance.
    pub hints: HintColors,
    /// Apply window transparency to explicit cell background colors.
    pub transparent_background_colors: bool,
    /// Use bright palette entries for bold text.
    pub draw_bold_text_with_bright_colors: bool,
    footer_bar: BarColors,
}

impl Colors {
    /// Return the configured footer-bar foreground color.
    pub fn footer_bar_foreground(&self) -> Rgb {
        self.footer_bar.foreground.unwrap_or(self.primary.background)
    }

    /// Return the configured footer-bar background color.
    pub fn footer_bar_background(&self) -> Rgb {
        self.footer_bar.background.unwrap_or(self.primary.foreground)
    }
}

#[derive(Serialize, Copy, Clone, Default, Debug, PartialEq, Eq)]
/// Foreground and background overrides for the line indicator.
pub struct LineIndicatorColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: Option<Rgb>,
    /// Background color or an override of the existing cell color.
    pub background: Option<Rgb>,
}

#[derive(Serialize, Default, Copy, Clone, Debug, PartialEq, Eq)]
/// Colors used for keyboard hint labels.
pub struct HintColors {
    /// Start of this range.
    pub start: HintStartColors,
    /// End of this range.
    pub end: HintEndColors,
}

#[derive(Serialize, Copy, Clone, Debug, PartialEq, Eq)]
/// Colors for the first character of a hint label.
pub struct HintStartColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: CellRgb,
    /// Background color or an override of the existing cell color.
    pub background: CellRgb,
}

impl Default for HintStartColors {
    fn default() -> Self {
        Self {
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
            background: CellRgb::Rgb(Rgb::new(0xf4, 0xbf, 0x75)),
        }
    }
}

#[derive(Serialize, Copy, Clone, Debug, PartialEq, Eq)]
/// Colors for subsequent hint-label characters.
pub struct HintEndColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: CellRgb,
    /// Background color or an override of the existing cell color.
    pub background: CellRgb,
}

impl Default for HintEndColors {
    fn default() -> Self {
        Self {
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
            background: CellRgb::Rgb(Rgb::new(0xac, 0x42, 0x42)),
        }
    }
}

#[derive(Deserialize, Serialize, Copy, Clone, Default, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// An override for one indexed terminal palette entry.
pub struct IndexedColor {
    /// Color used to paint this item.
    pub color: Rgb,

    index: ColorIndex,
}

impl IndexedColor {
    #[inline]
    /// Return the indexed palette entry being overridden.
    pub fn index(&self) -> u8 {
        self.index.0
    }
}

#[derive(Serialize, Copy, Clone, Default, Debug, PartialEq, Eq)]
struct ColorIndex(u8);

impl<'de> Deserialize<'de> for ColorIndex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let index = u8::deserialize(deserializer)?;

        if index < 16 {
            Err(SerdeError::custom(
                "Config error: indexed_color's index is {}, but a value bigger than 15 was \
                 expected; ignoring setting",
            ))
        } else {
            Ok(Self(index))
        }
    }
}

#[derive(Serialize, Debug, Copy, Clone, PartialEq, Eq)]
/// Foreground and background overrides that can reference existing cell colors.
pub struct InvertedCellColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: CellRgb,
    /// Background color or an override of the existing cell color.
    pub background: CellRgb,
}

impl Default for InvertedCellColors {
    fn default() -> Self {
        Self { foreground: CellRgb::CellBackground, background: CellRgb::CellForeground }
    }
}

#[derive(Serialize, Debug, Copy, Clone, Default, PartialEq, Eq)]
/// Colors for focused and other search matches.
pub struct SearchColors {
    /// Appearance of the focused search match.
    pub focused_match: FocusedMatchColors,
    /// Appearance of other visible search matches.
    pub matches: MatchColors,
}

#[derive(Serialize, Debug, Copy, Clone, PartialEq, Eq)]
/// Colors for the currently focused search match.
pub struct FocusedMatchColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: CellRgb,
    /// Background color or an override of the existing cell color.
    pub background: CellRgb,
}

impl Default for FocusedMatchColors {
    fn default() -> Self {
        Self {
            background: CellRgb::Rgb(Rgb::new(0xf4, 0xbf, 0x75)),
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
        }
    }
}

#[derive(Serialize, Debug, Copy, Clone, PartialEq, Eq)]
/// Colors for other visible search matches.
pub struct MatchColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: CellRgb,
    /// Background color or an override of the existing cell color.
    pub background: CellRgb,
}

impl Default for MatchColors {
    fn default() -> Self {
        Self {
            background: CellRgb::Rgb(Rgb::new(0xac, 0x42, 0x42)),
            foreground: CellRgb::Rgb(Rgb::new(0x18, 0x18, 0x18)),
        }
    }
}

#[derive(Serialize, Debug, Copy, Clone, Default, PartialEq, Eq)]
/// Foreground and background colors for a display bar.
pub struct BarColors {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
/// Default terminal foreground and background colors.
pub struct PrimaryColors {
    /// Foreground color or an override of the existing cell color.
    pub foreground: Rgb,
    /// Background color or an override of the existing cell color.
    pub background: Rgb,
    /// Optional foreground override for bright text.
    pub bright_foreground: Option<Rgb>,
    /// Optional foreground override for dim text.
    pub dim_foreground: Option<Rgb>,
}

impl Default for PrimaryColors {
    fn default() -> Self {
        PrimaryColors {
            background: Rgb::new(0x18, 0x18, 0x18),
            foreground: Rgb::new(0xd8, 0xd8, 0xd8),
            bright_foreground: Default::default(),
            dim_foreground: Default::default(),
        }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
/// The eight normal ANSI palette colors.
pub struct NormalColors {
    /// Black palette entry.
    pub black: Rgb,
    /// Red palette entry.
    pub red: Rgb,
    /// Green palette entry.
    pub green: Rgb,
    /// Yellow palette entry.
    pub yellow: Rgb,
    /// Blue palette entry.
    pub blue: Rgb,
    /// Magenta palette entry.
    pub magenta: Rgb,
    /// Cyan palette entry.
    pub cyan: Rgb,
    /// White palette entry.
    pub white: Rgb,
}

impl Default for NormalColors {
    fn default() -> Self {
        NormalColors {
            black: Rgb::new(0x18, 0x18, 0x18),
            red: Rgb::new(0xac, 0x42, 0x42),
            green: Rgb::new(0x90, 0xa9, 0x59),
            yellow: Rgb::new(0xf4, 0xbf, 0x75),
            blue: Rgb::new(0x6a, 0x9f, 0xb5),
            magenta: Rgb::new(0xaa, 0x75, 0x9f),
            cyan: Rgb::new(0x75, 0xb5, 0xaa),
            white: Rgb::new(0xd8, 0xd8, 0xd8),
        }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
/// The eight bright ANSI palette colors.
pub struct BrightColors {
    /// Black palette entry.
    pub black: Rgb,
    /// Red palette entry.
    pub red: Rgb,
    /// Green palette entry.
    pub green: Rgb,
    /// Yellow palette entry.
    pub yellow: Rgb,
    /// Blue palette entry.
    pub blue: Rgb,
    /// Magenta palette entry.
    pub magenta: Rgb,
    /// Cyan palette entry.
    pub cyan: Rgb,
    /// White palette entry.
    pub white: Rgb,
}

impl Default for BrightColors {
    fn default() -> Self {
        // Generated with oklab by multiplying brightness by 1.12 and then adjusting numbers
        // to make them look "nicer". Yellow color was generated the same way, however the first
        // srgb representable color was picked.
        BrightColors {
            black: Rgb::new(0x6b, 0x6b, 0x6b),
            red: Rgb::new(0xc5, 0x55, 0x55),
            green: Rgb::new(0xaa, 0xc4, 0x74),
            yellow: Rgb::new(0xfe, 0xca, 0x88),
            blue: Rgb::new(0x82, 0xb8, 0xc8),
            magenta: Rgb::new(0xc2, 0x8c, 0xb8),
            cyan: Rgb::new(0x93, 0xd3, 0xc3),
            white: Rgb::new(0xf8, 0xf8, 0xf8),
        }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
/// The eight dim ANSI palette colors.
pub struct DimColors {
    /// Black palette entry.
    pub black: Rgb,
    /// Red palette entry.
    pub red: Rgb,
    /// Green palette entry.
    pub green: Rgb,
    /// Yellow palette entry.
    pub yellow: Rgb,
    /// Blue palette entry.
    pub blue: Rgb,
    /// Magenta palette entry.
    pub magenta: Rgb,
    /// Cyan palette entry.
    pub cyan: Rgb,
    /// White palette entry.
    pub white: Rgb,
}

impl Default for DimColors {
    fn default() -> Self {
        // Generated with builtin vivido's color dimming function.
        DimColors {
            black: Rgb::new(0x0f, 0x0f, 0x0f),
            red: Rgb::new(0x71, 0x2b, 0x2b),
            green: Rgb::new(0x5f, 0x6f, 0x3a),
            yellow: Rgb::new(0xa1, 0x7e, 0x4d),
            blue: Rgb::new(0x45, 0x68, 0x77),
            magenta: Rgb::new(0x70, 0x4d, 0x68),
            cyan: Rgb::new(0x4d, 0x77, 0x70),
            white: Rgb::new(0x8e, 0x8e, 0x8e),
        }
    }
}

impl_config_deserialize!(Colors {
    primary,
    cursor,
    selection,
    normal,
    bright,
    dim: option,
    indexed_colors,
    search,
    line_indicator,
    hints,
    transparent_background_colors,
    draw_bold_text_with_bright_colors,
    footer_bar,
});
impl_config_deserialize!(LineIndicatorColors { foreground: option, background: option });
impl_config_deserialize!(HintColors { start, end });
impl_config_deserialize!(HintStartColors { foreground, background });
impl_config_deserialize!(HintEndColors { foreground, background });
impl_config_deserialize!(InvertedCellColors {
    foreground: alias("text"),
    background: alias("cursor"),
});
impl_config_deserialize!(SearchColors { focused_match, matches });
impl_config_deserialize!(FocusedMatchColors { foreground, background });
impl_config_deserialize!(MatchColors { foreground, background });
impl_config_deserialize!(BarColors { foreground: option, background: option });
impl_config_deserialize!(PrimaryColors {
    foreground,
    background,
    bright_foreground: option,
    dim_foreground: option,
});
impl_config_deserialize!(NormalColors { black, red, green, yellow, blue, magenta, cyan, white });
impl_config_deserialize!(BrightColors { black, red, green, yellow, blue, magenta, cyan, white });
impl_config_deserialize!(DimColors { black, red, green, yellow, blue, magenta, cyan, white });
