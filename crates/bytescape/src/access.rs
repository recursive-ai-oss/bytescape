//! The bridge to the platform's screen-reader interface: UI Automation on
//! Windows. Linux builds have no adapter yet (AT-SPI would bring a D-Bus
//! client and an async runtime into the program), so there nothing attaches
//! and no adapter is made.

use std::time::Duration;

use bs_ui::access::{AccessTree, ActionRequest};
use bs_ui::shell::{Shell, ShellEffect};
use bs_ui::tree::TreeSource;
use winit::event_loop::ActiveEventLoop;
use winit::event_loop::EventLoopProxy;
use winit::window::Window;

/// What the event loop is woken with from other threads.
#[derive(Debug)]
pub enum UserEvent {
    Access(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(event: accesskit_winit::Event) -> Self {
        UserEvent::Access(event)
    }
}

/// Whether this build has a screen-reader interface to offer.
pub const AVAILABLE: bool = cfg!(windows);

/// The adapter and what has been told to it.
pub struct Access {
    adapter: accesskit_winit::Adapter,
    tree: AccessTree,
}

impl Access {
    /// Must run before the window is first shown.
    pub fn new(el: &ActiveEventLoop, window: &Window, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            adapter: accesskit_winit::Adapter::with_event_loop_proxy(el, window, proxy),
            tree: AccessTree::new(),
        }
    }

    /// Lets the adapter see every window event before the app does.
    pub fn process(&mut self, window: &Window, event: &winit::event::WindowEvent) {
        self.adapter.process_event(window, event);
    }

    /// Tells an attached screen reader what changed. Does nothing, and costs
    /// nothing, while none is attached.
    pub fn sync(&mut self, shell: &mut Shell<TreeSource>, now: Duration) {
        let tree = &mut self.tree;
        self.adapter
            .update_if_active(|| shell.access_update(tree, now));
    }

    /// A screen reader attached and wants the whole tree.
    pub fn attach(&mut self, shell: &mut Shell<TreeSource>, now: Duration) {
        let tree = &mut self.tree;
        self.adapter
            .update_if_active(|| shell.access_full(tree, now));
    }

    /// Carries out an action the screen reader asked for.
    pub fn act(
        &mut self,
        shell: &mut Shell<TreeSource>,
        request: &ActionRequest,
        now: Duration,
    ) -> Vec<ShellEffect> {
        shell.access_action(&self.tree, request, now)
    }
}
