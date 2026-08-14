use std::ops::Range;
use std::sync::{Arc, Mutex};

use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityElement, NSAccessibilityFocusedUIElementChangedNotification,
    NSAccessibilityLayoutChangedNotification, NSAccessibilityPostNotification, NSAccessibilityRole,
    NSAccessibilitySelectedTextChangedNotification, NSAccessibilityTextAreaRole,
    NSAccessibilityTitleChangedNotification, NSAccessibilityValueChangedNotification, NSView,
};
use objc2_foundation::{
    NSArray, NSCopying, NSInteger, NSPoint, NSRange, NSRect, NSSize, NSString, NSValue,
};
use winit::raw_window_handle::RawWindowHandle;

use super::{AccessibilitySnapshot, AccessibleRange};
use crate::cli::VividTarget;
use crate::display::window::Window;

struct TerminalElementIvars {
    snapshot: Arc<Mutex<AccessibilitySnapshot>>,
    parent: Weak<NSView>,
}

define_class!(
    #[unsafe(super(NSAccessibilityElement, objc2_foundation::NSObject))]
    #[name = "VividoAccessibilityTerminal"]
    #[ivars = TerminalElementIvars]
    struct TerminalElement;

    impl TerminalElement {
        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn role(&self) -> Retained<NSAccessibilityRole> {
            unsafe { NSAccessibilityTextAreaRole }.copy()
        }

        #[unsafe(method_id(accessibilityRoleDescription))]
        fn role_description(&self) -> Retained<NSString> {
            NSString::from_str("terminal")
        }

        #[unsafe(method_id(accessibilityTitle))]
        fn title(&self) -> Retained<NSString> {
            NSString::from_str(&self.snapshot().title)
        }

        #[unsafe(method_id(accessibilityValue))]
        fn value(&self) -> Retained<NSString> {
            NSString::from_str(&self.snapshot().text)
        }

        #[unsafe(method(accessibilityFrame))]
        fn frame(&self) -> NSRect {
            self.ivars()
                .parent
                .load()
                .map_or(NSRect::ZERO, |view| NSAccessibility::accessibilityFrame(&*view))
        }

        #[unsafe(method_id(accessibilityParent))]
        fn parent(&self) -> Option<Retained<AnyObject>> {
            self.ivars()
                .parent
                .load()
                .map(|view| view.into_super().into_super().into_super())
        }

        #[unsafe(method_id(accessibilityWindow))]
        fn window(&self) -> Option<Retained<AnyObject>> {
            self.ivars()
                .parent
                .load()
                .and_then(|view| view.window())
                .map(|window| window.into_super().into_super().into_super())
        }

        #[unsafe(method_id(accessibilityTopLevelUIElement))]
        fn top_level(&self) -> Option<Retained<AnyObject>> {
            self.ivars()
                .parent
                .load()
                .and_then(|view| view.window())
                .map(|window| window.into_super().into_super().into_super())
        }

        #[unsafe(method(isAccessibilityFocused))]
        fn is_focused(&self) -> bool {
            self.snapshot().focused
        }

        #[unsafe(method(isAccessibilityEnabled))]
        fn is_enabled(&self) -> bool {
            true
        }

        #[unsafe(method(accessibilityNumberOfCharacters))]
        fn number_of_characters(&self) -> NSInteger {
            NSInteger::try_from(self.snapshot().text.encode_utf16().count()).unwrap_or(NSInteger::MAX)
        }

        #[unsafe(method(accessibilityVisibleCharacterRange))]
        fn visible_character_range(&self) -> NSRange {
            to_ns_range(&self.snapshot().visible.utf16)
        }

        #[unsafe(method(accessibilitySelectedTextRange))]
        fn selected_text_range(&self) -> NSRange {
            to_ns_range(&primary_selection(&self.snapshot()).utf16)
        }

        #[unsafe(method_id(accessibilitySelectedTextRanges))]
        fn selected_text_ranges(&self) -> Retained<NSArray<NSValue>> {
            let snapshot = self.snapshot();
            let ranges: Vec<_> = if snapshot.block_selection.is_empty() {
                vec![primary_selection(&snapshot)]
            } else {
                snapshot.block_selection.clone()
            };
            let values: Vec<_> = ranges
                .iter()
                .map(|range| unsafe { NSValue::valueWithRange(to_ns_range(&range.utf16)) })
                .collect();
            NSArray::from_retained_slice(&values)
        }

        #[unsafe(method_id(accessibilitySelectedText))]
        fn selected_text(&self) -> Retained<NSString> {
            let snapshot = self.snapshot();
            NSString::from_str(&snapshot.utf16_text(primary_selection(&snapshot).utf16))
        }

        #[unsafe(method(accessibilityInsertionPointLineNumber))]
        fn insertion_point_line_number(&self) -> NSInteger {
            let snapshot = self.snapshot();
            let cursor = snapshot.cursor.utf16.start;
            NSInteger::try_from(
                snapshot
                    .lines
                    .iter()
                    .position(|line| cursor <= line.range.utf16.end)
                    .unwrap_or_else(|| snapshot.lines.len().saturating_sub(1)),
            )
            .unwrap_or(NSInteger::MAX)
        }

        #[unsafe(method(accessibilityRangeForLine:))]
        fn range_for_line(&self, line: NSInteger) -> NSRange {
            usize::try_from(line)
                .ok()
                .and_then(|line| self.snapshot().lines.get(line).map(|line| to_ns_range(&line.range.utf16)))
                .unwrap_or_default()
        }

        #[unsafe(method(accessibilityLineForIndex:))]
        fn line_for_index(&self, index: NSInteger) -> NSInteger {
            let Ok(index) = usize::try_from(index) else { return 0 };
            NSInteger::try_from(
                self.snapshot()
                    .lines
                    .iter()
                    .position(|line| index <= line.range.utf16.end)
                    .unwrap_or(0),
            )
            .unwrap_or(NSInteger::MAX)
        }

        #[unsafe(method(accessibilityRangeForIndex:))]
        fn range_for_index(&self, index: NSInteger) -> NSRange {
            let Ok(index) = usize::try_from(index) else { return NSRange::default() };
            let snapshot = self.snapshot();
            snapshot
                .lines
                .iter()
                .flat_map(|line| &line.characters)
                .find(|character| character.utf16.contains(&index))
                .map(|character| to_ns_range(&character.utf16))
                .unwrap_or_else(|| NSRange::new(index.min(snapshot.text.encode_utf16().count()), 0))
        }

        #[unsafe(method_id(accessibilityStringForRange:))]
        fn string_for_range(&self, range: NSRange) -> Retained<NSString> {
            NSString::from_str(&self.snapshot().utf16_text(from_ns_range(range)))
        }

        #[unsafe(method(accessibilityFrameForRange:))]
        fn frame_for_range(&self, range: NSRange) -> NSRect {
            self.range_frame(from_ns_range(range))
        }

        #[unsafe(method(accessibilityRangeForPosition:))]
        fn range_for_position(&self, point: NSPoint) -> NSRange {
            let Some(view) = self.ivars().parent.load() else { return NSRange::default() };
            let Some(window) = view.window() else { return NSRange::default() };
            let point = window.convertPointFromScreen(point);
            let point = view.convertPoint_fromView(point, None);
            let point = view.convertPointToBacking(point);
            let snapshot = self.snapshot();
            let y = snapshot.height - point.y as f32;
            snapshot
                .lines
                .iter()
                .filter(|line| y >= line.y && y < line.y + snapshot.cell_height)
                .flat_map(|line| &line.characters)
                .min_by(|left, right| {
                    let left_distance = (point.x as f32 - left.x).abs();
                    let right_distance = (point.x as f32 - right.x).abs();
                    left_distance.total_cmp(&right_distance)
                })
                .map(|character| to_ns_range(&character.utf16))
                .unwrap_or_default()
        }

        #[unsafe(method(isAccessibilitySelectorAllowed:))]
        fn is_selector_allowed(&self, selector: Sel) -> bool {
            selector == sel!(isAccessibilityElement)
                || selector == sel!(accessibilityRole)
                || selector == sel!(accessibilityRoleDescription)
                || selector == sel!(accessibilityTitle)
                || selector == sel!(accessibilityValue)
                || selector == sel!(accessibilityFrame)
                || selector == sel!(accessibilityParent)
                || selector == sel!(accessibilityWindow)
                || selector == sel!(accessibilityTopLevelUIElement)
                || selector == sel!(isAccessibilityFocused)
                || selector == sel!(isAccessibilityEnabled)
                || selector == sel!(accessibilityNumberOfCharacters)
                || selector == sel!(accessibilityVisibleCharacterRange)
                || selector == sel!(accessibilitySelectedTextRange)
                || selector == sel!(accessibilitySelectedTextRanges)
                || selector == sel!(accessibilitySelectedText)
                || selector == sel!(accessibilityInsertionPointLineNumber)
                || selector == sel!(accessibilityRangeForLine:)
                || selector == sel!(accessibilityLineForIndex:)
                || selector == sel!(accessibilityRangeForIndex:)
                || selector == sel!(accessibilityStringForRange:)
                || selector == sel!(accessibilityFrameForRange:)
                || selector == sel!(accessibilityRangeForPosition:)
                || selector == sel!(isAccessibilitySelectorAllowed:)
        }
    }
);

impl TerminalElement {
    fn new(snapshot: Arc<Mutex<AccessibilitySnapshot>>, parent: &NSView) -> Retained<Self> {
        let this =
            Self::alloc().set_ivars(TerminalElementIvars { snapshot, parent: Weak::new(parent) });
        unsafe { msg_send![super(this), init] }
    }

    fn snapshot(&self) -> std::sync::MutexGuard<'_, AccessibilitySnapshot> {
        self.ivars().snapshot.lock().unwrap()
    }

    fn range_frame(&self, range: Range<usize>) -> NSRect {
        let Some(view) = self.ivars().parent.load() else { return NSRect::ZERO };
        let snapshot = self.snapshot();
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for line in &snapshot.lines {
            for character in &line.characters {
                if character.utf16.start < range.end && character.utf16.end > range.start {
                    min_x = min_x.min(character.x);
                    min_y = min_y.min(line.y);
                    max_x = max_x.max(character.x + character.width.max(snapshot.cell_width));
                    max_y = max_y.max(line.y + snapshot.cell_height);
                }
            }
        }
        if min_x == f32::MAX {
            return NSRect::ZERO;
        }
        let backing = NSRect::new(
            NSPoint::new(f64::from(min_x), f64::from(snapshot.height - max_y)),
            NSSize::new(f64::from(max_x - min_x), f64::from(max_y - min_y)),
        );
        let view_rect = view.convertRectFromBacking(backing);
        let window_rect = view.convertRect_toView(view_rect, None);
        view.window().map_or(NSRect::ZERO, |window| window.convertRectToScreen(window_rect))
    }
}

pub(crate) struct AccessibilityState {
    snapshot: Option<Arc<Mutex<AccessibilitySnapshot>>>,
    element: Option<Retained<TerminalElement>>,
    parent: Option<Weak<NSView>>,
}

impl AccessibilityState {
    pub(crate) fn new(
        window: &Window,
        target: VividTarget,
        snapshot: AccessibilitySnapshot,
    ) -> Self {
        if target != VividTarget::Terminal {
            return Self { snapshot: None, element: None, parent: None };
        }
        let view = match window.raw_window_handle() {
            Some(RawWindowHandle::AppKit(handle)) => unsafe {
                handle.ns_view.cast::<NSView>().as_ref()
            },
            _ => return Self { snapshot: None, element: None, parent: None },
        };
        let snapshot = Arc::new(Mutex::new(snapshot));
        let element = TerminalElement::new(Arc::clone(&snapshot), view);
        let child: Retained<AnyObject> = element.clone().into_super().into_super().into_super();
        let children = NSArray::from_retained_slice(&[child]);
        unsafe { NSAccessibility::setAccessibilityChildren(view, Some(&children)) };
        Self { snapshot: Some(snapshot), element: Some(element), parent: Some(Weak::new(view)) }
    }

    pub(crate) fn update(&mut self, snapshot: AccessibilitySnapshot) {
        let (Some(shared), Some(element)) = (&self.snapshot, &self.element) else { return };
        let mut current = shared.lock().unwrap();
        if *current == snapshot {
            return;
        }
        let text_changed = current.text != snapshot.text;
        let selection_changed = current.selection != snapshot.selection
            || current.block_selection != snapshot.block_selection
            || current.cursor != snapshot.cursor;
        let layout_changed = current.visible != snapshot.visible
            || current.width != snapshot.width
            || current.height != snapshot.height;
        let title_changed = current.title != snapshot.title;
        let focus_changed = current.focused != snapshot.focused;
        *current = snapshot;
        drop(current);

        let object: &AnyObject = element;
        if text_changed {
            unsafe {
                NSAccessibilityPostNotification(object, NSAccessibilityValueChangedNotification);
            }
        }
        if selection_changed {
            unsafe {
                NSAccessibilityPostNotification(
                    object,
                    NSAccessibilitySelectedTextChangedNotification,
                );
            }
        }
        if layout_changed {
            unsafe {
                NSAccessibilityPostNotification(object, NSAccessibilityLayoutChangedNotification);
            }
        }
        if title_changed {
            unsafe {
                NSAccessibilityPostNotification(object, NSAccessibilityTitleChangedNotification);
            }
        }
        if focus_changed {
            unsafe {
                NSAccessibilityPostNotification(
                    object,
                    NSAccessibilityFocusedUIElementChangedNotification,
                );
            }
        }
    }
}

impl Drop for AccessibilityState {
    fn drop(&mut self) {
        if let Some(parent) = self.parent.as_ref().and_then(Weak::load) {
            unsafe { NSAccessibility::setAccessibilityChildren(&*parent, None) };
        }
    }
}

fn primary_selection(snapshot: &AccessibilitySnapshot) -> AccessibleRange {
    snapshot
        .selection
        .clone()
        .or_else(|| snapshot.block_selection.first().cloned())
        .unwrap_or_else(|| snapshot.cursor.clone())
}

fn to_ns_range(range: &Range<usize>) -> NSRange {
    NSRange::new(range.start, range.end.saturating_sub(range.start))
}

fn from_ns_range(range: NSRange) -> Range<usize> {
    range.location..range.location.saturating_add(range.length)
}
