//! Keyboard and mouse handling for the panes.
//!
//! Precedence: an open modal owns everything (`modal_keys`); then the global
//! toggles; then the focused pane. Every pane action funnels into
//! `actions::run` with an `InstanceAction`, so a letter, Enter on a button and
//! a click do the same thing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use crate::events::EventHandler;
use crate::state::app_state::AppState;
use crate::tui::actions;
use crate::tui::app::{DashboardApp, Focus, UiKey};
use crate::tui::hit::{HitTarget, SegmentId};
use crate::tui::inspector::{self, InspectorTab, InstanceAction};
use crate::tui::modal::Modal;
use crate::tui::rail::{is_selectable, list_rows, ListRow};
use crate::tui::uimsg::{ActionOrigin, UiMsg};

/// What the event loop should do after handling an event.
pub enum Outcome {
    Continue,
    Quit,
}

pub async fn handle_key(
    app: &mut DashboardApp,
    key: KeyEvent,
    state: &AppState,
    event_handler: &EventHandler,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Outcome {
    app.dirty = true;

    if app.modal().is_some() {
        return crate::tui::modal_keys::handle_modal_key(app, key, state).await;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Readline chords on text being typed come before the global toggles:
    // Ctrl-E and Ctrl-W mean "end of line" and "delete word" to anyone with
    // a line in the box, and only cycle scripting / web search once it is
    // empty. The legacy TUI never resolved this and lost the editing keys.
    if app.focus == Focus::ChatInput && !app.input.text().is_empty() {
        if readline_key(app, key) {
            let text = app.input.text();
            app.core.update_slash_suggestions(&text);
            return Outcome::Continue;
        }
    }

    match key.code {
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => return Outcome::Quit,
        KeyCode::Char('l') | KeyCode::Char('L') if ctrl => {
            let level = app.core.log_level.cycle();
            app.core.set_log_level(level);
            app.push_system(format!("Log level: {}", level.as_str()));
            return Outcome::Continue;
        }
        KeyCode::Char('w') | KeyCode::Char('W') if ctrl => {
            let mode = state.cycle_web_search_mode().await;
            app.status.web_search = format!("{mode:?}").to_uppercase();
            app.push_system(format!("Web search: {mode:?}"));
            return Outcome::Continue;
        }
        KeyCode::Char('h') | KeyCode::Char('H') if ctrl => {
            let mode = state.cycle_event_handler_mode().await;
            app.status.handler_mode = format!("{mode:?}").to_uppercase();
            app.push_system(format!("Handler mode: {mode:?}"));
            return Outcome::Continue;
        }
        KeyCode::Char('e') | KeyCode::Char('E') if ctrl => {
            let (mode, switched) = state.cycle_scripting_mode().await;
            if switched {
                app.status.scripting = mode.as_str().to_string();
                app.push_system(format!("Scripting: {}", mode.as_str()));
            }
            return Outcome::Continue;
        }
        KeyCode::Char('t') | KeyCode::Char('T') if ctrl => {
            app.mouse_capture = !app.mouse_capture;
            app.push_system(if app.mouse_capture {
                "Mouse capture ON"
            } else {
                "Mouse capture OFF — native text selection works; Ctrl-T to re-enable"
            });
            return Outcome::Continue;
        }
        KeyCode::F(1) => {
            app.modals.push(Modal::Help { scroll: 0 });
            return Outcome::Continue;
        }
        KeyCode::F(2) => {
            app.right_layout = app.right_layout.next();
            app.push_system(format!("Layout: {}", app.right_layout.describe()));
            return Outcome::Continue;
        }
        KeyCode::Tab if !alt => {
            cycle_focus(app, false);
            return Outcome::Continue;
        }
        KeyCode::BackTab => {
            cycle_focus(app, true);
            return Outcome::Continue;
        }
        _ => {}
    }

    match app.focus {
        Focus::ChatInput => handle_chat_key(app, key, state, event_handler, status_tx).await,
        Focus::ChatHistory => {
            match key.code {
                KeyCode::Up => app.chat.scroll_up(1),
                KeyCode::Down => app.chat.scroll_down(1),
                KeyCode::PageUp => app.chat.scroll_up(10),
                KeyCode::PageDown => app.chat.scroll_down(10),
                KeyCode::End | KeyCode::Esc => {
                    app.chat.scroll_to_follow();
                    app.focus = Focus::ChatInput;
                }
                _ => {}
            }
            Outcome::Continue
        }
        Focus::Instances => handle_instances_key(app, key, state).await,
        Focus::Inspector => handle_inspector_key(app, key, state).await,
        Focus::Activity => handle_activity_key(app, key, state).await,
    }
}

/// Instances → Inspector → Activity → Chat → Instances. The inspector is
/// skipped when nothing is selected (there is nothing in it to focus).
/// Three stops: the management column (list and inspector together), the
/// feed, the chat. Inside the management column the arrows do the moving.
fn cycle_focus(app: &mut DashboardApp, backward: bool) {
    let column = |f: Focus| match f {
        Focus::Instances | Focus::Inspector => 0,
        Focus::Activity => 1,
        Focus::ChatInput | Focus::ChatHistory => 2,
    };
    let current = column(app.focus);
    let next = if backward {
        (current + 2) % 3
    } else {
        (current + 1) % 3
    };
    app.focus = match next {
        0 => Focus::Instances,
        1 => Focus::Activity,
        _ => Focus::ChatInput,
    };
    if app.focus == Focus::Activity {
        app.activity.cursor = None;
    }
    app.clamp_selection();
}

async fn handle_chat_key(
    app: &mut DashboardApp,
    key: KeyEvent,
    state: &AppState,
    event_handler: &EventHandler,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Outcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Enter if ctrl || alt => {
            app.input.insert_newline();
        }
        KeyCode::Char('n') | KeyCode::Char('N') if ctrl || alt => {
            app.input.insert_newline();
        }
        KeyCode::Enter => {
            let text = app.input.text();
            app.input.clear();
            app.core.slash_suggestions.clear();
            crate::tui::commands::submit(app, text, state, event_handler, status_tx).await;
        }
        KeyCode::PageUp => {
            app.focus = Focus::ChatHistory;
            app.chat.scroll_up(10);
        }
        KeyCode::Up if app.input.is_on_first_line() => history_previous(app),
        KeyCode::Down if app.input.is_on_last_line() => history_next(app),
        KeyCode::Esc => {
            app.input.clear();
            app.core.slash_suggestions.clear();
        }
        _ => {
            if !readline_key(app, key) {
                app.input.handle_key(key.code, key.modifiers);
            }
        }
    }
    let text = app.input.text();
    app.core.update_slash_suggestions(&text);
    Outcome::Continue
}

/// The readline chords the legacy TUI's footer had. Returns whether `key`
/// was one.
fn readline_key(app: &mut DashboardApp, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => app.input.move_to_start_of_line(),
        KeyCode::Char('e') | KeyCode::Char('E') if ctrl => app.input.move_to_end_of_line(),
        KeyCode::Char('k') | KeyCode::Char('K') if ctrl => app.input.delete_to_end_of_line(),
        KeyCode::Char('u') | KeyCode::Char('U') if ctrl => app.input.delete_line(),
        KeyCode::Char('w') | KeyCode::Char('W') if ctrl => app.input.delete_word(),
        KeyCode::Backspace if alt => app.input.delete_word(),
        KeyCode::Delete if alt => app.input.delete_word_forward(),
        KeyCode::Left if alt => app.input.move_cursor_word_left(),
        KeyCode::Right if alt => app.input.move_cursor_word_right(),
        KeyCode::Char('b') if alt => app.input.move_cursor_word_left(),
        KeyCode::Char('f') if alt => app.input.move_cursor_word_right(),
        KeyCode::Char('d') if alt => app.input.delete_word_forward(),
        _ => return false,
    }
    true
}

/// Command-history navigation, mirroring the legacy TUI: entering history
/// stashes the in-progress input, leaving it restores it.
fn history_previous(app: &mut DashboardApp) {
    use crate::cli::input_state::InputState;
    if app.core.command_history.is_empty() {
        return;
    }
    match app.core.history_position {
        None => {
            let current = app.input.text();
            if !current.is_empty() {
                app.core.history_temp_input = Some(current);
            }
            let pos = app.core.command_history.len() - 1;
            app.core.history_position = Some(pos);
            app.input = InputState::from_lines(
                app.core.command_history[pos]
                    .lines()
                    .map(|s| s.to_string())
                    .collect(),
            );
            app.input.move_to_bottom();
            app.input.move_to_end_of_line();
        }
        Some(pos) if pos > 0 => {
            let new_pos = pos - 1;
            app.core.history_position = Some(new_pos);
            app.input = InputState::from_lines(
                app.core.command_history[new_pos]
                    .lines()
                    .map(|s| s.to_string())
                    .collect(),
            );
            app.input.move_to_bottom();
            app.input.move_to_end_of_line();
        }
        _ => {}
    }
}

fn history_next(app: &mut DashboardApp) {
    use crate::cli::input_state::InputState;
    match app.core.history_position {
        Some(pos) if pos + 1 < app.core.command_history.len() => {
            let new_pos = pos + 1;
            app.core.history_position = Some(new_pos);
            app.input = InputState::from_lines(
                app.core.command_history[new_pos]
                    .lines()
                    .map(|s| s.to_string())
                    .collect(),
            );
            app.input.move_to_bottom();
            app.input.move_to_end_of_line();
        }
        Some(_) => {
            app.core.history_position = None;
            let temp = app.core.history_temp_input.take().unwrap_or_default();
            app.input = InputState::from_lines(temp.lines().map(|s| s.to_string()).collect());
            app.input.move_to_bottom();
            app.input.move_to_end_of_line();
        }
        None => {}
    }
}

/// Move the list cursor by `delta` selectable rows.
fn move_list_cursor(app: &mut DashboardApp, delta: isize) {
    let rows = list_rows(&app.snapshot);
    let selectable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| is_selectable(r))
        .map(|(i, _)| i)
        .collect();
    if selectable.is_empty() {
        return;
    }
    let current = crate::tui::render::rail::cursor_index(app, &rows)
        .and_then(|i| selectable.iter().position(|s| *s == i));
    let next = match current {
        None => {
            if delta < 0 {
                selectable.len() - 1
            } else {
                0
            }
        }
        Some(pos) => (pos as isize + delta).clamp(0, selectable.len() as isize - 1) as usize,
    };
    set_list_cursor(app, rows[selectable[next]]);
}

fn set_list_cursor(app: &mut DashboardApp, row: ListRow) {
    match row {
        ListRow::Instance(key) => app.select(key),
        ListRow::New => {
            app.instances.selected = None;
            app.instances.on_new = true;
            app.inspector.item = 0;
        }
        ListRow::Header(..) => {}
    }
}

fn list_cursor_row(app: &DashboardApp) -> Option<ListRow> {
    let rows = list_rows(&app.snapshot);
    crate::tui::render::rail::cursor_index(app, &rows).map(|i| rows[i])
}

/// Letters that act on the selected instance from either left pane.
/// Letters that act on the selected instance from either left pane.
async fn instance_letter(app: &mut DashboardApp, code: KeyCode, state: &AppState) -> bool {
    if matches!(code, KeyCode::Char('a') | KeyCode::Char('A')) {
        actions::open_protocol_picker(app, None, state).await;
        return true;
    }
    let Some(key) = app.selected() else {
        return false;
    };
    let action = match (code, key) {
        (KeyCode::Char('x'), _) => InstanceAction::Stop,
        (KeyCode::Char('e'), _) => InstanceAction::Edit,
        (KeyCode::Char('r'), _) => InstanceAction::Rules,
        (KeyCode::Char('m'), _) => InstanceAction::CycleDriver,
        (KeyCode::Char('w'), _) => InstanceAction::Wireshark,
        (KeyCode::Char('d'), _) => InstanceAction::Docs,
        (KeyCode::Char('c'), UiKey::Server(_)) => InstanceAction::ConnectClient,
        (KeyCode::Char('n'), UiKey::Client(_)) => InstanceAction::Send,
        _ => return false,
    };
    actions::run(app, key, action, state).await;
    true
}

/// Digits jump straight to a tab and focus the inspector.
fn tab_digit(code: KeyCode) -> Option<usize> {
    match code {
        KeyCode::Char(c @ '1'..='6') => Some(c as usize - '1' as usize),
        _ => None,
    }
}

fn jump_to_tab(app: &mut DashboardApp, index: usize) {
    let Some(key) = app.selected() else {
        return;
    };
    let tabs = InspectorTab::for_key(key);
    if let Some(tab) = tabs.get(index) {
        set_tab(app, *tab);
    }
}

/// ←/→ anywhere in the management column flips the inspector's tab, so
/// instances can be browsed with ↑/↓ and compared on one tab without
/// leaving the list.
fn step_tab(app: &mut DashboardApp, backward: bool) {
    let Some(key) = app.selected() else {
        return;
    };
    let tabs = InspectorTab::for_key(key);
    let current = inspector::effective_tab(key, app.inspector.tab);
    let pos = tabs.iter().position(|t| *t == current).unwrap_or(0);
    let next = if backward {
        (pos + tabs.len() - 1) % tabs.len()
    } else {
        (pos + 1) % tabs.len()
    };
    set_tab(app, tabs[next]);
}

fn set_tab(app: &mut DashboardApp, tab: InspectorTab) {
    if app.inspector.tab != tab {
        app.inspector.tab = tab;
        app.inspector.item = 0;
        app.inspector.scroll = 0;
    }
}

/// How many items the inspector has for the selected instance right now.
fn inspector_item_count(app: &DashboardApp) -> usize {
    app.selected_instance()
        .map(|instance| {
            let key = instance.key();
            inspector::build(
                instance,
                &app.inspector,
                app.instances.metrics.get(&key),
                60,
            )
            .item_count()
        })
        .unwrap_or(0)
}

/// Whether the cursor sits on the last selectable list row.
fn at_list_end(app: &DashboardApp) -> bool {
    let rows = list_rows(&app.snapshot);
    let last = rows.iter().rposition(is_selectable);
    crate::tui::render::rail::cursor_index(app, &rows) == last
}

async fn handle_instances_key(app: &mut DashboardApp, key: KeyEvent, state: &AppState) -> Outcome {
    match key.code {
        // ↓ past the last row walks into the inspector's items.
        KeyCode::Down => {
            if at_list_end(app) {
                if app.selected().is_some() && inspector_item_count(app) > 0 {
                    app.focus = Focus::Inspector;
                    app.inspector.item = 0;
                }
            } else {
                move_list_cursor(app, 1);
            }
        }
        KeyCode::Up => move_list_cursor(app, -1),
        KeyCode::PageDown => move_list_cursor(app, 10),
        KeyCode::PageUp => move_list_cursor(app, -10),
        KeyCode::Home => move_list_cursor(app, isize::MIN / 2),
        KeyCode::End => move_list_cursor(app, isize::MAX / 2),
        KeyCode::Left => step_tab(app, true),
        KeyCode::Right => step_tab(app, false),
        KeyCode::Enter | KeyCode::Char(' ') => match list_cursor_row(app) {
            Some(ListRow::New) => {
                actions::open_protocol_picker(app, None, state).await;
            }
            Some(ListRow::Instance(_)) => actions::open_action_menu(app),
            _ => {}
        },
        KeyCode::Esc => {
            app.focus = Focus::ChatInput;
        }
        code => {
            if let Some(index) = tab_digit(code) {
                jump_to_tab(app, index);
            } else {
                instance_letter(app, code, state).await;
            }
        }
    }
    app.clamp_selection();
    Outcome::Continue
}

async fn handle_inspector_key(app: &mut DashboardApp, key: KeyEvent, state: &AppState) -> Outcome {
    let Some(instance) = app.selected_instance() else {
        app.focus = Focus::Instances;
        return Outcome::Continue;
    };
    let key_id = instance.key();
    let view = inspector::build(
        instance,
        &app.inspector,
        app.instances.metrics.get(&key_id),
        60,
    );
    let items = view.item_count();

    match key.code {
        KeyCode::Esc => {
            app.focus = Focus::Instances;
        }
        KeyCode::Left => step_tab(app, true),
        KeyCode::Right => step_tab(app, false),
        // ↑ past the first item walks back up into the list.
        KeyCode::Up => {
            if app.inspector.item == 0 || items == 0 {
                app.focus = Focus::Instances;
                app.instances.on_new = false;
            } else {
                app.inspector.item -= 1;
            }
        }
        KeyCode::Down => {
            if app.inspector.item + 1 < items {
                app.inspector.item += 1;
            }
        }
        KeyCode::PageDown => {
            app.inspector.item = (app.inspector.item + 10).min(items.saturating_sub(1));
        }
        KeyCode::PageUp => app.inspector.item = app.inspector.item.saturating_sub(10),
        KeyCode::Home => app.inspector.item = 0,
        KeyCode::End => app.inspector.item = items.saturating_sub(1),
        KeyCode::Enter => {
            if let Some(action) = view.default_action(app.inspector.item) {
                actions::run(app, key_id, action, state).await;
            } else {
                actions::open_action_menu(app);
            }
        }
        KeyCode::Char(' ') => actions::open_action_menu(app),
        code => {
            if let Some(index) = tab_digit(code) {
                jump_to_tab(app, index);
            } else {
                instance_letter(app, code, state).await;
            }
        }
    }
    app.clamp_selection();
    Outcome::Continue
}

async fn handle_activity_key(app: &mut DashboardApp, key: KeyEvent, state: &AppState) -> Outcome {
    let visible = crate::tui::render::activity::visible_entries(app).len();
    match key.code {
        KeyCode::Esc | KeyCode::End => {
            app.activity.scroll_to_follow();
            app.focus = Focus::ChatInput;
        }
        KeyCode::Up => {
            let cursor = match app.activity.cursor {
                None => visible.checked_sub(1),
                Some(c) => Some(c.saturating_sub(1)),
            };
            app.activity.cursor = cursor;
            if cursor.is_some() && app.activity.scroll == crate::tui::chat::ScrollPos::Follow {
                app.activity.scroll_up(0);
            }
        }
        // ↓ past the newest line walks on into the chat.
        KeyCode::Down => match app.activity.cursor {
            Some(c) if c + 1 < visible => app.activity.cursor = Some(c + 1),
            _ => {
                app.activity.scroll_to_follow();
                app.focus = Focus::ChatInput;
            }
        },
        KeyCode::PageUp => {
            let cursor = app.activity.cursor.unwrap_or(visible).saturating_sub(10);
            app.activity.cursor = Some(cursor);
            if app.activity.scroll == crate::tui::chat::ScrollPos::Follow {
                app.activity.scroll_up(0);
            }
        }
        KeyCode::PageDown => {
            if let Some(c) = app.activity.cursor {
                if c + 10 < visible {
                    app.activity.cursor = Some(c + 10);
                } else {
                    app.activity.scroll_to_follow();
                }
            }
        }
        KeyCode::Home => {
            if visible > 0 {
                app.activity.cursor = Some(0);
                app.activity.scroll_up(0);
            }
        }
        KeyCode::Char('f') => {
            app.activity.only_selected = !app.activity.only_selected;
            app.activity.cursor = None;
        }
        KeyCode::Enter => {
            if let Some(cursor) = app.activity.cursor {
                let link = crate::tui::render::activity::visible_entries(app)
                    .get(cursor)
                    .and_then(|e| e.event.link);
                follow_link(app, link, state).await;
            }
        }
        code => {
            instance_letter(app, code, state).await;
        }
    }
    Outcome::Continue
}

async fn follow_link(
    app: &mut DashboardApp,
    link: Option<crate::tui::activity::Link>,
    state: &AppState,
) {
    use crate::tui::activity::Link;
    match link {
        Some(Link::Request(key, id)) => {
            app.select(key);
            actions::run(app, key, InstanceAction::OpenRequest(id), state).await;
        }
        Some(Link::Intercept(key, id)) => {
            app.select(key);
            actions::run(app, key, InstanceAction::Answer(id), state).await;
        }
        Some(Link::Instance(key)) => {
            if app.instance(key).is_some() {
                app.select(key);
                app.focus = Focus::Inspector;
                app.activity.scroll_to_follow();
            } else {
                app.push_system(format!("{} is gone", key.describe()));
            }
        }
        None => {}
    }
}

pub async fn handle_mouse(app: &mut DashboardApp, event: MouseEvent, state: &AppState) -> Outcome {
    let target = app.hits.hit(event.column, event.row).cloned();

    // A right click opens the action menu for whatever it lands on.
    if event.kind == MouseEventKind::Down(MouseButton::Right) && app.modal().is_none() {
        match target {
            Some(HitTarget::ListRow(index)) => {
                let rows = list_rows(&app.snapshot);
                if let Some(ListRow::Instance(key)) = rows.get(index).copied() {
                    app.focus = Focus::Instances;
                    app.select(key);
                    actions::open_action_menu(app);
                }
            }
            Some(HitTarget::InspectorItem(index)) => {
                app.focus = Focus::Inspector;
                app.inspector.item = index;
                actions::open_action_menu(app);
            }
            Some(HitTarget::InspectorBody) | Some(HitTarget::InspectorTab(_)) => {
                app.focus = Focus::Inspector;
                actions::open_action_menu(app);
            }
            _ => {}
        }
        app.dirty = true;
        return Outcome::Continue;
    }

    match event.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = event.kind == MouseEventKind::ScrollUp;
            match target {
                Some(HitTarget::ChatHistory) | Some(HitTarget::ChatInput) => {
                    if up {
                        app.focus = Focus::ChatHistory;
                        app.chat.scroll_up(3);
                    } else {
                        app.chat.scroll_down(3);
                    }
                }
                Some(HitTarget::ModalBody) | Some(HitTarget::ModalRow(_)) => {
                    if let Some(modal) = app.modal_mut() {
                        modal.scroll_by(if up { -3 } else { 3 });
                    }
                }
                Some(HitTarget::ListRow(_)) => {
                    let rows = list_rows(&app.snapshot).len();
                    app.instances.scroll = if up {
                        app.instances.scroll.saturating_sub(3)
                    } else {
                        (app.instances.scroll + 3).min(rows.saturating_sub(1))
                    };
                }
                Some(HitTarget::InspectorBody) | Some(HitTarget::InspectorItem(_)) => {
                    app.inspector.scroll = if up {
                        app.inspector.scroll.saturating_sub(3)
                    } else {
                        app.inspector.scroll + 3
                    };
                }
                Some(HitTarget::Activity) | Some(HitTarget::ActivityRow(_)) => {
                    if up {
                        app.activity.scroll_up(3);
                    } else {
                        app.activity.scroll_down(3);
                    }
                }
                _ => {}
            }
            app.dirty = true;
            return Outcome::Continue;
        }
        MouseEventKind::Down(MouseButton::Left) => {}
        _ => return Outcome::Continue,
    }

    app.dirty = true;
    let Some(target) = target else {
        return Outcome::Continue;
    };

    // Buttons inside a modal are clickable; every other click in a modal is
    // swallowed so it cannot reach the panes beneath.
    if app.modal().is_some() {
        if let HitTarget::ModalActionButton(action) = target {
            return crate::tui::modal_keys::run_modal_action(app, action, state).await;
        }
        if let HitTarget::ModalRow(index) = target {
            // A menu entry is a verb: clicking it runs it. Every other
            // modal's rows only select on click.
            if matches!(app.modal(), Some(Modal::ActionMenu(_))) {
                crate::tui::modal_keys::run_action_menu_item(app, index, state).await;
            } else {
                crate::tui::modal_keys::select_modal_row(app, index);
            }
        }
        return Outcome::Continue;
    }

    match target {
        HitTarget::ChatHistory => app.focus = Focus::ChatHistory,
        HitTarget::ChatInput => app.focus = Focus::ChatInput,
        HitTarget::ListRow(index) => {
            let rows = list_rows(&app.snapshot);
            let Some(row) = rows.get(index).copied() else {
                return Outcome::Continue;
            };
            match row {
                ListRow::Header(..) => {}
                ListRow::New => {
                    app.focus = Focus::Instances;
                    set_list_cursor(app, row);
                    actions::open_protocol_picker(app, None, state).await;
                }
                ListRow::Instance(key) => {
                    // First click selects; a click on the selection opens
                    // its actions.
                    if app.selected() == Some(key) && app.focus == Focus::Instances {
                        actions::open_action_menu(app);
                    } else {
                        app.focus = Focus::Instances;
                        app.select(key);
                    }
                }
            }
        }
        HitTarget::InspectorTab(tab) => {
            app.focus = Focus::Inspector;
            set_tab(app, tab);
        }
        HitTarget::InspectorItem(index) => {
            app.focus = Focus::Inspector;
            // First click selects; a click on the selection activates it.
            if app.inspector.item == index {
                if let Some(instance) = app.selected_instance() {
                    let key = instance.key();
                    let view = inspector::build(
                        instance,
                        &app.inspector,
                        app.instances.metrics.get(&key),
                        60,
                    );
                    if let Some(action) = view.default_action(index) {
                        actions::run(app, key, action, state).await;
                    }
                }
            } else {
                app.inspector.item = index;
            }
        }
        HitTarget::InspectorBody => app.focus = Focus::Inspector,
        HitTarget::Activity => app.focus = Focus::Activity,
        HitTarget::ActivityRow(index) => {
            app.focus = Focus::Activity;
            if app.activity.cursor == Some(index) {
                let link = crate::tui::render::activity::visible_entries(app)
                    .get(index)
                    .and_then(|e| e.event.link);
                follow_link(app, link, state).await;
            } else {
                app.activity.cursor = Some(index);
                if app.activity.scroll == crate::tui::chat::ScrollPos::Follow {
                    app.activity.scroll_up(0);
                }
            }
        }
        HitTarget::StatusSegment(segment) => match segment {
            SegmentId::LogLevel => {
                let level = app.core.log_level.cycle();
                app.core.set_log_level(level);
                app.push_system(format!("Log level: {}", level.as_str()));
            }
            SegmentId::WebSearch => {
                let mode = state.cycle_web_search_mode().await;
                app.status.web_search = format!("{mode:?}").to_uppercase();
            }
            SegmentId::Handler => {
                let mode = state.cycle_event_handler_mode().await;
                app.status.handler_mode = format!("{mode:?}").to_uppercase();
            }
            SegmentId::Scripting => {
                let (mode, switched) = state.cycle_scripting_mode().await;
                if switched {
                    app.status.scripting = mode.as_str().to_string();
                }
            }
            SegmentId::Help => app.modals.push(Modal::Help { scroll: 0 }),
            SegmentId::Waiting => actions::answer_next_waiting(app, state),
            SegmentId::Instances => {
                app.focus = Focus::Instances;
                app.clamp_selection();
            }
            SegmentId::Model => {
                let lines = crate::tui::command_exec::execute(
                    crate::events::UserCommand::ShowUsage,
                    state,
                    &dummy_channel(),
                )
                .await;
                let model = if app.status.model.is_empty() {
                    "No model — /model <name> picks one; manual, static and script rules need none."
                        .to_string()
                } else {
                    format!("Model: {} — /model lists alternatives.", app.status.model)
                };
                app.push_system(model);
                for line in lines {
                    app.push_system(line);
                }
            }
            SegmentId::Backend | SegmentId::Usage => {
                let lines = crate::tui::command_exec::execute(
                    crate::events::UserCommand::ShowUsage,
                    state,
                    &dummy_channel(),
                )
                .await;
                for line in lines {
                    app.push_system(line);
                }
            }
        },
        HitTarget::ModalBody
        | HitTarget::ModalRow(_)
        | HitTarget::ModalButton(_)
        | HitTarget::ModalActionButton(_) => {}
    }
    app.clamp_selection();
    Outcome::Continue
}

/// Fold the result of a spawned action back into the UI: on success the
/// originating modal closes and the summary goes to chat; on failure the modal
/// stays open showing the error, so the user can fix and retry.
pub fn handle_ui_msg(app: &mut DashboardApp, msg: UiMsg) {
    app.dirty = true;
    let (origin, result) = match msg {
        UiMsg::Chat(text) => {
            app.push_system(text);
            return;
        }
        UiMsg::ActionDone { origin, result } => (origin, result),
    };

    let matches_origin = matches!(
        (origin, app.modal()),
        (ActionOrigin::Form, Some(Modal::Form(_)))
            | (ActionOrigin::Routing, Some(Modal::Routing(_)))
    );

    match result {
        Ok(summary) => {
            if matches_origin {
                app.modals.pop();
            }
            app.push_system(summary);
        }
        Err(error) => {
            if matches_origin {
                match app.modals.last_mut() {
                    Some(Modal::Form(form)) => {
                        form.busy = false;
                        form.error = Some(error);
                    }
                    Some(Modal::Routing(model)) => {
                        model.busy = false;
                        model.error = Some(error);
                    }
                    Some(Modal::Composer(composer)) => {
                        composer.busy = false;
                        composer.error = Some(error);
                    }
                    _ => {}
                }
            } else {
                app.push_error(format!("✗ {error}"));
            }
        }
    }
}

/// A detached sender for command paths that only need the return value.
fn dummy_channel() -> mpsc::UnboundedSender<String> {
    let (tx, _rx) = mpsc::unbounded_channel();
    tx
}
