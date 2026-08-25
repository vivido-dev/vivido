use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;
use std::sync::Arc;
use std::{array, env};

use ahash::{AHashMap, AHashSet};
use parley::fontique::{FallbackKey, FamilyId, Language, ScriptExt};
use parley::layout::PositionedLayoutItem;
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontFamilyName,
    FontStyle as ParleyFontStyle, FontWeight, GenericFamily, Layout, LayoutContext, LineHeight,
    StyleProperty,
};
use unicode_script::UnicodeScript as _;
use vello::kurbo::Affine;
use vello::peniko::{Brush, Color, Fill};
use vello::{Glyph, Scene};

use crate::terminal::term::cell::Flags;

use crate::config::font::Font;
use crate::display::color::Rgb;
use crate::display::content::RenderableCell;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextMetrics {
    pub cell_width: f32,
    pub cell_height: f32,
    pub baseline: f32,
    pub descent: f32,
    pub underline_position: f32,
    pub underline_thickness: f32,
    pub strikeout_position: f32,
    pub strikeout_thickness: f32,
    pub glyph_offset_x: f32,
    pub glyph_offset_y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum FontVariant {
    Normal,
    Bold,
    Italic,
    BoldItalic,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LayoutTextKey {
    Char(char),
    String(Box<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LayoutKey {
    text: LayoutTextKey,
    variant: FontVariant,
    font_size_bits: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TerminalLayoutKey {
    text: Box<str>,
    spans: Box<[TerminalStyleKey]>,
    ligatures: bool,
    font_size_bits: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TerminalStyleKey {
    end: usize,
    variant: FontVariant,
    color: (u8, u8, u8),
}

/// One contiguous presentation span in a terminal row shaping run.
#[derive(Clone, Debug)]
pub(crate) struct TerminalTextStyle {
    pub range: Range<usize>,
    pub flags: Flags,
    pub color: Rgb,
}

/// Shaped layouts kept alive at once.
///
/// The cache is keyed partly by the cluster's text, so a program emitting many distinct
/// multi-character clusters — emoji sequences, combining marks, a fuzzer — grew it without bound;
/// only a font change ever cleared it. A terminal's real working set is a few hundred distinct
/// clusters, so four thousand keeps every ordinary session entirely cached and still bounds the
/// pathological one.
const MAX_CACHED_LAYOUTS: usize = 4096;

/// How much of the cache one eviction reclaims.
///
/// Reclaiming a batch rather than a single entry is what keeps the scan amortized: one pass over
/// the cache buys the next thousand insertions, so a flood of distinct clusters pays a constant
/// amount each instead of a full scan apiece.
const LAYOUT_EVICTION_BATCH: usize = MAX_CACHED_LAYOUTS / 4;

thread_local! {
    /// Parley designs its font context as one application/thread resource. Keep a pristine
    /// per-thread template so every pane and the integrated chrome share the same DirectWrite
    /// system collection while retaining independent query, source, and layout caches.
    static FONT_CONTEXT: RefCell<Option<FontContext>> = const { RefCell::new(None) };

    #[cfg(test)]
    static FONT_CONTEXT_INITIALIZATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

struct CachedLayout {
    layout: Arc<Layout<()>>,
    /// The value of `cache_clock` when this layout was last handed out.
    used: u64,
}

struct CachedTerminalLayout {
    layout: Arc<Layout<Rgb>>,
    used: u64,
}

pub struct TextSystem {
    font: Font,
    font_cx: FontContext,
    layout_cx: LayoutContext<()>,
    terminal_layout_cx: LayoutContext<Rgb>,
    metrics: TextMetrics,
    locale: Option<Language>,
    fallback_search_families: Arc<[FamilyId]>,
    checked_fallbacks: AHashSet<(FallbackKey, char)>,
    pua_fallback_family_names: Arc<[FontFamilyName<'static>]>,
    family_stacks: [FontFamily<'static>; 4],
    variant_styles: [(ParleyFontStyle, FontWeight); 4],
    cache: AHashMap<LayoutKey, CachedLayout>,
    cache_clock: u64,
    terminal_cache: AHashMap<TerminalLayoutKey, CachedTerminalLayout>,
    terminal_cache_clock: u64,
}

impl TextSystem {
    pub fn new(font: Font) -> Self {
        let mut font_cx = shared_font_context();
        let fallback_search_families = fallback_search_families(&mut font_cx);
        let pua_fallback_family_names = pua_fallback_family_names(&mut font_cx);
        let mut text_system = Self {
            family_stacks: family_stacks_for_font(&font, &pua_fallback_family_names),
            variant_styles: variant_styles_for_font(&font),
            font,
            font_cx,
            layout_cx: LayoutContext::default(),
            terminal_layout_cx: LayoutContext::default(),
            metrics: TextMetrics::default(),
            locale: text_locale(),
            fallback_search_families,
            checked_fallbacks: AHashSet::default(),
            pua_fallback_family_names,
            cache: AHashMap::new(),
            cache_clock: 0,
            terminal_cache: AHashMap::new(),
            terminal_cache_clock: 0,
        };
        text_system.metrics = text_system.measure_metrics();
        text_system
    }

    pub fn metrics(&self) -> TextMetrics {
        self.metrics
    }

    /// Whether compatible terminal cells should be shaped as ligature runs.
    pub fn ligatures(&self) -> bool {
        self.font.ligatures()
    }

    pub fn update_font(&mut self, font: Font) {
        self.font = font;
        self.family_stacks = family_stacks_for_font(&self.font, &self.pua_fallback_family_names);
        self.variant_styles = variant_styles_for_font(&self.font);
        self.metrics = self.measure_metrics();
        self.checked_fallbacks.clear();
        self.cache.clear();
        self.terminal_cache.clear();
    }

    pub fn shape_cell(&mut self, cell: &RenderableCell) -> Option<Arc<Layout<()>>> {
        if cell.flags.contains(Flags::HIDDEN) {
            return None;
        }

        // Tabs are stored in the grid so selection/copy can reconstruct them, but they should
        // never be shaped as visible glyphs.
        if cell.character == '\t' {
            return None;
        }

        let variant = font_variant(cell.flags);
        if let Some(extra) = cell.extra.as_ref().and_then(|extra| extra.zerowidth.as_ref()) {
            let mut text = String::with_capacity(1 + extra.len());
            text.push(cell.character);
            text.extend(extra.iter().copied());
            Some(self.shape_text(text, variant))
        } else {
            Some(self.shape_char(cell.character, variant))
        }
    }

    /// Shape a complete terminal row run so contextual shaping and the Unicode BiDi algorithm
    /// see the neighboring cells they require.
    pub(crate) fn shape_terminal_run(
        &mut self,
        text: String,
        styles: &[TerminalTextStyle],
        ligatures: bool,
    ) -> Arc<Layout<Rgb>> {
        self.ensure_fontique_fallbacks(&text);

        let style_keys = styles
            .iter()
            .map(|style| TerminalStyleKey {
                end: style.range.end,
                variant: font_variant(style.flags),
                color: style.color.as_tuple(),
            })
            .collect::<Box<[_]>>();
        let key = TerminalLayoutKey {
            text: text.clone().into_boxed_str(),
            spans: style_keys,
            ligatures,
            font_size_bits: self.font.size().as_px().to_bits(),
        };
        if let Some(layout) = self.cached_terminal_layout(&key) {
            return layout;
        }

        self.build_and_cache_terminal_layout(key, &text, styles, ligatures)
    }

    #[cfg(test)]
    pub fn shape_string(
        &mut self,
        text: impl Into<String>,
        bold: bool,
        italic: bool,
    ) -> Option<Arc<Layout<()>>> {
        let text = text.into();
        if text.is_empty() {
            return None;
        }

        let variant = font_variant_from_style(bold, italic);
        if let Some(character) = single_char(&text) {
            Some(self.shape_char(character, variant))
        } else {
            Some(self.shape_text(text, variant))
        }
    }

    pub fn shape_character(
        &mut self,
        character: char,
        bold: bool,
        italic: bool,
    ) -> Arc<Layout<()>> {
        self.shape_char(character, font_variant_from_style(bold, italic))
    }

    /// Measure one UI text run without painting it.
    ///
    /// Chrome which sizes itself around a label — a menu panel, a button — needs the advance before
    /// it has a scene to paint into, and shaping goes through the same cache `paint_text` uses.
    pub fn measure_text(&mut self, text: &str, bold: bool) -> f32 {
        if text.is_empty() {
            return 0.0;
        }

        self.shape_text(text.to_owned(), font_variant_from_style(bold, false)).full_width()
    }

    /// Shape and paint one UI text run at a physical-pixel origin.
    ///
    /// This uses the same font fallback, named-style, and glyph positioning path as terminal text,
    /// allowing an embedding shell to draw chrome without maintaining a second text stack.
    pub fn paint_text(
        &mut self,
        scene: &mut Scene,
        text: &str,
        origin: (f32, f32),
        color: Rgb,
        bold: bool,
    ) -> f32 {
        if text.is_empty() {
            return 0.0;
        }

        let layout = self.shape_text(text.to_owned(), font_variant_from_style(bold, false));
        let transform = Affine::translate((f64::from(origin.0), f64::from(origin.1)));
        let brush = Brush::Solid(color_from_rgb(color));

        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };

                let run = glyph_run.run();
                let mut cursor_x = glyph_run.offset();
                let baseline = glyph_run.baseline();
                scene
                    .draw_glyphs(run.font())
                    .brush(&brush)
                    .hint(false)
                    .transform(transform)
                    .font_size(run.font_size())
                    .normalized_coords(run.normalized_coords())
                    .draw(
                        Fill::NonZero,
                        glyph_run.glyphs().map(|glyph| {
                            let positioned = Glyph {
                                id: glyph.id,
                                x: cursor_x + glyph.x,
                                y: baseline - glyph.y,
                            };
                            cursor_x += glyph.advance;
                            positioned
                        }),
                    );
            }
        }

        layout.full_width()
    }

    fn shape_char(&mut self, character: char, variant: FontVariant) -> Arc<Layout<()>> {
        let mut buffer = [0; 4];
        let text = character.encode_utf8(&mut buffer);
        self.ensure_fontique_fallbacks(text);

        let key = LayoutKey {
            text: LayoutTextKey::Char(character),
            variant,
            font_size_bits: self.font.size().as_px().to_bits(),
        };
        if let Some(layout) = self.cached_layout(&key) {
            return layout;
        }

        self.build_and_cache_layout(key, text, variant)
    }

    fn shape_text(&mut self, text: String, variant: FontVariant) -> Arc<Layout<()>> {
        self.ensure_fontique_fallbacks(&text);

        let key = LayoutKey {
            text: LayoutTextKey::String(text.clone().into_boxed_str()),
            variant,
            font_size_bits: self.font.size().as_px().to_bits(),
        };
        if let Some(layout) = self.cached_layout(&key) {
            return layout;
        }

        self.build_and_cache_layout(key, &text, variant)
    }

    /// Take a layout out of the cache, recording that it is the most recently wanted one.
    fn cached_layout(&mut self, key: &LayoutKey) -> Option<Arc<Layout<()>>> {
        let clock = self.cache_clock.saturating_add(1);
        let entry = self.cache.get_mut(key)?;
        entry.used = clock;
        self.cache_clock = clock;
        Some(Arc::clone(&entry.layout))
    }

    fn cached_terminal_layout(&mut self, key: &TerminalLayoutKey) -> Option<Arc<Layout<Rgb>>> {
        let clock = self.terminal_cache_clock.saturating_add(1);
        let entry = self.terminal_cache.get_mut(key)?;
        entry.used = clock;
        self.terminal_cache_clock = clock;
        Some(Arc::clone(&entry.layout))
    }

    /// Drop the least recently used part of the cache once it reaches its bound.
    ///
    /// The eviction is by recency rather than by insertion order, so the handful of clusters a
    /// session actually repeats survives any flood of one-off ones passing through around it.
    fn evict_cached_layouts(&mut self) {
        if self.cache.len() < MAX_CACHED_LAYOUTS {
            return;
        }
        let mut used = self.cache.values().map(|entry| entry.used).collect::<Vec<_>>();
        // Every entry's clock value is distinct, so this is the exact recency of the
        // `LAYOUT_EVICTION_BATCH`th oldest, and everything at or below it goes.
        let (_, threshold, _) = used.select_nth_unstable(LAYOUT_EVICTION_BATCH);
        let threshold = *threshold;
        self.cache.retain(|_, entry| entry.used > threshold);
    }

    fn evict_cached_terminal_layouts(&mut self) {
        if self.terminal_cache.len() < MAX_CACHED_LAYOUTS {
            return;
        }
        let mut used = self.terminal_cache.values().map(|entry| entry.used).collect::<Vec<_>>();
        let (_, threshold, _) = used.select_nth_unstable(LAYOUT_EVICTION_BATCH);
        let threshold = *threshold;
        self.terminal_cache.retain(|_, entry| entry.used > threshold);
    }

    fn build_and_cache_layout(
        &mut self,
        key: LayoutKey,
        text: &str,
        variant: FontVariant,
    ) -> Arc<Layout<()>> {
        let family = self.family_stacks[variant.as_index()].clone();
        let (font_style, font_weight) = self.variant_styles[variant.as_index()];

        let mut builder = self.layout_cx.ranged_builder(&mut self.font_cx, text, 1.0, true);
        builder.push_default(family);
        builder.push_default(StyleProperty::FontSize(self.font.size().as_px()));
        builder.push_default(StyleProperty::FontStyle(font_style));
        builder.push_default(StyleProperty::FontWeight(font_weight));
        builder.push_default(StyleProperty::Locale(self.locale));
        builder.push_default(LineHeight::Absolute(self.metrics.cell_height));

        let mut layout = builder.build(text);
        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());

        let layout = Arc::new(layout);
        self.evict_cached_layouts();
        self.cache_clock = self.cache_clock.saturating_add(1);
        self.cache
            .insert(key, CachedLayout { layout: Arc::clone(&layout), used: self.cache_clock });
        layout
    }

    fn build_and_cache_terminal_layout(
        &mut self,
        key: TerminalLayoutKey,
        text: &str,
        styles: &[TerminalTextStyle],
        ligatures: bool,
    ) -> Arc<Layout<Rgb>> {
        let mut builder =
            self.terminal_layout_cx.ranged_builder(&mut self.font_cx, text, 1.0, true);
        builder.push_default(self.family_stacks[FontVariant::Normal.as_index()].clone());
        builder.push_default(StyleProperty::FontSize(self.font.size().as_px()));
        builder.push_default(StyleProperty::Locale(self.locale));
        builder.push_default(LineHeight::Absolute(self.metrics.cell_height));
        if !ligatures {
            // Keep the features required for cursive scripts, but honor the terminal option for
            // discretionary Latin/code ligatures inside a row that also contains complex text.
            builder.push_default(parley::FontFeatures::from(
                "\"liga\" 0, \"clig\" 0, \"dlig\" 0, \"calt\" 0",
            ));
        }

        for style in styles {
            let variant = font_variant(style.flags);
            let (font_style, font_weight) = self.variant_styles[variant.as_index()];
            builder.push(self.family_stacks[variant.as_index()].clone(), style.range.clone());
            builder.push(StyleProperty::FontStyle(font_style), style.range.clone());
            builder.push(StyleProperty::FontWeight(font_weight), style.range.clone());
            builder.push(StyleProperty::Brush(style.color), style.range.clone());
        }

        let mut layout = builder.build(text);
        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());

        let layout = Arc::new(layout);
        self.evict_cached_terminal_layouts();
        self.terminal_cache_clock = self.terminal_cache_clock.saturating_add(1);
        self.terminal_cache.insert(
            key,
            CachedTerminalLayout { layout: Arc::clone(&layout), used: self.terminal_cache_clock },
        );
        layout
    }

    fn measure_metrics(&mut self) -> TextMetrics {
        let sample = "M";
        let family = self.family_stacks[FontVariant::Normal.as_index()].clone();
        let font_size = self.font.size().as_px();
        let mut builder = self.layout_cx.ranged_builder(&mut self.font_cx, sample, 1.0, true);
        builder.push_default(family);
        builder.push_default(StyleProperty::FontSize(font_size));
        builder.push_default(StyleProperty::Locale(self.locale));

        let mut layout = builder.build(sample);
        layout.break_all_lines(None);
        layout.align(Alignment::Start, AlignmentOptions::default());

        let line = layout.lines().next().expect("sample line");
        let run_metrics = line
            .items()
            .find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(glyph_run) => Some(*glyph_run.run().metrics()),
                _ => None,
            })
            .unwrap_or_default();

        TextMetrics {
            cell_width: (layout.full_width() + f32::from(self.font.offset.x)).floor().max(1.0),
            cell_height: (line.metrics().line_height + f32::from(self.font.offset.y))
                .floor()
                .max(1.0),
            baseline: line.metrics().baseline,
            descent: line.metrics().descent,
            underline_position: run_metrics.underline_offset,
            underline_thickness: run_metrics.underline_size.max(1.0),
            strikeout_position: run_metrics.strikethrough_offset,
            strikeout_thickness: run_metrics.strikethrough_size.max(1.0),
            glyph_offset_x: f32::from(self.font.glyph_offset.x),
            glyph_offset_y: f32::from(self.font.glyph_offset.y),
        }
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.cache.len()
    }

    fn ensure_fontique_fallbacks(&mut self, text: &str) {
        let mut changed = false;

        for character in text.chars() {
            let Some(key) = self.fallback_key_for_char(character) else {
                continue;
            };

            if !self.checked_fallbacks.insert((key, character)) {
                continue;
            }

            if self.fallbacks_support_character(key, character) {
                continue;
            }

            changed |= self.seed_fontique_fallbacks(key, character);
        }

        if changed {
            self.checked_fallbacks.clear();
            self.cache.clear();
            self.terminal_cache.clear();
        }
    }

    fn fallback_key_for_char(&self, character: char) -> Option<FallbackKey> {
        let script = fontique_script_for_char(character)?;
        let localized = self.locale.as_ref().map(|locale| FallbackKey::from((script, locale)));
        match localized {
            Some(key) if key.is_tracked() => Some(key),
            _ => Some(FallbackKey::from(script)),
        }
    }

    fn fallbacks_support_character(&mut self, key: FallbackKey, character: char) -> bool {
        let fallback_families = self.font_cx.collection.fallback_families(key).collect::<Vec<_>>();
        let mut buffer = [0; 4];
        let character_text = character.encode_utf8(&mut buffer);
        fallback_families
            .into_iter()
            .any(|family_id| self.family_supports_text(family_id, character_text))
    }

    fn seed_fontique_fallbacks(&mut self, key: FallbackKey, character: char) -> bool {
        let fallback_families = self.find_fallback_families(key.script(), character);
        if fallback_families.is_empty() {
            return false;
        }

        self.font_cx.collection.append_fallbacks(key, fallback_families.into_iter())
    }

    fn find_fallback_families(
        &mut self,
        script: parley::fontique::Script,
        character: char,
    ) -> Vec<FamilyId> {
        let mut character_buffer = [0; 4];
        let character_text = character.encode_utf8(&mut character_buffer);
        let sample_text = script.sample().unwrap_or(character_text);
        let use_sample_text = sample_text != character_text;
        let search_families = Arc::clone(&self.fallback_search_families);

        let mut preferred = Vec::new();
        let mut fallback_only = Vec::new();
        for &family_id in search_families.iter() {
            if !self.family_supports_text(family_id, character_text) {
                continue;
            }

            if use_sample_text && self.family_supports_text(family_id, sample_text) {
                preferred.push(family_id);
            } else {
                fallback_only.push(family_id);
            }
        }

        preferred.extend(fallback_only);
        preferred
    }

    fn family_supports_text(&mut self, family_id: FamilyId, text: &str) -> bool {
        let Some(family) = self.font_cx.collection.family(family_id) else {
            return false;
        };

        family.fonts().iter().any(|font| {
            let Some(data) = font.load(Some(&mut self.font_cx.source_cache)) else {
                return false;
            };
            let Some(charmap) = font.charmap_index().charmap(data.as_ref()) else {
                return false;
            };

            text.chars()
                .all(|character| charmap.map(character).is_some_and(|glyph_id| glyph_id != 0))
        })
    }
}

fn shared_font_context() -> FontContext {
    FONT_CONTEXT.with_borrow_mut(|shared| {
        shared
            .get_or_insert_with(|| {
                #[cfg(test)]
                FONT_CONTEXT_INITIALIZATIONS.with(|count| count.set(count.get() + 1));
                FontContext::default()
            })
            .clone()
    })
}

impl FontVariant {
    const fn as_index(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Bold => 1,
            Self::Italic => 2,
            Self::BoldItalic => 3,
        }
    }
}

#[cfg(test)]
fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

fn font_variant(flags: Flags) -> FontVariant {
    font_variant_from_style(
        flags.intersects(Flags::BOLD | Flags::DIM_BOLD),
        flags.contains(Flags::ITALIC),
    )
}

fn font_variant_from_style(bold: bool, italic: bool) -> FontVariant {
    match (bold, italic) {
        (true, true) => FontVariant::BoldItalic,
        (true, false) => FontVariant::Bold,
        (false, true) => FontVariant::Italic,
        (false, false) => FontVariant::Normal,
    }
}

fn font_style(variant: FontVariant) -> (ParleyFontStyle, FontWeight) {
    match variant {
        FontVariant::Normal => (ParleyFontStyle::Normal, FontWeight::NORMAL),
        FontVariant::Bold => (ParleyFontStyle::Normal, FontWeight::BOLD),
        FontVariant::Italic => (ParleyFontStyle::Italic, FontWeight::NORMAL),
        FontVariant::BoldItalic => (ParleyFontStyle::Italic, FontWeight::BOLD),
    }
}

fn variant_styles_for_font(font: &Font) -> [(ParleyFontStyle, FontWeight); 4] {
    array::from_fn(|index| {
        let variant = match index {
            0 => FontVariant::Normal,
            1 => FontVariant::Bold,
            2 => FontVariant::Italic,
            3 => FontVariant::BoldItalic,
            _ => unreachable!("font variant index out of range"),
        };
        let description = match variant {
            FontVariant::Normal => font.normal().clone(),
            FontVariant::Bold => font.bold(),
            FontVariant::Italic => font.italic(),
            FontVariant::BoldItalic => font.bold_italic(),
        };
        parse_named_style(description.style(), variant)
    })
}

fn parse_named_style(style: Option<&str>, variant: FontVariant) -> (ParleyFontStyle, FontWeight) {
    let fallback = font_style(variant);
    let Some(style) = style.map(str::trim).filter(|style| !style.is_empty()) else {
        return fallback;
    };
    let normalized = style.to_ascii_lowercase().replace(['-', '_'], " ");
    let slant = if normalized.contains("italic") {
        ParleyFontStyle::Italic
    } else if normalized.contains("oblique") {
        ParleyFontStyle::Oblique(Some(14.0))
    } else {
        ParleyFontStyle::Normal
    };
    let weight = if normalized.contains("extra light") || normalized.contains("ultra light") {
        FontWeight::EXTRA_LIGHT
    } else if normalized.contains("thin") {
        FontWeight::THIN
    } else if normalized.contains("semi bold") || normalized.contains("demibold") {
        FontWeight::SEMI_BOLD
    } else if normalized.contains("extra bold") || normalized.contains("ultra bold") {
        FontWeight::EXTRA_BOLD
    } else if normalized.contains("black") || normalized.contains("heavy") {
        FontWeight::BLACK
    } else if normalized.contains("light") {
        FontWeight::LIGHT
    } else if normalized.contains("medium") {
        FontWeight::MEDIUM
    } else if normalized.contains("bold") {
        FontWeight::BOLD
    } else if ["regular", "normal", "book", "italic", "oblique"]
        .iter()
        .any(|name| normalized == *name)
    {
        FontWeight::NORMAL
    } else {
        log::warn!("Unsupported font style {style:?}; using the terminal variant default");
        return fallback;
    };
    (slant, weight)
}

pub fn color_from_rgb(color: Rgb) -> Color {
    Color::from_rgb8(color.r, color.g, color.b)
}

fn family_stacks_for_font(
    font: &Font,
    pua_fallbacks: &[FontFamilyName<'static>],
) -> [FontFamily<'static>; 4] {
    array::from_fn(|index| {
        let variant = match index {
            0 => FontVariant::Normal,
            1 => FontVariant::Bold,
            2 => FontVariant::Italic,
            3 => FontVariant::BoldItalic,
            _ => unreachable!("font variant index out of range"),
        };
        FontFamily::List(Cow::Owned(font_family_stack(font, variant, pua_fallbacks)))
    })
}

fn font_family_stack(
    font: &Font,
    variant: FontVariant,
    pua_fallbacks: &[FontFamilyName<'static>],
) -> Vec<FontFamilyName<'static>> {
    let mut families = Vec::new();
    push_configured_family_names(&mut families, variant_family_spec(font, variant));

    if variant != FontVariant::Normal {
        push_configured_family_names(&mut families, &font.normal().family);
    }

    push_family_name(&mut families, GenericFamily::UiMonospace.into());
    push_family_name(&mut families, GenericFamily::Monospace.into());
    push_family_name(&mut families, GenericFamily::SystemUi.into());
    push_family_name(&mut families, GenericFamily::Emoji.into());

    for family in pua_fallbacks {
        push_family_name(&mut families, family.clone());
    }

    families
}

fn pua_fallback_family_names(font_cx: &mut FontContext) -> Arc<[FontFamilyName<'static>]> {
    let mut names = font_cx
        .collection
        .family_names()
        .filter(|name| name.to_ascii_lowercase().contains("nerd font"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    names.sort_unstable_by_key(|name| {
        let lowercase = name.to_ascii_lowercase();
        (!lowercase.starts_with("symbols nerd font"), lowercase)
    });
    names.dedup();
    names.into_iter().map(named_family).collect::<Vec<_>>().into()
}

fn variant_family_spec(font: &Font, variant: FontVariant) -> Cow<'_, str> {
    match variant {
        FontVariant::Normal => Cow::Borrowed(&font.normal().family),
        FontVariant::Bold => Cow::Owned(font.bold().family),
        FontVariant::Italic => Cow::Owned(font.italic().family),
        FontVariant::BoldItalic => Cow::Owned(font.bold_italic().family),
    }
}

fn push_configured_family_names(
    families: &mut Vec<FontFamilyName<'static>>,
    spec: impl AsRef<str>,
) {
    let spec = spec.as_ref().trim();
    if spec.is_empty() {
        return;
    }

    match FontFamilyName::parse_css_list(spec).collect::<Result<Vec<_>, _>>() {
        Ok(parsed) if !parsed.is_empty() => {
            for family in parsed {
                push_family_name(families, family.into_owned());
            }
        },
        _ => {
            let literal = spec.trim_matches(|character| matches!(character, '\'' | '"'));
            push_family_name(families, named_family(literal));
        },
    }
}

fn named_family(name: impl AsRef<str>) -> FontFamilyName<'static> {
    FontFamilyName::Named(Cow::Owned(name.as_ref().to_owned()))
}

fn push_family_name(families: &mut Vec<FontFamilyName<'static>>, family: FontFamilyName<'static>) {
    if !families.contains(&family) {
        families.push(family);
    }
}

fn fallback_search_families(font_cx: &mut FontContext) -> Arc<[FamilyId]> {
    let mut families = Vec::new();
    let mut seen = AHashSet::default();

    for generic_family in [
        GenericFamily::UiMonospace,
        GenericFamily::Monospace,
        GenericFamily::SystemUi,
        GenericFamily::Emoji,
    ] {
        for family_id in font_cx.collection.generic_families(generic_family) {
            if seen.insert(family_id) {
                families.push(family_id);
            }
        }
    }

    let mut family_names = font_cx.collection.family_names().map(str::to_owned).collect::<Vec<_>>();
    family_names.sort_unstable_by_key(|family_name| family_name_sort_key(family_name));
    family_names.dedup();

    for family_name in family_names {
        let Some(family_id) = font_cx.collection.family_id(&family_name) else {
            continue;
        };
        if seen.insert(family_id) {
            families.push(family_id);
        }
    }

    Arc::from(families)
}

fn family_name_sort_key(family_name: &str) -> (bool, String) {
    (family_name.starts_with('.'), family_name.to_ascii_lowercase())
}

fn text_locale() -> Option<Language> {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|key| env::var(key).ok())
        .and_then(|value| normalize_locale(&value))
        .and_then(|locale| Language::parse(&locale).ok())
}

fn normalize_locale(locale: &str) -> Option<String> {
    let locale = locale.trim();
    if locale.is_empty() || matches!(locale, "C" | "POSIX") {
        return None;
    }

    let locale = locale.split_once('.').map(|(locale, _)| locale).unwrap_or(locale);
    let locale = locale.split_once('@').map(|(locale, _)| locale).unwrap_or(locale);
    let locale = locale.replace('_', "-");

    if locale.is_empty() { None } else { Some(locale) }
}

fn fontique_script_for_char(character: char) -> Option<parley::fontique::Script> {
    let script = character.script();
    let name = script.short_name();
    // Private-use characters have the Unknown script, but terminal icon fonts intentionally map
    // them. Keeping the Zzzz key gives Fontique a fallback bucket matching the shaped run; generic
    // Common/Inherited punctuation still uses the neighboring run's normal fallback behavior.
    if name == "Zzzz" && is_private_use(character) {
        return parley::fontique::Script::parse(name).ok();
    }
    if matches!(name, "Zyyy" | "Zinh" | "Zzzz") {
        return None;
    }
    parley::fontique::Script::parse(name).ok()
}

fn is_private_use(character: char) -> bool {
    matches!(character as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD)
}

#[cfg(test)]
#[path = "glyph_corpus.rs"]
mod glyph_corpus;

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use crate::terminal::index::Point;
    use crate::terminal::term::cell::Flags;
    use parley::fontique::{FallbackKey, Language, Script};
    use parley::layout::PositionedLayoutItem;
    use parley::{FontFamilyName, GenericFamily};

    use super::{
        FONT_CONTEXT_INITIALIZATIONS, FontVariant, LAYOUT_EVICTION_BATCH, MAX_CACHED_LAYOUTS,
        TerminalTextStyle, TextSystem, family_name_sort_key, font_family_stack,
        fontique_script_for_char, glyph_corpus::GLYPH_CORPUS, is_private_use, normalize_locale,
        parse_named_style, push_configured_family_names,
    };
    use crate::config::font::Font;
    use crate::display::color::Rgb;
    use crate::display::content::RenderableCell;

    #[test]
    fn text_systems_reuse_the_thread_font_collection() {
        let before = FONT_CONTEXT_INITIALIZATIONS.with(std::cell::Cell::get);
        let _first = TextSystem::new(Font::default());
        let after_first = FONT_CONTEXT_INITIALIZATIONS.with(std::cell::Cell::get);
        let _second = TextSystem::new(Font::default());
        let after_second = FONT_CONTEXT_INITIALIZATIONS.with(std::cell::Cell::get);

        assert!(after_first == before || after_first == before + 1);
        assert_eq!(after_second, after_first);
    }

    #[test]
    fn font_update_recomputes_metrics() {
        let mut text = TextSystem::new(Font::default());
        let original = text.metrics();
        let updated_font = Font::default().with_size(crate::config::font::FontSize::from_px(22.0));

        text.update_font(updated_font);

        let updated = text.metrics();
        assert!(updated.cell_width >= original.cell_width);
        assert!(updated.cell_height >= original.cell_height);
    }

    #[test]
    fn single_character_layouts_are_reused_without_cloning_the_layout() {
        let mut text = TextSystem::new(Font::default());

        let first = text.shape_string("x", false, false).unwrap();
        let second = text.shape_string("x", false, false).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(text.cache_len(), 1);
    }

    #[test]
    fn repeated_multicharacter_layouts_share_cached_layout() {
        let mut text = TextSystem::new(Font::default());

        let first = text.shape_string("hello", false, false).unwrap();
        let second = text.shape_string("hello", false, false).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(text.cache_len(), 1);
    }

    #[test]
    fn terminal_arabic_is_contextually_shaped_as_one_rtl_run() {
        let mut text = TextSystem::new(Font::default());
        let content = "بب";
        let styles = [
            TerminalTextStyle {
                range: 0..'ب'.len_utf8(),
                flags: Flags::empty(),
                color: Rgb::new(255, 255, 255),
            },
            TerminalTextStyle {
                range: 'ب'.len_utf8()..content.len(),
                flags: Flags::empty(),
                color: Rgb::new(255, 0, 0),
            },
        ];

        let layout = text.shape_terminal_run(content.to_owned(), &styles, true);
        let isolated = text.shape_character('ب', false, false);
        let isolated_glyph = isolated
            .lines()
            .flat_map(|line| line.items())
            .find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => run.glyphs().next().map(|glyph| glyph.id),
                _ => None,
            })
            .expect("isolated Arabic glyph");
        let glyphs = layout
            .lines()
            .flat_map(|line| line.items())
            .filter_map(|item| match item {
                PositionedLayoutItem::GlyphRun(run) => {
                    Some(run.glyphs().map(|glyph| glyph.id).collect::<Vec<_>>())
                },
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();

        assert!(layout.is_rtl());
        assert!(
            glyphs.iter().any(|glyph| *glyph != isolated_glyph),
            "at least one Arabic letter must select a contextual form"
        );
    }

    #[test]
    fn terminal_mixed_direction_text_exposes_visual_rtl_cluster_order() {
        let mut text = TextSystem::new(Font::default());
        let content = "abc אבג";
        let styles = [TerminalTextStyle {
            range: 0..content.len(),
            flags: Flags::empty(),
            color: Rgb::new(255, 255, 255),
        }];

        let layout = text.shape_terminal_run(content.to_owned(), &styles, true);
        let line = layout.lines().next().expect("mixed-direction line");
        let rtl_clusters = line
            .runs()
            .find(|run| run.is_rtl())
            .expect("RTL run")
            .visual_clusters()
            .map(|cluster| cluster.text_range().start)
            .collect::<Vec<_>>();

        assert!(!layout.is_rtl(), "the first strong character establishes an LTR paragraph");
        assert!(rtl_clusters.len() >= 3);
        assert!(rtl_clusters.windows(2).all(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn direct_character_shaping_reuses_cached_layout() {
        let mut text = TextSystem::new(Font::default());

        let first = text.shape_character('x', false, false);
        let second = text.shape_character('x', false, false);

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(text.cache_len(), 1);
    }

    #[test]
    fn foreground_color_does_not_split_shape_cache() {
        let mut text = TextSystem::new(Font::default());
        let white = Rgb::new(255, 255, 255);
        let red = Rgb::new(255, 0, 0);
        let base_cell = RenderableCell {
            character: 'x',
            point: Point::default(),
            fg: white,
            bg: Rgb::default(),
            bg_alpha: 0.0,
            underline: Rgb::default(),
            flags: Flags::empty(),
            extra: None,
        };
        let red_cell = RenderableCell { fg: red, ..base_cell.clone() };

        let first = text.shape_cell(&base_cell).unwrap();
        let second = text.shape_cell(&red_cell).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(text.cache_len(), 1);
    }

    #[test]
    fn tabs_are_not_shaped_as_visible_glyphs() {
        let mut text = TextSystem::new(Font::default());
        let tab_cell = RenderableCell {
            character: '\t',
            point: Point::default(),
            fg: Rgb::new(255, 255, 255),
            bg: Rgb::default(),
            bg_alpha: 0.0,
            underline: Rgb::default(),
            flags: Flags::empty(),
            extra: None,
        };

        assert!(text.shape_cell(&tab_cell).is_none());
        assert_eq!(text.cache_len(), 0);
    }

    /// The cache was cleared only on a font change, so a program printing distinct clusters — an
    /// emoji or combining-mark flood — grew it for as long as the session lived.
    #[test]
    fn the_shape_cache_stays_bounded_under_a_cluster_flood() {
        let mut text = TextSystem::new(Font::default());
        let hot = "hot";
        text.shape_string(hot, false, false).unwrap();

        for index in 0..MAX_CACHED_LAYOUTS * 2 {
            text.shape_string(format!("flood-{index}"), false, false).unwrap();
            // Kept in use throughout, so recency — not insertion order — decides what survives.
            text.shape_string(hot, false, false).unwrap();
            assert!(
                text.cache_len() <= MAX_CACHED_LAYOUTS,
                "cache grew to {} entries",
                text.cache_len()
            );
        }

        assert!(text.cache_len() >= LAYOUT_EVICTION_BATCH, "eviction must not empty the cache");
        let before = text.cache_len();
        let cached = text.shape_string(hot, false, false).unwrap();
        assert_eq!(text.cache_len(), before, "the cluster in constant use survived the flood");
        assert!(Arc::ptr_eq(&cached, &text.shape_string(hot, false, false).unwrap()));
    }

    #[test]
    fn css_family_lists_are_preserved_before_terminal_fallbacks() {
        let mut families = Vec::new();

        push_configured_family_names(&mut families, "'SF Mono', monospace, 'Noto Sans Symbols 2'");

        assert_eq!(
            families,
            vec![
                FontFamilyName::Named(Cow::Owned(String::from("SF Mono"))),
                GenericFamily::Monospace.into(),
                FontFamilyName::Named(Cow::Owned(String::from("Noto Sans Symbols 2"))),
            ]
        );
    }

    #[test]
    fn locale_normalization_strips_encoding_and_uses_bcp47_separators() {
        assert_eq!(normalize_locale("ja_JP.UTF-8"), Some(String::from("ja-JP")));
        assert_eq!(
            normalize_locale("zh_Hans_CN@calendar=gregorian"),
            Some(String::from("zh-Hans-CN"))
        );
        assert_eq!(normalize_locale("C"), None);
    }

    #[test]
    fn character_scripts_are_mapped_to_fontique_tags() {
        assert_eq!(fontique_script_for_char('今'), Some(Script::from_str_unchecked("Hani")));
        assert_eq!(fontique_script_for_char('あ'), Some(Script::from_str_unchecked("Hira")));
        assert_eq!(fontique_script_for_char('한'), Some(Script::from_str_unchecked("Hang")));
        assert_eq!(fontique_script_for_char('!'), None);
        assert_eq!(fontique_script_for_char('\u{f026}'), Some(Script::from_str_unchecked("Zzzz")));
        assert!(is_private_use('\u{f0001}'));
    }

    #[test]
    fn glyph_corpus_shapes_real_glyphs_with_nerd_font_fallback_when_available() {
        let mut text = TextSystem::new(Font::default());
        let nerd_text = GLYPH_CORPUS
            .iter()
            .filter(|entry| entry.requires_nerd_font)
            .map(|entry| entry.codepoint)
            .collect::<String>();
        let family_names = text
            .font_cx
            .collection
            .family_names()
            .filter(|name| name.to_ascii_lowercase().contains("nerd font"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let nerd_family = family_names.into_iter().find(|family_name| {
            let Some(family_id) = text.font_cx.collection.family_id(family_name) else {
                return false;
            };
            text.family_supports_text(family_id, &nerd_text)
        });
        let Some(nerd_family) = nerd_family else {
            eprintln!("skipping Nerd Font PUA corpus: no installed Nerd Font covers every fixture");
            return;
        };

        for entry in GLYPH_CORPUS {
            let content = entry.codepoint.to_string();
            let glyph_id = first_layout_glyph_id(&mut text, &content);
            assert!(
                glyph_id.is_some_and(|glyph_id| glyph_id != 0),
                "missing U+{:04X} group={} expected={} note={} installed_reference={nerd_family}",
                entry.codepoint as u32,
                entry.group,
                entry.expected_family,
                entry.note,
            );
            if entry.requires_nerd_font {
                let selected = selected_family_name(&mut text, &content);
                assert!(
                    selected
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("nerd font")),
                    "U+{:04X} resolved through unexpected family {selected:?}",
                    entry.codepoint as u32,
                );
            }
        }
    }

    #[test]
    fn fallback_checks_are_memoized_and_font_updates_clear_them() {
        let mut text = TextSystem::new(Font::default());
        let character = '\u{f026}';

        // The first PUA shape may seed Fontique and clear the probe set. The second records the
        // now-satisfied check; the third must reuse it without growing the set.
        text.shape_character(character, false, false);
        text.shape_character(character, false, false);
        let checked = text.checked_fallbacks.len();
        text.shape_character(character, false, false);
        assert_eq!(text.checked_fallbacks.len(), checked);
        assert!(checked <= 1);

        text.update_font(Font::default());
        assert!(text.checked_fallbacks.is_empty());
    }

    #[test]
    fn hidden_family_names_sort_after_visible_names() {
        assert!(
            family_name_sort_key("Apple SD Gothic Neo")
                < family_name_sort_key(".Apple SD Gothic Neo")
        );
    }

    #[test]
    fn invalid_css_family_spec_falls_back_to_literal_name() {
        let mut families = Vec::new();

        push_configured_family_names(&mut families, "'broken");

        assert_eq!(families, vec![FontFamilyName::Named(Cow::Owned(String::from("broken")))]);
    }

    #[test]
    fn variant_family_stack_deduplicates_configured_and_generic_fallbacks() {
        let families = font_family_stack(&Font::default(), FontVariant::Bold, &[]);

        assert_eq!(
            families.iter().filter(|family| **family == GenericFamily::Monospace.into()).count(),
            1
        );
        assert!(families.contains(&GenericFamily::UiMonospace.into()));
        assert!(families.contains(&GenericFamily::SystemUi.into()));
        assert!(families.contains(&GenericFamily::Emoji.into()));
        assert!(!families.is_empty());
    }

    #[test]
    fn named_styles_are_case_insensitive_and_preserve_slant() {
        assert_eq!(
            parse_named_style(Some("Semi-Bold Italic"), FontVariant::Normal),
            (parley::FontStyle::Italic, parley::FontWeight::SEMI_BOLD)
        );
        assert_eq!(
            parse_named_style(Some("HEAVY OBLIQUE"), FontVariant::Normal),
            (parley::FontStyle::Oblique(Some(14.0)), parley::FontWeight::BLACK)
        );
    }

    #[test]
    fn unknown_named_style_uses_the_cell_variant_default() {
        assert_eq!(
            parse_named_style(Some("Mystery Face"), FontVariant::BoldItalic),
            (parley::FontStyle::Italic, parley::FontWeight::BOLD)
        );
    }

    #[test]
    fn untracked_han_locale_falls_back_to_script_default_key() {
        let mut text = TextSystem::new(Font::default());
        text.locale = Some(Language::parse("en-US").unwrap());

        let key = text.fallback_key_for_char('今').expect("han fallback key");

        assert_eq!(key, FallbackKey::from(Script::from_str_unchecked("Hani")));
        assert!(key.is_tracked());
        assert!(key.is_default());
    }

    fn selected_run_font(text: &mut TextSystem, content: &str) -> Option<parley::FontData> {
        let layout = text.shape_string(content.to_owned(), false, false)?;
        layout.lines().find_map(|line| {
            line.items().find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(glyph_run) => Some(glyph_run.run().font().clone()),
                _ => None,
            })
        })
    }

    fn selected_family_name(text: &mut TextSystem, content: &str) -> Option<String> {
        let target = selected_run_font(text, content)?;
        let family_names =
            text.font_cx.collection.family_names().map(str::to_owned).collect::<Vec<_>>();
        for family_name in family_names {
            let Some(family_id) = text.font_cx.collection.family_id(&family_name) else {
                continue;
            };
            let Some(family) = text.font_cx.collection.family(family_id) else {
                continue;
            };
            if family.fonts().iter().any(|font| {
                font.index() == target.index
                    && font
                        .load(Some(&mut text.font_cx.source_cache))
                        .is_some_and(|data| data.as_ref() == target.data.as_ref())
            }) {
                return Some(family.name().to_owned());
            }
        }
        None
    }

    fn family_glyph_id_for(
        text: &mut TextSystem,
        family_name: &str,
        character: char,
    ) -> Option<u32> {
        let family_id = text.font_cx.collection.family_id(family_name)?;
        let family = text.font_cx.collection.family(family_id)?;
        family.fonts().iter().find_map(|font| {
            let data = font.load(Some(&mut text.font_cx.source_cache))?;
            let charmap = font.charmap_index().charmap(data.as_ref())?;
            charmap.map(character)
        })
    }

    fn first_layout_glyph_id(text: &mut TextSystem, content: &str) -> Option<u32> {
        let layout = text.shape_string(content.to_owned(), false, false)?;
        layout.lines().find_map(|line| {
            line.items().find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(glyph_run) => {
                    glyph_run.glyphs().next().map(|glyph| glyph.id)
                },
                _ => None,
            })
        })
    }

    #[test]
    #[ignore = "diagnostic helper"]
    fn debug_cjk_sample_glyphs() {
        let mut text = TextSystem::new(Font::default());
        for character in "今語漢あア한글".chars() {
            let family = selected_family_name(&mut text, &character.to_string());
            let layout = first_layout_glyph_id(&mut text, &character.to_string());
            let family_glyph = family
                .as_deref()
                .and_then(|family_name| family_glyph_id_for(&mut text, family_name, character));
            let square = family
                .as_deref()
                .and_then(|family_name| family_glyph_id_for(&mut text, family_name, '□'));
            println!(
                "char={character:?} family={family:?} layout={layout:?} \
                 family_glyph={family_glyph:?} square={square:?}"
            );
        }
    }
}
