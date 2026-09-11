//! Touch activation for mouse-driven Windows shell controls.

use winit::event::{ElementState, MouseButton, TouchPhase, WindowEvent};

/// Convert contact with a shell control into one complete left click.
///
/// Shell controls activate on press, so act on `Started`, just like a mouse. Move first so hit
/// testing uses the contact's physical client coordinates rather than the last mouse position.
/// Complete the click immediately: switching tabs or opening a popup can change focus before the
/// contact ends. Motion, release and cancellation must not activate another control.
///
/// Call this only for shell windows; terminal panes retain their own touch gestures.
pub fn touch_click_events(event: &WindowEvent) -> Option<[WindowEvent; 3]> {
    let WindowEvent::Touch(touch) = event else { return None };
    if touch.phase != TouchPhase::Started {
        return None;
    }
    Some([
        WindowEvent::CursorMoved { device_id: touch.device_id, position: touch.location },
        WindowEvent::MouseInput {
            device_id: touch.device_id,
            state: ElementState::Pressed,
            button: MouseButton::Left,
        },
        WindowEvent::MouseInput {
            device_id: touch.device_id,
            state: ElementState::Released,
            button: MouseButton::Left,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;
    use winit::event::{DeviceId, Touch};

    fn contact(phase: TouchPhase) -> WindowEvent {
        WindowEvent::Touch(Touch {
            device_id: DeviceId::dummy(),
            phase,
            location: PhysicalPosition::new(321.5, 47.25),
            force: None,
            id: 7,
        })
    }

    #[test]
    fn contact_positions_pointer_before_press_and_balances_release() {
        let events = touch_click_events(&contact(TouchPhase::Started)).unwrap();
        assert!(matches!(events[0], WindowEvent::CursorMoved { position, .. }
            if position == PhysicalPosition::new(321.5, 47.25)));
        assert!(matches!(
            events[1],
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. }
        ));
        assert!(matches!(
            events[2],
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            }
        ));
    }

    #[test]
    fn remaining_contact_events_do_not_click_again() {
        for phase in [TouchPhase::Moved, TouchPhase::Ended, TouchPhase::Cancelled] {
            assert!(touch_click_events(&contact(phase)).is_none());
        }
        assert!(touch_click_events(&WindowEvent::Focused(true)).is_none());
    }
}
