//! The bridge to the platform's screen-reader interface: UI Automation on
//! Windows and AT-SPI on Linux. On Linux the adapter is made only after both
//! of the buses it talks to were checked to be local sockets (see
//! `bs_platform::a11y_bus`); when that check refuses, the program runs
//! without it.

use std::sync::atomic::{AtomicBool, Ordering};
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

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether a screen-reader adapter should be made for this run.
pub fn available() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Decides whether an adapter is made. On Linux this first checks the buses
/// it would use and pins the accessibility bus address, which is why it must
/// run at the start of `main`, before any thread exists. When a bus is
/// refused, one line says so and the run goes on without the adapter. With
/// `skip` set (measurements that compare against no adapter) nothing is
/// checked and no adapter is made.
pub fn prepare(skip: bool) {
    if skip {
        return;
    }
    #[cfg(target_os = "linux")]
    match bs_platform::a11y_bus::pin_for_adapter() {
        Ok(()) => ENABLED.store(true, Ordering::Relaxed),
        Err(refusal) => eprintln!("bytescape: screen-reader support is off: {refusal}"),
    }
    #[cfg(windows)]
    ENABLED.store(true, Ordering::Relaxed);
}

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
