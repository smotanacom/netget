//! The action menu: everything that can be done to the selected instance
//! (and to the selected item of its open tab), as one vertical list.
//!
//! It replaced a row of buttons that wrapped onto a second line at forty
//! columns, where ←/→ walked across the wrap. A list has no wrap: ↑/↓, Enter,
//! or the letter shown beside an entry. Disabled entries stay listed with
//! their reason, so a capability that is not available here is not mistaken
//! for one that does not exist.

use crate::tui::app::UiKey;
use crate::tui::inspector::MenuItem;

#[derive(Debug, Clone)]
pub struct ActionMenuModel {
    pub key: UiKey,
    /// The instance, as the inspector titles it.
    pub subject: String,
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ActionMenuModel {
    pub fn new(key: UiKey, subject: String, items: Vec<MenuItem>) -> Self {
        Self {
            key,
            subject,
            items,
            selected: 0,
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let len = self.items.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
    }

    /// The entry a letter names, if any.
    pub fn by_key(&self, c: char) -> Option<usize> {
        self.items
            .iter()
            .position(|i| i.key == Some(c.to_ascii_lowercase()))
    }
}
