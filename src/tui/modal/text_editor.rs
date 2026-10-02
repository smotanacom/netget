//! Multi-line editor for instruction text, script code and JSON fields,
//! backed by tui-textarea. Parse-on-accept for JSON targets, so a malformed
//! value is caught before it reaches a form.
//!
//! Tab leaves the text for the `[ Accept ]` / `[ Cancel ]` buttons; typing
//! returns to it. Nothing here requires a chord.

use tui_textarea::TextArea;

use crate::tui::hit::ModalAction;

pub struct TextEditorModel {
    pub label: String,
    pub help: String,
    pub textarea: TextArea<'static>,
    /// Validate as JSON on accept (routing/params fields).
    pub json: bool,
    pub error: Option<String>,
    /// `Some(i)` when focus sits on the i-th button rather than in the text.
    pub focused_button: Option<usize>,
}

impl std::fmt::Debug for TextEditorModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEditorModel")
            .field("label", &self.label)
            .field("json", &self.json)
            .field("error", &self.error)
            .field("focused_button", &self.focused_button)
            .finish()
    }
}

impl Clone for TextEditorModel {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            help: self.help.clone(),
            textarea: self.textarea.clone(),
            json: self.json,
            error: self.error.clone(),
            focused_button: self.focused_button,
        }
    }
}

impl TextEditorModel {
    pub fn new(label: &str, help: &str, initial: &str, json: bool) -> Self {
        let lines: Vec<String> = if initial.is_empty() {
            vec![String::new()]
        } else {
            initial.split('\n').map(|l| l.to_string()).collect()
        };
        Self {
            label: label.to_string(),
            help: help.to_string(),
            textarea: TextArea::new(lines),
            json,
            error: None,
            focused_button: None,
        }
    }

    /// The buttons, in Tab order.
    pub fn buttons(&self) -> Vec<ModalAction> {
        vec![ModalAction::EditorAccept, ModalAction::EditorCancel]
    }

    pub fn focused_action(&self) -> Option<ModalAction> {
        self.focused_button
            .and_then(|index| self.buttons().get(index).copied())
    }

    /// text → Accept → Cancel → text.
    pub fn cycle_focus(&mut self, backward: bool) {
        let buttons = self.buttons().len();
        // Position 0 is the text itself; 1..=buttons are the buttons.
        let current = self.focused_button.map(|i| i + 1).unwrap_or(0);
        let total = buttons + 1;
        let next = if backward {
            (current + total - 1) % total
        } else {
            (current + 1) % total
        };
        self.focused_button = if next == 0 { None } else { Some(next - 1) };
    }

    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Validate and return the text, or an error to display in place.
    pub fn accept(&mut self) -> Option<String> {
        let text = self.text();
        if self.json && !text.trim().is_empty() {
            if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                self.error = Some(format!("invalid JSON: {e}"));
                return None;
            }
        }
        self.error = None;
        Some(text)
    }
}
