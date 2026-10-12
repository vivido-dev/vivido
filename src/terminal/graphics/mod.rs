//! Kitty graphics protocol.
//!
//! Applications draw images by sending `ESC _ G <control> ; <base64 payload> ESC \`. The parser
//! hands every application program command to [`KittyGraphics`] through `apc_start`, `apc_put`
//! and `apc_end`, in stream order with all other terminal output, so a placement lands at the
//! cursor position the application had at that point even inside a synchronized update.
//!
//! # What is supported
//!
//! Transmitting images directly (`t=d`) as 24-bit, 32-bit or PNG data, optionally zlib-compressed
//! and chunked; placing them with a source rectangle, a size in cells, a pixel offset, a z-index
//! and a placement id; deleting by every selector the protocol defines; querying; image numbers;
//! and Unicode placeholders through virtual placements.
//!
//! Reading images from files or shared memory, animation, and placements relative to another
//! placement are not. Each is answered with an error in the protocol's reply format, which is how
//! applications decide to fall back to direct transmission of still images.
//!
//! # Ownership and bounds
//!
//! The primary and the alternate screen each own a separate image store, so nothing addressed on
//! one can affect the other. Everything an application can make this module hold is bounded: see
//! the constants in `load` and `store`.
//!
//! # Cost
//!
//! Nothing here runs unless an application program command arrives. Uncompressed pixels are
//! decoded incrementally as they are read. Decoding compressed data is the one expensive step,
//! and [`KittyGraphics::take_jobs`] lets the PTY reader do it after releasing the terminal lock.

// Rust guideline compliant 2026-10-07

mod command;
mod load;
mod placeholder;
mod store;

use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;

use command::{Command, MAX_CONTROL_BYTES};
use load::{DecodeJob, Failure, Load, Pixels, Received};
pub use placeholder::PLACEHOLDER;
pub(crate) use placeholder::{decode_cell, runs, slice};
pub use store::CellPixels;
pub(crate) use store::{ImageStore, Scroll};

/// Most replies held back behind a decode that has not finished.
///
/// Replies are held only for the remainder of one PTY read, so in practice a handful. The cap is
/// what keeps a read full of queries from growing the queue without limit; replies past it are
/// dropped, which a well-behaved application sees as a query that timed out.
const MAX_REPLIES: usize = 256;

/// Most decodes queued for the PTY reader before further ones run in place.
const MAX_JOBS: usize = 16;

/// Index of the alternate screen's store.
const ALTERNATE: usize = 1;

/// Where the cursor is when a command runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    /// Grid line.
    pub row: i32,
    /// Grid column.
    pub col: i32,
}

/// What the terminal has to do after a command ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// Columns and rows of a new placement the cursor must move past.
    pub advance: Option<(u32, u32)>,
}

/// Where the bytes of the current application program command go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sink {
    /// Not inside a command.
    Idle,
    /// Inside a command whose first byte has not arrived.
    Start,
    /// Collecting the control data of a graphics command.
    Control,
    /// Past the `;`. `feeding` is whether the payload belongs to a transmission.
    Payload { feeding: bool },
    /// A command that is not a graphics command, or a malformed one.
    Ignore,
}

/// A reply the application is owed, in the order it must receive it.
#[derive(Debug)]
enum Reply {
    Ready(String),
    /// Waiting for the decode with this sequence number.
    Pending(u64),
}

/// The image a decode is for.
///
/// A decode can outlive what it was started for: the image may be deleted, or the terminal reset
/// and its keys handed out afresh. The `uid` is what tells the same image from a later one that
/// merely sits at the same key.
#[derive(Clone, Copy, Debug)]
struct Target {
    screen: usize,
    key: u64,
    uid: u64,
}

/// A decode queued for the PTY reader.
#[derive(Debug)]
pub(crate) struct QueuedJob {
    seq: u64,
    job: DecodeJob,
    /// The image awaiting these pixels; `None` for a query.
    target: Option<Target>,
    command: Command,
}

impl QueuedJob {
    /// Decode the image. Touches no terminal state, so it needs no lock.
    pub(crate) fn run(self) -> FinishedJob {
        FinishedJob {
            seq: self.seq,
            target: self.target,
            command: self.command,
            result: self.job.run(),
        }
    }
}

/// The result of a [`QueuedJob`], to be handed back to [`KittyGraphics::finish_jobs`].
#[derive(Debug)]
pub(crate) struct FinishedJob {
    seq: u64,
    target: Option<Target>,
    command: Command,
    result: Result<Pixels, Failure>,
}

/// Kitty graphics state of one terminal.
#[derive(Debug)]
pub struct KittyGraphics {
    /// Image stores of the primary and the alternate screen.
    screens: [ImageStore; 2],
    /// Index into `screens` of the screen being displayed.
    active: usize,
    sink: Sink,
    /// Control data of the command being received. Reused between commands.
    control: Vec<u8>,
    /// Parsed control data of the command being received.
    command: Option<Command>,
    /// The transmission in progress, which later commands may continue.
    loader: Option<Load>,
    cell: CellPixels,
    /// Changes on every visible change; what the renderer compares to know it is current.
    revision: u64,
    /// Changes on screen switches and resets, invalidating retained renderer frames.
    screen_epoch: u64,
    /// Set by a visible change and cleared by [`Self::take_dirty`].
    dirty: bool,
    /// Bytes consumed since [`Self::take_quiet_bytes`] that changed nothing on screen.
    quiet_bytes: usize,
    defer_decode: bool,
    jobs: Vec<QueuedJob>,
    last_job: u64,
    replies: VecDeque<Reply>,
}

impl Default for KittyGraphics {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyGraphics {
    /// Create empty graphics state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            screens: [ImageStore::default(), ImageStore::default()],
            active: 0,
            sink: Sink::Idle,
            control: Vec::new(),
            command: None,
            loader: None,
            cell: CellPixels::FALLBACK,
            revision: 0,
            screen_epoch: 0,
            dirty: false,
            quiet_bytes: 0,
            defer_decode: false,
            jobs: Vec::new(),
            last_job: 0,
            replies: VecDeque::new(),
        }
    }

    /// Counter that changes whenever anything that could be drawn changes.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Identity of the current screen lifetime for retaining a frame during decoding.
    pub(crate) fn screen_epoch(&self) -> u64 {
        self.screen_epoch
    }

    /// Cell size placements are currently fitted to.
    #[must_use]
    pub fn cell(&self) -> CellPixels {
        self.cell
    }

    /// Image store of the screen being displayed.
    pub(crate) fn store(&self) -> &ImageStore {
        &self.screens[self.active]
    }

    /// Whether the displayed screen has a placement that scrolling must move.
    pub(crate) fn has_placements(&self) -> bool {
        self.store().has_placements()
    }

    /// Decide whether compressed images are decoded in place or queued for [`Self::take_jobs`].
    ///
    /// Queueing is for a driver that will call `take_jobs` after every batch of input, as the PTY
    /// reader does. Without one, a queued image would wait forever, so the default is in place.
    pub(crate) fn set_deferred_decode(&mut self, defer: bool) {
        self.defer_decode = defer;
    }

    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
    }

    /// Whether anything visible changed since the last call.
    pub(crate) fn take_dirty(&mut self) -> bool {
        mem::take(&mut self.dirty)
    }

    /// Bytes consumed since the last call that changed nothing on screen.
    pub(crate) fn take_quiet_bytes(&mut self) -> usize {
        mem::take(&mut self.quiet_bytes)
    }

    /// Adopt a new cell size, refitting every placement to it.
    pub(crate) fn set_cell(&mut self, cell: CellPixels) {
        if cell == self.cell {
            return;
        }
        self.cell = cell;
        for store in &mut self.screens {
            store.rescale(cell);
        }
        self.touch();
    }

    /// An application program command has started.
    pub(crate) fn apc_start(&mut self) {
        self.sink = Sink::Start;
        self.control.clear();
        self.command = None;
        // The two bytes of the `ESC _` introducer.
        self.quiet_bytes += 2;
    }

    /// Accept the next run of bytes of the current application program command.
    pub(crate) fn apc_put(&mut self, mut bytes: &[u8]) {
        self.quiet_bytes += bytes.len();
        loop {
            match self.sink {
                Sink::Start => match bytes.split_first() {
                    Some((b'G', rest)) => {
                        self.sink = Sink::Control;
                        bytes = rest;
                    },
                    Some(_) => self.sink = Sink::Ignore,
                    None => return,
                },
                Sink::Control => {
                    let end = memchr::memchr(b';', bytes);
                    let control = &bytes[..end.unwrap_or(bytes.len())];
                    if self.control.len() + control.len() > MAX_CONTROL_BYTES {
                        self.sink = Sink::Ignore;
                        return;
                    }
                    self.control.extend_from_slice(control);
                    let Some(end) = end else {
                        return;
                    };
                    bytes = &bytes[end + 1..];
                    self.sink = self.control_complete();
                },
                Sink::Payload { feeding: true } => {
                    if let Some(load) = &mut self.loader {
                        load.push(bytes);
                    }
                    return;
                },
                Sink::Payload { feeding: false } | Sink::Ignore | Sink::Idle => return,
            }
        }
    }

    /// The current application program command has ended; run it if it was a graphics command.
    pub(crate) fn apc_end(&mut self, cancelled: bool, cursor: Cursor) -> Outcome {
        if !cancelled {
            // The two bytes of the `ESC \\` terminator. A cancelled command ends in bytes that
            // belong to something else, which may well be visible, so those are not counted.
            self.quiet_bytes += 2;
        }
        let sink = match mem::replace(&mut self.sink, Sink::Idle) {
            // No `;` arrived, so the control data ends with the command.
            Sink::Control => self.control_complete(),
            sink => sink,
        };
        let command = self.command.take();
        let Sink::Payload { feeding } = sink else {
            return Outcome::default();
        };
        if cancelled {
            // CAN or SUB cut the command short; whatever it was part of cannot be trusted.
            if feeding {
                self.loader = None;
            }
            return Outcome::default();
        }
        let Some(command) = command else {
            return Outcome::default();
        };
        if feeding {
            return self.chunk_complete(&command, cursor);
        }
        match command.action {
            b'p' => self.place(&command, cursor),
            b'd' => {
                // Kitty abandons a transmission that a delete interrupts.
                self.loader = None;
                if self.screens[self.active].delete(&command, (cursor.row, cursor.col)) {
                    self.touch();
                }
                Outcome::default()
            },
            b'a' | b'c' => {
                self.reply(&command, Err(Failure::new("EINVAL", "Animation is not supported")));
                Outcome::default()
            },
            _ => Outcome::default(),
        }
    }

    /// Parse the collected control data and decide what the command's payload is for.
    fn control_complete(&mut self) -> Sink {
        let Some(command) = command::parse(&self.control) else {
            return Sink::Ignore;
        };
        self.command = Some(command);
        let continues = match &self.loader {
            // Later chunks of a transmission carry no action, or repeat the first one's.
            Some(load) if load.start.action == b'f' => matches!(command.action, 0 | b'f'),
            Some(_) => matches!(command.action, 0 | b't' | b'T' | b'q'),
            None => false,
        };
        if continues {
            return Sink::Payload { feeding: true };
        }
        match command.action {
            0 | b't' | b'T' | b'q' => {
                self.begin_load(command);
                Sink::Payload { feeding: true }
            },
            b'f' => {
                // Animation frames are refused, but their chunks still have to be consumed.
                let mut load = Load::begin(command);
                load.reject(Failure::new("EINVAL", "Animation is not supported"));
                self.loader = Some(load);
                Sink::Payload { feeding: true }
            },
            _ => Sink::Payload { feeding: false },
        }
    }

    /// Start a transmission from the control data of its first command.
    fn begin_load(&mut self, mut start: Command) {
        let both = start.id != 0 && start.number != 0;
        if !both && start.action != b'q' {
            let store = &mut self.screens[self.active];
            if start.id != 0 {
                // Retransmitting an id replaces the image at once, whether or not the new data
                // turns out to be usable.
                if store.remove_by_id(start.id) {
                    self.touch();
                }
            } else if start.number != 0 {
                start.id = store.free_id();
            }
        }
        let mut load = Load::begin(start);
        if both {
            load.reject(Failure::new("EINVAL", "Must not specify both image id and image number"));
        }
        self.loader = Some(load);
    }

    /// One chunk of a transmission has ended; finish the transmission if it was the last.
    fn chunk_complete(&mut self, command: &Command, cursor: Cursor) -> Outcome {
        let Some(load) = &mut self.loader else {
            return Outcome::default();
        };
        load.end_chunk();
        if command.quiet != 0 {
            load.start.quiet = command.quiet;
        }
        if command.more != 0 {
            return Outcome::default();
        }
        match self.loader.take() {
            Some(load) => self.load_complete(load, cursor),
            None => Outcome::default(),
        }
    }

    /// A transmission has ended: store the image, or queue its decode, and place it if asked.
    fn load_complete(&mut self, load: Load, cursor: Cursor) -> Outcome {
        let start = load.start;
        let query = start.action == b'q';
        // A query has to name an image to be answered, and stores nothing either way.
        if query && start.id == 0 {
            return Outcome::default();
        }
        let received = match load.finish() {
            Ok(received) => received,
            Err(failure) => {
                self.reply(&start, Err(failure));
                return Outcome::default();
            },
        };
        let (size, pixels, job) = match received {
            Received::Pixels(pixels) => ((pixels.width, pixels.height), Some(pixels), None),
            Received::Job(job) => match job.dimensions() {
                Ok(size) if self.defer_decode && self.jobs.len() < MAX_JOBS => {
                    (size, None, Some(job))
                },
                Ok(_) => match job.run() {
                    Ok(pixels) => ((pixels.width, pixels.height), Some(pixels), None),
                    Err(failure) => {
                        self.reply(&start, Err(failure));
                        return Outcome::default();
                    },
                },
                Err(failure) => {
                    self.reply(&start, Err(failure));
                    return Outcome::default();
                },
            },
        };
        if query {
            match job {
                Some(job) => self.queue(job, None, start),
                None => self.reply(&start, Ok(())),
            }
            return Outcome::default();
        }

        let (rgba, opaque) = match pixels {
            Some(pixels) => (Some(Arc::new(pixels.rgba)), pixels.opaque),
            None => (None, false),
        };
        let key = self.screens[self.active].insert(start.id, start.number, size, rgba, opaque);
        match job {
            Some(job) => {
                let uid = self.screens[self.active].image(key).map_or(0, |image| image.uid);
                self.queue(job, Some(Target { screen: self.active, key, uid }), start);
            },
            None => self.reply(&start, Ok(())),
        }
        let outcome =
            if start.action == b'T' { self.put(key, &start, cursor) } else { Outcome::default() };
        self.screens[self.active].enforce_quota(Some(key));
        self.touch();
        outcome
    }

    /// Carry out a put command.
    fn place(&mut self, command: &Command, cursor: Cursor) -> Outcome {
        if command.id == 0 && command.number == 0 {
            return Outcome::default();
        }
        if command.id != 0 && command.number != 0 {
            let both = Failure::new("EINVAL", "Must not specify both image id and image number");
            self.reply(command, Err(both));
            return Outcome::default();
        }
        if command.parent != 0 {
            let relative = Failure::new("EINVAL", "Relative placements are not supported");
            self.reply(command, Err(relative));
            return Outcome::default();
        }
        let store = &self.screens[self.active];
        let key = if command.id != 0 {
            store.key_by_id(command.id)
        } else {
            store.key_by_number(command.number)
        };
        let Some((key, id)) = key.and_then(|key| Some((key, store.image(key)?.id))) else {
            let missing = Failure::new("ENOENT", "Put command refers to non-existent image");
            self.reply(command, Err(missing));
            return Outcome::default();
        };
        let outcome = self.put(key, command, cursor);
        // The reply names the image by id even when the command used its number.
        self.reply(&Command { id, ..*command }, Ok(()));
        outcome
    }

    fn put(&mut self, key: u64, command: &Command, cursor: Cursor) -> Outcome {
        let extent =
            self.screens[self.active].put(key, command, (cursor.row, cursor.col), self.cell);
        self.touch();
        // A virtual placement is not on the screen, and `C=1` asks for the cursor to stay.
        Outcome { advance: extent.filter(|_| command.cursor != 1 && command.unicode != 1) }
    }

    /// Queue `job`, reserving the place of its reply among the ones already owed.
    fn queue(&mut self, job: DecodeJob, target: Option<Target>, command: Command) {
        self.last_job += 1;
        let seq = self.last_job;
        if reply_wanted(&command) && self.replies.len() < MAX_REPLIES {
            self.replies.push_back(Reply::Pending(seq));
        }
        self.jobs.push(QueuedJob { seq, job, target, command });
    }

    /// Decodes waiting to be run. Run each and return the results to [`Self::finish_jobs`].
    pub(crate) fn take_jobs(&mut self) -> Vec<QueuedJob> {
        mem::take(&mut self.jobs)
    }

    /// Whether [`Self::take_jobs`] would return anything.
    pub(crate) fn has_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// Install decoded pixels, or remove the image whose data turned out to be unusable.
    ///
    /// An image that was deleted while it decoded, or whose terminal was reset, is no longer
    /// there to receive anything; its result is dropped without touching whatever now holds its
    /// key.
    pub(crate) fn finish_jobs(&mut self, finished: impl IntoIterator<Item = FinishedJob>) {
        for job in finished {
            let live = job.target.filter(|target| {
                self.screens[target.screen].image(target.key).is_some_and(|i| i.uid == target.uid)
            });
            let result = match (job.result, live) {
                (Ok(pixels), Some(target)) => {
                    let store = &mut self.screens[target.screen];
                    let declared = store.image(target.key).map(|image| (image.width, image.height));
                    if declared == Some((pixels.width, pixels.height)) {
                        store.set_pixels(target.key, Arc::new(pixels.rgba), pixels.opaque);
                        Ok(())
                    } else {
                        // The header promised a size the data did not deliver.
                        store.remove_image(target.key);
                        Err(Failure::new("EBADPNG", "Failed to decode PNG data"))
                    }
                },
                (Ok(_), None) => Ok(()),
                (Err(failure), live) => {
                    if let Some(target) = live {
                        self.screens[target.screen].remove_image(target.key);
                    }
                    Err(failure)
                },
            };
            if live.is_some() {
                self.touch();
            }
            let text = reply_text(&job.command, result);
            let slot = self
                .replies
                .iter()
                .position(|reply| matches!(reply, Reply::Pending(seq) if *seq == job.seq));
            match (slot, text) {
                (Some(slot), Some(text)) => self.replies[slot] = Reply::Ready(text),
                (Some(slot), None) => {
                    self.replies.remove(slot);
                },
                (None, _) => {},
            }
        }
    }

    /// Queue the reply to `command`, if the protocol calls for one.
    fn reply(&mut self, command: &Command, result: Result<(), Failure>) {
        if let Some(text) = reply_text(command, result)
            && self.replies.len() < MAX_REPLIES
        {
            self.replies.push_back(Reply::Ready(text));
        }
    }

    /// Next reply to write to the application, if one is ready.
    ///
    /// Replies come out in command order; one still waiting on a decode holds back those after it.
    pub(crate) fn pop_reply(&mut self) -> Option<String> {
        match self.replies.front() {
            Some(Reply::Ready(_)) => match self.replies.pop_front() {
                Some(Reply::Ready(text)) => Some(text),
                _ => None,
            },
            _ => None,
        }
    }

    /// Follow a scroll of the displayed screen's grid.
    pub(crate) fn scroll(&mut self, scroll: Scroll) {
        if self.screens[self.active].scroll(scroll, self.cell) {
            self.touch();
        }
    }

    /// The displayed screen was erased; with `history`, its scrollback too.
    pub(crate) fn clear_screen(&mut self, history: bool) {
        if self.screens[self.active].clear(history) {
            self.touch();
        }
    }

    /// The displayed screen's scrollback was erased.
    pub(crate) fn clear_history(&mut self) {
        if self.screens[self.active].clear_history() {
            self.touch();
        }
    }

    /// The terminal switched to or from the alternate screen.
    ///
    /// The alternate grid is blank every time it is entered and never outlives being left, so its
    /// on-screen placements are dropped at both ends. Its virtual placements stay, as in Kitty,
    /// so that placeholder text drawn after a suspend and resume still finds its images.
    pub(crate) fn set_alternate(&mut self, alternate: bool) {
        let active = usize::from(alternate);
        if active == self.active {
            return;
        }
        // A transmission belongs to the screen it started on.
        self.loader = None;
        self.screens[ALTERNATE].clear(true);
        self.active = active;
        self.screen_epoch = self.screen_epoch.wrapping_add(1);
        self.touch();
    }

    /// Forget everything, as a terminal reset does.
    ///
    /// Decodes already handed out by [`Self::take_jobs`] find their images gone and are dropped.
    pub(crate) fn reset(&mut self) {
        let empty = self.screens.iter().all(ImageStore::is_empty);
        self.screens = [ImageStore::default(), ImageStore::default()];
        self.active = 0;
        self.screen_epoch = self.screen_epoch.wrapping_add(1);
        self.sink = Sink::Idle;
        self.command = None;
        self.loader = None;
        self.jobs.clear();
        self.replies.clear();
        if !empty {
            self.touch();
        }
    }

    /// First cell of every placement, with the screen it is on, for a grid resize to track.
    pub(crate) fn anchors(&self) -> Vec<(bool, u64, i32, i32)> {
        self.screens
            .iter()
            .enumerate()
            .flat_map(|(screen, store)| {
                store.anchors().map(move |(key, row, col)| (screen == ALTERNATE, key, row, col))
            })
            .collect()
    }

    /// Move every placement to where a grid resize put its first cell.
    ///
    /// `moved` is called with a placement's screen and key and returns its new line and column,
    /// or `None` when the cell did not survive.
    pub(crate) fn apply_anchors(&mut self, mut moved: impl FnMut(bool, u64) -> Option<(i32, i32)>) {
        for (screen, store) in self.screens.iter_mut().enumerate() {
            store.apply_anchors(|key| moved(screen == ALTERNATE, key));
        }
        self.touch();
    }
}

/// Whether the protocol allows any reply to `command` at all.
fn reply_wanted(command: &Command) -> bool {
    command.quiet < 2 && (command.id != 0 || command.number != 0)
}

/// The reply to `command`, or `None` when it asked for silence or named no image.
fn reply_text(command: &Command, result: Result<(), Failure>) -> Option<String> {
    use std::fmt::Write;

    if !reply_wanted(command) || (result.is_ok() && command.quiet == 1) {
        return None;
    }
    let mut text = String::from("\x1b_G");
    let mut separator = "";
    for (name, value) in [("i", command.id), ("I", command.number), ("p", command.placement)] {
        if value != 0 {
            // Writing to a `String` cannot fail.
            let _ = write!(text, "{separator}{name}={value}");
            separator = ",";
        }
    }
    match result {
        Ok(()) => text.push_str(";OK"),
        Err(failure) => {
            let _ = write!(text, ";{}:{}", failure.code, failure.message);
        },
    }
    text.push_str("\x1b\\");
    Some(text)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::*;

    const ORIGIN: Cursor = Cursor { row: 0, col: 0 };

    /// Feed one whole command, split into `pieces` runs, and end it.
    fn send_split(
        graphics: &mut KittyGraphics,
        body: &str,
        cursor: Cursor,
        pieces: usize,
    ) -> Outcome {
        graphics.apc_start();
        let bytes = format!("G{body}").into_bytes();
        for piece in bytes.chunks(bytes.len().div_ceil(pieces).max(1)) {
            graphics.apc_put(piece);
        }
        graphics.apc_end(false, cursor)
    }

    fn send(graphics: &mut KittyGraphics, body: &str) -> Outcome {
        send_split(graphics, body, ORIGIN, 1)
    }

    fn replies(graphics: &mut KittyGraphics) -> Vec<String> {
        std::iter::from_fn(|| graphics.pop_reply())
            .map(|reply| reply.trim_start_matches("\x1b_G").trim_end_matches("\x1b\\").to_owned())
            .collect()
    }

    fn rgba(width: u32, height: u32, alpha: u8) -> String {
        STANDARD.encode([9, 9, 9, alpha].repeat((width * height) as usize))
    }

    fn png(width: u32, height: u32) -> String {
        use image::ImageEncoder;
        let mut file = Vec::new();
        image::codecs::png::PngEncoder::new(&mut file)
            .write_image(
                &vec![200; (width * height * 4) as usize],
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        STANDARD.encode(file)
    }

    fn placed(graphics: &KittyGraphics) -> Vec<(i32, i32, u32, u32)> {
        graphics
            .store()
            .placements()
            .iter()
            .map(|p| (p.row, p.col, p.eff_cols, p.eff_rows))
            .collect()
    }

    #[test]
    fn transmit_and_display_places_at_the_cursor_and_asks_it_to_move() {
        let mut graphics = KittyGraphics::new();
        let body = format!("a=T,f=32,s=25,v=41,q=2;{}", rgba(25, 41, 255));
        let outcome = send_split(&mut graphics, &body, Cursor { row: 3, col: 4 }, 1);
        assert_eq!(outcome.advance, Some((3, 3)));
        assert_eq!(placed(&graphics), [(3, 4, 3, 3)]);
        assert!(replies(&mut graphics).is_empty(), "q=2 silences everything");
    }

    #[test]
    fn the_result_is_the_same_however_the_command_is_fragmented() {
        let body = format!("a=T,i=5,s=4,v=4;{}", rgba(4, 4, 255));
        for pieces in [1, 2, 3, 7, 50, body.len() + 1] {
            let mut graphics = KittyGraphics::new();
            let outcome = send_split(&mut graphics, &body, ORIGIN, pieces);
            assert_eq!(outcome.advance, Some((1, 1)), "{pieces} pieces");
            assert_eq!(replies(&mut graphics), ["i=5;OK"], "{pieces} pieces");
        }
    }

    #[test]
    fn c1_and_virtual_placements_leave_the_cursor_alone() {
        let mut graphics = KittyGraphics::new();
        assert_eq!(
            send(&mut graphics, &format!("a=T,C=1,s=4,v=4;{}", rgba(4, 4, 255))).advance,
            None
        );
        assert_eq!(
            send(&mut graphics, &format!("a=T,U=1,i=1,c=2,r=2,s=4,v=4;{}", rgba(4, 4, 255)))
                .advance,
            None
        );
        assert_eq!(
            send(&mut graphics, &format!("a=t,i=2,s=4,v=4;{}", rgba(4, 4, 255))).advance,
            None
        );
        assert_eq!(send(&mut graphics, "a=p,i=2").advance, Some((1, 1)));
        assert_eq!(send(&mut graphics, "a=p,i=2,C=1").advance, None);
    }

    /// What kitim does for every video frame: three chunks, the later ones carrying only `m`.
    #[test]
    fn a_chunked_transmission_is_one_image() {
        let mut graphics = KittyGraphics::new();
        let data = rgba(16, 16, 255);
        let (first, rest) = data.split_at(400);
        let (second, third) = rest.split_at(400);
        assert_eq!(
            send(&mut graphics, &format!("a=T,f=32,s=16,v=16,C=1,q=2,m=1;{first}")),
            Outcome::default()
        );
        assert!(graphics.store().is_empty(), "nothing exists until the last chunk");
        let revision = graphics.revision();
        send(&mut graphics, &format!("q=2,m=1;{second}"));
        assert_eq!(graphics.revision(), revision, "a chunk in the middle changes nothing visible");
        assert!(!graphics.take_dirty());
        send(&mut graphics, &format!("q=2,m=0;{third}"));
        assert_eq!(placed(&graphics), [(0, 0, 2, 1)]);
        assert!(graphics.take_dirty());
        assert!(replies(&mut graphics).is_empty());
    }

    #[test]
    fn frames_without_ids_drawn_in_place_leave_one_image() {
        let mut graphics = KittyGraphics::new();
        for _ in 0..50 {
            send(&mut graphics, &format!("a=T,f=32,s=8,v=8,C=1,q=2;{}", rgba(8, 8, 255)));
        }
        assert_eq!(graphics.store().placements().len(), 1);
    }

    #[test]
    fn replies_follow_the_protocols_quiet_and_identity_rules() {
        let mut graphics = KittyGraphics::new();
        let pixel = rgba(1, 1, 255);
        send(&mut graphics, &format!("a=t,s=1,v=1;{pixel}"));
        assert!(replies(&mut graphics).is_empty(), "no id, no reply");
        send(&mut graphics, &format!("a=t,i=7,s=1,v=1;{pixel}"));
        send(&mut graphics, &format!("a=t,i=8,q=1,s=1,v=1;{pixel}"));
        send(&mut graphics, &format!("a=t,i=9,q=1,s=1,v=2;{pixel}"));
        send(&mut graphics, &format!("a=t,i=9,q=2,s=1,v=2;{pixel}"));
        send(&mut graphics, "a=p,i=7,p=3");
        send(&mut graphics, "a=p,i=99");
        send(&mut graphics, "a=p,i=7,P=7");
        assert_eq!(
            replies(&mut graphics),
            [
                "i=7;OK",
                "i=9;ENODATA:Insufficient image data",
                "i=7,p=3;OK",
                "i=99;ENOENT:Put command refers to non-existent image",
                "i=7;EINVAL:Relative placements are not supported",
            ]
        );
    }

    #[test]
    fn a_query_answers_without_storing_anything() {
        let mut graphics = KittyGraphics::new();
        // The exact probe kitim sends under vvmux.
        send(&mut graphics, "a=q,t=d,f=24,s=1,v=1,i=31,q=2;AAAA");
        send(&mut graphics, "a=q,t=d,f=24,s=1,v=1,i=31;AAAA");
        send(&mut graphics, "a=q,t=d,f=24,s=1,v=1;AAAA");
        send(&mut graphics, "a=q,i=32,t=f;L3RtcC94");
        send(&mut graphics, "a=q,i=33,t=s;L3RtcC94");
        send(&mut graphics, "a=q,i=34,f=24,s=2,v=2;AAAA");
        assert_eq!(
            replies(&mut graphics),
            [
                "i=31;OK",
                "i=32;EINVAL:Unsupported transmission medium",
                "i=33;EINVAL:Unsupported transmission medium",
                "i=34;ENODATA:Insufficient image data",
            ]
        );
        assert!(graphics.store().is_empty());
        assert_eq!(graphics.revision(), 0);
    }

    #[test]
    fn a_query_does_not_disturb_the_image_it_names() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=T,i=31,s=1,v=1;{}", rgba(1, 1, 255)));
        send(&mut graphics, "a=q,i=31,f=24,s=1,v=1;AAAA");
        assert_eq!(graphics.store().placements().len(), 1);
    }

    #[test]
    fn image_numbers_are_given_ids_and_resolve_to_the_newest() {
        let mut graphics = KittyGraphics::new();
        let pixel = rgba(1, 1, 255);
        send(&mut graphics, &format!("a=t,I=40,s=1,v=1;{pixel}"));
        send(&mut graphics, &format!("a=t,I=40,s=1,v=1;{pixel}"));
        send(&mut graphics, "a=p,I=40");
        send(&mut graphics, &format!("a=t,i=1,I=40,s=1,v=1;{pixel}"));
        send(&mut graphics, "a=p,i=1,I=40");
        assert_eq!(
            replies(&mut graphics),
            [
                "i=1,I=40;OK",
                "i=2,I=40;OK",
                "i=2,I=40;OK",
                "i=1,I=40;EINVAL:Must not specify both image id and image number",
                "i=1,I=40;EINVAL:Must not specify both image id and image number",
            ]
        );
        let shown = graphics.store().placements()[0].image;
        assert_eq!(graphics.store().image(shown).unwrap().id, 2);
    }

    #[test]
    fn a_refused_transmission_swallows_its_remaining_chunks_and_replies_once() {
        let mut graphics = KittyGraphics::new();
        let chunk = STANDARD.encode([0; 300]);
        send(&mut graphics, &format!("a=T,i=3,s=1,v=1,m=1;{chunk}"));
        send(&mut graphics, &format!("m=1;{chunk}"));
        send(&mut graphics, &format!("m=0;{chunk}"));
        assert_eq!(replies(&mut graphics), ["i=3;EFBIG:Too much data"]);
        assert!(graphics.store().is_empty());
        // The next transmission starts clean.
        send(&mut graphics, &format!("a=t,i=4,s=1,v=1;{}", rgba(1, 1, 255)));
        assert_eq!(replies(&mut graphics), ["i=4;OK"]);
    }

    #[test]
    fn animation_is_refused_including_its_chunked_frame_data() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=t,i=1,s=1,v=1;{}", rgba(1, 1, 255)));
        send(&mut graphics, "a=f,i=1,s=1,v=1,m=1;AAAA");
        send(&mut graphics, "a=f,m=0;AAAA");
        send(&mut graphics, "a=a,i=1,s=3");
        send(&mut graphics, "a=c,i=1,r=1,c=2");
        assert_eq!(
            replies(&mut graphics),
            [
                "i=1;OK",
                "i=1;EINVAL:Animation is not supported",
                "i=1;EINVAL:Animation is not supported",
                "i=1;EINVAL:Animation is not supported",
            ]
        );
        assert!(graphics.store().key_by_id(1).is_some(), "the still image is untouched");
    }

    #[test]
    fn retransmitting_an_id_removes_the_old_image_even_if_the_new_data_is_bad() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=T,i=6,s=1,v=1;{}", rgba(1, 1, 255)));
        send(&mut graphics, "a=t,i=6,s=2,v=2;AAAA");
        assert!(graphics.store().is_empty());
    }

    #[test]
    fn a_delete_abandons_the_transmission_it_interrupts() {
        let mut graphics = KittyGraphics::new();
        let data = rgba(2, 2, 255);
        let (first, second) = data.split_at(8);
        send(&mut graphics, &format!("a=t,i=1,s=2,v=2,m=1;{first}"));
        send(&mut graphics, "a=d");
        // With the transmission gone this is a new one, for an image of no size.
        send(&mut graphics, &format!("i=1,m=0;{second}"));
        assert_eq!(replies(&mut graphics), ["i=1;EINVAL:Zero width/height not allowed"]);
        assert!(graphics.store().is_empty());
    }

    #[test]
    fn a_put_between_chunks_does_not_disturb_the_transmission() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=t,i=1,s=1,v=1;{}", rgba(1, 1, 255)));
        let data = rgba(2, 2, 255);
        let (first, second) = data.split_at(8);
        send(&mut graphics, &format!("a=t,i=2,s=2,v=2,m=1;{first}"));
        send(&mut graphics, "a=p,i=1");
        send(&mut graphics, &format!("m=0;{second}"));
        assert_eq!(replies(&mut graphics), ["i=1;OK", "i=1;OK", "i=2;OK"]);
    }

    #[test]
    fn a_cancelled_command_does_nothing_and_abandons_its_transmission() {
        let mut graphics = KittyGraphics::new();
        graphics.apc_start();
        graphics.apc_put(format!("Ga=T,i=1,s=1,v=1;{}", rgba(1, 1, 255)).as_bytes());
        assert_eq!(graphics.apc_end(true, ORIGIN), Outcome::default());
        assert!(graphics.store().is_empty());
        assert!(replies(&mut graphics).is_empty());
        assert!(graphics.loader.is_none());
    }

    #[test]
    fn malformed_and_foreign_commands_are_ignored_without_reply() {
        let mut graphics = KittyGraphics::new();
        for body in ["a=T,bogus=1,i=1;AAAA", "a=p,i", &format!("a=p,i=1,{}", "x=1,".repeat(200))] {
            assert_eq!(send(&mut graphics, body), Outcome::default(), "{body:.20}");
        }
        // The Vivid anchor marker is an application program command too, and is not ours.
        graphics.apc_start();
        graphics.apc_put(b"VIVID;3;a=T,i=1,s=1,v=1;AAAA");
        assert_eq!(graphics.apc_end(false, ORIGIN), Outcome::default());
        graphics.apc_start();
        assert_eq!(graphics.apc_end(false, ORIGIN), Outcome::default());
        assert!(replies(&mut graphics).is_empty());
        assert_eq!(graphics.revision(), 0);
    }

    #[test]
    fn png_and_zlib_are_decoded_in_place_by_default() {
        let mut graphics = KittyGraphics::new();
        let outcome = send(&mut graphics, &format!("a=T,f=100,i=1;{}", png(30, 40)));
        assert_eq!(outcome.advance, Some((3, 2)));
        assert!(!graphics.has_jobs());
        let image = graphics.store().image(graphics.store().key_by_id(1).unwrap()).unwrap();
        assert!(image.pixels.is_some());

        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&[255; 16]).unwrap();
        let zlib = STANDARD.encode(encoder.finish().unwrap());
        send(&mut graphics, &format!("a=t,i=2,o=z,s=2,v=2;{zlib}"));
        send(&mut graphics, "a=t,f=100,i=3;AAAA");
        assert_eq!(
            replies(&mut graphics),
            ["i=1;OK", "i=2;OK", "i=3;EBADPNG:Failed to decode PNG data"]
        );
    }

    #[test]
    fn a_deferred_decode_places_from_the_header_and_holds_replies_in_order() {
        let mut graphics = KittyGraphics::new();
        graphics.set_deferred_decode(true);
        let outcome = send(&mut graphics, &format!("a=T,f=100,i=1;{}", png(30, 40)));
        assert_eq!(outcome.advance, Some((3, 2)), "the header is enough to place it");
        send(&mut graphics, &format!("a=t,i=2,s=1,v=1;{}", rgba(1, 1, 255)));
        send(&mut graphics, &format!("a=q,f=100,i=9;{}", png(2, 2)));
        assert!(replies(&mut graphics).is_empty(), "later replies wait behind the decode");

        let key = graphics.store().key_by_id(1).unwrap();
        assert!(graphics.store().image(key).unwrap().pixels.is_none());
        let revision = graphics.revision();

        let jobs = graphics.take_jobs();
        assert_eq!(jobs.len(), 2);
        assert!(!graphics.has_jobs());
        graphics.finish_jobs(jobs.into_iter().map(QueuedJob::run));

        assert_eq!(replies(&mut graphics), ["i=1;OK", "i=2;OK", "i=9;OK"]);
        let image = graphics.store().image(key).unwrap();
        assert_eq!(image.pixels.as_ref().unwrap().len(), 30 * 40 * 4);
        assert_ne!(graphics.revision(), revision, "the pixels arriving is a visible change");
    }

    #[test]
    fn a_deferred_decode_that_fails_takes_its_image_and_placement_with_it() {
        let mut graphics = KittyGraphics::new();
        graphics.set_deferred_decode(true);
        let mut file = STANDARD.decode(png(8, 8)).unwrap();
        let cut = file.len() - 20;
        file.truncate(cut);
        send(&mut graphics, &format!("a=T,f=100,i=1;{}", STANDARD.encode(file)));
        assert_eq!(graphics.store().placements().len(), 1);
        let jobs = graphics.take_jobs();
        graphics.finish_jobs(jobs.into_iter().map(QueuedJob::run));
        assert!(graphics.store().is_empty());
        assert_eq!(replies(&mut graphics), ["i=1;EBADPNG:Failed to decode PNG data"]);
    }

    #[test]
    fn an_image_deleted_while_decoding_is_simply_gone() {
        let mut graphics = KittyGraphics::new();
        graphics.set_deferred_decode(true);
        send(&mut graphics, &format!("a=T,f=100,i=1,q=1;{}", png(8, 8)));
        let jobs = graphics.take_jobs();
        send(&mut graphics, "a=d,d=I,i=1");
        graphics.finish_jobs(jobs.into_iter().map(QueuedJob::run));
        assert!(graphics.store().is_empty());
        assert!(replies(&mut graphics).is_empty(), "q=1 hides the success");
    }

    #[test]
    fn a_reset_drops_decodes_that_are_still_out() {
        let mut graphics = KittyGraphics::new();
        graphics.set_deferred_decode(true);
        send(&mut graphics, &format!("a=T,f=100,i=1;{}", png(8, 8)));
        let jobs = graphics.take_jobs();
        graphics.reset();
        send(&mut graphics, &format!("a=t,i=1,s=1,v=1;{}", rgba(1, 1, 255)));
        graphics.finish_jobs(jobs.into_iter().map(QueuedJob::run));
        let image = graphics.store().image(graphics.store().key_by_id(1).unwrap()).unwrap();
        assert_eq!((image.width, image.height), (1, 1), "the stale decode found nothing to fill");
        assert_eq!(replies(&mut graphics), ["i=1;OK"]);
    }

    /// The two screens are separate owners that happen to use the same ids.
    #[test]
    fn the_two_screens_do_not_share_images_or_placements() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=T,i=1,s=2,v=2;{}", rgba(2, 2, 255)));
        graphics.set_alternate(true);
        assert!(graphics.store().is_empty(), "the alternate screen starts with nothing");
        send(&mut graphics, "a=p,i=1");
        send(&mut graphics, &format!("a=T,i=1,s=4,v=4;{}", rgba(4, 4, 255)));
        send(&mut graphics, "a=d,d=I,i=1");
        assert!(graphics.store().is_empty());

        graphics.set_alternate(false);
        let key = graphics.store().key_by_id(1).unwrap();
        assert_eq!(graphics.store().image(key).unwrap().width, 2, "the primary image is untouched");
        assert_eq!(graphics.store().placements().len(), 1);
        assert_eq!(
            replies(&mut graphics),
            ["i=1;OK", "i=1;ENOENT:Put command refers to non-existent image", "i=1;OK",]
        );
    }

    #[test]
    fn leaving_the_alternate_screen_frees_what_was_on_it_but_keeps_virtual_placements() {
        let mut graphics = KittyGraphics::new();
        graphics.set_alternate(true);
        send(&mut graphics, &format!("a=T,s=2,v=2,q=2;{}", rgba(2, 2, 255)));
        send(&mut graphics, &format!("a=T,U=1,i=5,c=1,r=1,s=2,v=2,q=2;{}", rgba(2, 2, 255)));
        graphics.set_alternate(false);
        graphics.set_alternate(true);
        assert!(!graphics.has_placements());
        assert!(graphics.store().virtual_placement(5, 0).is_some());
    }

    #[test]
    fn clearing_scrolling_and_resetting_reach_only_the_displayed_screen() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=T,i=1,s=2,v=2,q=2;{}", rgba(2, 2, 255)));
        graphics.set_alternate(true);
        send(&mut graphics, &format!("a=T,i=1,s=2,v=2,q=2;{}", rgba(2, 2, 255)));
        graphics.scroll(Scroll { top: 0, end: 10, lines: 1, screen_lines: 10, history_top: 0 });
        assert!(!graphics.has_placements(), "scrolled off a screen with no scrollback");
        graphics.clear_screen(true);
        assert!(graphics.store().is_empty(), "an erase frees images nothing shows, id or not");
        graphics.set_alternate(false);
        assert_eq!(placed(&graphics), [(0, 0, 1, 1)]);

        graphics.clear_screen(false);
        assert!(graphics.store().is_empty());
        let revision = graphics.revision();
        graphics.reset();
        assert_eq!(graphics.revision(), revision, "resetting nothing is not a visible change");
    }

    #[test]
    fn a_new_cell_size_refits_placements_and_is_a_visible_change() {
        let mut graphics = KittyGraphics::new();
        send(&mut graphics, &format!("a=T,s=20,v=20,q=2;{}", rgba(20, 20, 255)));
        assert_eq!(placed(&graphics), [(0, 0, 2, 1)]);
        let revision = graphics.revision();
        graphics.set_cell(CellPixels::new(5, 5));
        assert_eq!(placed(&graphics), [(0, 0, 4, 4)]);
        assert_ne!(graphics.revision(), revision);
        graphics.set_cell(CellPixels::new(5, 5));
        assert_eq!(graphics.revision(), revision + 1, "the same size again changes nothing");
    }

    #[test]
    fn bytes_that_only_accumulate_are_counted_as_quiet() {
        let mut graphics = KittyGraphics::new();
        let body = format!("a=T,s=2,v=2,q=2,m=1;{}", &rgba(2, 2, 255)[..8]);
        send(&mut graphics, &body);
        // `ESC _`, the `G`, the body, and `ESC \\`.
        assert_eq!(graphics.take_quiet_bytes(), 2 + 1 + body.len() + 2);
        assert_eq!(graphics.take_quiet_bytes(), 0);
        assert!(!graphics.take_dirty());
    }

    #[test]
    fn the_reply_queue_is_bounded() {
        let mut graphics = KittyGraphics::new();
        for _ in 0..MAX_REPLIES + 50 {
            send(&mut graphics, "a=p,i=1");
        }
        assert_eq!(replies(&mut graphics).len(), MAX_REPLIES);
    }

    #[test]
    fn resize_anchors_cover_both_screens_and_move_or_drop_placements() {
        let mut graphics = KittyGraphics::new();
        send_split(
            &mut graphics,
            &format!("a=T,s=2,v=2,q=2;{}", rgba(2, 2, 255)),
            Cursor { row: 3, col: 1 },
            1,
        );
        graphics.set_alternate(true);
        send_split(
            &mut graphics,
            &format!("a=T,s=2,v=2,q=2;{}", rgba(2, 2, 255)),
            Cursor { row: 5, col: 2 },
            1,
        );
        let anchors = graphics.anchors();
        assert_eq!(
            anchors.iter().map(|a| (a.0, a.2, a.3)).collect::<Vec<_>>(),
            [(false, 3, 1), (true, 5, 2)]
        );
        graphics.apply_anchors(|alternate, _| (!alternate).then_some((-2, 0)));
        assert!(graphics.store().is_empty(), "the alternate placement lost its cell");
        graphics.set_alternate(false);
        assert_eq!(placed(&graphics), [(-2, 0, 1, 1)]);
    }
}
