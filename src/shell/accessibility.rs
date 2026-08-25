//! Accessibility tree for the integrated tab chrome.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use accesskit::{
    Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId,
    Rect, Role, Tree, TreeId, TreeUpdate,
};
#[cfg(target_os = "linux")]
use accesskit::{TextPosition, TextSelection};
use accesskit_winit::Adapter;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

use crate::accessibility::AccessibilitySnapshot;

use super::menu::NewTabMenu;
use super::{ChromeHitMap, ChromeLayout, PhysicalRect, Tabs};

const WINDOW_ID: NodeId = NodeId(1);
const TAB_LIST_ID: NodeId = NodeId(2);
#[cfg(target_os = "linux")]
const TERMINAL_ID: NodeId = NodeId(3);
const FIRST_TAB_ID: u64 = 0x100;
const FIRST_CLOSE_ID: u64 = 0x1_000;
const NEW_TAB_ID: NodeId = NodeId(0x2_000);
const PREVIOUS_ID: NodeId = NodeId(0x2_001);
const NEXT_ID: NodeId = NodeId(0x2_002);
const MINIMIZE_ID: NodeId = NodeId(0x2_003);
const MAXIMIZE_ID: NodeId = NodeId(0x2_004);
const CLOSE_WINDOW_ID: NodeId = NodeId(0x2_005);
#[cfg(target_os = "linux")]
const MENU_ID: NodeId = NodeId(0x2_006);
#[cfg(target_os = "linux")]
const FIRST_MENU_ITEM_ID: u64 = 0x3_000;
#[cfg(target_os = "linux")]
const FIRST_LINE_ID: u64 = 0x10_000;

/// Command produced by an assistive-technology action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityCommand {
    SelectTab(usize),
    CloseTab(usize),
    NewTab,
    /// Open the `+` button's launch menu.
    ShowNewTabMenu,
    /// Run the launch entry at this index of the open menu.
    MenuItem(usize),
    PreviousTabs,
    NextTabs,
    Minimize,
    ToggleMaximized,
    CloseWindow,
}

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

struct Actions {
    commands: Arc<Mutex<Vec<AccessibilityCommand>>>,
}

impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if let Some(command) = command_for_action(request.action, request.target_node) {
            self.commands.lock().unwrap().push(command);
        }
    }
}

struct Deactivation {
    active: Arc<AtomicBool>,
}

impl DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Everything the chrome is currently showing, as one argument for the tree.
pub(super) struct ChromeState<'a> {
    pub(super) title: &'a str,
    pub(super) tabs: &'a Tabs,
    pub(super) layout: ChromeLayout,
    pub(super) hits: &'a ChromeHitMap,
    pub(super) draw_controls: bool,
    pub(super) menu: Option<&'a NewTabMenu>,
    pub(super) terminal: Option<&'a AccessibilitySnapshot>,
}

/// AccessKit adapter owned by the integrated tab window.
pub(super) struct ShellAccessibility {
    adapter: Adapter,
    active: Arc<AtomicBool>,
    latest: Arc<Mutex<TreeUpdate>>,
    commands: Arc<Mutex<Vec<AccessibilityCommand>>>,
}

impl ShellAccessibility {
    pub(super) fn new(event_loop: &ActiveEventLoop, window: &Window, title: &str) -> Self {
        let latest = Arc::new(Mutex::new(empty_tree(title)));
        let active = Arc::new(AtomicBool::new(false));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let adapter = Adapter::with_direct_handlers(
            event_loop,
            window,
            Activation { latest: Arc::clone(&latest), active: Arc::clone(&active) },
            Actions { commands: Arc::clone(&commands) },
            Deactivation { active: Arc::clone(&active) },
        );
        Self { adapter, active, latest, commands }
    }

    pub(super) fn process_event(&mut self, window: &Window, event: &WindowEvent) {
        self.adapter.process_event(window, event);
    }

    pub(super) fn update(&mut self, state: ChromeState<'_>) {
        let update = build_tree(
            state.title,
            state.tabs,
            state.layout,
            state.hits,
            state.draw_controls,
            state.menu,
            state.terminal,
        );
        *self.latest.lock().unwrap() = update.clone();
        if self.active.load(Ordering::Acquire) {
            self.adapter.update_if_active(|| update);
        }
    }

    pub(super) fn take_commands(&self) -> Vec<AccessibilityCommand> {
        std::mem::take(&mut *self.commands.lock().unwrap())
    }
}

fn empty_tree(title: &str) -> TreeUpdate {
    let mut root = Node::new(Role::Window);
    root.set_label(title);
    TreeUpdate {
        nodes: vec![(WINDOW_ID, root)],
        tree: Some(Tree::new(WINDOW_ID)),
        tree_id: TreeId::ROOT,
        focus: WINDOW_ID,
    }
}

fn build_tree(
    title: &str,
    tabs: &Tabs,
    layout: ChromeLayout,
    hits: &ChromeHitMap,
    draw_controls: bool,
    menu: Option<&NewTabMenu>,
    terminal: Option<&AccessibilitySnapshot>,
) -> TreeUpdate {
    let mut nodes = Vec::new();
    #[allow(unused_mut)]
    let mut root_children = vec![TAB_LIST_ID];
    #[cfg(target_os = "linux")]
    if terminal.is_some() {
        root_children.push(TERMINAL_ID);
    }
    // Windows presents the menu in its own child window, which carries no adapter of its own yet.
    #[cfg(target_os = "linux")]
    if menu.is_some() {
        root_children.push(MENU_ID);
    }
    #[cfg(windows)]
    let _ = menu;

    let mut root = Node::new(Role::Window);
    root.set_label(title);
    root.set_bounds(rect(PhysicalRect {
        x: 0,
        y: 0,
        width: layout.tab_bar.width,
        height: layout.tab_bar.height.saturating_add(layout.content.height),
    }));
    root.set_clips_children();
    root.set_children(root_children);

    let mut tab_list_children = Vec::new();
    for (index, tab_rect) in &hits.tabs {
        let Some(tab) = tabs.as_slice().get(*index) else { continue };
        let tab_node_id = tab_id(*index);
        let close_node_id = close_id(*index);
        tab_list_children.extend([tab_node_id, close_node_id]);

        let mut node = Node::new(Role::Tab);
        node.set_label(tab.title.clone());
        node.set_bounds(rect(*tab_rect));
        node.set_selected(tabs.active_index() == Some(*index));
        node.add_action(Action::Click);
        node.add_action(Action::Focus);
        nodes.push((tab_node_id, node));

        if let Some((_, close_rect)) = hits.tab_closes.iter().find(|(i, _)| i == index) {
            nodes.push((close_node_id, button("Close tab", *close_rect)));
        }
    }
    for (id, label, bounds) in
        [(PREVIOUS_ID, "Previous tabs", hits.previous), (NEXT_ID, "Next tabs", hits.next)]
    {
        if bounds.width > 0 {
            tab_list_children.push(id);
            nodes.push((id, button(label, bounds)));
        }
    }
    if hits.new_tab.width > 0 {
        tab_list_children.push(NEW_TAB_ID);
        let mut new_tab = button("New tab", hits.new_tab);
        // The same button offers the launch menu a right-click opens.
        new_tab.add_action(Action::ShowContextMenu);
        nodes.push((NEW_TAB_ID, new_tab));
    }
    if draw_controls {
        for (id, label, bounds) in [
            (MINIMIZE_ID, "Minimize", hits.minimize),
            (MAXIMIZE_ID, "Maximize or restore", hits.maximize),
            (CLOSE_WINDOW_ID, "Close window", hits.close),
        ] {
            tab_list_children.push(id);
            nodes.push((id, button(label, bounds)));
        }
    }
    let mut tab_list = Node::new(Role::TabList);
    tab_list.set_label("Tabs");
    tab_list.set_bounds(rect(layout.tab_bar));
    tab_list.set_children(tab_list_children);
    nodes.push((TAB_LIST_ID, tab_list));

    #[cfg(target_os = "linux")]
    if let Some(menu) = menu {
        add_menu_nodes(&mut nodes, menu);
    }

    #[cfg(target_os = "linux")]
    if let Some(snapshot) = terminal {
        add_terminal_nodes(&mut nodes, snapshot, layout.content);
    }
    #[cfg(windows)]
    let _ = terminal;

    nodes.push((WINDOW_ID, root));
    #[cfg(target_os = "linux")]
    let focus = if terminal.is_some_and(|snapshot| snapshot.focused) {
        TERMINAL_ID
    } else {
        tabs.active_index().map(tab_id).unwrap_or(WINDOW_ID)
    };
    #[cfg(windows)]
    let focus = tabs.active_index().map(tab_id).unwrap_or(WINDOW_ID);
    TreeUpdate { nodes, tree: Some(Tree::new(WINDOW_ID)), tree_id: TreeId::ROOT, focus }
}

#[cfg(target_os = "linux")]
fn add_menu_nodes(nodes: &mut Vec<(NodeId, Node)>, menu: &NewTabMenu) {
    let mut children = Vec::new();
    for (index, bounds) in menu.rows() {
        let Some(entry) = menu.entries().get(index) else { continue };
        let id = menu_item_id(index);
        children.push(id);
        let mut node = Node::new(Role::MenuItem);
        node.set_label(entry.label.clone());
        node.set_bounds(rect(bounds));
        node.add_action(Action::Click);
        nodes.push((id, node));
    }
    let mut node = Node::new(Role::Menu);
    node.set_label("New tab options");
    node.set_bounds(rect(menu.rect()));
    node.set_children(children);
    nodes.push((MENU_ID, node));
}

#[cfg(target_os = "linux")]
fn add_terminal_nodes(
    nodes: &mut Vec<(NodeId, Node)>,
    snapshot: &AccessibilitySnapshot,
    content: PhysicalRect,
) {
    let children = (0..snapshot.lines.len()).map(line_id).collect::<Vec<_>>();
    let mut terminal = Node::new(Role::Terminal);
    terminal.set_label(snapshot.title.clone());
    terminal.set_read_only();
    terminal.set_clips_children();
    terminal.set_bounds(rect(content));
    terminal.set_children(children);
    terminal.set_text_selection(text_selection(snapshot));
    nodes.push((TERMINAL_ID, terminal));

    for (index, line) in snapshot.lines.iter().enumerate() {
        let mut node = Node::new(Role::TextRun);
        node.set_value(snapshot.text.get(line.bytes.clone()).unwrap_or_default());
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
            x0: f64::from(content.x) + f64::from(snapshot.padding_x),
            y0: f64::from(content.y) + f64::from(line.y),
            x1: f64::from(content.right()) - f64::from(snapshot.padding_x),
            y1: f64::from(content.y) + f64::from(line.y + snapshot.cell_height),
        });
        nodes.push((line_id(index), node));
    }
}

fn button(label: &str, bounds: PhysicalRect) -> Node {
    let mut node = Node::new(Role::Button);
    node.set_label(label);
    node.set_bounds(rect(bounds));
    node.add_action(Action::Click);
    node
}

fn rect(rectangle: PhysicalRect) -> Rect {
    Rect {
        x0: f64::from(rectangle.x),
        y0: f64::from(rectangle.y),
        x1: f64::from(rectangle.right()),
        y1: f64::from(rectangle.bottom()),
    }
}

fn tab_id(index: usize) -> NodeId {
    NodeId(FIRST_TAB_ID.saturating_add(u64::try_from(index).unwrap_or(u64::MAX - FIRST_TAB_ID)))
}

fn close_id(index: usize) -> NodeId {
    NodeId(FIRST_CLOSE_ID.saturating_add(u64::try_from(index).unwrap_or(u64::MAX - FIRST_CLOSE_ID)))
}

#[cfg(target_os = "linux")]
fn menu_item_id(index: usize) -> NodeId {
    NodeId(
        FIRST_MENU_ITEM_ID
            .saturating_add(u64::try_from(index).unwrap_or(u64::MAX - FIRST_MENU_ITEM_ID)),
    )
}

#[cfg(target_os = "linux")]
fn line_id(index: usize) -> NodeId {
    NodeId(FIRST_LINE_ID.saturating_add(u64::try_from(index).unwrap_or(u64::MAX - FIRST_LINE_ID)))
}

fn command_for_action(action: Action, id: NodeId) -> Option<AccessibilityCommand> {
    if action == Action::ShowContextMenu {
        return (id == NEW_TAB_ID).then_some(AccessibilityCommand::ShowNewTabMenu);
    }
    if !matches!(action, Action::Click | Action::Focus) {
        return None;
    }
    command_for_node(id)
}

fn command_for_node(id: NodeId) -> Option<AccessibilityCommand> {
    #[cfg(target_os = "linux")]
    if id.0 >= FIRST_MENU_ITEM_ID && id.0 < FIRST_LINE_ID {
        return usize::try_from(id.0 - FIRST_MENU_ITEM_ID).ok().map(AccessibilityCommand::MenuItem);
    }
    if (FIRST_TAB_ID..FIRST_CLOSE_ID).contains(&id.0) {
        return usize::try_from(id.0 - FIRST_TAB_ID).ok().map(AccessibilityCommand::SelectTab);
    }
    if (FIRST_CLOSE_ID..NEW_TAB_ID.0).contains(&id.0) {
        return usize::try_from(id.0 - FIRST_CLOSE_ID).ok().map(AccessibilityCommand::CloseTab);
    }
    match id {
        NEW_TAB_ID => Some(AccessibilityCommand::NewTab),
        PREVIOUS_ID => Some(AccessibilityCommand::PreviousTabs),
        NEXT_ID => Some(AccessibilityCommand::NextTabs),
        MINIMIZE_ID => Some(AccessibilityCommand::Minimize),
        MAXIMIZE_ID => Some(AccessibilityCommand::ToggleMaximized),
        CLOSE_WINDOW_ID => Some(AccessibilityCommand::CloseWindow),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn text_selection(snapshot: &AccessibilitySnapshot) -> TextSelection {
    let range = snapshot.selection.as_ref().unwrap_or(&snapshot.cursor).scalar.clone();
    TextSelection {
        anchor: text_position(snapshot, range.start),
        focus: text_position(snapshot, range.end),
    }
}

#[cfg(target_os = "linux")]
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

    #[test]
    fn tab_and_close_nodes_map_to_commands() {
        assert_eq!(command_for_node(tab_id(7)), Some(AccessibilityCommand::SelectTab(7)));
        assert_eq!(command_for_node(close_id(3)), Some(AccessibilityCommand::CloseTab(3)));
    }

    #[test]
    fn the_new_tab_button_answers_click_and_context_menu_differently() {
        assert_eq!(
            command_for_action(Action::Click, NEW_TAB_ID),
            Some(AccessibilityCommand::NewTab)
        );
        assert_eq!(
            command_for_action(Action::ShowContextMenu, NEW_TAB_ID),
            Some(AccessibilityCommand::ShowNewTabMenu)
        );
        assert_eq!(command_for_action(Action::ShowContextMenu, CLOSE_WINDOW_ID), None);
        assert_eq!(command_for_action(Action::Increment, NEW_TAB_ID), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn menu_item_nodes_map_to_their_entry() {
        assert_eq!(command_for_node(menu_item_id(2)), Some(AccessibilityCommand::MenuItem(2)));
    }
}
