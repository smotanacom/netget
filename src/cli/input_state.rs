//! Simple multi-line input state management
//!
//! Replaces tui-textarea with a lightweight implementation
//! for the rolling terminal interface.
//!
//! # Cursor indexing model (read this before editing)
//!
//! `cursor_col` is a **character index** — a count of Unicode scalar values into the
//! current line — and never a byte offset. Every method in this file must agree on
//! that, and the renderer counts characters too.
//!
//! This file previously mixed the two: `insert_char`, `insert_newline`, `delete_char`,
//! `delete_char_forward` and `delete_to_end_of_line` treated `cursor_col` as a byte
//! offset while the word-movement and word-delete methods treated it as a character
//! index. That is a crash, not a style problem: typing `я` once leaves `cursor_col = 1`
//! on a line that is 2 bytes long, and the *second* keystroke calls
//! `String::insert(1, …)` in the middle of a UTF-8 sequence, which panics. Any accented
//! Latin letter, any CJK character and any emoji did it, in the box the user types into.
//!
//! Character index rather than byte index because `cursor_col` is what Left/Right move
//! by: one keypress should traverse one thing the user sees, and a byte is not one.
//!
//! **Known limitation of character (rather than grapheme cluster) semantics:** a
//! multi-codepoint grapheme — a flag (`🇺🇸` = 2 scalars), a family emoji joined with
//! ZWJ, a skin-tone modifier, or combining accents — takes one keypress *per scalar* to
//! cross or erase. That is a cosmetic annoyance; it cannot panic, because every scalar
//! boundary is also a valid UTF-8 boundary. Moving to grapheme semantics means adding a
//! segmentation dependency and changing only these helpers, not the call sites.

use crossterm::event::{KeyCode, KeyModifiers};

/// Number of characters in a line — the unit `cursor_col` is measured in.
fn char_len(line: &str) -> usize {
    line.chars().count()
}

/// Convert a character index into the byte offset `String` methods need.
///
/// Saturates at the end of the line, so an out-of-range `cursor_col` truncates or
/// appends rather than panicking.
fn byte_offset(line: &str, char_idx: usize) -> usize {
    line.char_indices()
        .nth(char_idx)
        .map(|(offset, _)| offset)
        .unwrap_or(line.len())
}

/// Direction for cursor movement
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// Multi-line input state with cursor management
#[derive(Debug, Clone)]
pub struct InputState {
    /// Lines of text
    lines: Vec<String>,
    /// Current cursor row (0-indexed)
    cursor_row: usize,
    /// Current cursor column (0-indexed, **character** offset within the line — see the
    /// module docs; this is deliberately not a byte offset)
    cursor_col: usize,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }
}

impl InputState {
    /// Create a new empty input state
    pub fn new() -> Self {
        Self::default()
    }

    /// Create from existing lines
    pub fn from_lines(lines: Vec<String>) -> Self {
        if lines.is_empty() {
            Self::default()
        } else {
            let cursor_row = lines.len() - 1;
            let cursor_col = char_len(&lines[cursor_row]);
            Self {
                lines,
                cursor_row,
                cursor_col,
            }
        }
    }

    /// Get all lines as a Vec<String>
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Get the full text as a single string with newlines
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Get current cursor position as (row, character column)
    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// Clear all input
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Insert a character at the cursor position
    pub fn insert_char(&mut self, c: char) {
        let offset = byte_offset(&self.lines[self.cursor_row], self.cursor_col);
        self.lines[self.cursor_row].insert(offset, c);
        self.cursor_col += 1;
    }

    /// Insert a newline at the cursor position
    pub fn insert_newline(&mut self) {
        let offset = byte_offset(&self.lines[self.cursor_row], self.cursor_col);
        let rest = self.lines[self.cursor_row][offset..].to_string();
        self.lines[self.cursor_row].truncate(offset);

        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, rest);
        self.cursor_col = 0;
    }

    /// Delete character before cursor (backspace)
    pub fn delete_char(&mut self) {
        if self.cursor_col > 0 {
            // Delete within current line
            let offset = byte_offset(&self.lines[self.cursor_row], self.cursor_col - 1);
            self.lines[self.cursor_row].remove(offset);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            // Join with previous line
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = char_len(&self.lines[self.cursor_row]);
            self.lines[self.cursor_row].push_str(&current_line);
        }
    }

    /// Delete character at cursor (delete key)
    pub fn delete_char_forward(&mut self) {
        if self.cursor_col < char_len(&self.lines[self.cursor_row]) {
            // Delete within current line
            let offset = byte_offset(&self.lines[self.cursor_row], self.cursor_col);
            self.lines[self.cursor_row].remove(offset);
        } else if self.cursor_row < self.lines.len() - 1 {
            // Join with next line
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
    }

    /// Delete from cursor to end of line (Ctrl+K)
    pub fn delete_to_end_of_line(&mut self) {
        let offset = byte_offset(&self.lines[self.cursor_row], self.cursor_col);
        self.lines[self.cursor_row].truncate(offset);
    }

    /// Delete entire line content (Ctrl+U)
    pub fn delete_line(&mut self) {
        self.lines[self.cursor_row].clear();
        self.cursor_col = 0;
    }

    /// Delete word before cursor (Ctrl+W, Alt+Backspace)
    pub fn delete_word(&mut self) {
        if self.cursor_col == 0 {
            return;
        }

        let line = &mut self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        let end_char = self.cursor_col.min(chars.len());

        // Find start of word
        let mut new_col = end_char;

        // Skip trailing whitespace
        while new_col > 0 && chars[new_col - 1].is_whitespace() {
            new_col -= 1;
        }

        // Delete word characters
        while new_col > 0 && !chars[new_col - 1].is_whitespace() {
            new_col -= 1;
        }

        // Remove the range
        let byte_start = byte_offset(line, new_col);
        let byte_end = byte_offset(line, end_char);
        line.replace_range(byte_start..byte_end, "");

        self.cursor_col = new_col;
    }

    /// Delete word after cursor (Alt+Delete, Ctrl+Delete)
    pub fn delete_word_forward(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        if self.cursor_col >= chars.len() {
            return;
        }

        // Find end of word
        let mut end_col = self.cursor_col;

        // Skip leading whitespace
        while end_col < chars.len() && chars[end_col].is_whitespace() {
            end_col += 1;
        }

        // Delete word characters
        while end_col < chars.len() && !chars[end_col].is_whitespace() {
            end_col += 1;
        }

        // Remove the range
        let byte_start = byte_offset(line, self.cursor_col);
        let byte_end = byte_offset(line, end_col);
        line.replace_range(byte_start..byte_end, "");
    }

    /// Move cursor to start of current or previous word (Alt+Left, Ctrl+Left)
    pub fn move_cursor_word_left(&mut self) {
        let line = &self.lines[self.cursor_row];
        if self.cursor_col == 0 {
            // Move to end of previous line
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = char_len(&self.lines[self.cursor_row]);
            }
            return;
        }

        let chars: Vec<char> = line.chars().collect();
        let mut new_col = self.cursor_col.min(chars.len());

        // Skip trailing whitespace
        while new_col > 0 && chars[new_col - 1].is_whitespace() {
            new_col -= 1;
        }

        // Skip word characters
        while new_col > 0 && !chars[new_col - 1].is_whitespace() {
            new_col -= 1;
        }

        self.cursor_col = new_col;
    }

    /// Move cursor to beginning of next word (Alt+Right, Ctrl+Right)
    pub fn move_cursor_word_right(&mut self) {
        let line = &self.lines[self.cursor_row];
        let chars: Vec<char> = line.chars().collect();
        if self.cursor_col >= chars.len() {
            // Move to start of next line
            if self.cursor_row < self.lines.len() - 1 {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            return;
        }

        let mut new_col = self.cursor_col;

        // Skip current word if we're in one
        if new_col < chars.len() && !chars[new_col].is_whitespace() {
            while new_col < chars.len() && !chars[new_col].is_whitespace() {
                new_col += 1;
            }
        }

        // Skip whitespace to reach beginning of next word
        while new_col < chars.len() && chars[new_col].is_whitespace() {
            new_col += 1;
        }

        self.cursor_col = new_col;
    }

    /// Move cursor in the specified direction
    pub fn move_cursor(&mut self, direction: Direction) {
        match direction {
            Direction::Up => {
                if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    // Clamp column to line length
                    let line_len = char_len(&self.lines[self.cursor_row]);
                    self.cursor_col = self.cursor_col.min(line_len);
                }
            }
            Direction::Down => {
                if self.cursor_row < self.lines.len() - 1 {
                    self.cursor_row += 1;
                    // Clamp column to line length
                    let line_len = char_len(&self.lines[self.cursor_row]);
                    self.cursor_col = self.cursor_col.min(line_len);
                }
            }
            Direction::Left => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                } else if self.cursor_row > 0 {
                    // Move to end of previous line
                    self.cursor_row -= 1;
                    self.cursor_col = char_len(&self.lines[self.cursor_row]);
                }
            }
            Direction::Right => {
                let line_len = char_len(&self.lines[self.cursor_row]);
                if self.cursor_col < line_len {
                    self.cursor_col += 1;
                } else if self.cursor_row < self.lines.len() - 1 {
                    // Move to start of next line
                    self.cursor_row += 1;
                    self.cursor_col = 0;
                }
            }
        }
    }

    /// Move cursor to start of line (Ctrl+A, Home)
    pub fn move_to_start_of_line(&mut self) {
        self.cursor_col = 0;
    }

    /// Move cursor to end of line (Ctrl+E, End)
    pub fn move_to_end_of_line(&mut self) {
        self.cursor_col = char_len(&self.lines[self.cursor_row]);
    }

    /// Move cursor to start of input (top-left)
    pub fn move_to_top(&mut self) {
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// Move cursor to end of input (bottom-right)
    pub fn move_to_bottom(&mut self) {
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = char_len(&self.lines[self.cursor_row]);
    }

    /// Check if cursor is on the first line
    pub fn is_on_first_line(&self) -> bool {
        self.cursor_row == 0
    }

    /// Check if cursor is on the last line
    pub fn is_on_last_line(&self) -> bool {
        self.cursor_row == self.lines.len() - 1
    }

    /// Handle a key event and return true if the key was handled
    pub fn handle_key(&mut self, key_code: KeyCode, modifiers: KeyModifiers) -> bool {
        match key_code {
            KeyCode::Char(c) => {
                // Check for special modifiers (not Shift which is normal)
                if modifiers.contains(KeyModifiers::CONTROL)
                    || modifiers.contains(KeyModifiers::ALT)
                {
                    // Let caller handle Ctrl+C, Ctrl+N, etc.
                    return false;
                }
                self.insert_char(c);
                true
            }
            KeyCode::Backspace => {
                self.delete_char();
                true
            }
            KeyCode::Delete => {
                self.delete_char_forward();
                true
            }
            KeyCode::Left => {
                self.move_cursor(Direction::Left);
                true
            }
            KeyCode::Right => {
                self.move_cursor(Direction::Right);
                true
            }
            KeyCode::Up => {
                self.move_cursor(Direction::Up);
                true
            }
            KeyCode::Down => {
                self.move_cursor(Direction::Down);
                true
            }
            KeyCode::Home => {
                self.move_to_start_of_line();
                true
            }
            KeyCode::End => {
                self.move_to_end_of_line();
                true
            }
            _ => false,
        }
    }
}
