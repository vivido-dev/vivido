//! Images and their placements on one terminal screen.
//!
//! An [`ImageStore`] is the protocol's object model for a single screen: transmitted images, the
//! placements that show them, and the rules by which both go away. The primary and the alternate
//! screen each own one, so nothing addressed on one screen can reach the other.
//!
//! The behavior follows `kitty/graphics.c`. Where this differs it says so; the one deliberate
//! addition is [`ImageStore::put`]'s replacement of a placement that an identical opaque one has
//! made permanently invisible.

// Rust guideline compliant 2026-10-07

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::command::Command;
use super::load::MAX_DIMENSION;

/// Decoded bytes one screen may hold before its oldest images are evicted.
///
/// Kitty's default storage limit, per screen as there.
pub(crate) const STORAGE_QUOTA: usize = 320 * 1024 * 1024;

/// Most images one screen may hold.
///
/// The byte quota alone would admit eighty million one-pixel images. This keeps the bookkeeping
/// bounded as well; no ordinary program comes near it.
pub(crate) const MAX_IMAGES: usize = 4096;

/// Most placements one screen may hold, real and virtual together.
pub(crate) const MAX_PLACEMENTS: usize = 8192;

/// Most columns or rows one placement may span.
///
/// The largest image at a one-pixel cell. It also keeps every row and column sum inside `i32`.
const MAX_SPAN: u32 = MAX_DIMENSION;

/// Source of [`Image::uid`].
///
/// Process-wide rather than per terminal: the renderer's texture cache outlives a restarted
/// terminal, and a counter that started over would hand a new image the identity of a texture
/// still cached from the old one.
static NEXT_UID: AtomicU64 = AtomicU64::new(1);

/// Physical-pixel size of one terminal cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellPixels {
    /// Width of one cell.
    pub width: u32,
    /// Height of one cell.
    pub height: u32,
}

impl CellPixels {
    /// Cell size used until the display reports the real one.
    ///
    /// Only headless tests ever see it; it matches what `kitim` itself assumes for a terminal
    /// that reports no pixel size.
    pub(crate) const FALLBACK: Self = Self { width: 10, height: 20 };

    /// Create a cell size, raising a zero dimension to one pixel.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self { width: width.max(1), height: height.max(1) }
    }
}

/// A transmitted image.
pub(crate) struct Image {
    /// Identity of this exact pixel payload, never reused within the process.
    pub uid: u64,
    /// Id the application chose or was assigned, zero for none.
    pub id: u32,
    /// Number the application chose, zero for none.
    pub number: u32,
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA, absent while the image is still being decoded.
    pub pixels: Option<Arc<Vec<u8>>>,
    /// Whether every pixel has full alpha. False while the pixels are absent.
    pub opaque: bool,
    /// Placements showing this image, real and virtual.
    refs: u32,
    /// When the image was last transmitted or placed, on the store's own clock.
    used: u64,
    /// Scratch mark for the delete that is currently running.
    matched: bool,
}

/// Leaves the pixels out: an image is megabytes, and a debug dump of one helps nobody.
impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("uid", &self.uid)
            .field("id", &self.id)
            .field("number", &self.number)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("decoded", &self.pixels.is_some())
            .field("opaque", &self.opaque)
            .field("refs", &self.refs)
            .finish_non_exhaustive()
    }
}

impl Image {
    /// Bytes this image counts for against [`STORAGE_QUOTA`].
    fn bytes(&self) -> usize {
        // Both dimensions are at most `MAX_DIMENSION`.
        self.width as usize * self.height as usize * 4
    }
}

/// One appearance of an image.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Placement {
    /// Creation order within the store; later placements draw over earlier ones of equal `z`.
    pub key: u64,
    /// Key of the image shown.
    pub image: u64,
    /// Id the application chose, zero for none.
    pub id: u32,
    /// Grid line of the first cell; negative lines are in scrollback.
    pub row: i32,
    pub col: i32,
    /// Source rectangle in image pixels. Fractional once a scroll region has clipped it.
    pub src_x: f32,
    pub src_y: f32,
    pub src_w: f32,
    pub src_h: f32,
    /// Pixel offset of the image inside its first cell.
    pub cell_x: u32,
    pub cell_y: u32,
    /// Requested extent in cells, zero where it follows from the image.
    pub cols: u32,
    pub rows: u32,
    /// Cells actually covered, which is what scrolling and deleting reason about.
    pub eff_cols: u32,
    pub eff_rows: u32,
    pub z: i32,
    /// A prototype for Unicode placeholders: it has no position and is never drawn itself.
    pub is_virtual: bool,
}

impl Placement {
    /// Size in pixels of the rectangle the placement is drawn into.
    ///
    /// Mirrors `grman_update_layers`: an explicit column or row count stretches to the far cell
    /// boundary, a missing one follows from the source rectangle's aspect ratio, and with neither
    /// the image is drawn at its natural size.
    pub(crate) fn dest_size(&self, cell: CellPixels) -> (f32, f32) {
        let width = |cols: u32| (cols * cell.width) as f32 - self.cell_x as f32;
        let height = |rows: u32| (rows * cell.height) as f32 - self.cell_y as f32;
        if self.src_w <= 0. || self.src_h <= 0. {
            return (0., 0.);
        }
        if self.rows != 0 {
            let h = height(self.rows);
            let w = if self.cols != 0 { width(self.cols) } else { h * self.src_w / self.src_h };
            (w, h)
        } else {
            let w = if self.cols != 0 { width(self.cols) } else { self.src_w };
            (w, w * self.src_h / self.src_w)
        }
    }

    /// Derive the covered cells from the requested extent, as `update_dest_rect` does.
    fn fit(&mut self, cell: CellPixels) {
        let cells = |pixels: f32, size: u32| (pixels / size as f32).ceil().max(1.) as u32;
        let (mut cols, mut rows) = (self.cols, self.rows);
        match (cols, rows) {
            (0, 0) => {
                cols = cells(self.src_w + self.cell_x as f32, cell.width);
                rows = cells(self.src_h + self.cell_y as f32, cell.height);
            },
            (0, _) if self.src_h > 0. => {
                let height = (rows * cell.height + self.cell_y) as f32;
                cols = cells(height * self.src_w / self.src_h, cell.width);
            },
            (_, 0) if self.src_w > 0. => {
                let width = (cols * cell.width + self.cell_x) as f32;
                rows = cells(width * self.src_h / self.src_w, cell.height);
            },
            _ => {},
        }
        self.eff_cols = cols.clamp(1, MAX_SPAN);
        self.eff_rows = rows.clamp(1, MAX_SPAN);
    }

    /// One past the last grid line the placement touches.
    fn end_row(&self) -> i32 {
        self.row.saturating_add(self.eff_rows as i32)
    }

    fn end_col(&self) -> i32 {
        self.col.saturating_add(self.eff_cols as i32)
    }

    /// Source pixels per screen pixel, vertically.
    fn vertical_scale(&self, cell: CellPixels) -> f32 {
        let (_, height) = self.dest_size(cell);
        if (self.rows == 0 && self.cols == 0) || height <= 0. { 1. } else { self.src_h / height }
    }

    /// Whether a placement drawn after `self` with `other`'s geometry covers exactly its pixels.
    fn same_geometry(&self, other: &Self, cell: CellPixels) -> bool {
        self.row == other.row
            && self.col == other.col
            && self.cell_x == other.cell_x
            && self.cell_y == other.cell_y
            && self.eff_cols == other.eff_cols
            && self.eff_rows == other.eff_rows
            && self.z == other.z
            && self.dest_size(cell) == other.dest_size(cell)
    }
}

/// A scroll of grid lines that placements must follow.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scroll {
    /// First line of the scrolled region.
    pub top: i32,
    /// One past the last line of the scrolled region.
    pub end: i32,
    /// Lines the content moved up by; negative when it moved down.
    pub lines: i32,
    /// Lines on the screen.
    pub screen_lines: i32,
    /// Topmost line that exists after the scroll: zero, or negative with scrollback.
    pub history_top: i32,
}

/// Which images an operation may free once it has removed placements.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reap {
    /// Images left with no placement and no id, which nothing can ever show again.
    Unaddressable,
    /// As [`Self::Unaddressable`], limited to images the operation removed a placement of.
    MatchedUnaddressable,
    /// Every image the operation removed the last placement of.
    Matched,
    /// Every image left with no placement.
    Unreferenced,
}

/// Images and placements of one screen.
#[derive(Debug, Default)]
pub(crate) struct ImageStore {
    images: BTreeMap<u64, Image>,
    /// Image key by application-visible id.
    ids: BTreeMap<u32, u64>,
    /// Placements in creation order.
    placements: Vec<Placement>,
    /// Last key handed out, shared by images and placements.
    last_key: u64,
    /// Monotonic stamp for [`Image::used`].
    clock: u64,
    /// Sum of [`Image::bytes`].
    stored: usize,
    virtual_count: usize,
}

impl ImageStore {
    /// Whether any placement exists that scrolling or drawing has to consider.
    pub(crate) fn has_placements(&self) -> bool {
        self.placements.len() > self.virtual_count
    }

    /// Whether any virtual placement exists, i.e. whether placeholder cells can mean anything.
    pub(crate) fn has_virtual(&self) -> bool {
        self.virtual_count > 0
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub(crate) fn placements(&self) -> &[Placement] {
        &self.placements
    }

    pub(crate) fn image(&self, key: u64) -> Option<&Image> {
        self.images.get(&key)
    }

    pub(crate) fn key_by_id(&self, id: u32) -> Option<u64> {
        self.ids.get(&id).copied()
    }

    /// Key of the most recently created image carrying `number`.
    pub(crate) fn key_by_number(&self, number: u32) -> Option<u64> {
        self.images.iter().rev().find(|(_, image)| image.number == number).map(|(key, _)| *key)
    }

    /// Smallest id no image uses.
    pub(crate) fn free_id(&self) -> u32 {
        let mut free = 1_u32;
        for id in self.ids.keys() {
            if *id != free {
                break;
            }
            free = free.saturating_add(1);
        }
        free
    }

    /// The virtual placement that placeholder cells naming `id` and `placement` refer to.
    ///
    /// A zero `placement` selects the image's first virtual placement, as Kitty does.
    pub(crate) fn virtual_placement(
        &self,
        id: u32,
        placement: u32,
    ) -> Option<(&Image, &Placement)> {
        let key = self.key_by_id(id)?;
        let found = self
            .placements
            .iter()
            .find(|p| p.is_virtual && p.image == key && (placement == 0 || p.id == placement))?;
        Some((self.images.get(&key)?, found))
    }

    fn next_key(&mut self) -> u64 {
        self.last_key += 1;
        self.last_key
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Add an image, returning its key.
    ///
    /// `pixels` is `None` for an image whose decode is still running. An existing image with the
    /// same nonzero `id` must already have been removed with [`Self::remove_by_id`].
    pub(crate) fn insert(
        &mut self,
        id: u32,
        number: u32,
        (width, height): (u32, u32),
        pixels: Option<Arc<Vec<u8>>>,
        opaque: bool,
    ) -> u64 {
        // Every transmission sweeps out images nothing can show again, as Kitty does.
        self.reap(Reap::Unaddressable);
        let key = self.next_key();
        let used = self.tick();
        let image = Image {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            id,
            number,
            width,
            height,
            pixels,
            opaque,
            refs: 0,
            used,
            matched: false,
        };
        self.stored += image.bytes();
        self.images.insert(key, image);
        if id != 0 {
            self.ids.insert(id, key);
        }
        key
    }

    /// Supply the pixels of an image that was inserted while still decoding.
    pub(crate) fn set_pixels(&mut self, key: u64, pixels: Arc<Vec<u8>>, opaque: bool) -> bool {
        let Some(image) = self.images.get_mut(&key) else {
            return false;
        };
        image.pixels = Some(pixels);
        image.opaque = opaque;
        true
    }

    /// Remove an image and every placement of it.
    pub(crate) fn remove_image(&mut self, key: u64) -> bool {
        let Some(image) = self.images.remove(&key) else {
            return false;
        };
        self.stored -= image.bytes();
        if image.id != 0 && self.ids.get(&image.id) == Some(&key) {
            self.ids.remove(&image.id);
        }
        if image.refs > 0 {
            let virtual_count = &mut self.virtual_count;
            self.placements.retain(|p| {
                let keep = p.image != key;
                if !keep && p.is_virtual {
                    *virtual_count -= 1;
                }
                keep
            });
        }
        true
    }

    /// Remove the image with application id `id`, with its placements.
    pub(crate) fn remove_by_id(&mut self, id: u32) -> bool {
        self.key_by_id(id).is_some_and(|key| self.remove_image(key))
    }

    /// Bring the store back under its byte and count limits, sparing `keep`.
    ///
    /// Images nothing shows go first, even ones that still have an id; then the least recently
    /// used, placements and all.
    pub(crate) fn enforce_quota(&mut self, keep: Option<u64>) -> bool {
        let over = |store: &Self| store.stored > STORAGE_QUOTA || store.images.len() > MAX_IMAGES;
        if !over(self) {
            return false;
        }
        let idle: Vec<u64> = self
            .images
            .iter()
            .filter(|(key, image)| image.refs == 0 && Some(**key) != keep)
            .map(|(key, _)| *key)
            .collect();
        for key in idle {
            self.remove_image(key);
        }
        while over(self) {
            let oldest = self
                .images
                .iter()
                .filter(|(key, _)| Some(**key) != keep)
                .min_by_key(|(_, image)| image.used)
                .map(|(key, _)| *key);
            let Some(oldest) = oldest else {
                break;
            };
            self.remove_image(oldest);
        }
        true
    }

    /// Create or move a placement of image `key` at the cursor, returning the cells it covers.
    ///
    /// A placement that names an id already used on this image is moved rather than duplicated.
    ///
    /// When the image is opaque, an older placement with exactly the same geometry is removed if
    /// nothing could ever address it apart from the new one: it has no id of its own, and its
    /// image either is this image or has no id. Such a placement can never be seen again, and
    /// removing it is what keeps a program that redraws frames in place, with no ids, at one live
    /// frame instead of a screen-quota's worth of stacked ones.
    pub(crate) fn put(
        &mut self,
        key: u64,
        command: &Command,
        (row, col): (i32, i32),
        cell: CellPixels,
    ) -> Option<(u32, u32)> {
        let used = self.tick();
        let image = self.images.get_mut(&key)?;
        image.used = used;
        let (image_id, opaque) = (image.id, image.opaque);
        let (width, height) = (image.width as f32, image.height as f32);
        let is_virtual = command.unicode == 1;

        let src_x = command.x as f32;
        let src_y = command.y as f32;
        let src_w = if command.width == 0 { width } else { command.width as f32 };
        let src_h = if command.height == 0 { height } else { command.height as f32 };
        let mut placement = Placement {
            key: 0,
            image: key,
            // A placement id only means something on an image that itself can be named.
            id: if image_id == 0 { 0 } else { command.placement },
            row: if is_virtual { 0 } else { row },
            col: if is_virtual { 0 } else { col },
            src_x,
            src_y,
            src_w: src_w.min(width - src_x.min(width)),
            src_h: src_h.min(height - src_y.min(height)),
            cell_x: command.cell_x.min(cell.width - 1),
            cell_y: command.cell_y.min(cell.height - 1),
            cols: command.columns.min(MAX_SPAN),
            rows: command.rows.min(MAX_SPAN),
            eff_cols: 0,
            eff_rows: 0,
            z: command.z,
            is_virtual,
        };
        placement.fit(cell);
        let extent = (placement.eff_cols, placement.eff_rows);

        let existing = (placement.id != 0)
            .then(|| self.placements.iter().position(|p| p.image == key && p.id == placement.id))
            .flatten();
        match existing {
            Some(index) => {
                placement.key = self.placements[index].key;
                self.virtual_count -= usize::from(self.placements[index].is_virtual);
                self.virtual_count += usize::from(is_virtual);
                self.placements[index] = placement.clone();
            },
            None => {
                placement.key = self.next_key();
                self.virtual_count += usize::from(is_virtual);
                self.placements.push(placement.clone());
                if let Some(image) = self.images.get_mut(&key) {
                    image.refs += 1;
                }
            },
        }

        if opaque && !is_virtual {
            self.remove_where(
                |p, image| {
                    p.key != placement.key
                        && !p.is_virtual
                        && p.id == 0
                        && (p.image == key || image.id == 0)
                        && p.same_geometry(&placement, cell)
                },
                Reap::Unaddressable,
            );
        }
        if self.placements.len() > MAX_PLACEMENTS {
            let oldest = self.placements[0].key;
            self.remove_where(|p, _| p.key == oldest, Reap::Unaddressable);
        }
        Some(extent)
    }

    /// Remove every placement `remove` selects, then free images as `reap` allows.
    ///
    /// The closure may also adjust a placement it keeps, which is how scrolling moves them.
    fn remove_where(
        &mut self,
        mut remove: impl FnMut(&mut Placement, &Image) -> bool,
        reap: Reap,
    ) -> bool {
        let before = self.placements.len();
        let (images, virtual_count) = (&mut self.images, &mut self.virtual_count);
        self.placements.retain_mut(|placement| {
            let Some(image) = images.get_mut(&placement.image) else {
                return false;
            };
            if !remove(placement, image) {
                return true;
            }
            image.refs -= 1;
            image.matched = true;
            *virtual_count -= usize::from(placement.is_virtual);
            false
        });
        let removed = self.placements.len() != before;
        self.reap(reap) || removed
    }

    /// Free the images `reap` allows and clear the per-operation marks.
    fn reap(&mut self, reap: Reap) -> bool {
        let doomed: Vec<u64> = self
            .images
            .iter_mut()
            .filter_map(|(key, image)| {
                let matched = std::mem::take(&mut image.matched);
                let free = image.refs == 0
                    && match reap {
                        Reap::Unaddressable => image.id == 0,
                        Reap::MatchedUnaddressable => matched && image.id == 0,
                        Reap::Matched => matched,
                        Reap::Unreferenced => true,
                    };
                free.then_some(*key)
            })
            .collect();
        for key in &doomed {
            self.remove_image(*key);
        }
        !doomed.is_empty()
    }

    /// Carry out a delete command. `cursor` is the cursor's line and column.
    ///
    /// A lowercase selector removes placements; its uppercase form also frees the images whose
    /// last placement it removed. Position selectors never touch virtual placements, which have
    /// no position.
    pub(crate) fn delete(&mut self, command: &Command, (row, col): (i32, i32)) -> bool {
        let selector = if command.delete == 0 { b'a' } else { command.delete };
        let reap =
            if selector.is_ascii_uppercase() { Reap::Matched } else { Reap::MatchedUnaddressable };
        // Coordinates in a delete are one-based cells.
        let (x, y) = (i64::from(command.x) - 1, i64::from(command.y) - 1);
        let in_cols = |p: &Placement, x: i64| i64::from(p.col) <= x && x < i64::from(p.end_col());
        let in_rows = |p: &Placement, y: i64| i64::from(p.row) <= y && y < i64::from(p.end_row());

        match selector.to_ascii_lowercase() {
            b'a' => self.remove_where(|p, _| !p.is_virtual && p.end_row() > 0, reap),
            b'i' | b'n' => {
                let key = if selector.eq_ignore_ascii_case(&b'i') {
                    self.key_by_id(command.id)
                } else {
                    self.key_by_number(command.number)
                };
                let Some(key) = key else {
                    return false;
                };
                // An image nothing shows has no placement to match, so the uppercase form frees
                // it directly.
                let idle = self.images.get(&key).is_some_and(|image| image.refs == 0);
                if idle && command.placement == 0 {
                    return selector.is_ascii_uppercase() && self.remove_image(key);
                }
                let placement = command.placement;
                self.remove_where(
                    |p, _| p.image == key && (placement == 0 || p.id == placement),
                    reap,
                )
            },
            b'r' => {
                let range = command.x..=command.y;
                let in_range = |image: &Image| image.id != 0 && range.contains(&image.id);
                let mut changed = false;
                if selector == b'R' {
                    let idle: Vec<u64> = self
                        .images
                        .iter()
                        .filter(|(_, image)| image.refs == 0 && in_range(image))
                        .map(|(key, _)| *key)
                        .collect();
                    for key in idle {
                        changed |= self.remove_image(key);
                    }
                }
                self.remove_where(|_, image| in_range(image), reap) || changed
            },
            b'p' => self.remove_where(|p, _| !p.is_virtual && in_cols(p, x) && in_rows(p, y), reap),
            b'q' => {
                let z = command.z;
                self.remove_where(
                    |p, _| !p.is_virtual && p.z == z && in_cols(p, x) && in_rows(p, y),
                    reap,
                )
            },
            b'x' => self.remove_where(|p, _| !p.is_virtual && in_cols(p, x), reap),
            b'y' => self.remove_where(|p, _| !p.is_virtual && in_rows(p, y), reap),
            b'z' => {
                let z = command.z;
                self.remove_where(|p, _| !p.is_virtual && p.z == z, reap)
            },
            b'c' => {
                let (x, y) = (i64::from(col), i64::from(row));
                self.remove_where(|p, _| !p.is_virtual && in_cols(p, x) && in_rows(p, y), reap)
            },
            // Animation frames are not supported, so there is never one to delete.
            _ => false,
        }
    }

    /// Remove placements from the screen, or with `history` from scrollback as well.
    ///
    /// Virtual placements survive, as in Kitty: they are not on the screen. Every image left
    /// without a placement is freed, including ones that still have an id.
    pub(crate) fn clear(&mut self, history: bool) -> bool {
        self.remove_where(|p, _| !p.is_virtual && (history || p.end_row() > 0), Reap::Unreferenced)
    }

    /// Remove the placements that lie wholly in scrollback.
    ///
    /// Like [`Self::clear`], an erase frees every image it leaves without a placement.
    pub(crate) fn clear_history(&mut self) -> bool {
        self.remove_where(|p, _| !p.is_virtual && p.end_row() <= 0, Reap::Unreferenced)
    }

    /// Move placements with a scroll of the grid.
    ///
    /// A scroll of the whole screen carries every placement along and expires one once it has
    /// left the top of scrollback. A scroll of a narrower region moves only placements that lie
    /// entirely inside it and clips them at its edges, so an image never bleeds into lines the
    /// scroll did not touch.
    pub(crate) fn scroll(&mut self, scroll: Scroll, cell: CellPixels) -> bool {
        if !self.has_placements() || scroll.lines == 0 {
            return false;
        }
        let full = scroll.top == 0 && scroll.end == scroll.screen_lines;
        let delta = -scroll.lines;
        let before = self.placements.clone();
        let removed = if full {
            self.remove_where(
                |p, _| {
                    // A reverse scroll leaves scrollback where it is.
                    if p.is_virtual || (delta > 0 && p.end_row() <= 0) {
                        return false;
                    }
                    p.row = p.row.saturating_add(delta);
                    p.end_row() <= scroll.history_top || p.row >= scroll.screen_lines
                },
                Reap::Unaddressable,
            )
        } else {
            self.remove_where(
                |p, _| !p.is_virtual && scroll_in_region(p, scroll, delta, cell),
                Reap::Unaddressable,
            )
        };
        removed || self.placements != before
    }

    /// Recompute what every placement covers after the cell size changed.
    pub(crate) fn rescale(&mut self, cell: CellPixels) {
        for placement in self.placements.iter_mut().filter(|p| !p.is_virtual) {
            placement.cell_x = placement.cell_x.min(cell.width - 1);
            placement.cell_y = placement.cell_y.min(cell.height - 1);
            placement.fit(cell);
        }
    }

    /// First cell of every real placement, for carrying through a grid resize.
    pub(crate) fn anchors(&self) -> impl Iterator<Item = (u64, i32, i32)> + '_ {
        self.placements.iter().filter(|p| !p.is_virtual).map(|p| (p.key, p.row, p.col))
    }

    /// Move every real placement to where its first cell ended up, dropping those whose cell was
    /// lost.
    pub(crate) fn apply_anchors(
        &mut self,
        mut moved: impl FnMut(u64) -> Option<(i32, i32)>,
    ) -> bool {
        self.remove_where(
            |p, _| {
                if p.is_virtual {
                    return false;
                }
                match moved(p.key) {
                    Some((row, col)) => {
                        (p.row, p.col) = (row, col);
                        false
                    },
                    None => true,
                }
            },
            Reap::Unaddressable,
        )
    }
}

/// Move one placement with a region scroll; returns whether it is gone.
///
/// A port of `scroll_filter_margins_func`.
fn scroll_in_region(p: &mut Placement, scroll: Scroll, delta: i32, cell: CellPixels) -> bool {
    let (top, bottom) = (scroll.top, scroll.end - 1);
    let outside = |p: &Placement| p.end_row() <= top || p.row > bottom;
    if p.row < top || p.end_row() - 1 > bottom {
        // Not entirely inside the region, so the scroll does not own it.
        return false;
    }
    p.row = p.row.saturating_add(delta);
    if outside(p) {
        return true;
    }
    let scale = p.vertical_scale(cell);
    if p.row < top {
        // Moved up past the top margin: cut the rows that left off the top of the source.
        let clipped = (top - p.row) as u32;
        let cut = scale * ((cell.height * clipped) as f32 - p.cell_y as f32);
        if p.src_h <= cut {
            return true;
        }
        p.src_y += cut;
        p.src_h -= cut;
        p.eff_rows -= clipped;
        if p.rows != 0 {
            p.rows -= clipped.min(p.rows - 1);
        }
        p.cell_y = 0;
        p.row += clipped as i32;
    } else if p.end_row() - 1 > bottom {
        // Moved down past the bottom margin: keep only the height that is still inside.
        let clipped = (p.end_row() - 1 - bottom) as u32;
        let visible = (cell.height * (p.eff_rows - clipped)) as f32 - p.cell_y as f32;
        let kept = scale * visible;
        if kept <= 0. {
            return true;
        }
        p.src_h = p.src_h.min(kept);
        p.eff_rows -= clipped;
        if p.rows != 0 {
            p.rows -= clipped.min(p.rows - 1);
        }
    }
    outside(p)
}

#[cfg(test)]
mod tests {
    use super::super::command::parse;
    use super::*;

    const CELL: CellPixels = CellPixels { width: 10, height: 20 };

    fn command(control: &str) -> Command {
        parse(control.as_bytes()).unwrap()
    }

    /// Add a `width`x`height` image of the given opacity.
    fn image(store: &mut ImageStore, id: u32, width: u32, height: u32, opaque: bool) -> u64 {
        let alpha = if opaque { 255 } else { 128 };
        let pixels = vec![alpha; width as usize * height as usize * 4];
        store.remove_by_id(id);
        store.insert(id, 0, (width, height), Some(Arc::new(pixels)), opaque)
    }

    fn put(store: &mut ImageStore, key: u64, control: &str, at: (i32, i32)) -> (u32, u32) {
        store.put(key, &command(control), at, CELL).unwrap()
    }

    fn rows(store: &ImageStore) -> Vec<i32> {
        store.placements().iter().map(|p| p.row).collect()
    }

    #[test]
    fn a_placement_covers_the_cells_its_pixels_reach() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 25, 41, true);
        assert_eq!(put(&mut store, key, "", (0, 0)), (3, 3));
        assert_eq!(
            put(&mut store, key, "X=6", (0, 0)),
            (4, 3),
            "the offset pushes it into a fourth column"
        );
        assert_eq!(put(&mut store, key, "c=5,r=2", (0, 0)), (5, 2));
        // 25x41 at a height of two rows (40px) is 24.4px wide: three columns.
        assert_eq!(put(&mut store, key, "r=2", (0, 0)), (3, 2));
        // At a width of five columns (50px) it is 82px tall: five rows.
        assert_eq!(put(&mut store, key, "c=5", (0, 0)), (5, 5));
        assert_eq!(
            put(&mut store, key, "w=10,h=20", (0, 0)),
            (1, 1),
            "a source rectangle shrinks it"
        );
    }

    #[test]
    fn the_source_rectangle_is_clamped_to_the_image() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 30, 40, true);
        put(&mut store, key, "x=20,y=30,w=100,h=100", (0, 0));
        let placement = &store.placements()[0];
        assert_eq!(
            (placement.src_x, placement.src_y, placement.src_w, placement.src_h),
            (20., 30., 10., 10.)
        );
        put(&mut store, key, "x=99,y=99", (0, 0));
        assert_eq!(store.placements()[1].dest_size(CELL), (0., 0.), "nothing is left to draw");
    }

    #[test]
    fn hostile_extents_are_clamped_before_any_arithmetic() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 1, 1, true);
        assert_eq!(
            put(&mut store, key, "c=4294967295,r=4294967295", (i32::MAX, i32::MAX)),
            (MAX_SPAN, MAX_SPAN)
        );
        let placement = store.placements()[0].clone();
        assert_eq!((placement.end_row(), placement.end_col()), (i32::MAX, i32::MAX));
        // Selectors and scrolls must not overflow on it either.
        store.scroll(Scroll { top: 0, end: 24, lines: 1, screen_lines: 24, history_top: 0 }, CELL);
        store.delete(&command("d=p,x=4294967295,y=4294967295"), (0, 0));
    }

    #[test]
    fn a_placement_id_moves_instead_of_duplicating() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 20, false);
        put(&mut store, key, "p=7", (1, 1));
        put(&mut store, key, "p=7", (5, 6));
        put(&mut store, key, "p=8", (2, 2));
        put(&mut store, key, "", (3, 3));
        put(&mut store, key, "", (3, 3));
        let at: Vec<_> = store.placements().iter().map(|p| (p.id, p.row, p.col)).collect();
        assert_eq!(at, [(7, 5, 6), (8, 2, 2), (0, 3, 3), (0, 3, 3)]);
        assert_eq!(store.image(key).unwrap().refs, 4);
    }

    #[test]
    fn an_image_without_an_id_cannot_carry_placement_ids() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 0, 10, 20, false);
        put(&mut store, key, "p=7", (0, 0));
        put(&mut store, key, "p=7", (1, 0));
        assert_eq!(store.placements().iter().map(|p| p.id).collect::<Vec<_>>(), [0, 0]);
    }

    /// The whole point of the replacement rule: a player redrawing frames in place with no ids
    /// holds one frame, not a quota's worth.
    #[test]
    fn frames_redrawn_in_place_without_ids_replace_each_other() {
        let mut store = ImageStore::default();
        for _ in 0..1000 {
            let key = image(&mut store, 0, 640, 360, true);
            put(&mut store, key, "C=1", (2, 8));
        }
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.images.len(), 1);
        assert_eq!(store.stored, 640 * 360 * 4);
    }

    #[test]
    fn a_placement_is_replaced_only_when_it_could_never_be_seen_or_named_again() {
        let stack = |first: (u32, bool, &str), second: (u32, bool, &str, (i32, i32))| {
            let mut store = ImageStore::default();
            let key = image(&mut store, first.0, 20, 20, first.1);
            put(&mut store, key, first.2, (0, 0));
            let key = image(&mut store, second.0, 20, 20, second.1);
            put(&mut store, key, second.2, second.3);
            store.placements().len()
        };
        assert_eq!(stack((0, true, ""), (0, true, "", (0, 0))), 1, "hidden and unaddressable");
        assert_eq!(
            stack((0, true, ""), (0, false, "", (0, 0))),
            2,
            "still visible through the new one"
        );
        assert_eq!(stack((0, true, ""), (0, true, "", (0, 1))), 2, "another cell");
        assert_eq!(stack((0, true, ""), (0, true, "z=1", (0, 0))), 2, "another layer");
        assert_eq!(stack((0, true, ""), (0, true, "X=1", (0, 0))), 2, "another offset");
        assert_eq!(stack((0, true, ""), (0, true, "c=3,r=3", (0, 0))), 2, "another size");
        assert_eq!(
            stack((5, true, ""), (0, true, "", (0, 0))),
            2,
            "its image can still be deleted by id"
        );
        assert_eq!(stack((5, true, "p=2"), (6, true, "", (0, 0))), 2, "it has a placement id");
        assert_eq!(
            stack((0, true, ""), (6, true, "", (0, 0))),
            1,
            "only the old one was unaddressable"
        );
    }

    #[test]
    fn repeated_placements_of_one_opaque_image_collapse() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 9, 20, 20, true);
        for _ in 0..5 {
            put(&mut store, key, "", (4, 4));
        }
        assert_eq!(store.placements().len(), 1);
        assert_eq!(store.image(key).unwrap().refs, 1);

        // A translucent image composites with itself, so its copies are all visible.
        let key = image(&mut store, 10, 20, 20, false);
        for _ in 0..5 {
            put(&mut store, key, "", (8, 8));
        }
        assert_eq!(store.image(key).unwrap().refs, 5);
    }

    #[test]
    fn retransmitting_an_id_drops_the_old_image_and_its_placements() {
        let mut store = ImageStore::default();
        let old = image(&mut store, 3, 10, 10, true);
        put(&mut store, old, "", (0, 0));
        let uid = store.image(old).unwrap().uid;
        let new = image(&mut store, 3, 4, 4, true);
        assert!(store.image(old).is_none());
        assert!(store.placements().is_empty());
        assert_eq!(store.key_by_id(3), Some(new));
        assert_ne!(store.image(new).unwrap().uid, uid, "new pixels are a new identity");
        assert_eq!(store.stored, 4 * 4 * 4);
    }

    #[test]
    fn transmitting_sweeps_out_images_nothing_can_show() {
        let mut store = ImageStore::default();
        let orphan = image(&mut store, 0, 10, 10, true);
        let named = image(&mut store, 4, 10, 10, true);
        assert!(store.image(orphan).is_none(), "no id and no placement");
        assert!(store.image(named).is_some(), "an id keeps it reachable");
    }

    #[test]
    fn numbers_resolve_to_the_newest_image_and_ids_are_allocated_densely() {
        let mut store = ImageStore::default();
        assert_eq!(store.free_id(), 1);
        let first = store.insert(1, 50, (1, 1), None, false);
        let second = store.insert(2, 50, (1, 1), None, false);
        store.insert(4, 0, (1, 1), None, false);
        assert_eq!(store.key_by_number(50), Some(second));
        assert_eq!(store.free_id(), 3);
        store.remove_image(second);
        assert_eq!(store.key_by_number(50), Some(first));
        assert_eq!(store.free_id(), 2);
        assert_eq!(store.key_by_number(51), None);
    }

    fn deletable() -> ImageStore {
        let mut store = ImageStore::default();
        // Three 2x2-cell images in a diagonal, one in scrollback, and a virtual placement.
        for (id, at, control) in
            [(1, (0, 0), "p=1"), (2, (2, 2), "z=5"), (3, (4, 4), ""), (4, (-9, 0), "")]
        {
            let key = image(&mut store, id, 20, 40, false);
            put(&mut store, key, control, at);
        }
        let key = image(&mut store, 5, 20, 40, false);
        put(&mut store, key, "U=1,c=2,r=2", (0, 0));
        store
    }

    fn ids(store: &ImageStore) -> Vec<u32> {
        store.placements().iter().map(|p| store.image(p.image).unwrap().id).collect()
    }

    #[test]
    fn each_delete_selector_removes_exactly_what_it_names() {
        for (control, cursor, placed, images) in [
            ("d=a", (0, 0), vec![4, 5], vec![1, 2, 3, 4, 5]),
            ("", (0, 0), vec![4, 5], vec![1, 2, 3, 4, 5]),
            ("d=A", (0, 0), vec![4, 5], vec![4, 5]),
            ("d=i,i=2", (0, 0), vec![1, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=I,i=2", (0, 0), vec![1, 3, 4, 5], vec![1, 3, 4, 5]),
            ("d=i,i=1,p=1", (0, 0), vec![2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=I,i=1,p=9", (0, 0), vec![1, 2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=I,i=5", (0, 0), vec![1, 2, 3, 4], vec![1, 2, 3, 4]),
            ("d=i,i=77", (0, 0), vec![1, 2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=r,x=2,y=3", (0, 0), vec![1, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=R,x=2,y=5", (0, 0), vec![1], vec![1]),
            ("d=p,x=3,y=3", (0, 0), vec![1, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=P,x=4,y=4", (0, 0), vec![1, 3, 4, 5], vec![1, 3, 4, 5]),
            ("d=p,x=5,y=3", (0, 0), vec![1, 2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=q,x=3,y=3,z=5", (0, 0), vec![1, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=q,x=3,y=3,z=4", (0, 0), vec![1, 2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=x,x=1", (0, 0), vec![2, 3, 5], vec![1, 2, 3, 4, 5]),
            ("d=X,x=6", (0, 0), vec![1, 2, 4, 5], vec![1, 2, 4, 5]),
            ("d=y,y=3", (0, 0), vec![1, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=z,z=0", (0, 0), vec![2, 5], vec![1, 2, 3, 4, 5]),
            ("d=Z,z=5", (0, 0), vec![1, 3, 4, 5], vec![1, 3, 4, 5]),
            ("d=c", (5, 5), vec![1, 2, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=C", (1, 1), vec![2, 3, 4, 5], vec![2, 3, 4, 5]),
            ("d=c", (9, 9), vec![1, 2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
            ("d=f,i=1", (0, 0), vec![1, 2, 3, 4, 5], vec![1, 2, 3, 4, 5]),
        ] {
            let mut store = deletable();
            store.delete(&command(control), cursor);
            assert_eq!(ids(&store), placed, "placements after {control:?}");
            let left: Vec<u32> = store.images.values().map(|image| image.id).collect();
            assert_eq!(left, images, "images after {control:?}");
            let stored: usize = store.images.values().map(Image::bytes).sum();
            assert_eq!(store.stored, stored, "accounting after {control:?}");
            assert_eq!(store.virtual_count, usize::from(placed.contains(&5)), "after {control:?}");
        }
    }

    #[test]
    fn deleting_frees_images_that_have_no_id_even_in_lowercase() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 0, 20, 40, false);
        put(&mut store, key, "", (0, 0));
        store.delete(&command("d=a"), (0, 0));
        assert!(store.is_empty());
    }

    #[test]
    fn an_idle_image_is_freed_by_the_uppercase_id_selectors_only() {
        let mut store = ImageStore::default();
        image(&mut store, 6, 2, 2, true);
        assert!(!store.delete(&command("d=i,i=6"), (0, 0)));
        assert!(store.key_by_id(6).is_some());
        assert!(store.delete(&command("d=I,i=6"), (0, 0)));
        assert!(store.is_empty());

        let key = store.insert(1, 33, (2, 2), None, false);
        assert!(!store.delete(&command("d=n,I=33"), (0, 0)));
        assert!(store.image(key).is_some());
        assert!(store.delete(&command("d=N,I=33"), (0, 0)));
        assert!(store.is_empty());
    }

    #[test]
    fn clearing_the_screen_spares_scrollback_and_virtual_placements() {
        let mut store = deletable();
        image(&mut store, 6, 2, 2, true);
        assert!(store.clear(false));
        assert_eq!(ids(&store), [4, 5]);
        assert_eq!(
            store.images.values().map(|i| i.id).collect::<Vec<_>>(),
            [4, 5],
            "idle images go too"
        );
        assert!(store.clear(true));
        assert_eq!(ids(&store), [5]);
        assert!(!store.clear(true), "nothing left to clear");

        let mut store = deletable();
        assert!(store.clear_history());
        assert_eq!(ids(&store), [1, 2, 3, 5]);
    }

    fn scroll(top: i32, end: i32, lines: i32, history_top: i32) -> Scroll {
        Scroll { top, end, lines, screen_lines: 10, history_top }
    }

    #[test]
    fn a_full_screen_scroll_carries_placements_into_scrollback_and_expires_them_there() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 40, false);
        put(&mut store, key, "", (0, 0));
        put(&mut store, key, "", (8, 0));

        assert!(store.scroll(scroll(0, 10, 3, -3), CELL));
        assert_eq!(rows(&store), [-3, 5]);
        assert!(store.scroll(scroll(0, 10, 1, -3), CELL));
        assert_eq!(rows(&store), [-4, 4], "one row of the first still exists at line -3");
        assert!(store.scroll(scroll(0, 10, 1, -3), CELL));
        assert_eq!(rows(&store), [3], "the first is wholly above the top of scrollback");
    }

    #[test]
    fn without_scrollback_a_placement_expires_as_it_leaves_the_screen() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 0, 10, 40, false);
        put(&mut store, key, "", (0, 0));
        store.scroll(scroll(0, 10, 1, 0), CELL);
        assert_eq!(rows(&store), [-1]);
        store.scroll(scroll(0, 10, 1, 0), CELL);
        assert!(store.is_empty(), "the image had no id, so it went with its placement");
    }

    #[test]
    fn a_reverse_scroll_leaves_scrollback_alone_and_drops_what_falls_off_the_bottom() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 40, false);
        for row in [-5, 0, 8] {
            put(&mut store, key, "", (row, 0));
        }
        assert!(store.scroll(scroll(0, 10, -1, -5), CELL));
        assert_eq!(rows(&store), [-5, 1, 9]);
        assert!(store.scroll(scroll(0, 10, -1, -5), CELL));
        assert_eq!(rows(&store), [-5, 2], "the last one is wholly below the screen");
    }

    #[test]
    fn a_region_scroll_moves_only_what_lies_entirely_inside_it() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 40, false);
        for row in [0, 3, 6, 8] {
            put(&mut store, key, "", (row, 0));
        }
        // Lines 2..8: the placement at 0 is above the region and the one at 8 below it.
        assert!(store.scroll(scroll(2, 8, 1, 0), CELL));
        assert_eq!(rows(&store), [0, 2, 5, 8]);
        assert!(!store.scroll(scroll(2, 8, 0, 0), CELL));
    }

    #[test]
    fn a_region_scroll_clips_at_the_top_margin_then_removes() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 60, false);
        put(&mut store, key, "", (2, 0));

        store.scroll(scroll(2, 8, 1, 0), CELL);
        let placement = store.placements()[0].clone();
        assert_eq!(
            (placement.row, placement.eff_rows),
            (2, 2),
            "it stays at the margin, one row shorter"
        );
        assert_eq!((placement.src_y, placement.src_h), (20., 40.), "the top row of pixels is cut");
        assert_eq!(placement.dest_size(CELL), (10., 40.));

        store.scroll(scroll(2, 8, 2, 0), CELL);
        assert!(store.placements().is_empty(), "nothing of it is left inside the region");
        assert!(store.image(key).is_some(), "the image has an id and stays");
    }

    #[test]
    fn a_region_scroll_clips_at_the_bottom_margin() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 60, false);
        put(&mut store, key, "", (5, 0));

        store.scroll(scroll(2, 8, -1, 0), CELL);
        let placement = store.placements()[0].clone();
        assert_eq!((placement.row, placement.eff_rows), (6, 2));
        assert_eq!(
            (placement.src_y, placement.src_h),
            (0., 40.),
            "the bottom row of pixels is cut"
        );

        store.scroll(scroll(2, 8, -2, 0), CELL);
        assert!(store.placements().is_empty());
    }

    #[test]
    fn clipping_a_scaled_placement_cuts_source_pixels_in_proportion() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 120, false);
        // 120 source pixels drawn into three rows (60px): two source pixels per screen pixel.
        put(&mut store, key, "c=1,r=3", (2, 0));
        store.scroll(scroll(2, 8, 1, 0), CELL);
        let placement = store.placements()[0].clone();
        assert_eq!((placement.rows, placement.eff_rows), (2, 2));
        assert_eq!((placement.src_y, placement.src_h), (40., 80.));
        assert_eq!(placement.dest_size(CELL), (10., 40.));
    }

    #[test]
    fn the_quota_evicts_idle_images_first_and_then_the_least_recently_used() {
        let mut store = ImageStore::default();
        // 64 MiB each: five fit under 320 MiB, the sixth does not.
        let mut keys = Vec::new();
        for id in 1..=5 {
            let key = store.insert(id, 0, (4096, 4096), None, false);
            assert!(!store.enforce_quota(Some(key)));
            keys.push(key);
        }
        // Everything is placed except image 3; image 1 is then the least recently used.
        for (index, key) in keys.iter().enumerate() {
            if index != 2 {
                put(&mut store, *key, "", (index as i32, 0));
            }
        }
        let sixth = store.insert(6, 0, (4096, 4096), None, false);
        assert!(store.enforce_quota(Some(sixth)));
        assert!(store.image(keys[2]).is_none(), "the unplaced image goes first");
        assert_eq!(store.images.len(), 5);

        let seventh = store.insert(7, 0, (4096, 4096), None, false);
        assert!(store.enforce_quota(Some(seventh)));
        assert!(store.image(sixth).is_none(), "then whatever is idle, which is now the sixth");
        let eighth = store.insert(8, 0, (4096, 4096), None, false);
        put(&mut store, seventh, "", (7, 0));
        assert!(store.enforce_quota(Some(eighth)));
        assert!(store.image(keys[0]).is_none(), "then the least recently used, placement and all");
        assert_eq!(ids(&store), [2, 4, 5, 7]);
        assert!(store.stored <= STORAGE_QUOTA);
    }

    #[test]
    fn the_image_and_placement_counts_are_bounded() {
        // None of these is placed, so going over the count sweeps out everything idle at once.
        let mut store = ImageStore::default();
        for id in 1..=MAX_IMAGES as u32 + 10 {
            let key = store.insert(id, 0, (1, 1), None, false);
            store.enforce_quota(Some(key));
            assert!(store.images.len() <= MAX_IMAGES);
        }
        assert_eq!(store.images.len(), 10);
        assert_eq!(store.stored, 10 * 4);

        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 10, 20, false);
        for index in 0..MAX_PLACEMENTS + 10 {
            put(&mut store, key, "", (index as i32, 0));
        }
        assert_eq!(store.placements().len(), MAX_PLACEMENTS);
        assert_eq!(store.placements()[0].row, 10, "the oldest were dropped");
        assert_eq!(store.image(key).unwrap().refs as usize, MAX_PLACEMENTS);
    }

    #[test]
    fn virtual_placements_have_no_position_and_are_found_by_image_and_placement_id() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 42, 100, 50, true);
        assert_eq!(put(&mut store, key, "U=1,c=10,r=5,p=3", (7, 7)), (10, 5));
        put(&mut store, key, "U=1,c=4,r=2,p=4", (7, 7));
        assert!(store.has_virtual());
        assert!(!store.has_placements());
        assert_eq!(store.placements()[0].row, 0);

        let found = |store: &ImageStore, placement| {
            store.virtual_placement(42, placement).map(|(_, p)| (p.cols, p.rows))
        };
        assert_eq!(found(&store, 0), Some((10, 5)), "zero selects the first");
        assert_eq!(found(&store, 4), Some((4, 2)));
        assert_eq!(found(&store, 9), None);
        assert!(store.virtual_placement(41, 0).is_none());

        // A scroll has nothing to move, and a screen clear leaves them be.
        assert!(!store.scroll(scroll(0, 10, 1, 0), CELL));
        assert!(!store.clear(true));
        assert!(store.delete(&command("d=i,i=42,p=3"), (0, 0)));
        assert_eq!(found(&store, 0), Some((4, 2)));
    }

    #[test]
    fn a_grid_resize_moves_placements_with_their_cells_and_drops_lost_ones() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 0, 10, 20, false);
        put(&mut store, key, "", (1, 1));
        put(&mut store, key, "", (2, 2));
        let anchors: Vec<_> = store.anchors().collect();
        assert_eq!(anchors.iter().map(|a| (a.1, a.2)).collect::<Vec<_>>(), [(1, 1), (2, 2)]);
        let lost = anchors[0].0;
        assert!(store.apply_anchors(|key| (key != lost).then_some((-3, 7))));
        assert_eq!(
            store.placements().iter().map(|p| (p.row, p.col)).collect::<Vec<_>>(),
            [(-3, 7)]
        );
        assert!(store.apply_anchors(|_| None));
        assert!(store.is_empty());
    }

    #[test]
    fn a_new_cell_size_changes_what_a_natural_size_placement_covers() {
        let mut store = ImageStore::default();
        let key = image(&mut store, 1, 40, 40, false);
        put(&mut store, key, "X=9,Y=19", (0, 0));
        store.rescale(CellPixels::new(5, 10));
        let placement = &store.placements()[0];
        assert_eq!((placement.cell_x, placement.cell_y), (4, 9), "offsets stay inside the cell");
        assert_eq!((placement.eff_cols, placement.eff_rows), (9, 5));
    }

    #[test]
    fn a_zero_cell_dimension_is_raised_to_one_pixel() {
        assert_eq!(CellPixels::new(0, 0), CellPixels { width: 1, height: 1 });
    }
}
