//! Native macOS application menus.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem, NSWorkspace};
use objc2_foundation::{MainThreadMarker, NSString, NSURL, ns_string};

use crate::event::{Event, EventSink, EventType};

/// Commands which need Vivido terminal state rather than AppKit's responder chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MenuCommand {
    NewWindow,
    NewTab,
    Copy,
    Paste,
    Find,
    Clear,
}

struct MenuTargetIvars {
    proxy: EventSink,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements and `MenuTarget` adds only Rust-owned
    // ivars which are dropped with the Objective-C object.
    #[unsafe(super(NSObject))]
    #[name = "VividoMenuTarget"]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(newWindow:))]
        fn new_window(&self, _sender: &AnyObject) {
            self.send(MenuCommand::NewWindow);
        }

        #[unsafe(method(newTab:))]
        fn new_tab(&self, _sender: &AnyObject) {
            self.send(MenuCommand::NewTab);
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: &AnyObject) {
            self.send(MenuCommand::Copy);
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: &AnyObject) {
            self.send(MenuCommand::Paste);
        }

        #[unsafe(method(find:))]
        fn find(&self, _sender: &AnyObject) {
            self.send(MenuCommand::Find);
        }

        #[unsafe(method(clear:))]
        fn clear(&self, _sender: &AnyObject) {
            self.send(MenuCommand::Clear);
        }

        #[unsafe(method(openDocumentation:))]
        fn open_documentation(&self, _sender: &AnyObject) {
            let Some(url) = NSURL::URLWithString(ns_string!("https://vivido.dev/docs")) else {
                return;
            };
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }
);

impl MenuTarget {
    fn new(proxy: EventSink) -> Retained<Self> {
        let this = Self::alloc().set_ivars(MenuTargetIvars { proxy });
        // SAFETY: `this` is a newly allocated `MenuTarget`; invoking `NSObject`'s designated
        // initializer establishes a valid Objective-C object.
        unsafe { msg_send![super(this), init] }
    }

    fn send(&self, command: MenuCommand) {
        let _ = self.ivars().proxy.send_event(Event::new(EventType::MacOsMenu(command), None));
    }
}

struct KeyEquivalent<'a> {
    key: &'a NSString,
    modifiers: Option<NSEventModifierFlags>,
}

/// Install Vivido's menus after winit has created the standard application menu.
pub(crate) fn install(proxy: EventSink) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = NSApplication::sharedApplication(mtm);
    let Some(menu_bar) = app.mainMenu() else { return };

    // `new_events(StartCause::Init)` should run once, but keep installation idempotent for event
    // loop embedding and future lifecycle changes.
    if menu_bar.itemWithTitle(ns_string!("File")).is_some() {
        return;
    }

    let target = MenuTarget::new(proxy);

    let file_menu = NSMenu::initWithTitle(mtm.alloc(), ns_string!("File"));
    add_target_item(
        mtm,
        &file_menu,
        ns_string!("New Window"),
        sel!(newWindow:),
        Some(command_key(ns_string!("n"))),
        &target,
    );
    add_target_item(
        mtm,
        &file_menu,
        ns_string!("New Tab"),
        sel!(newTab:),
        Some(command_key(ns_string!("t"))),
        &target,
    );
    file_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_responder_item(
        mtm,
        &file_menu,
        ns_string!("Close"),
        sel!(performClose:),
        Some(command_key(ns_string!("w"))),
    );
    let file_root = add_submenu(mtm, &menu_bar, ns_string!("File"), &file_menu);
    // `NSMenuItem.target` is weak. Keep the shared target alive for the lifetime of the menu bar
    // through the root item's retained represented object.
    // SAFETY: `target` is a valid Objective-C object and represented objects accept any object.
    unsafe { file_root.setRepresentedObject(Some(&target)) };

    let edit_menu = NSMenu::initWithTitle(mtm.alloc(), ns_string!("Edit"));
    for (title, selector, key) in [
        (ns_string!("Copy"), sel!(copy:), ns_string!("c")),
        (ns_string!("Paste"), sel!(paste:), ns_string!("v")),
        (ns_string!("Find"), sel!(find:), ns_string!("f")),
        (ns_string!("Clear"), sel!(clear:), ns_string!("k")),
    ] {
        add_target_item(mtm, &edit_menu, title, selector, Some(command_key(key)), &target);
    }
    add_submenu(mtm, &menu_bar, ns_string!("Edit"), &edit_menu);

    let window_menu = NSMenu::initWithTitle(mtm.alloc(), ns_string!("Window"));
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Minimize"),
        sel!(performMiniaturize:),
        Some(command_key(ns_string!("m"))),
    );
    add_responder_item(mtm, &window_menu, ns_string!("Zoom"), sel!(performZoom:), None);
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Enter Full Screen"),
        sel!(toggleFullScreen:),
        Some(KeyEquivalent {
            key: ns_string!("f"),
            modifiers: Some(NSEventModifierFlags::Control | NSEventModifierFlags::Command),
        }),
    );
    window_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Show Previous Tab"),
        sel!(selectPreviousTab:),
        Some(KeyEquivalent {
            key: ns_string!("\t"),
            modifiers: Some(NSEventModifierFlags::Control | NSEventModifierFlags::Shift),
        }),
    );
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Show Next Tab"),
        sel!(selectNextTab:),
        Some(KeyEquivalent {
            key: ns_string!("\t"),
            modifiers: Some(NSEventModifierFlags::Control),
        }),
    );
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Move Tab to New Window"),
        sel!(moveTabToNewWindow:),
        None,
    );
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Merge All Windows"),
        sel!(mergeAllWindows:),
        None,
    );
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Show Tab Bar"),
        sel!(toggleTabBar:),
        Some(KeyEquivalent {
            key: ns_string!("t"),
            modifiers: Some(NSEventModifierFlags::Command | NSEventModifierFlags::Shift),
        }),
    );
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Show All Tabs"),
        sel!(toggleTabOverview:),
        Some(KeyEquivalent {
            key: ns_string!("\\"),
            modifiers: Some(NSEventModifierFlags::Command | NSEventModifierFlags::Shift),
        }),
    );
    window_menu.addItem(&NSMenuItem::separatorItem(mtm));
    add_responder_item(
        mtm,
        &window_menu,
        ns_string!("Bring All to Front"),
        sel!(arrangeInFront:),
        None,
    );
    add_submenu(mtm, &menu_bar, ns_string!("Window"), &window_menu);
    app.setWindowsMenu(Some(&window_menu));

    let help_menu = NSMenu::initWithTitle(mtm.alloc(), ns_string!("Help"));
    add_target_item(
        mtm,
        &help_menu,
        ns_string!("Vivido documentation"),
        sel!(openDocumentation:),
        None,
        &target,
    );
    add_submenu(mtm, &menu_bar, ns_string!("Help"), &help_menu);
    app.setHelpMenu(Some(&help_menu));
}

fn command_key(key: &NSString) -> KeyEquivalent<'_> {
    KeyEquivalent { key, modifiers: None }
}

fn add_submenu(
    mtm: MainThreadMarker,
    menu_bar: &NSMenu,
    title: &NSString,
    submenu: &NSMenu,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(title);
    item.setSubmenu(Some(submenu));
    menu_bar.addItem(&item);
    item
}

fn add_target_item(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &NSString,
    selector: Sel,
    key: Option<KeyEquivalent<'_>>,
    target: &MenuTarget,
) {
    let item = menu_item(mtm, title, selector, key);
    // SAFETY: `target` implements every selector passed to this helper.
    unsafe { item.setTarget(Some(target)) };
    menu.addItem(&item);
}

fn add_responder_item(
    mtm: MainThreadMarker,
    menu: &NSMenu,
    title: &NSString,
    selector: Sel,
    key: Option<KeyEquivalent<'_>>,
) {
    menu.addItem(&menu_item(mtm, title, selector, key));
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &NSString,
    selector: Sel,
    key: Option<KeyEquivalent<'_>>,
) -> Retained<NSMenuItem> {
    let (key_equivalent, modifiers) =
        key.map(|key| (key.key, key.modifiers)).unwrap_or((ns_string!(""), None));
    // SAFETY: Every selector is either implemented by `MenuTarget` or is a documented AppKit
    // responder-chain action with the standard single-sender signature.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc(),
            title,
            Some(selector),
            key_equivalent,
        )
    };
    if let Some(modifiers) = modifiers {
        item.setKeyEquivalentModifierMask(modifiers);
    }
    item
}
