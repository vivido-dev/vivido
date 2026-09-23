use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use accesskit::{
    Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId,
    Rect, Role, TextPosition, TextSelection, Toggled, TreeId, TreeInfo, TreeUpdate,
};
use accesskit_winit::Adapter;
use vivid_protocol::overlay::AccessibleAction;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use super::{AccessibilitySnapshot, OverlaySemantics};
use crate::cli::VividTarget;

const WINDOW_ID: NodeId = NodeId(1);
const TERMINAL_ID: NodeId = NodeId(2);
const FIRST_LINE_ID: u64 = 16;
/// Application semantic nodes start well above any line index a terminal could reach.
const FIRST_OVERLAY_ID: u64 = 1 << 32;

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

/// Forwards an assistive-technology request to the application that owns the node.
///
/// The adapter runs on the platform's accessibility thread, so the handler cannot touch window
/// state; it carries a callback that hands the request to the presenter, which owns the lane.
struct ForwardingActions {
    /// The tree the last update published, so a platform node ID can be mapped back to the
    /// application's own node ID and the window that published it.
    latest: Arc<Mutex<Option<OverlaySemantics>>>,
    invoke:
        Arc<dyn Fn(vivid_protocol::identity::SurfaceIdentity, u64, AccessibleAction) + Send + Sync>,
}

impl ActionHandler for ForwardingActions {
    fn do_action(&mut self, request: ActionRequest) {
        let Some(action) = accessible_action(request.action) else {
            return;
        };
        let Ok(latest) = self.latest.lock() else {
            return;
        };
        let Some(semantics) = latest.as_ref() else {
            return;
        };
        let Some(index) = request
            .target_node
            .0
            .checked_sub(FIRST_OVERLAY_ID)
            .and_then(|index| usize::try_from(index).ok())
        else {
            // A request for the window or terminal node is the host's own, not an application's.
            return;
        };
        let Some(node) = semantics.nodes.get(index) else {
            return;
        };
        (self.invoke)(semantics.window, node.id, action);
    }
}

/// AccessKit's action onto the protocol's closed set. An action the profile does not offer is
/// dropped rather than guessed at.
fn accessible_action(action: Action) -> Option<AccessibleAction> {
    Some(match action {
        // Activation arrives as a click for a button and as a focus request for most else; both
        // mean the node's own default action to the application that asked for one.
        Action::Click => AccessibleAction::Default,
        Action::Focus => AccessibleAction::Focus,
        Action::Increment => AccessibleAction::Increment,
        Action::Decrement => AccessibleAction::Decrement,
        Action::Expand => AccessibleAction::Expand,
        Action::Collapse => AccessibleAction::Collapse,
        _ => return None,
    })
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
    /// The application tree the last update published, for mapping an incoming action back.
    published: Arc<Mutex<Option<OverlaySemantics>>>,
}

impl AccessibilityState {
    pub(crate) fn new(
        event_loop: &ActiveEventLoop,
        window: &Window,
        target: VividTarget,
        snapshot: AccessibilitySnapshot,
        invoke: Arc<
            dyn Fn(vivid_protocol::identity::SurfaceIdentity, u64, AccessibleAction) + Send + Sync,
        >,
    ) -> Self {
        let initial = build_tree(&snapshot, target);
        let latest = Arc::new(Mutex::new(initial));
        let published = Arc::new(Mutex::new(snapshot.semantics.clone()));
        let active = Arc::new(AtomicBool::new(false));
        let adapter = Adapter::with_direct_handlers(
            event_loop,
            window,
            Activation { latest: Arc::clone(&latest), active: Arc::clone(&active) },
            ForwardingActions { latest: Arc::clone(&published), invoke },
            Deactivation { active: Arc::clone(&active) },
        );
        Self { adapter, active, latest, target, last_snapshot: snapshot, published }
    }

    pub(crate) fn process_event(&mut self, window: &Window, event: &WindowEvent) {
        self.adapter.process_event(window, event);
    }

    /// Whether an assistive-technology client has requested this window's tree.
    #[cfg(windows)]
    pub(crate) fn should_sync(&self, visible: bool) -> bool {
        should_sync(self.active.load(Ordering::Acquire), visible)
    }

    pub(crate) fn update(&mut self, snapshot: AccessibilitySnapshot) {
        if snapshot == self.last_snapshot {
            return;
        }
        let update = build_tree(&snapshot, self.target);
        *self.published.lock().expect("published semantics") = snapshot.semantics.clone();
        self.last_snapshot = snapshot;
        *self.latest.lock().unwrap() = update.clone();
        if self.active.load(Ordering::Acquire) {
            self.adapter.update_if_active(|| update);
        }
    }
}

#[cfg(windows)]
fn should_sync(active: bool, visible: bool) -> bool {
    active && visible
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

    // An application's own tree hangs beneath the window, so assistive technology reads it
    // alongside the terminal rather than instead of it.
    if let Some(semantics) = &snapshot.semantics {
        let mut children = root.children().to_vec();
        children.extend((0..semantics.nodes.len()).map(overlay_id));
        root.set_children(children);
        for (index, node) in semantics.nodes.iter().enumerate() {
            let mut accessible = Node::new(overlay_role(node.role));
            if !node.label.is_empty() {
                accessible.set_label(node.label.clone());
            }
            accessible.set_bounds(Rect {
                x0: node.bounds.origin.x.get(),
                y0: node.bounds.origin.y.get(),
                x1: node.bounds.origin.x.get() + node.bounds.width.get(),
                y1: node.bounds.origin.y.get() + node.bounds.height.get(),
            });
            if node.disabled {
                accessible.set_disabled();
            }
            if let Some(level) = node.level {
                accessible.set_level(usize::from(level));
            }
            if let Some([position, size]) = node.set {
                accessible.set_position_in_set(usize::from(position));
                accessible.set_size_of_set(usize::from(size));
            }
            if let Some(toggled) = node.toggled {
                use vivid_protocol::overlay::Toggled as SemanticToggled;
                accessible.set_toggled(match toggled {
                    SemanticToggled::Off => Toggled::False,
                    SemanticToggled::On => Toggled::True,
                    SemanticToggled::Mixed => Toggled::Mixed,
                });
            }
            if let Some([value, minimum, maximum]) = node.numeric {
                accessible.set_numeric_value(value.get());
                accessible.set_min_numeric_value(minimum.get());
                accessible.set_max_numeric_value(maximum.get());
            }
            for action in &node.actions {
                accessible.add_action(overlay_action(*action));
            }
            // A leaf is not expandable, and saying so stops a screen reader offering to expand it.
            if node.children.is_empty() {
                accessible.set_children([]);
            } else {
                accessible.set_children(
                    node.children.iter().map(|c| overlay_id(*c as usize)).collect::<Vec<_>>(),
                );
            }
            nodes.push((overlay_id(index), accessible));
        }
    }

    nodes.push((WINDOW_ID, root));
    TreeUpdate { nodes, tree: Some(TreeInfo::new(WINDOW_ID)), tree_id: TreeId::ROOT, focus }
}

/// Application node IDs live above the terminal's line IDs so the two spaces cannot collide.
fn overlay_id(index: usize) -> NodeId {
    NodeId(FIRST_OVERLAY_ID.saturating_add(u64::try_from(index).unwrap_or(u64::MAX)))
}

/// The protocol's closed role set onto AccessKit's. Every role has a target, so a producer's
/// choice is never silently downgraded to something generic.
fn overlay_role(role: vivid_protocol::overlay::SemanticRole) -> Role {
    use vivid_protocol::overlay::SemanticRole as Semantic;
    match role {
        Semantic::Generic => Role::GenericContainer,
        Semantic::Application => Role::Application,
        Semantic::Group => Role::Group,
        Semantic::Heading => Role::Heading,
        Semantic::Text => Role::Label,
        Semantic::Button => Role::Button,
        Semantic::Switch => Role::Switch,
        Semantic::CheckBox => Role::CheckBox,
        Semantic::RadioButton => Role::RadioButton,
        Semantic::TextInput => Role::TextInput,
        Semantic::Slider => Role::Slider,
        Semantic::SpinButton => Role::SpinButton,
        Semantic::ProgressIndicator => Role::ProgressIndicator,
        Semantic::List => Role::List,
        Semantic::ListItem => Role::ListItem,
        Semantic::Image => Role::Image,
        Semantic::Link => Role::Link,
        Semantic::Dialog => Role::Dialog,
        Semantic::Tab => Role::Tab,
        Semantic::Separator => Role::Splitter,
    }
}

fn overlay_action(action: vivid_protocol::overlay::AccessibleAction) -> Action {
    use vivid_protocol::overlay::AccessibleAction as Accessible;
    match action {
        // Activation arrives as a click, so a node that asks to be activatable advertises one.
        // Every variant has a target here: the set was trimmed to what a toolkit can honor
        // rather than carrying an action a host would have to silently ignore.
        Accessible::Default => Action::Click,
        Accessible::Focus => Action::Focus,
        Accessible::Click => Action::Click,
        Accessible::Increment => Action::Increment,
        Accessible::Decrement => Action::Decrement,
        Accessible::Expand => Action::Expand,
        Accessible::Collapse => Action::Collapse,
    }
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

    #[cfg(windows)]
    #[test]
    fn terminal_document_sync_requires_an_active_client_and_visible_pane() {
        assert!(!should_sync(false, true));
        assert!(!should_sync(true, false));
        assert!(!should_sync(false, false));
        assert!(should_sync(true, true));
    }
}
