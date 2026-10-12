use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSString, NSUserDefaults, ns_string};

pub mod activity;
pub mod locale;
pub mod menu;
pub mod proc;

/// Disable AppKit text-completion features that interfere with terminal input.
pub fn disable_autofill() {
    // SAFETY: Foundation retains these initialized dictionary keys and values; no Rust borrows escape.
    unsafe {
        NSUserDefaults::standardUserDefaults().registerDefaults(
            &NSDictionary::<NSString, AnyObject>::from_slices(
                &[ns_string!("NSAutoFillHeuristicControllerEnabled")],
                &[ns_string!("NO")],
            ),
        );
    }
}
