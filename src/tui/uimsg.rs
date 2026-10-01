//! Results of work that must not run on the event loop.
//!
//! Creating a server, connecting a client and sending through one all perform
//! network I/O. Awaiting them inline in the key handler blocks the whole UI —
//! a connect to a blackholed address parks the dashboard for the kernel's full
//! SYN-retry window, which reads as a freeze. So those actions are spawned and
//! report back through this channel, which the event loop selects on alongside
//! input and ticks.

/// Which modal a spawned action belongs to, so its result lands in the right
/// place even if the user has opened something else meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOrigin {
    Form,
    Routing,
}

#[derive(Debug)]
pub enum UiMsg {
    /// A page API needs input before creating an instance. Show the same form as the
    /// dashboard button, so credentials can be supplied with the normal field editor.
    OpenForm(Box<crate::tui::modal::form::FormModel>),
    /// A spawned action finished. On success the originating modal closes and
    /// `message` goes to chat; on failure the modal stays open and shows it.
    ActionDone {
        origin: ActionOrigin,
        result: Result<String, String>,
    },
    /// Text from background work that is purely informational — it goes to
    /// chat and must never close whatever modal happens to be open.
    Chat(String),
}
