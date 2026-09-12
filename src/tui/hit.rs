//! Mouse hit-testing.
//!
//! ratatui is immediate-mode, so there is no retained widget tree to hit-test
//! against. Instead each renderer pushes the `Rect`s it drew, paired with what
//! they mean, into a per-frame registry; a click walks that registry in
//! reverse (topmost/most-recently-drawn wins, which puts modals above the
//! rail) and the first containing rect decides the action.

use ratatui::layout::Rect;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitTarget {
    /// A row of the instance list, by index into `rail::list_rows`.
    ListRow(usize),
    /// The inspector's body (scroll wheel target).
    InspectorBody,
    /// A tab in the inspector's strip.
    InspectorTab(crate::tui::inspector::InspectorTab),
    /// A selectable line in the inspector's body, by item ordinal.
    InspectorItem(usize),
    /// The activity feed (scroll wheel target).
    Activity,
    /// One visible feed entry, by index into the visible list.
    ActivityRow(usize),
    ChatHistory,
    ChatInput,
    /// A clickable segment of the bottom status bar.
    StatusSegment(SegmentId),
    /// Anywhere inside the active modal (swallows clicks so they do not reach
    /// the panes beneath).
    ModalBody,
    ModalRow(usize),
    ModalButton(ModalButtonId),
    /// A labelled button inside a modal.
    ModalActionButton(ModalAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentId {
    Instances,
    Waiting,
    Model,
    Backend,
    LogLevel,
    WebSearch,
    Handler,
    Scripting,
    Usage,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalButtonId {
    Confirm,
    Cancel,
}

/// A labelled action inside a modal. These are focusable with Tab and
/// clickable — the editors should not depend on remembering shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalAction {
    RoutingAdd,
    RoutingEdit,
    RoutingDelete,
    RoutingMoveUp,
    RoutingMoveDown,
    RoutingSave,
    RoutingCancel,
    DraftSave,
    DraftCancel,
    /// One segment of the handler editor's kind control.
    DraftKind(crate::tui::modal::routing::HandlerKind),
    FormApply,
    FormCancel,
    /// Show the Wireshark command for the instance the form describes — before
    /// it exists, so the capture can be running when it starts.
    FormWireshark,
    /// Open the composer on the pending intercept: pick an action, fill its
    /// fields, send — that answers it.
    InterceptCompose,
    /// Answer with zero actions: acknowledge, send nothing.
    InterceptSend,
    /// Refuse the request: the peer gets the fail-closed reply now.
    InterceptDismiss,
    /// Send the composed action through the client (the composer's button).
    ComposerSend,
    /// Toggle the composer between fields and raw JSON.
    ComposerRaw,
    /// Back out of the composer's field form to the action list.
    ComposerBack,
    /// Accept the text editor's content.
    EditorAccept,
    /// Discard the text editor's content (same as Esc).
    EditorCancel,
    /// The confirm dialog's two answers.
    ConfirmYes,
    ConfirmNo,
}

impl ModalAction {
    pub fn label(&self) -> &'static str {
        match self {
            ModalAction::RoutingAdd => "[ Add ]",
            ModalAction::RoutingEdit => "[ Edit ]",
            ModalAction::RoutingDelete => "[ Delete ]",
            ModalAction::RoutingMoveUp => "[ Move up ]",
            ModalAction::RoutingMoveDown => "[ Move down ]",
            ModalAction::RoutingSave => "[ Save ]",
            ModalAction::RoutingCancel => "[ Cancel ]",
            ModalAction::DraftSave => "[ Save response ]",
            ModalAction::DraftCancel => "[ Cancel ]",
            ModalAction::DraftKind(kind) => kind.label(),
            ModalAction::FormApply => "[ Apply ]",
            ModalAction::FormCancel => "[ Cancel ]",
            ModalAction::FormWireshark => "[ View in Wireshark ]",
            ModalAction::InterceptCompose => "[ Compose answer… ]",
            ModalAction::InterceptSend => "[ Answer with nothing ]",
            ModalAction::InterceptDismiss => "[ Fail closed ]",
            ModalAction::ComposerSend => "[ Send ]",
            ModalAction::ComposerRaw => "[ Raw JSON ]",
            ModalAction::ComposerBack => "[ Back ]",
            ModalAction::EditorAccept => "[ Accept ]",
            ModalAction::EditorCancel => "[ Cancel ]",
            ModalAction::ConfirmYes => "[ Yes ]",
            ModalAction::ConfirmNo => "[ No ]",
        }
    }
}

/// Per-frame registry of drawn regions.
#[derive(Default)]
pub struct HitRegistry {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitRegistry {
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn push(&mut self, area: Rect, target: HitTarget) {
        self.regions.push((area, target));
    }

    /// The topmost target containing this cell, if any.
    pub fn hit(&self, column: u16, row: u16) -> Option<&HitTarget> {
        self.regions
            .iter()
            .rev()
            .find(|(rect, _)| {
                column >= rect.x
                    && column < rect.x.saturating_add(rect.width)
                    && row >= rect.y
                    && row < rect.y.saturating_add(rect.height)
            })
            .map(|(_, target)| target)
    }
}
