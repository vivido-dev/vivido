//! Kitty graphics placements as the quads the media compositor draws.
//!
//! The terminal owns the protocol state; this module turns what is visible of it into window
//! pixels once per change. A [`KittyFrame`] is rebuilt only when the graphics state, the scroll
//! position or the geometry moved, and for placeholder images also when the grid changed, so a
//! frame in which nothing about the images changed costs one comparison.
//!
//! Stacking follows the protocol's z-index, mapped onto the compositor's three layers: far
//! negative values go under cell backgrounds, other negative values between backgrounds and text,
//! and everything else over the text.

// Rust guideline compliant 2026-10-07

use std::sync::Arc;

use crate::display::SizeInfo;
use crate::terminal::graphics::{self, CellPixels};
use crate::terminal::grid::Dimensions;
use crate::terminal::index::{Column, Line};
use crate::terminal::term::Term;

/// Most quads one frame draws.
///
/// The stores admit 8192 placements and a screen of placeholder cells can add thousands of runs.
/// The compositor sizes one vertex buffer for all of them, and nothing a person could tell apart
/// needs more than this; what is dropped is the top of the stack.
pub(crate) const MAX_QUADS: usize = 4096;

/// Z-index below which an image goes under cell backgrounds, as the protocol defines it.
const UNDER_BACKGROUND: i32 = i32::MIN / 2;

/// Z-index Kitty gives an image shown through placeholder cells: under the text of its cells.
const PLACEHOLDER_Z: i32 = -1;

/// One textured rectangle.
#[derive(Clone)]
pub struct Quad {
    /// Identity of the image's pixels; the key textures are cached under.
    pub uid: u64,
    /// Straight-alpha RGBA of the whole image.
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub opaque: bool,
    /// Compositor layer: 0 under backgrounds, 1 under text, 2 over text.
    pub layer: usize,
    /// Left, top, right and bottom in window pixels, already clipped to the grid.
    pub dest: [f32; 4],
    /// Left, top, right and bottom of the source in texture coordinates.
    pub uv: [f32; 4],
    z: i32,
    /// Creation order, which breaks ties in `z`.
    order: u64,
}

/// Leaves the pixels out, which are megabytes and say nothing a debug dump needs.
impl std::fmt::Debug for Quad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Quad")
            .field("uid", &self.uid)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("layer", &self.layer)
            .field("dest", &self.dest)
            .field("uv", &self.uv)
            .field("z", &self.z)
            .finish_non_exhaustive()
    }
}

/// Everything that decides what a frame looks like, apart from the grid's text.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Key {
    revision: u64,
    screen_epoch: u64,
    display_offset: usize,
    size: SizeInfo,
}

/// The quads of the current frame.
#[derive(Debug, Default)]
pub struct KittyFrame {
    quads: Vec<Quad>,
    key: Option<Key>,
    /// Changes whenever `quads` is rebuilt.
    generation: u64,
}

impl KittyFrame {
    /// Quads in drawing order: by layer, then z-index, then age.
    pub(crate) fn quads(&self) -> &[Quad] {
        &self.quads
    }

    /// Counter the compositor compares to know whether it has drawn this frame already.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Bring the frame up to date with `term`, returning whether it changed.
    ///
    /// `grid_changed` says the grid's text may differ from the last call. It matters only while
    /// virtual placements exist, because only then can text cells stand for images.
    pub(crate) fn update<T>(
        &mut self,
        term: &Term<T>,
        size: &SizeInfo,
        display_offset: usize,
        grid_changed: bool,
    ) -> bool {
        let graphics = term.graphics();
        let store = graphics.store();
        if !store.has_placements() && !store.has_virtual() {
            self.key = None;
            if self.quads.is_empty() {
                return false;
            }
            self.quads.clear();
            self.generation = self.generation.wrapping_add(1);
            return true;
        }
        let key = Key {
            revision: graphics.revision(),
            screen_epoch: graphics.screen_epoch(),
            display_offset,
            size: *size,
        };
        if self.key == Some(key) && !(grid_changed && store.has_virtual()) {
            return false;
        }
        // A synchronized erase-and-replace has already changed the store when the PTY reader
        // drops the terminal lock to decode. Keep the previous image frame until every placed
        // image has pixels, while text, Vivid media and the rest of the UI can still draw.
        // Screen switches and resets must never retain another screen lifetime's images.
        if self.key.is_some_and(|previous| previous.screen_epoch == key.screen_epoch)
            && store.placements().iter().any(|placement| {
                store.image(placement.image).is_some_and(|image| image.pixels.is_none())
            })
        {
            return false;
        }
        self.key = Some(key);

        self.quads.clear();
        let view = View::new(size, display_offset);
        for placement in store.placements().iter().filter(|placement| !placement.is_virtual) {
            let Some(image) = store.image(placement.image) else {
                continue;
            };
            let Some(pixels) = &image.pixels else {
                continue;
            };
            let (width, height) = placement.dest_size(graphics.cell());
            let left = view.column(placement.col) + placement.cell_x as f32;
            let top = view.line(placement.row) + placement.cell_y as f32;
            let source = [placement.src_x, placement.src_y, placement.src_w, placement.src_h];
            let Some((dest, uv)) = view.clip(
                [left, top, left + width, top + height],
                source,
                (image.width, image.height),
            ) else {
                continue;
            };
            self.quads.push(Quad {
                uid: image.uid,
                pixels: Arc::clone(pixels),
                width: image.width,
                height: image.height,
                opaque: image.opaque,
                layer: layer(placement.z),
                dest,
                uv,
                z: placement.z,
                order: placement.key,
            });
        }
        if store.has_virtual() {
            self.placeholders(term, &view, graphics.cell());
        }
        // Stable, so placeholder runs keep the left-to-right order they were found in.
        self.quads.sort_by_key(|quad| (quad.layer, quad.z, quad.order));
        self.quads.truncate(MAX_QUADS);
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Add a quad for every run of placeholder cells in the viewport.
    fn placeholders<T>(&mut self, term: &Term<T>, view: &View, cell: CellPixels) {
        let store = term.graphics().store();
        let grid = term.grid();
        let offset = i32::try_from(view.display_offset).unwrap_or(i32::MAX);
        let quads = &mut self.quads;
        for viewport_line in 0..grid.screen_lines() {
            let line = Line(viewport_line as i32 - offset);
            if line < grid.topmost_line() {
                continue;
            }
            let row = &grid[line];
            let cells =
                (0..grid.columns()).map(|column| graphics::decode_cell(&row[Column(column)]));
            graphics::runs(cells, |run| {
                let Some((image, placement)) = store.virtual_placement(run.image, run.placement)
                else {
                    return;
                };
                let Some(pixels) = &image.pixels else {
                    return;
                };
                let size = (image.width, image.height);
                let Some(slice) =
                    graphics::slice(size, (placement.cols, placement.rows), &run, cell)
                else {
                    return;
                };
                let left =
                    view.column(i32::try_from(run.column).unwrap_or(i32::MAX)) + slice.dest_x;
                let top = view.line(line.0) + slice.dest_y;
                let source = [slice.src_x, slice.src_y, slice.src_w, slice.src_h];
                let dest = [left, top, left + slice.dest_w, top + slice.dest_h];
                if let Some((dest, uv)) = view.clip(dest, source, size) {
                    quads.push(Quad {
                        uid: image.uid,
                        pixels: Arc::clone(pixels),
                        width: image.width,
                        height: image.height,
                        opaque: image.opaque,
                        layer: layer(PLACEHOLDER_Z),
                        dest,
                        uv,
                        z: PLACEHOLDER_Z,
                        // After every real placement of the same z, in the order found.
                        order: u64::MAX,
                    });
                }
            });
        }
    }
}

/// Compositor layer for a z-index.
fn layer(z: i32) -> usize {
    if z < UNDER_BACKGROUND {
        0
    } else if z < 0 {
        1
    } else {
        2
    }
}

/// The grid's place in the window.
struct View {
    cell_width: f32,
    cell_height: f32,
    /// Left, top, right and bottom of the cell area in window pixels.
    grid: [f32; 4],
    display_offset: usize,
}

impl View {
    fn new(size: &SizeInfo, display_offset: usize) -> Self {
        let (left, top) = (size.padding_x(), size.padding_y());
        Self {
            cell_width: size.cell_width(),
            cell_height: size.cell_height(),
            grid: [
                left,
                top,
                left + size.columns() as f32 * size.cell_width(),
                top + size.screen_lines() as f32 * size.cell_height(),
            ],
            display_offset,
        }
    }

    /// Window x of the left edge of a column.
    fn column(&self, column: i32) -> f32 {
        self.grid[0] + column as f32 * self.cell_width
    }

    /// Window y of the top edge of a grid line, which scrollback moves down the screen.
    fn line(&self, line: i32) -> f32 {
        self.grid[1] + (line as f32 + self.display_offset as f32) * self.cell_height
    }

    /// Clip `dest` to the cell area and cut `source` down to match.
    ///
    /// `dest` is left, top, right, bottom in window pixels; `source` is x, y, width, height in
    /// image pixels. Returns the clipped destination and the source as texture coordinates, or
    /// `None` when nothing is left to draw.
    fn clip(
        &self,
        dest: [f32; 4],
        source: [f32; 4],
        (width, height): (u32, u32),
    ) -> Option<([f32; 4], [f32; 4])> {
        let [left, top, right, bottom] = dest;
        let [src_x, src_y, src_w, src_h] = source;
        // The negated comparisons also reject a rectangle with a NaN in it.
        if !(right > left && bottom > top && src_w > 0. && src_h > 0.) || width == 0 || height == 0
        {
            return None;
        }
        let clipped = [
            left.max(self.grid[0]),
            top.max(self.grid[1]),
            right.min(self.grid[2]),
            bottom.min(self.grid[3]),
        ];
        if !(clipped[2] > clipped[0] && clipped[3] > clipped[1]) {
            return None;
        }
        // Source pixels per destination pixel.
        let (scale_x, scale_y) = (src_w / (right - left), src_h / (bottom - top));
        let (width, height) = (width as f32, height as f32);
        let uv = [
            (src_x + (clipped[0] - left) * scale_x) / width,
            (src_y + (clipped[1] - top) * scale_y) / height,
            (src_x + (clipped[2] - left) * scale_x) / width,
            (src_y + (clipped[3] - top) * scale_y) / height,
        ];
        Some((clipped, uv))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::event::VoidListener;
    use crate::terminal::term::Config;
    use crate::terminal::vvte::ansi;

    /// A 20x10 grid of 10x20 cells with a 5-pixel margin on every side.
    fn size() -> SizeInfo {
        SizeInfo::new(210., 210., 10., 20., 5., 5., false)
    }

    fn term() -> (Term<VoidListener>, ansi::Processor) {
        (Term::new(Config::default(), &size(), VoidListener), ansi::Processor::new())
    }

    fn feed(term: &mut Term<VoidListener>, parser: &mut ansi::Processor, bytes: &str) {
        term.advance(parser, bytes.as_bytes());
    }

    /// A transmit-and-display command for a solid `width`x`height` image.
    fn image(width: u32, height: u32, keys: &str) -> String {
        use base64::Engine;
        let pixels = [10, 20, 30, 255].repeat((width * height) as usize);
        let data = base64::engine::general_purpose::STANDARD.encode(pixels);
        format!("\x1b_Ga=T,f=32,s={width},v={height},q=2{keys};{data}\x1b\\")
    }

    /// A chunked zlib RGB replacement, as kitdoom transmits it.
    fn compressed_image(keys: &str) -> String {
        use std::io::Write;

        use base64::Engine;
        use flate2::{Compression, write::ZlibEncoder};

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&[70, 80, 90].repeat(20 * 40)).unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(encoder.finish().unwrap());
        format!(
            "\x1b_Ga=T,f=24,o=z,s=20,v=40,C=1,q=2{keys},m=1;{}\x1b\\\x1b_Gq=2,m=0;{}\x1b\\",
            &data[..4],
            &data[4..],
        )
    }

    fn frame(term: &Term<VoidListener>, display_offset: usize) -> KittyFrame {
        let mut frame = KittyFrame::default();
        frame.update(term, &size(), display_offset, true);
        frame
    }

    /// kitdoom clears and replaces an anonymous RGB image inside each synchronized update.
    /// The PTY reader releases the terminal while decoding its zlib payload, allowing a draw
    /// between parsing the erase and installing the new pixels.
    #[test]
    fn a_synchronized_compressed_replacement_keeps_the_previous_frame_until_decoded() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, "\x1b[?1049h");
        feed(&mut term, &mut parser, &image(20, 40, ",C=1"));
        let mut frame = frame(&term, 0);
        let previous = frame.quads()[0].clone();
        let generation = frame.generation();

        let update = format!("\x1b[?2026h\x1b[H\x1b[2J{}\x1b[?2026l", compressed_image(""));
        term.set_deferred_graphics_decode(true);
        // Include a split inside the final APC terminator, as PTY reads can end anywhere.
        let split = update.len() - "\\\x1b[?2026l".len();
        feed(&mut term, &mut parser, &update[..split]);
        assert!(!frame.update(&term, &size(), 0, true));
        feed(&mut term, &mut parser, &update[split..]);
        assert!(term.has_graphics_jobs());
        let jobs = term.take_graphics_jobs();
        assert!(!term.has_graphics_jobs(), "the decode is now outside the terminal lock");

        assert!(!frame.update(&term, &size(), 0, true), "a draw during decode keeps the old image");
        assert_eq!(frame.generation(), generation);
        assert_eq!(frame.quads()[0].uid, previous.uid);
        assert!(Arc::ptr_eq(&frame.quads()[0].pixels, &previous.pixels));

        term.finish_graphics_jobs(jobs.into_iter().map(|job| job.run()).collect());
        assert!(frame.update(&term, &size(), 0, true));
        assert_eq!(frame.quads().len(), 1);
        assert_ne!(frame.quads()[0].uid, previous.uid);
        assert_eq!(&frame.quads()[0].pixels[..4], &[70, 80, 90, 255]);
        assert_eq!(frame.generation(), generation + 1);
    }

    #[test]
    fn a_pending_image_never_retains_quads_from_a_previous_screen_lifetime() {
        for (enter, switch) in
            [("", "\x1b[?1049h"), ("\x1b[?1049h", "\x1b[?1049l\x1b[?1049h"), ("", "\x1bc")]
        {
            let (mut term, mut parser) = term();
            feed(&mut term, &mut parser, &image(20, 40, ",i=1,C=1"));
            let primary = frame(&term, 0).quads()[0].clone();
            if !enter.is_empty() {
                feed(&mut term, &mut parser, enter);
                feed(&mut term, &mut parser, &image(20, 40, ",i=1,C=1"));
            }
            let mut frame = frame(&term, 0);
            term.set_deferred_graphics_decode(true);
            feed(&mut term, &mut parser, &format!("{switch}{}", compressed_image(",i=1")));
            let jobs = term.take_graphics_jobs();

            assert!(frame.update(&term, &size(), 0, true), "{switch:?}");
            assert!(frame.quads().is_empty(), "another screen's images must disappear at once");
            term.finish_graphics_jobs(jobs.into_iter().map(|job| job.run()).collect());
            assert!(frame.update(&term, &size(), 0, true));
            assert_eq!(frame.quads().len(), 1);
            assert_eq!(&frame.quads()[0].pixels[..4], &[70, 80, 90, 255]);

            if switch != "\x1bc" {
                feed(&mut term, &mut parser, "\x1b[?1049l");
                assert!(frame.update(&term, &size(), 0, true));
                assert_eq!(frame.quads()[0].uid, primary.uid);
                assert!(Arc::ptr_eq(&frame.quads()[0].pixels, &primary.pixels));
            }
        }
    }

    #[test]
    fn a_failed_replacement_decode_releases_the_retained_frame() {
        use base64::Engine;

        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, "\x1b[?1049h");
        feed(&mut term, &mut parser, &image(20, 40, ",C=1"));
        let mut frame = frame(&term, 0);
        term.set_deferred_graphics_decode(true);
        let data = base64::engine::general_purpose::STANDARD.encode(b"invalid zlib");
        feed(
            &mut term,
            &mut parser,
            &format!("\x1b[2J\x1b_Ga=T,f=24,o=z,s=20,v=40,C=1,q=2;{data}\x1b\\"),
        );
        let jobs = term.take_graphics_jobs();
        assert!(!jobs.is_empty());
        assert!(!frame.update(&term, &size(), 0, true));
        assert_eq!(frame.quads().len(), 1);

        term.finish_graphics_jobs(jobs.into_iter().map(|job| job.run()).collect());
        assert!(frame.update(&term, &size(), 0, true));
        assert!(frame.quads().is_empty(), "an invalid image must not freeze the old frame");
        feed(&mut term, &mut parser, &image(20, 40, ",C=1"));
        assert!(frame.update(&term, &size(), 0, true));
        assert_eq!(frame.quads().len(), 1);
    }

    #[test]
    fn the_terminal_learns_the_cell_size_from_the_display() {
        assert_eq!(term().0.graphics().cell(), CellPixels::new(10, 20));
    }

    #[test]
    fn a_placement_is_drawn_at_its_cell_in_window_pixels() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &format!("\x1b[3;4H{}", image(25, 30, ",C=1")));
        let frame = frame(&term, 0);
        let [quad] = frame.quads() else {
            panic!("one placement, one quad");
        };
        // Column 3, line 2: 5 + 30 across, 5 + 40 down, at the image's own size.
        assert_eq!(quad.dest, [35., 45., 60., 75.]);
        assert_eq!(quad.uv, [0., 0., 1., 1.]);
        assert_eq!((quad.width, quad.height, quad.layer, quad.opaque), (25, 30, 2, true));
        assert_eq!(quad.pixels.len(), 25 * 30 * 4);
    }

    #[test]
    fn cell_offsets_scaling_and_source_rectangles_shape_the_quad() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &image(20, 40, ",i=1,C=1,X=3,Y=4"));
        feed(&mut term, &mut parser, "\x1b[2;1H\x1b_Ga=p,i=1,C=1,c=4,r=1,q=2\x1b\\");
        feed(&mut term, &mut parser, "\x1b[4;1H\x1b_Ga=p,i=1,C=1,x=10,y=20,w=10,h=20,q=2\x1b\\");
        let frame = frame(&term, 0);
        let quads: Vec<_> = frame.quads().iter().map(|quad| (quad.dest, quad.uv)).collect();
        assert_eq!(
            quads,
            [
                ([8., 9., 28., 49.], [0., 0., 1., 1.]),
                // Stretched over four columns and one row of line 1.
                ([5., 25., 45., 45.], [0., 0., 1., 1.]),
                // The bottom-right quarter of the image, at its natural size on line 3.
                ([5., 65., 15., 85.], [0.5, 0.5, 1., 1.]),
            ]
        );
    }

    #[test]
    fn z_index_selects_the_layer_and_orders_within_it() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &image(10, 20, ",i=1,C=1,z=5"));
        for z in [0, -1, -1_073_741_824, -1_073_741_825, i32::MIN, 3] {
            feed(
                &mut term,
                &mut parser,
                &format!("\x1b_Ga=p,i=1,C=1,z={z},p={},q=2\x1b\\", z.unsigned_abs()),
            );
        }
        let order: Vec<_> =
            frame(&term, 0).quads().iter().map(|quad| (quad.layer, quad.z)).collect();
        assert_eq!(
            order,
            [
                (0, i32::MIN),
                (0, -1_073_741_825),
                (1, -1_073_741_824),
                (1, -1),
                (2, 0),
                (2, 3),
                (2, 5),
            ]
        );
    }

    #[test]
    fn a_placement_is_clipped_to_the_grid_and_its_texture_coordinates_follow() {
        let (mut term, mut parser) = term();
        // Bottom-right cell: only 10x20 of the 40x40 image is inside the grid.
        feed(&mut term, &mut parser, &format!("\x1b[10;20H{}", image(40, 40, ",C=1")));
        let frame = frame(&term, 0);
        let quad = &frame.quads()[0];
        assert_eq!(quad.dest, [195., 185., 205., 205.]);
        assert_eq!(quad.uv, [0., 0., 0.25, 0.5]);
    }

    #[test]
    fn scrolled_back_placements_follow_the_view_and_vanish_when_out_of_it() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &image(10, 40, ",C=1"));
        // Push the image three lines into scrollback; one of its two rows is still on line -2.
        feed(&mut term, &mut parser, &"\n".repeat(12));
        let at = |offset| frame(&term, offset).quads().first().map(|quad| (quad.dest, quad.uv));
        assert_eq!(at(0), None, "wholly above the viewport");
        assert_eq!(at(2), Some(([5., 5., 15., 25.], [0., 0.5, 1., 1.])), "its lower half shows");
        assert_eq!(at(3), Some(([5., 5., 15., 45.], [0., 0., 1., 1.])));
    }

    #[test]
    fn the_frame_is_rebuilt_only_when_something_it_depends_on_changed() {
        let (mut term, mut parser) = term();
        let mut frame = KittyFrame::default();
        assert!(!frame.update(&term, &size(), 0, true), "no images, nothing to build");
        feed(&mut term, &mut parser, &image(10, 20, ",C=1"));
        assert!(frame.update(&term, &size(), 0, false));
        let generation = frame.generation();
        assert!(!frame.update(&term, &size(), 0, false));
        assert!(!frame.update(&term, &size(), 0, true), "text changes do not move real placements");
        assert_eq!(frame.generation(), generation);
        assert!(frame.update(&term, &size(), 1, false), "the view scrolled");
        let wider = SizeInfo::new(310., 210., 10., 20., 5., 5., false);
        assert!(frame.update(&term, &wider, 1, false), "the window changed");
        feed(&mut term, &mut parser, "\x1b_Ga=d\x1b\\");
        assert!(frame.update(&term, &wider, 1, false), "the image went away");
        assert!(frame.quads().is_empty());
        assert!(!frame.update(&term, &wider, 1, false));
    }

    /// The bytes kitim prints under a multiplexer: a virtual placement, then placeholder cells
    /// carrying the image id in their color and the row, column and id byte as combining marks.
    fn placeholders(id: u32, cols: u32, rows: u32) -> String {
        // The first three protocol diacritics, standing for 0, 1 and 2.
        const MARKS: [char; 3] = ['\u{0305}', '\u{030D}', '\u{030E}'];
        let color = format!("\x1b[38:2:{}:{}:{}m", (id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
        let mut text = String::new();
        for row in 0..rows {
            text.push_str(&color);
            for col in 0..cols {
                text.push(graphics::PLACEHOLDER);
                text.push(MARKS[row as usize]);
                text.push(MARKS[col as usize]);
                text.push(MARKS[(id >> 24) as usize]);
            }
            text.push_str("\x1b[39m\r\n");
        }
        text
    }

    #[test]
    fn placeholder_cells_show_the_part_of_the_image_they_name() {
        let (mut term, mut parser) = term();
        let id = 0x0201_0203;
        feed(&mut term, &mut parser, &image(30, 40, &format!(",i={id},U=1,c=3,r=2")));
        assert!(frame(&term, 0).quads().is_empty(), "a virtual placement shows nothing by itself");

        feed(&mut term, &mut parser, &format!("\x1b[2;1H{}", placeholders(id, 3, 2)));
        let frame = frame(&term, 0);
        let quads: Vec<_> =
            frame.quads().iter().map(|quad| (quad.dest, quad.uv, quad.layer)).collect();
        assert_eq!(
            quads,
            [
                ([5., 25., 35., 45.], [0., 0., 1., 0.5], 1),
                ([5., 45., 35., 65.], [0., 0.5, 1., 1.], 1),
            ]
        );
    }

    #[test]
    fn placeholder_images_move_with_their_text_and_go_when_it_is_erased() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &image(10, 20, ",i=7,U=1,c=1,r=1"));
        feed(
            &mut term,
            &mut parser,
            &format!("\x1b[38;5;7m{}\u{0305}\u{0305}\x1b[39m", graphics::PLACEHOLDER),
        );
        let mut frame = KittyFrame::default();
        assert!(frame.update(&term, &size(), 0, true));
        assert_eq!(frame.quads()[0].dest, [5., 5., 15., 25.]);

        // Insert three blanks before it: plain text editing, no graphics command.
        feed(&mut term, &mut parser, "\x1b[1;1H\x1b[3@");
        assert!(
            !frame.update(&term, &size(), 0, false),
            "without grid changes nothing is rescanned"
        );
        assert!(frame.update(&term, &size(), 0, true));
        assert_eq!(frame.quads()[0].dest, [35., 5., 45., 25.]);

        feed(&mut term, &mut parser, "\x1b[2K");
        assert!(frame.update(&term, &size(), 0, true));
        assert!(frame.quads().is_empty());
    }

    #[test]
    fn placeholders_naming_no_known_image_are_skipped() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &image(10, 20, ",i=7,U=1,c=1,r=1"));
        feed(&mut term, &mut parser, &format!("\x1b[38;5;8m{}\x1b[39m", graphics::PLACEHOLDER));
        feed(&mut term, &mut parser, &format!("{}", graphics::PLACEHOLDER));
        assert!(frame(&term, 0).quads().is_empty());
    }

    #[test]
    fn degenerate_rectangles_draw_nothing() {
        let view = View::new(&size(), 0);
        let whole = [0., 0., 10., 10.];
        assert!(view.clip([10., 10., 10., 30.], whole, (10, 10)).is_none(), "no width");
        assert!(
            view.clip([10., 10., 20., 20.], [0., 0., 0., 10.], (10, 10)).is_none(),
            "no source"
        );
        assert!(view.clip([10., 10., 20., 20.], whole, (0, 10)).is_none(), "no image");
        assert!(view.clip([f32::NAN, 10., 20., 20.], whole, (10, 10)).is_none());
        assert!(view.clip([300., 10., 320., 20.], whole, (10, 10)).is_none(), "outside the grid");
        assert!(view.clip([10., 10., 20., 20.], whole, (10, 10)).is_some());
    }

    #[test]
    fn the_number_of_quads_is_bounded() {
        let (mut term, mut parser) = term();
        feed(&mut term, &mut parser, &image(10, 20, ",i=1,C=1,z=1"));
        // Translucent copies would stack; an opaque image at one spot collapses, so move along.
        for index in 0..MAX_QUADS + 100 {
            let (line, column) = (index / 20 % 10 + 1, index % 20 + 1);
            feed(
                &mut term,
                &mut parser,
                &format!("\x1b[{line};{column}H\x1b_Ga=p,i=1,C=1,z={index},q=2\x1b\\"),
            );
        }
        assert_eq!(frame(&term, 0).quads().len(), MAX_QUADS);
    }
}
