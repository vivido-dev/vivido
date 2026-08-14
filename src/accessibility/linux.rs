use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use accesskit::{
    ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId, Rect, Role,
    TextPosition, TextSelection, Tree, TreeId, TreeUpdate,
};
use accesskit_winit::Adapter;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use super::AccessibilitySnapshot;
use crate::cli::VividTarget;

const WINDOW_ID: NodeId = NodeId(1);
const TERMINAL_ID: NodeId = NodeId(2);
const FIRST_LINE_ID: u64 = 16;

struct Activation {
    latest: Arc<Mutex<TreeUpdate>>,
    active: Arc<AtomicBool>,
}

impl ActivationHandler for Activation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.active.store(true, Ordering::Release);
        Some(self.latest.lock().unwrap().clone())
    }
}

struct ReadOnlyActions;

impl ActionHandler for ReadOnlyActions {
    fn do_action(&mut self, _request: ActionRequest) {}
}

struct Deactivation {
    active: Arc<AtomicBool>,
}

impl DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

pub(crate) struct AccessibilityState {
    adapter: Adapter,
    active: Arc<AtomicBool>,
    latest: Arc<Mutex<TreeUpdate>>,
    target: VividTarget,
    last_snapshot: AccessibilitySnapshot,
}

impl AccessibilityState {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        window: &Window,
        target: VividTarget,
        snapshot: AccessibilitySnapshot,
    ) -> Self {
        let initial = build_tree(&snapshot, target);
        let latest = Arc::new(Mutex::new(initial));
        let active = Arc::new(AtomicBool::new(false));
        let adapter = Adapter::with_direct_handlers(
            event_loop,
            window,
            Activation { latest: Arc::clone(&latest), active: Arc::clone(&active) },
            ReadOnlyActions,
            Deactivation { active: Arc::clone(&active) },
        );
        Self { adapter, active, latest, target, last_snapshot: snapshot }
    }

    pub(crate) fn process_event(&mut self, window: &Window, event: &WindowEvent) {
        self.adapter.process_event(window, event);
    }

    pub(crate) fn update(&mut self, snapshot: AccessibilitySnapshot) {
        if snapshot == self.last_snapshot {
            return;
        }
        self.last_snapshot = snapshot.clone();
        let update = build_tree(&snapshot, self.target);
        *self.latest.lock().unwrap() = update.clone();
        if self.active.load(Ordering::Acquire) {
            self.adapter.update_if_active(|| update);
        }
    }
}

fn line_id(index: usize) -> NodeId {
    NodeId(FIRST_LINE_ID.saturating_add(u64::try_from(index).unwrap_or(u64::MAX - FIRST_LINE_ID)))
}

fn build_tree(snapshot: &AccessibilitySnapshot, target: VividTarget) -> TreeUpdate {
    let mut root = Node::new(Role::Window);
    root.set_label(snapshot.title.clone());
    root.set_bounds(Rect {
        x0: 0.0,
        y0: 0.0,
        x1: f64::from(snapshot.width),
        y1: f64::from(snapshot.height),
    });
    root.set_clips_children();

    let mut nodes = Vec::with_capacity(snapshot.lines.len().saturating_add(2));
    let focus =
        if target == VividTarget::Terminal && snapshot.focused { TERMINAL_ID } else { WINDOW_ID };

    if target == VividTarget::Terminal {
        let children: Vec<_> = (0..snapshot.lines.len()).map(line_id).collect();
        root.set_children([TERMINAL_ID]);

        let mut terminal = Node::new(Role::Terminal);
        terminal.set_label(snapshot.title.clone());
        terminal.set_read_only();
        terminal.set_clips_children();
        terminal.set_bounds(Rect {
            x0: f64::from(snapshot.padding_x),
            y0: f64::from(snapshot.padding_y),
            x1: f64::from(snapshot.width - snapshot.padding_x),
            y1: f64::from(snapshot.height - snapshot.padding_y),
        });
        terminal.set_children(children);
        terminal.set_text_selection(text_selection(snapshot));
        nodes.push((TERMINAL_ID, terminal));

        for (index, line) in snapshot.lines.iter().enumerate() {
            let value = snapshot.text.get(line.bytes.clone()).unwrap_or_default();
            let mut node = Node::new(Role::TextRun);
            node.set_value(value);
            node.set_character_lengths(
                line.characters
                    .iter()
                    .map(|character| {
                        u8::try_from(character.bytes.end.saturating_sub(character.bytes.start))
                            .unwrap_or(4)
                    })
                    .collect::<Vec<_>>(),
            );
            node.set_character_positions(
                line.characters
                    .iter()
                    .map(|character| character.x - snapshot.padding_x)
                    .collect::<Vec<_>>(),
            );
            node.set_character_widths(
                line.characters.iter().map(|character| character.width).collect::<Vec<_>>(),
            );
            node.set_bounds(Rect {
                x0: f64::from(snapshot.padding_x),
                y0: f64::from(line.y),
                x1: f64::from(snapshot.width - snapshot.padding_x),
                y1: f64::from(line.y + snapshot.cell_height),
            });
            nodes.push((line_id(index), node));
        }
    }

    nodes.push((WINDOW_ID, root));
    TreeUpdate { nodes, tree: Some(Tree::new(WINDOW_ID)), tree_id: TreeId::ROOT, focus }
}

fn text_selection(snapshot: &AccessibilitySnapshot) -> TextSelection {
    let range = snapshot.selection.as_ref().unwrap_or(&snapshot.cursor).scalar.clone();
    TextSelection {
        anchor: text_position(snapshot, range.start),
        focus: text_position(snapshot, range.end),
    }
}

fn text_position(snapshot: &AccessibilitySnapshot, offset: usize) -> TextPosition {
    for (index, line) in snapshot.lines.iter().enumerate() {
        if offset <= line.range.scalar.end {
            return TextPosition {
                node: line_id(index),
                character_index: offset.saturating_sub(line.range.scalar.start),
            };
        }
    }
    let index = snapshot.lines.len().saturating_sub(1);
    TextPosition {
        node: line_id(index),
        character_index: snapshot.lines.last().map_or(0, |line| line.characters.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::terminal::event::VoidListener;
    use crate::terminal::term::test::TermSize;

    #[test]
    fn desktop_tree_has_only_window_role() {
        let term = crate::terminal::term::Term::<VoidListener>::new(
            Default::default(),
            &TermSize::new(4, 2),
            VoidListener,
        );
        let size = crate::display::SizeInfo::new(40.0, 20.0, 10.0, 10.0, 0.0, 0.0, false);
        let update =
            build_tree(&AccessibilitySnapshot::new(&term, size, "Vivido"), VividTarget::Desktop);
        assert_eq!(update.nodes.len(), 1);
        assert_eq!(update.nodes[0].0, WINDOW_ID);
        assert!(update.nodes[0].1.children().is_empty());
    }
}
