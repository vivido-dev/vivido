//! Runtime-independent event delivery.

use std::sync::mpsc;

use winit::event_loop::EventLoopProxy;

use crate::event::Event;

/// Cloneable event sender used by both the native window loop and the headless loop.
#[derive(Clone, Debug)]
pub enum RuntimeProxy {
    Winit(EventLoopProxy<Event>),
    Headless(mpsc::Sender<Event>),
}

impl RuntimeProxy {
    pub fn send_event(&self, event: Event) -> Result<(), ()> {
        match self {
            Self::Winit(proxy) => proxy.send_event(event).map_err(|_| ()),
            Self::Headless(sender) => sender.send(event).map_err(|_| ()),
        }
    }
}

impl From<EventLoopProxy<Event>> for RuntimeProxy {
    fn from(proxy: EventLoopProxy<Event>) -> Self {
        Self::Winit(proxy)
    }
}
