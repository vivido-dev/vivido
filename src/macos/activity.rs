//! Keep terminal I/O responsive when macOS considers the application idle or covered.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, ns_string};

/// App Nap throttles timers and I/O for the entire application, including its PTY threads.
/// A terminal remains user-initiated work even when its window loses focus or is covered.
/// Hold the activity for the pane's lifetime and end it when the pane is destroyed.
pub struct TerminalActivity {
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl TerminalActivity {
    pub fn new() -> Self {
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            activity_options(),
            ns_string!("Keep terminal sessions responsive in the background"),
        );
        Self { token }
    }
}

fn activity_options() -> NSActivityOptions {
    // Prevent App Nap without overriding the user's display or system sleep settings.
    NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
        & !(NSActivityOptions::SuddenTerminationDisabled
            | NSActivityOptions::AutomaticTerminationDisabled)
}

impl Drop for TerminalActivity {
    fn drop(&mut self) {
        // SAFETY: the retained token was returned by beginActivityWithOptions:reason: and
        // belongs to this guard, which ends the activity exactly once.
        unsafe { NSProcessInfo::processInfo().endActivity(&self.token) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_activity_allows_system_sleep_and_normal_termination() {
        let options = activity_options();
        assert!(!options.intersects(
            NSActivityOptions::IdleSystemSleepDisabled
                | NSActivityOptions::IdleDisplaySleepDisabled
                | NSActivityOptions::SuddenTerminationDisabled
                | NSActivityOptions::AutomaticTerminationDisabled
        ));
        assert!(!options.is_empty());
    }

    #[test]
    fn terminal_activity_can_be_started_and_released_for_multiple_panes() {
        let first = TerminalActivity::new();
        let second = TerminalActivity::new();
        drop(first);
        drop(second);
    }
}
