//! Rolling terminal TUI - output flows like tail -f with sticky footer
//!
//! This module implements the interactive TUI mode using a rolling terminal
//! approach where output naturally scrolls into the terminal's scrollback buffer,
//! while input and connection info remain sticky at the bottom.

use anyhow::Result;
use crossterm::{
    cursor,
    event::{Event, EventStream, KeyCode, KeyModifiers},
    execute,
    style::{Print, ResetColor, SetForegroundColor},
    terminal,
};
use futures::StreamExt;
use std::io::{stdout, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, Instant};

use tracing::{debug, error, info};

use crate::events::{EventHandler, UserCommand};
use crate::llm::OllamaClient;
use crate::settings::Settings;
use crate::state::app_state::AppState;
use crate::ui::{app::LogLevel, App};

use super::input_state::InputState;
use super::sticky_footer::{ConnectionInfo, FooterContent, StickyFooter};
use super::theme::ColorPalette;

/// Format scripting mode for display in status bar
/// Returns "LLM", "Python", or "JavaScript" based on selected mode
fn format_scripting_mode(mode: crate::state::app_state::ScriptingMode) -> String {
    mode.as_str().to_string()
}

/// Run the interactive rolling TUI mode
pub async fn run_rolling_tui(
    state: AppState,
    mut app: App,
    mut event_handler: EventHandler,
    llm_client: OllamaClient,
    settings: Settings,
    args: &super::Args,
    palette: ColorPalette,
) -> Result<()> {
    info!("Starting rolling TUI mode");
    debug!("Rolling TUI: Entry point reached");

    // Wrap settings in Arc<Mutex> for sharing with event handlers
    debug!("Rolling TUI: Wrapping settings in Arc<Mutex>");
    let settings = Arc::new(Mutex::new(settings));
    debug!("Rolling TUI: Settings wrapped");

    // Wrap palette in Arc for sharing
    let palette = Arc::new(palette);

    // Resolve model + backend URL (shared logic with the dashboard)
    let resolved = {
        let settings_guard = settings.lock().await;
        crate::cli::model_select::resolve_startup_model(args, &settings_guard).await?
    };
    let base_url = resolved.base_url;
    let base_url = base_url.as_str();
    let selected_model = resolved.model;
    let model_messages = resolved.messages;

    state
        .set_ollama_model(if selected_model.is_empty() {
            None
        } else {
            Some(selected_model.clone())
        })
        .await;
    app.connection_info.model = selected_model.clone();

    // Load web search setting from settings file
    let web_search_mode = settings.lock().await.get_web_search_mode();
    state.set_web_search_mode(web_search_mode).await;

    // Apply event handler mode from CLI if provided
    if let Ok(Some(handler_mode)) = args.parse_event_handler_mode() {
        state.set_event_handler_mode(handler_mode).await;
    }

    // Setup terminal (raw mode only, no alternate screen)
    // Capture the cooked terminal state and arm the native-crash restorer BEFORE
    // entering raw mode, so a SIGSEGV/SIGABRT/SIGTRAP from a C/ObjC library does not
    // leave the shell wedged in raw mode. See `crash_restore`.
    crate::cli::crash_restore::install(b"");
    debug!("Rolling TUI: Enabling raw mode...");
    terminal::enable_raw_mode()?;
    debug!("Rolling TUI: Raw mode enabled");

    // Ensure raw mode is disabled even if we panic
    // This prevents the terminal from being left in a broken state
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
        }
    }
    let _guard = RawModeGuard;

    // Get terminal size (use defaults if detection fails or returns 0, e.g., in PTY tests)
    debug!("Rolling TUI: Getting terminal size...");
    let (width, height) = match terminal::size() {
        Ok((w, h)) if w > 0 && h > 0 => {
            debug!("Rolling TUI: Terminal size: {}x{}", w, h);
            (w, h)
        }
        _ => {
            debug!("Rolling TUI: Terminal size detection failed, using default 80x24");
            (80, 24) // Default to 80x24 if size detection fails or returns 0
        }
    };

    // Create sticky footer with system capabilities
    debug!("Rolling TUI: Getting system capabilities for footer...");
    let system_capabilities = state.get_system_capabilities().await;
    debug!("Rolling TUI: Creating sticky footer...");
    let mut footer = StickyFooter::new(width, height, system_capabilities, (*palette).clone())?;
    debug!("Rolling TUI: Sticky footer created");
    let scroll_height = footer.scroll_region_height();
    let footer_height = height.saturating_sub(scroll_height);
    debug!(
        "Rolling TUI: Scroll height: {}, Footer height: {}",
        scroll_height, footer_height
    );

    // Create web approval channel for ASK mode
    let (web_approval_tx, mut web_approval_rx) = tokio::sync::mpsc::unbounded_channel();
    state.set_web_approval_channel(web_approval_tx).await;

    // BEFORE setting scrolling region, push any existing terminal content up
    // by printing newlines. This makes room for the footer without overwriting content.
    // Move to actual bottom of terminal using a large line number that will clamp.
    // Note: terminal::size() may return wrong values in PTY tests, so we use ESC[9999;1H
    // which moves to line 9999 (clamped to actual terminal height) instead of relying on detected height
    print!("\x1b[9999;1H"); // CSI 9999;1 H - Move to line 9999, column 1 (clamps to actual terminal bottom)
    stdout().flush()?;

    // Print footer_height newlines to push existing content up
    for _ in 0..footer_height {
        execute!(stdout(), Print("\n"))?;
    }
    stdout().flush()?;

    // Now set up scrolling region (lines 1 to scroll_region_height)
    // This tells the terminal that only these lines should scroll, keeping footer fixed
    // DECSTBM: ESC[<top>;<bottom>r - Set scrolling region
    print!("\x1b[1;{}r", scroll_height);
    stdout().flush()?;

    let scripting_mode = state.get_selected_scripting_mode().await;
    let scripting_status = format_scripting_mode(scripting_mode);
    let web_search_mode = state.get_web_search_mode().await;
    let event_handler_mode = state.get_event_handler_mode().await;

    footer.set_connection_info(ConnectionInfo {
        model: app.connection_info.model.clone(),
        scripting_env: scripting_status,
        web_search_mode,
        event_handler_mode,
    });
    footer.set_packet_stats(app.packet_stats.clone());
    footer.set_log_level(app.log_level);

    // Print welcome messages to scrolling region
    debug!("Rolling TUI: Printing welcome messages...");
    print_welcome_messages(&mut footer, &palette)?;
    debug!("Rolling TUI: Welcome messages printed");

    // Render footer initially to position cursor correctly
    // Without this, the cursor sits at the terminal default position until first keystroke
    debug!("Rolling TUI: Rendering footer...");
    footer.render(&mut stdout())?;
    debug!("Rolling TUI: Footer rendered");

    // Create status channel for server messages
    debug!("Rolling TUI: Creating status channel...");
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    debug!("Rolling TUI: Status channel created");

    // Send model messages to TUI (if any)
    for msg in model_messages {
        let _ = status_tx.send(msg);
    }

    // Spawn async task to generate and stream ASCII art banner (unless suppressed)
    // This runs in the background and doesn't block TUI startup
    // The banner is sent through status_tx and appears in the TUI output
    if args.show_art {
        let ollama_url_clone = base_url.to_string();
        let model_clone = selected_model.clone();
        let status_tx_clone = status_tx.clone();
        tokio::spawn(async move {
            let _ = crate::cli::banner::generate_and_stream_ascii_banner(
                &ollama_url_clone,
                &model_clone,
                status_tx_clone,
            )
            .await;
        });
    }

    // Create keyboard event stream
    debug!("Rolling TUI: Creating event stream...");
    let mut event_stream = EventStream::new();
    debug!("Rolling TUI: Event stream created");

    // Create tick interval for UI updates
    let mut tick_interval = interval(Duration::from_millis(100));

    // Create system stats monitor and update interval (1 second)
    let stats_monitor = Arc::new(crate::system_stats::SystemStatsMonitor::new());
    let mut stats_update_interval = interval(Duration::from_secs(1));

    // Cleanup configuration constants
    const CLEANUP_INTERVAL_SECS: u64 = 5;
    const SERVER_CLEANUP_TIMEOUT_SECS: u64 = 30;
    const CONNECTION_CLEANUP_TIMEOUT_SECS: u64 = 10;
    const CONNECTIONLESS_CLEANUP_TIMEOUT_SECS: u64 = 10;

    // Create cleanup interval
    let mut cleanup_interval = interval(Duration::from_secs(CLEANUP_INTERVAL_SECS));

    // Create task execution interval (check every 1 second)
    let mut task_execution_interval = interval(Duration::from_secs(1));
    task_execution_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Create test interval for debugging footer behavior (disabled for stable snapshots)
    // Set to a very long duration so it doesn't fire during tests
    let mut test_interval = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(3600), // Start in 1 hour
        Duration::from_secs(3600),                  // Tick every hour
    );
    test_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Counter for test heartbeats
    let mut _heartbeat_counter = 0u64;

    // Resize debouncing - store pending resize dimensions
    let mut pending_resize: Option<(u16, u16)> = None;
    const RESIZE_DEBOUNCE_MS: u64 = 100; // Wait 100ms after last resize before rendering

    // Main event loop
    info!("Entering main event loop");

    loop {
        // Drain status messages from spawned tasks
        let mut ui_needs_update = false;
        while let Ok(msg) = status_rx.try_recv() {
            if msg == "__UPDATE_UI__" {
                // Special signal to update UI from state
                ui_needs_update = true;
            } else {
                // Filter messages by log level
                let should_show = if msg.starts_with("[ERROR]") {
                    true
                } else if msg.starts_with("[WARN]") {
                    app.log_level >= LogLevel::Warn
                } else if msg.starts_with("[INFO]") {
                    app.log_level >= LogLevel::Info
                } else if msg.starts_with("[DEBUG]") {
                    app.log_level >= LogLevel::Debug
                } else if msg.starts_with("[TRACE]") {
                    app.log_level >= LogLevel::Trace
                } else if msg.starts_with("[REASONING]") {
                    // Streamed reasoning is shown at the default INFO level and above,
                    // and hidden if the operator drops to WARN/ERROR to cut the noise.
                    app.log_level >= LogLevel::Info
                } else {
                    // Unprefixed messages always show
                    true
                };

                if should_show {
                    print_output_line(&msg, &mut footer, &palette)?;
                    ui_needs_update = true;
                }
            }
        }

        // Render footer immediately if messages were printed to reposition cursor
        // This ensures cursor is in the input field before select! blocks
        if ui_needs_update {
            update_ui_from_state(&mut app, &state, &mut footer).await;
            footer.render(&mut stdout())?;
            ui_needs_update = false; // Reset flag since we just rendered
        }

        tokio::select! {
            // Debounce timer for resize events
            _ = tokio::time::sleep(Duration::from_millis(RESIZE_DEBOUNCE_MS)), if pending_resize.is_some() => {
                // Debounce period has passed, apply the resize
                if let Some((width, height)) = pending_resize {
                    footer.handle_resize(width, height);
                    update_ui_from_state(&mut app, &state, &mut footer).await;
                    footer.render(&mut stdout())?;
                    pending_resize = None;
                }
            }
            // Keyboard events
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Resize(width, height))) => {
                        // Store the resize event but don't render yet - wait for debounce
                        pending_resize = Some((width, height));
                    }
                    Some(Ok(event)) => {
                        if handle_event(event, &mut app, &state, &mut event_handler, &status_tx, &mut footer, settings.clone(), palette.clone(), &stats_monitor).await? {
                            info!("Quit requested by user");
                            break; // Quit requested
                        }
                    }
                    Some(Err(e)) => {
                        error!("Keyboard event error: {}", e);
                    }
                    None => {
                        info!("Event stream ended unexpectedly");
                        break;
                    }
                }
            }

            // Web search approval requests
            Some(request) = web_approval_rx.recv() => {
                debug!("Received web approval request for: {}", request.url);

                // Store approval request in footer
                footer.pending_approval = Some(crate::cli::sticky_footer::PendingApproval {
                    url: request.url,
                    response_tx: request.response_tx,
                });

                // Re-render footer to show approval prompt
                footer.render(&mut stdout())?;
                ui_needs_update = false;
            }

            // Periodic tick for UI updates
            _ = tick_interval.tick() => {
                // Just triggers potential updates
            }

            // Execute due tasks, and drain any feedback that has become due. Both are
            // timer-driven LLM work that adjusts a running instance, so they share a tick.
            _ = task_execution_interval.tick() => {
                execute_due_tasks(&state, &llm_client, &status_tx).await;
                crate::llm::feedback::execute_due_feedback(&state, &llm_client, &status_tx).await;
            }

            // Periodic stats update (1 second)
            _ = stats_update_interval.tick() => {
                // Get system stats
                let system_stats = stats_monitor.get_stats().await;

                // Get LLM stats
                let (input_tokens, output_tokens, llm_calls) = state.get_llm_stats().await;

                // Update app state
                app.system_stats = system_stats.clone();

                // Update footer with stats
                footer.set_show_usage_stats(app.show_usage_stats);
                footer.set_system_stats(system_stats);
                footer.set_llm_stats(input_tokens, output_tokens, llm_calls);

                // Re-render if usage stats are visible
                if app.show_usage_stats {
                    footer.render(&mut stdout())?;
                }
            }

            // Periodic cleanup of old servers and connections
            _ = cleanup_interval.tick() => {
                state.cleanup_old_servers(SERVER_CLEANUP_TIMEOUT_SECS).await;
                state.cleanup_closed_connections(CONNECTION_CLEANUP_TIMEOUT_SECS).await;
                state.cleanup_old_connections(CONNECTIONLESS_CLEANUP_TIMEOUT_SECS).await;
                state.cleanup_old_conversations().await;
                ui_needs_update = true;
            }
        }

        // Update UI after handling events
        if ui_needs_update {
            update_ui_from_state(&mut app, &state, &mut footer).await;
            footer.render(&mut stdout())?;
        }
    }

    // Cleanup terminal
    // Reset scrolling region to full terminal (DECSTBM with no args)
    print!("\x1b[r");
    // Clear the sticky footer before exiting
    clear_sticky_footer(&footer)?;
    terminal::disable_raw_mode()?;
    println!(); // Final newline

    // Save command history before exiting
    let _ = app.save_history();
    info!("Rolling TUI mode exited");

    Ok(())
}

/// Print welcome messages to the scrolling region
fn print_welcome_messages(footer: &mut StickyFooter, palette: &ColorPalette) -> Result<()> {
    let title_lines = vec![
        "░█▀█░█▀▀░▀█▀░█▀▀░█▀▀░▀█▀",
        "░█░█░█▀▀░░█░░█░█░█▀▀░░█░",
        "░▀░▀░▀▀▀░░▀░░▀▀▀░▀▀▀░░▀░",
    ];

    // Find the longest line to determine box width
    let max_width = title_lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    let box_width = max_width + 4; // +4 for "│ " and " │"
    let left_margin = "  "; // 2 spaces left margin

    let mut stdout = stdout();
    let scroll_height = footer.scroll_region_height();
    let last_scroll_line = scroll_height.saturating_sub(1);

    // Blank line above the box
    execute!(stdout, cursor::MoveTo(0, last_scroll_line), Print("\n"))?;

    // Top border (green) with left margin
    let top_border = format!("{}┌{}┐", left_margin, "─".repeat(box_width - 2));
    execute!(
        stdout,
        cursor::MoveTo(0, last_scroll_line),
        SetForegroundColor(palette.success),
        Print(&top_border),
        Print("\n"),
        ResetColor
    )?;

    // Title lines (green borders, white text) with left margin
    for line in title_lines {
        let line_width = line.chars().count();
        let padding = max_width - line_width;
        execute!(
            stdout,
            cursor::MoveTo(0, last_scroll_line),
            Print(left_margin),
            SetForegroundColor(palette.success),
            Print("│ "),
            ResetColor,
            Print(line),
            Print(" ".repeat(padding)),
            Print(" "),
            SetForegroundColor(palette.success),
            Print("│"),
            Print("\n"),
            ResetColor
        )?;
    }

    // Bottom border (green) with left margin
    let bottom_border = format!("{}└{}┘", left_margin, "─".repeat(box_width - 2));
    execute!(
        stdout,
        cursor::MoveTo(0, last_scroll_line),
        SetForegroundColor(palette.success),
        Print(&bottom_border),
        Print("\n"),
        ResetColor
    )?;

    // Blank line below the box
    execute!(stdout, cursor::MoveTo(0, last_scroll_line), Print("\n"))?;

    stdout.flush()?;

    Ok(())
}

/// Print a line to stdout (scrolls naturally within scroll region - no flickering!)
fn print_output_line(line: &str, footer: &mut StickyFooter, palette: &ColorPalette) -> Result<()> {
    let mut stdout = stdout();

    // Move cursor to the LAST line of the scrolling region
    // The scrolling region is set to lines 1-scroll_region_height (1-indexed)
    // cursor::MoveTo uses 0-indexed coordinates, so last line is scroll_region_height - 1
    // When we print with \n, the scroll region will scroll naturally,
    // and the footer (outside the scroll region) will remain in place - no flickering!
    let scroll_height = footer.scroll_region_height();
    let last_scroll_line = scroll_height.saturating_sub(1); // 0-indexed

    // Position cursor at the last line of scroll region
    execute!(stdout, cursor::MoveTo(0, last_scroll_line))?;

    if line.starts_with("[ERROR]") {
        execute!(
            stdout,
            SetForegroundColor(palette.error),
            Print("✗ "),
            ResetColor,
            Print(line.strip_prefix("[ERROR]").unwrap()),
        )?;
    } else if line.starts_with("[WARN]") {
        execute!(
            stdout,
            SetForegroundColor(palette.warning),
            Print("⚠ "),
            ResetColor,
            Print(line.strip_prefix("[WARN]").unwrap()),
        )?;
    } else if line.starts_with("[INFO]") {
        execute!(
            stdout,
            SetForegroundColor(palette.info),
            Print("● "),
            ResetColor,
            Print(line.strip_prefix("[INFO]").unwrap()),
        )?;
    } else if line.starts_with("[DEBUG]") {
        execute!(
            stdout,
            SetForegroundColor(palette.debug),
            Print("○ "),
            ResetColor,
            Print(line.strip_prefix("[DEBUG]").unwrap()),
        )?;
    } else if line.starts_with("[TRACE]") {
        let content = line.strip_prefix("[TRACE]").unwrap();

        // Special handling for LLM request/response/prompt headers and conversation messages
        if content.trim_start().starts_with("LLM request:")
            || content.trim_start().starts_with("LLM response")
            || content.trim_start().starts_with("LLM prompt:")
            || content.trim_start().starts_with("JSON schema:")
            || content.trim_start().starts_with("Initial conversation:")
            || content.trim_start().starts_with("Conversation updated:")
        {
            // LLM headers: grey bullet, grey text
            execute!(
                stdout,
                SetForegroundColor(palette.trace),
                Print("· "),
                Print(content),
                ResetColor,
            )?;
        } else if content.trim_start().starts_with("Message ") {
            // Conversation messages: split at colon to show prefix in normal color, content in trace color
            if let Some(colon_pos) = content.find(':') {
                let prefix = &content[..=colon_pos]; // Include the colon
                let message_content = &content[colon_pos + 1..];
                execute!(
                    stdout,
                    SetForegroundColor(palette.trace),
                    Print("· "),
                    ResetColor,
                    Print(prefix),
                    SetForegroundColor(palette.trace),
                    Print(message_content),
                    ResetColor,
                )?;
            } else {
                // No colon found, just print normally
                execute!(
                    stdout,
                    SetForegroundColor(palette.trace),
                    Print("· "),
                    ResetColor,
                    Print(content),
                )?;
            }
        } else {
            // For all other TRACE content (including multi-line LLM output), keep grey
            execute!(
                stdout,
                SetForegroundColor(palette.trace),
                Print("· "),
                Print(content),
                ResetColor,
            )?;
        }
    } else if line.starts_with("[REASONING]") {
        // Streamed model chain-of-thought: its own colour and glyph so it reads as
        // the model thinking out loud, distinct from action output.
        execute!(
            stdout,
            SetForegroundColor(palette.reasoning),
            Print("∴ "),
            Print(line.strip_prefix("[REASONING]").unwrap()),
            ResetColor,
        )?;
    } else if line.starts_with("[USER]") {
        execute!(
            stdout,
            SetForegroundColor(palette.user),
            Print("▶ "),
            ResetColor,
            Print(line.strip_prefix("[USER]").unwrap()),
        )?;
    } else if line.starts_with("[SERVER]") {
        execute!(
            stdout,
            SetForegroundColor(palette.server),
            Print("◆ "),
            ResetColor,
            Print(line.strip_prefix("[SERVER]").unwrap()),
        )?;
    } else if line.starts_with("[CLIENT]") {
        execute!(
            stdout,
            SetForegroundColor(palette.client),
            Print("◁ "),
            ResetColor,
            Print(line.strip_prefix("[CLIENT]").unwrap()),
        )?;
    } else if line.starts_with("[CONN]") {
        execute!(
            stdout,
            SetForegroundColor(palette.connection),
            Print("◇ "),
            ResetColor,
            Print(line.strip_prefix("[CONN]").unwrap()),
        )?;
    } else {
        execute!(stdout, Print(line))?;
    }

    // Print newline - this will scroll the terminal up by one line
    execute!(stdout, Print("\n"))?;
    stdout.flush()?;

    // IMPORTANT: After printing a line, decrement the blank lines buffer
    // This line now occupies what was previously a blank line at the top
    footer.decrement_blank_lines_buffer();

    Ok(())
}

/// Execute all tasks that are due (public wrapper for non-interactive mode)
pub async fn execute_due_tasks_public(
    state: &AppState,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    execute_due_tasks(state, llm_client, status_tx).await
}

/// Execute all tasks that are due
async fn execute_due_tasks(
    state: &AppState,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    use crate::state::task::TaskStatus;
    use std::time::Instant;

    let now = Instant::now();
    let tasks = state.get_all_tasks().await;

    for task in tasks {
        // Skip if not scheduled or not yet due
        if task.status != TaskStatus::Scheduled {
            continue;
        }

        if task.next_execution > now {
            continue;
        }

        // Mark as executing
        state
            .update_task_status(task.id, TaskStatus::Executing)
            .await;

        // Spawn task execution to avoid blocking
        let state_clone = state.clone();
        let llm_clone = llm_client.clone();
        let status_tx_clone = status_tx.clone();
        let task_clone = task.clone();

        tokio::spawn(async move {
            execute_single_task(state_clone, llm_clone, status_tx_clone, task_clone).await
        });
    }
}

/// Build the list of actions a scheduled task may invoke.
///
/// This mirrors, scope for scope, the action list that
/// `PromptBuilder::build_task_execution_prompt` advertises to the model, and then applies
/// the same scripting-mode filter `PromptBuilder::build_action_prompt` applies. The result
/// is that the set the model is told about and the set `ConversationHandler` validates
/// against are identical rather than merely overlapping.
///
/// Previously an empty `Vec` was handed to the validator, so every action a scheduled task
/// returned was flagged unknown, retried twice, then `bail!`d — no scheduled task could
/// ever execute an action.
async fn build_task_actions(
    state: &AppState,
    scope: &crate::state::task::TaskScope,
    protocol_actions: Vec<crate::llm::actions::ActionDefinition>,
) -> Vec<crate::llm::actions::ActionDefinition> {
    use crate::llm::actions::{
        get_all_tool_actions, get_network_event_common_actions, get_network_event_tool_actions,
        get_user_input_common_actions,
    };
    use crate::llm::prompt::PromptBuilder;
    use crate::state::task::TaskScope;

    let selected_mode = state.get_selected_scripting_mode().await;
    let web_search_mode = state.get_web_search_mode().await;

    let actions = match scope {
        TaskScope::Global => {
            // Global tasks run in the user-input context, with open_server/open_client enabled.
            let scripting_env = state.get_scripting_env().await;
            let mut actions =
                get_user_input_common_actions(selected_mode, &scripting_env, true, true);
            actions.extend(get_all_tool_actions(web_search_mode));
            actions
        }
        TaskScope::Server(_) | TaskScope::Connection(_, _) | TaskScope::Client(_) => {
            // Server-, connection- and client-scoped tasks run in the network-event context.
            let mut actions = get_network_event_common_actions();
            actions.extend(protocol_actions);
            actions.extend(get_network_event_tool_actions(web_search_mode));
            actions
        }
    };

    let has_scripting = selected_mode != crate::state::app_state::ScriptingMode::Off;
    PromptBuilder::filter_actions_by_scripting_mode(actions, has_scripting)
}

/// Execute a single task
async fn execute_single_task(
    state: AppState,
    llm_client: OllamaClient,
    status_tx: mpsc::UnboundedSender<String>,
    task: crate::state::ScheduledTask,
) {
    use crate::llm::prompt::PromptBuilder;
    use crate::state::task::{TaskExecutionResult, TaskScope};

    let _ = status_tx.send(format!("[TASK] Executing task '{}'", task.name));

    // Get protocol actions if server, connection, or client-scoped
    let protocol_actions = match &task.scope {
        TaskScope::Server(server_id) | TaskScope::Connection(server_id, _) => {
            if let Some(protocol_name) = state.get_protocol_name(*server_id).await {
                if let Some(protocol) =
                    crate::protocol::server_registry::registry().get(&protocol_name)
                {
                    protocol.get_sync_actions()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
        TaskScope::Client(client_id) => {
            if let Some(protocol_name) = state.get_protocol_name_for_client(*client_id).await {
                if let Some(protocol) =
                    crate::protocol::client_registry::CLIENT_REGISTRY.get(&protocol_name)
                {
                    protocol.as_ref().get_sync_actions()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
        TaskScope::Global => Vec::new(),
    };

    // Actions genuinely available to this task. This MUST match the set advertised by
    // `PromptBuilder::build_task_execution_prompt` below, because `ConversationHandler`
    // derives `valid_action_names` from it and rejects anything else as an unknown action.
    let task_actions = build_task_actions(&state, &task.scope, protocol_actions.clone()).await;

    // Build prompt
    let prompt = PromptBuilder::build_task_execution_prompt(&state, &task, protocol_actions).await;

    // Get current model, ensuring one is selected
    let model = match crate::llm::ensure_model_selected(state.get_ollama_model().await).await {
        Ok(m) => m,
        Err(e) => {
            let error_msg = format!("Model selection failed: {}", e);
            let _ = status_tx.send(format!(
                "[ERROR] Failed to ensure model is selected for task execution: {}",
                e
            ));
            // Update task status to failed
            state
                .update_task_status(
                    task.id,
                    crate::state::task::TaskStatus::Failed(error_msg.clone()),
                )
                .await;
            let result = TaskExecutionResult {
                success: false,
                actions: Vec::new(),
                error: Some(error_msg),
            };
            state.record_task_execution(task.id, &result).await;
            return;
        }
    };

    // Register task as conversation
    let conversation_source = match &task.scope {
        TaskScope::Global => crate::state::app_state::ConversationSource::Task {
            task_name: task.name.clone(),
        },
        TaskScope::Server(server_id) => crate::state::app_state::ConversationSource::Task {
            task_name: format!("{}#{}", task.name, server_id.as_u32()),
        },
        TaskScope::Connection(server_id, conn_id) => {
            crate::state::app_state::ConversationSource::Task {
                task_name: format!("{}#{}/{}", task.name, server_id.as_u32(), conn_id),
            }
        }
        TaskScope::Client(client_id) => crate::state::app_state::ConversationSource::Task {
            task_name: format!("{}@{}", task.name, client_id.as_u32()),
        },
    };

    let truncated_instruction = crate::utils::truncate_for_log(&task.instruction, 27);

    // Get rate limiter for scheduled tasks (discards if rate limited)
    let rate_limiter = state.get_rate_limiter().await;

    // Create conversation handler with tracking
    let mut conversation = crate::llm::ConversationHandler::new(
        prompt.clone(),
        std::sync::Arc::new(llm_client.clone()),
        model.clone(),
        rate_limiter,
        crate::llm::RequestSource::Network, // Scheduled tasks are discarded if rate limited
    )
    .with_native_tools(&task_actions)
    .with_status_tx(status_tx.clone())
    .with_tracking(state.clone(), conversation_source, truncated_instruction);

    // Add empty user message to trigger generation
    conversation.add_user_message("Execute the task.".to_string());

    // Generate with conversation handler (handles tracking automatically)
    let web_search_mode = state.get_web_search_mode().await;
    let actions = match conversation
        .generate_with_tools_and_retry(
            state.get_web_approval_channel().await,
            web_search_mode,
            task_actions,
        )
        .await
    {
        Ok(actions) => actions,
        Err(e) => {
            // Execution failed
            let error = format!("LLM call failed: {}", e);
            let _ = status_tx.send(format!("[ERROR] Task '{}' failed: {}", task.name, error));

            let result = TaskExecutionResult {
                success: false,
                actions: Vec::new(),
                error: Some(error),
            };

            handle_task_failure(&state, &status_tx, task, result).await;
            return;
        }
    };

    // Get protocol for execution (if server, connection, or client-scoped)
    let protocol = match &task.scope {
        TaskScope::Server(server_id) | TaskScope::Connection(server_id, _) => state
            .get_protocol_name(*server_id)
            .await
            .and_then(|name| crate::protocol::server_registry::registry().get(&name)),
        TaskScope::Client(_client_id) => {
            // Client protocols are handled differently - they don't use the server protocol registry
            // For now, return None as task execution for clients needs client-specific implementation
            None
        }
        TaskScope::Global => None,
    };

    // Extract server_id and client_id from task scope for context
    let (server_id, client_id) = match &task.scope {
        TaskScope::Server(sid) | TaskScope::Connection(sid, _) => (Some(*sid), None),
        TaskScope::Client(cid) => (None, Some(*cid)),
        TaskScope::Global => (None, None),
    };

    // Execute actions with task context
    match crate::llm::execute_actions(
        actions.clone(),
        &state,
        protocol.as_deref(),
        server_id,
        client_id,
    )
    .await
    {
        Ok(_exec_result) => {
            // Success
            let _ = status_tx.send(format!(
                "[TASK] Task '{}' completed successfully",
                task.name
            ));

            let result = TaskExecutionResult {
                success: true,
                actions,
                error: None,
            };

            handle_task_success(&state, &status_tx, task, result).await;
        }
        Err(e) => {
            // Execution failed
            let error = format!("Action execution failed: {}", e);
            let _ = status_tx.send(format!("[ERROR] Task '{}' failed: {}", task.name, error));

            let result = TaskExecutionResult {
                success: false,
                actions,
                error: Some(error),
            };

            handle_task_failure(&state, &status_tx, task, result).await;
        }
    }
}

/// Handle task success
async fn handle_task_success(
    state: &AppState,
    status_tx: &mpsc::UnboundedSender<String>,
    task: crate::state::ScheduledTask,
    result: crate::state::TaskExecutionResult,
) {
    use crate::state::task::{TaskStatus, TaskType};
    use std::time::{Duration, Instant};

    // Record execution
    state.record_task_execution(task.id, &result).await;

    match &task.task_type {
        TaskType::OneShot { .. } => {
            // One-shot task completed
            state
                .update_task_status(task.id, TaskStatus::Completed)
                .await;
            state.remove_task(task.id).await;
            let _ = status_tx.send(format!(
                "[TASK] One-shot task '{}' completed and removed",
                task.name
            ));
        }
        TaskType::Recurring {
            interval_secs,
            max_executions,
            executions_count,
        } => {
            // Check if max executions reached
            if let Some(max) = max_executions {
                if *executions_count >= *max {
                    state
                        .update_task_status(task.id, TaskStatus::Completed)
                        .await;
                    state.remove_task(task.id).await;
                    let _ = status_tx.send(format!(
                        "[TASK] Recurring task '{}' reached max executions ({}) and removed",
                        task.name, max
                    ));
                    return;
                }
            }

            // Schedule next execution
            let next = Instant::now() + Duration::from_secs(*interval_secs);
            state.update_task_next_execution(task.id, next).await;
            state
                .update_task_status(task.id, TaskStatus::Scheduled)
                .await;
        }
    }
}

/// Handle task failure with exponential backoff retry
async fn handle_task_failure(
    state: &AppState,
    status_tx: &mpsc::UnboundedSender<String>,
    task: crate::state::ScheduledTask,
    result: crate::state::TaskExecutionResult,
) {
    use crate::state::task::TaskStatus;
    use std::time::{Duration, Instant};

    const MAX_FAILURES: u64 = 5;
    const BACKOFF_BASE_SECS: u64 = 60; // 1 minute base backoff

    // Record execution
    state.record_task_execution(task.id, &result).await;

    let failure_count = task.failure_count + 1;

    if failure_count >= MAX_FAILURES {
        // Too many failures, disable task
        state
            .update_task_status(
                task.id,
                TaskStatus::Failed(result.error.unwrap_or_else(|| "Unknown error".to_string())),
            )
            .await;
        state.remove_task(task.id).await;
        let _ = status_tx.send(format!(
            "[ERROR] Task '{}' failed {} times, removing from schedule",
            task.name, MAX_FAILURES
        ));
    } else {
        // Retry with exponential backoff
        let backoff_secs = BACKOFF_BASE_SECS * 2u64.pow((failure_count - 1) as u32);
        let next = Instant::now() + Duration::from_secs(backoff_secs);

        state.update_task_next_execution(task.id, next).await;
        state
            .update_task_status(task.id, TaskStatus::Scheduled)
            .await;

        let _ = status_tx.send(format!(
            "[WARN] Task '{}' failed (attempt {}/{}), retrying in {} seconds",
            task.name, failure_count, MAX_FAILURES, backoff_secs
        ));
    }
}

/// Clear the sticky footer area
fn clear_sticky_footer(footer: &StickyFooter) -> Result<()> {
    let mut stdout = stdout();
    let (_, height) = terminal::size()?;
    let footer_height = footer.calculate_footer_height();
    let footer_start = height.saturating_sub(footer_height);

    // Clear footer lines
    for line in footer_start..height {
        execute!(
            stdout,
            cursor::MoveTo(0, line),
            terminal::Clear(terminal::ClearType::CurrentLine),
        )?;
    }

    stdout.flush()?;
    Ok(())
}

/// Handle keyboard and other events
#[allow(clippy::too_many_arguments)]
async fn handle_event(
    event: Event,
    app: &mut App,
    state: &AppState,
    event_handler: &mut EventHandler,
    status_tx: &mpsc::UnboundedSender<String>,
    footer: &mut StickyFooter,
    settings: Arc<Mutex<Settings>>,
    palette: Arc<ColorPalette>,
    stats_monitor: &Arc<crate::system_stats::SystemStatsMonitor>,
) -> Result<bool> {
    match event {
        Event::Key(key) => {
            handle_key_event(
                key.code,
                key.modifiers,
                app,
                state,
                event_handler,
                status_tx,
                footer,
                settings,
                palette,
                stats_monitor,
            )
            .await
        }
        _ => Ok(false),
    }
}

/// Handle keyboard key events
#[allow(clippy::too_many_arguments)]
async fn handle_key_event(
    key_code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    state: &AppState,
    event_handler: &mut EventHandler,
    status_tx: &mpsc::UnboundedSender<String>,
    footer: &mut StickyFooter,
    settings: Arc<Mutex<Settings>>,
    palette: Arc<ColorPalette>,
    stats_monitor: &Arc<crate::system_stats::SystemStatsMonitor>,
) -> Result<bool> {
    // Handle web approval prompt first (if active)
    if let Some(approval) = footer.pending_approval.take() {
        use crate::state::app_state::{WebApprovalResponse, WebSearchMode};

        match (key_code, modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                // Ctrl-C during approval - deny and quit
                debug!("User pressed Ctrl-C during approval - denying and quitting");
                let _ = approval.response_tx.send(WebApprovalResponse::Deny);
                return Ok(true); // Signal quit
            }
            (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => {
                debug!("User approved web search");
                let _ = approval.response_tx.send(WebApprovalResponse::Allow);
                footer.render(&mut stdout())?;
                return Ok(false);
            }
            (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
                debug!("User denied web search");
                let _ = approval.response_tx.send(WebApprovalResponse::Deny);
                footer.render(&mut stdout())?;
                return Ok(false);
            }
            (KeyCode::Char('a'), _) | (KeyCode::Char('A'), _) => {
                debug!("User chose always allow - switching to ON mode");

                // Switch mode to ON
                state.set_web_search_mode(WebSearchMode::On).await;

                // Save to settings
                if let Err(e) = settings.lock().await.set_web_search_mode(WebSearchMode::On) {
                    error!("Failed to save web search mode: {}", e);
                }

                // Send response
                let _ = approval.response_tx.send(WebApprovalResponse::AlwaysAllow);

                // Update UI
                update_ui_from_state(app, state, footer).await;
                footer.render(&mut stdout())?;
                return Ok(false);
            }
            _ => {
                // Any other key - restore the approval and ignore
                footer.pending_approval = Some(approval);
                return Ok(false);
            }
        }
    }

    // If an interactive create/update form is active, drive it: plain Enter submits
    // the current field and advances, Esc cancels. Every other key falls through to
    // the normal input-editing keys below, so the operator edits the prefilled value
    // exactly as they would any input line.
    if app.active_form.is_some() {
        match (key_code, modifiers) {
            (KeyCode::Enter, m)
                if !m.contains(KeyModifiers::SHIFT)
                    && !m.contains(KeyModifiers::CONTROL)
                    && !m.contains(KeyModifiers::ALT) =>
            {
                advance_form(app, state, event_handler, footer, &palette).await?;
                update_ui_from_state(app, state, footer).await;
                footer.render(&mut stdout())?;
                return Ok(false);
            }
            (KeyCode::Esc, _) => {
                cancel_form(app, footer, &palette)?;
                footer.render(&mut stdout())?;
                return Ok(false);
            }
            _ => {}
        }
    }

    // Handle special keys first
    match key_code {
        // Ctrl+C to quit
        KeyCode::Char('c') | KeyCode::Char('C') if modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(true);
        }

        // Ctrl+E to toggle scripting ON/OFF
        KeyCode::Char('e') | KeyCode::Char('E') if modifiers.contains(KeyModifiers::CONTROL) => {
            let (new_mode, switched) = state.cycle_scripting_mode().await;

            if switched {
                let message = match new_mode {
                    crate::state::app_state::ScriptingMode::On => {
                        "Scripting enabled: LLM will choose runtime for each script"
                    }
                    crate::state::app_state::ScriptingMode::Off => {
                        "Scripting disabled: LLM will handle all requests directly"
                    }
                    crate::state::app_state::ScriptingMode::Python => {
                        "Scripting mode: Python (use /env to change)"
                    }
                    crate::state::app_state::ScriptingMode::JavaScript => {
                        "Scripting mode: JavaScript (use /env to change)"
                    }
                    crate::state::app_state::ScriptingMode::Go => {
                        "Scripting mode: Go (use /env to change)"
                    }
                    crate::state::app_state::ScriptingMode::Perl => {
                        "Scripting mode: Perl (use /env to change)"
                    }
                };
                print_output_line(message, footer, &palette)?;

                // Save the new scripting mode to settings
                let mode_str = new_mode.as_str().to_lowercase();
                if let Err(e) = settings.lock().await.set_scripting_mode(mode_str) {
                    error!("Failed to save scripting mode setting: {}", e);
                }

                update_ui_from_state(app, state, footer).await;
                footer.render(&mut stdout())?;
            }

            return Ok(false);
        }

        // Ctrl+L to cycle log level
        KeyCode::Char('l') | KeyCode::Char('L') if modifiers.contains(KeyModifiers::CONTROL) => {
            let new_level = app.log_level.cycle();
            app.set_log_level(new_level);
            footer.set_log_level(new_level);
            print_output_line(
                &format!("Log level set to: {}", new_level.as_str()),
                footer,
                &palette,
            )?;
            footer.render(&mut stdout())?;
            return Ok(false);
        }

        // Ctrl+W to cycle web search mode (ON -> ASK -> OFF -> ON)
        KeyCode::Char('w') | KeyCode::Char('W') if modifiers.contains(KeyModifiers::CONTROL) => {
            let new_mode = state.cycle_web_search_mode().await;
            let message = match new_mode {
                crate::state::app_state::WebSearchMode::On => {
                    "Web search: ON - LLM may perform web searches"
                }
                crate::state::app_state::WebSearchMode::Ask => {
                    "Web search: ASK - LLM will request approval before searching"
                }
                crate::state::app_state::WebSearchMode::Off => {
                    "Web search: OFF - LLM cannot perform web searches"
                }
            };
            print_output_line(message, footer, &palette)?;

            // Save the new web search mode to settings
            if let Err(e) = settings.lock().await.set_web_search_mode(new_mode) {
                error!("Failed to save web search setting: {}", e);
            }

            update_ui_from_state(app, state, footer).await;
            footer.render(&mut stdout())?;
            return Ok(false);
        }

        // Ctrl+H to cycle event handler mode (ANY -> SCRIPT -> STATIC -> LLM -> ANY)
        KeyCode::Char('h') | KeyCode::Char('H') if modifiers.contains(KeyModifiers::CONTROL) => {
            let new_mode = state.cycle_event_handler_mode().await;
            let message = match new_mode {
                crate::state::app_state::EventHandlerMode::Any => {
                    "Handler mode: ANY - LLM chooses handler types (script/static/llm) as appropriate"
                }
                crate::state::app_state::EventHandlerMode::Script => {
                    "Handler mode: SCRIPT - LLM must configure all events with script handlers"
                }
                crate::state::app_state::EventHandlerMode::Static => {
                    "Handler mode: STATIC - LLM must configure all events with static response handlers"
                }
                crate::state::app_state::EventHandlerMode::Llm => {
                    "Handler mode: LLM - LLM must configure all events to be handled by LLM (no scripts/static)"
                }
            };
            print_output_line(message, footer, &palette)?;

            update_ui_from_state(app, state, footer).await;
            footer.render(&mut stdout())?;
            return Ok(false);
        }

        // Ctrl+N or Alt+N to insert newline
        KeyCode::Char('n') | KeyCode::Char('N')
            if modifiers.contains(KeyModifiers::CONTROL)
                || modifiers.contains(KeyModifiers::ALT) =>
        {
            footer.input_mut().insert_newline();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Alt+Enter or Ctrl+Enter to insert newline (alternative to Shift+Enter)
        KeyCode::Enter
            if modifiers.contains(KeyModifiers::ALT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
        {
            footer.input_mut().insert_newline();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Enter to submit (plain enter only, not with modifiers)
        KeyCode::Enter
            if !modifiers.contains(KeyModifiers::SHIFT)
                && !modifiers.contains(KeyModifiers::CONTROL)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            let text = footer.input().text();
            if !text.is_empty() {
                // Add to history
                app.add_to_history(text.clone());

                // Parse command
                let command = UserCommand::parse(&text);

                // CRITICAL: Clear input and slash suggestions BEFORE executing command
                // This ensures the footer shrinks and scroll region is correct before we print output
                footer.input_mut().clear();
                app.update_slash_suggestions(&footer.input().text());

                // Update footer content (switch back to Normal mode since input is cleared)
                if app.slash_suggestions.is_empty() {
                    footer.set_content(FooterContent::Normal {
                        servers: app.servers.clone(),
                        clients: app.clients.clone(),
                        connections: app.connections.clone(),
                        tasks: app.tasks.clone(),
                        expand_all: app.expand_all_connections,
                        conversations: app.conversations.clone(),
                    });
                }

                // Render footer now so scroll region is updated before command execution
                footer.render(&mut stdout())?;

                // IMPORTANT: For SetFooterStatus and TestOutput, we DON'T print the command echo
                // - SetFooterStatus: Avoids positioning issues during footer expansion/shrinking
                // - TestOutput: Direct scroll region manipulation makes the echo unnecessary
                let print_echo_before = !matches!(
                    command,
                    UserCommand::SetFooterStatus { .. } | UserCommand::TestOutput { .. }
                );

                if print_echo_before {
                    print_output_line(&format!("[USER] {}", text), footer, &palette)?;
                }

                // `/create` and `/edit` open an interactive prefilled form. They are
                // handled here (not as `UserCommand` variants) so the form's field
                // model lives entirely in `cli::management`; once a form is active the
                // key interception above drives it.
                if let Some(form_start) = parse_form_start(&text) {
                    start_form(form_start, app, state, event_handler, footer, &palette).await?;
                    update_ui_from_state(app, state, footer).await;
                    footer.render(&mut stdout())?;
                    return Ok(false);
                }

                // Handle command
                match command {
                    UserCommand::Status
                    | UserCommand::ShowModel
                    | UserCommand::ShowLogLevel
                    | UserCommand::ShowWebSearch
                    | UserCommand::ShowEventHandler
                    | UserCommand::ShowEnvironment
                    | UserCommand::ShowStability
                    | UserCommand::ShowUsage => {
                        // Handle status/info commands
                        handle_status_command(
                            &command,
                            app,
                            state,
                            event_handler,
                            footer,
                            &palette,
                            &stats_monitor,
                        )
                        .await?;
                    }
                    UserCommand::ChangeModel { model } => {
                        state.set_ollama_model(Some(model.clone())).await;
                        app.connection_info.model = model.clone();
                        print_output_line(
                            &format!("Model changed to: {}", model),
                            footer,
                            &palette,
                        )?;

                        // Save the new model to settings
                        if let Err(e) = settings.lock().await.set_model(Some(model.clone())) {
                            error!("Failed to save model setting: {}", e);
                        }

                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::ChangeLogLevel { level } => {
                        if let Some(log_level) = crate::ui::app::LogLevel::parse(&level) {
                            app.set_log_level(log_level);
                            footer.set_log_level(log_level);
                            print_output_line(
                                &format!("Log level set to: {}", log_level.as_str()),
                                footer,
                                &palette,
                            )?;
                            footer.render(&mut stdout())?;
                        } else {
                            print_output_line(
                                &format!("Unknown log level: {}", level),
                                footer,
                                &palette,
                            )?;
                        }
                    }
                    UserCommand::TestOutput { count } => {
                        // Generate test output lines using print_output_line (scrolling mechanism)
                        // This ensures content is properly preserved during footer expansion/shrinking
                        for i in 1..=count {
                            print_output_line(
                                &format!("Test line {} of {}", i, count),
                                footer,
                                &palette,
                            )?;
                        }

                        // Re-render footer
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::TestAsk => {
                        // Test web search approval by triggering a search
                        use crate::llm::actions::tools::{execute_tool, ToolAction};

                        print_output_line(
                            "[INFO] Testing web search approval with DuckDuckGo...",
                            footer,
                            &palette,
                        )?;

                        // Get web search mode and approval channel
                        let web_search_mode = state.get_web_search_mode().await;
                        let approval_tx = state.get_web_approval_channel().await;

                        // Create a web search action for DuckDuckGo with a long path to test truncation
                        let action = ToolAction::WebSearch {
                            query: "https://duckduckgo.com/?q=test+search+query+with+very+long+parameters&ia=web&category=general&filters=none".to_string(),
                        };

                        // Execute the tool asynchronously (this will trigger approval prompt if in ASK mode)
                        let status_tx_clone = status_tx.clone();
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            let result = execute_tool(
                                &action,
                                approval_tx.as_ref(),
                                web_search_mode,
                                Some(&state_clone),
                            )
                            .await;

                            // Send result to status channel
                            if result.success {
                                let _ = status_tx_clone
                                    .send("[INFO] Web search completed successfully".to_string());
                                // Truncate result if too long
                                let result_preview =
                                    crate::utils::truncate_for_log(&result.result, 500);
                                let _ = status_tx_clone
                                    .send(format!("[DEBUG] Result preview: {}", result_preview));
                            } else {
                                let _ = status_tx_clone
                                    .send(format!("[ERROR] Web search failed: {}", result.result));
                            }
                        });
                    }
                    UserCommand::SetFooterStatus { message } => {
                        use std::fs::OpenOptions;
                        use std::io::Write as IoWrite;

                        // Write debug info to file
                        if let Ok(mut file) = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("./netget_debug.log")
                        {
                            let _ = writeln!(
                                file,
                                "[DEBUG] SetFooterStatus handler called with message: {:?}",
                                message
                            );
                        }

                        // Get current terminal dimensions from footer (terminal::size() returns 0 in PTY)
                        let term_width = footer.terminal_width();
                        let term_height = footer.terminal_height();

                        // Calculate old and new footer heights
                        let old_scroll_height = footer.scroll_region_height();
                        let old_footer_height = term_height.saturating_sub(old_scroll_height);
                        let old_footer_start = term_height.saturating_sub(old_footer_height);

                        // Set custom footer status message (this recalculates footer height)
                        footer.set_custom_status(message.clone());

                        let new_scroll_height = footer.scroll_region_height();
                        let new_footer_height = term_height.saturating_sub(new_scroll_height);

                        // Write footer height info to file
                        if let Ok(mut file) = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("./netget_debug.log")
                        {
                            let _ = writeln!(
                                file,
                                "[DEBUG] Footer heights: old={}, new={}, term_height={}",
                                old_footer_height, new_footer_height, term_height
                            );
                        }

                        // Handle footer size changes
                        if new_footer_height > old_footer_height {
                            // Footer is EXPANDING (e.g., 5 lines → 7 lines, increase by 2)
                            let lines_to_add = new_footer_height - old_footer_height;

                            // Try to consume from blank lines buffer first
                            let consumed = footer.consume_blank_lines_buffer(lines_to_add);
                            let lines_to_push = lines_to_add - consumed;

                            // Write debug info to file
                            if let Ok(mut file) = OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open("./netget_debug.log")
                            {
                                let _ = writeln!(file, "[DEBUG-EXPAND] Footer expanding: old_height={}, new_height={}, lines_to_add={}, consumed={}, lines_to_push={}",
                                    old_footer_height, new_footer_height, lines_to_add, consumed, lines_to_push);
                            }

                            // If buffer didn't have enough space, push content up BEFORE changing scroll region
                            if lines_to_push > 0 {
                                // Move cursor to bottom of the OLD scroll region (0-indexed)
                                let last_old_scroll_line = old_scroll_height.saturating_sub(1);
                                execute!(stdout(), cursor::MoveTo(0, last_old_scroll_line))?;

                                // Print newlines to scroll content up within the OLD scroll region
                                // This preserves all content by scrolling it up before we shrink the region
                                for _ in 0..lines_to_push {
                                    execute!(stdout(), Print("\n"))?;
                                }
                                stdout().flush()?;
                            }

                            // NOW set the new (smaller) scrolling region
                            print!("\x1b[1;{}r", new_scroll_height);
                            stdout().flush()?;

                            // Footer.render() will clear and draw the footer area
                        } else if new_footer_height < old_footer_height {
                            // Footer is SHRINKING (e.g., 7 lines → 5 lines, decrease by 2)
                            let lines_to_remove = old_footer_height - new_footer_height;

                            // Add shrunk lines to blank lines buffer - they become available blank lines at top
                            footer.add_to_blank_lines_buffer(lines_to_remove);

                            // Write debug info to file
                            if let Ok(mut file) = OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open("./netget_debug.log")
                            {
                                let _ = writeln!(file, "[DEBUG-SHRINK] Footer shrinking: lines_to_remove={}, buffer now={}",
                                    lines_to_remove, footer.blank_lines_buffer());
                            }

                            // Step 1: Clear the top N lines of the old footer (where N = lines_to_remove)
                            let blank_line = " ".repeat(term_width as usize);
                            for line_offset in 0..lines_to_remove {
                                execute!(
                                    stdout(),
                                    cursor::MoveTo(0, old_footer_start + line_offset),
                                    Print(&blank_line),
                                )?;
                            }
                            stdout().flush()?;

                            // Step 2: Update scrolling region to new height
                            print!("\x1b[1;{}r", new_scroll_height);
                            stdout().flush()?;
                        } else {
                            // Footer size UNCHANGED - no buffer manipulation needed
                            // Just log for debugging
                            if let Ok(mut file) = OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open("./netget_debug.log")
                            {
                                let _ = writeln!(
                                    file,
                                    "[DEBUG-UNCHANGED] Footer size unchanged: height={}, buffer={}",
                                    new_footer_height,
                                    footer.blank_lines_buffer()
                                );
                            }
                        }

                        // Step 4 (all cases): Redraw the footer at the new position
                        if let Ok(mut file) = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("./netget_debug.log")
                        {
                            let final_scroll_height = footer.scroll_region_height();
                            let final_footer_height =
                                footer.terminal_height().saturating_sub(final_scroll_height);
                            let final_footer_start =
                                footer.terminal_height().saturating_sub(final_footer_height);
                            let _ = writeln!(file, "[DEBUG] Before footer.render(): scroll_height={}, footer_height={}, footer_start={}",
                                final_scroll_height, final_footer_height, final_footer_start);
                        }
                        footer.render(&mut stdout())?;

                        // Command echo is suppressed for SetFooterStatus (see print_echo_before logic above)
                    }
                    UserCommand::ShowDocs { protocol } => {
                        use crate::docs;

                        if let Some(protocol_name) = protocol {
                            // Show detailed docs for specific protocol
                            match docs::show_protocol_docs(&protocol_name) {
                                Ok(docs_text) => {
                                    for line in docs_text.lines() {
                                        print_output_line(line, footer, &palette)?;
                                    }
                                }
                                Err(err_msg) => {
                                    print_output_line(&err_msg, footer, &palette)?;
                                }
                            }
                        } else {
                            // List all protocols
                            let docs_text = docs::list_all_protocols();
                            for line in docs_text.lines() {
                                print_output_line(line, footer, &palette)?;
                            }
                        }

                        footer.render(&mut stdout())?;
                    }
                    UserCommand::StopAll => {
                        // Stop all servers and clients
                        handle_stop_all(state, footer, &palette).await?;
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::StopById { id } => {
                        // Stop specific server, client, or connection by ID
                        handle_stop_by_id(id, state, footer, &palette).await?;
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::Save { name, id } => {
                        // Save configuration to file
                        handle_save(name, id, state, footer, &palette).await?;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::Load { name } => {
                        // Load configuration from file
                        let llm = event_handler.get_llm_client();
                        handle_load(name, state, footer, &palette, &llm).await?;
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    #[cfg(feature = "sqlite")]
                    UserCommand::Sqlite { db_id, query } => {
                        // Handle SQLite database commands
                        handle_sqlite(db_id, query, state, footer, &palette).await?;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::ListSimple => {
                        // List available simple protocols
                        handle_list_simple(footer, &palette)?;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::StartSimple { protocol } => {
                        // Start a simple protocol server
                        let llm = event_handler.get_llm_client();
                        handle_start_simple(
                            &protocol,
                            state,
                            footer,
                            &palette,
                            &llm,
                            status_tx.clone(),
                        )
                        .await?;
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::ShowBackend => {
                        let llm = event_handler.get_llm_client();
                        let backend_type = llm.backend_type();
                        let backend_url = llm.backend_url();
                        let current_model = state
                            .get_ollama_model()
                            .await
                            .unwrap_or_else(|| "None".to_string());
                        print_output_line(
                            &format!("LLM Backend: {}", backend_type),
                            footer,
                            &palette,
                        )?;
                        print_output_line(&format!("  URL: {}", backend_url), footer, &palette)?;
                        print_output_line(
                            &format!("  Model: {}", current_model),
                            footer,
                            &palette,
                        )?;
                        print_output_line("", footer, &palette)?;
                        print_output_line("To switch backend:", footer, &palette)?;
                        print_output_line(
                            "  /backend ollama [url]              - Switch to Ollama",
                            footer,
                            &palette,
                        )?;
                        print_output_line(
                            "  /backend openai <url> [api-key]    - Switch to OpenAI-compatible",
                            footer,
                            &palette,
                        )?;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::SetBackend { args: backend_args } => {
                        let parts: Vec<&str> = backend_args.splitn(3, ' ').collect();
                        match parts.first().map(|s| s.to_lowercase()).as_deref() {
                            Some("ollama") => {
                                let url = parts.get(1).unwrap_or(&"http://localhost:11434");
                                let new_client = crate::llm::OllamaClient::new(url.to_string())
                                    .with_app_state(state.clone());
                                event_handler.set_llm_client(new_client.clone());
                                state.set_llm_client(new_client).await;
                                print_output_line(
                                    &format!("✓ Switched to Ollama backend: {}", url),
                                    footer,
                                    &palette,
                                )?;
                            }
                            Some("openai") => {
                                if parts.len() < 2 {
                                    print_output_line(
                                        "✗ Usage: /backend openai <url> [api-key]",
                                        footer,
                                        &palette,
                                    )?;
                                } else {
                                    let url = parts[1];
                                    let api_key = if parts.len() >= 3 {
                                        parts[2].to_string()
                                    } else {
                                        std::env::var("NETGET_API_KEY")
                                            .or_else(|_| std::env::var("OPENAI_API_KEY"))
                                            .unwrap_or_default()
                                    };
                                    if api_key.is_empty() {
                                        print_output_line("✗ API key required. Use third argument or set NETGET_API_KEY/OPENAI_API_KEY env var.", footer, &palette)?;
                                    } else {
                                        let new_client =
                                            crate::llm::OllamaClient::new_openai(url, &api_key)
                                                .with_app_state(state.clone());
                                        event_handler.set_llm_client(new_client.clone());
                                        state.set_llm_client(new_client).await;
                                        print_output_line(
                                            &format!(
                                                "✓ Switched to OpenAI-compatible backend: {}",
                                                url
                                            ),
                                            footer,
                                            &palette,
                                        )?;
                                    }
                                }
                            }
                            _ => {
                                print_output_line("✗ Unknown backend. Use: /backend ollama [url] or /backend openai <url> [api-key]", footer, &palette)?;
                            }
                        }
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::Quit => {
                        return Ok(true);
                    }
                    UserCommand::UnknownSlashCommand { command } => {
                        print_output_line(
                            &format!("Unknown command: {}", command),
                            footer,
                            &palette,
                        )?;
                    }
                    UserCommand::Interpret { input: llm_input } => {
                        // Spawn async task to process with LLM
                        let mut handler_clone = event_handler.clone();
                        let status_tx_clone = status_tx.clone();
                        tokio::spawn(async move {
                            let _ = handler_clone
                                .handle_interpret_with_actions(llm_input, status_tx_clone, None)
                                .await;
                        });
                    }
                    UserCommand::SetEventHandler { mode } => {
                        // Set event handler mode
                        state.set_event_handler_mode(mode).await;
                        let message = match mode {
                            crate::state::app_state::EventHandlerMode::Any => {
                                "Event handler mode set to: ANY (LLM chooses handler types)"
                            }
                            crate::state::app_state::EventHandlerMode::Script => {
                                "Event handler mode set to: SCRIPT (force script handlers)"
                            }
                            crate::state::app_state::EventHandlerMode::Static => {
                                "Event handler mode set to: STATIC (force static responses)"
                            }
                            crate::state::app_state::EventHandlerMode::Llm => {
                                "Event handler mode set to: LLM (force LLM handlers)"
                            }
                        };

                        print_output_line(message, footer, &palette)?;

                        // Update footer status bar
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::SetWebSearch { mode } => {
                        state.set_web_search_mode(mode).await;
                        let message = match mode {
                            crate::state::app_state::WebSearchMode::On => "Web search: ON",
                            crate::state::app_state::WebSearchMode::Ask => {
                                "Web search: ASK - will request approval"
                            }
                            crate::state::app_state::WebSearchMode::Off => "Web search: OFF",
                        };
                        print_output_line(message, footer, &palette)?;

                        // Save the new web search mode to settings
                        if let Err(e) = settings.lock().await.set_web_search_mode(mode) {
                            error!("Failed to save web search setting: {}", e);
                        }

                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::Manage => {
                        // List running servers/clients and show create/update shapes
                        handle_manage(state, footer, &palette).await?;
                        footer.render(&mut stdout())?;
                    }
                    UserCommand::Update { id, instruction } => {
                        // Update a running server/client instruction by unified id
                        let llm = event_handler.get_llm_client();
                        handle_update(id, instruction, state, &llm, footer, &palette).await?;
                        update_ui_from_state(app, state, footer).await;
                        footer.render(&mut stdout())?;
                    }
                }
            }

            // Re-render footer after command execution (content may have changed)
            footer.render(&mut stdout())?;
            return Ok(false);
        }

        // Up arrow - command history navigation
        KeyCode::Up if footer.input().is_on_first_line() => {
            navigate_history_previous(app, footer);
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Down arrow - command history navigation
        KeyCode::Down if footer.input().is_on_last_line() => {
            navigate_history_next(app, footer);
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Ctrl+A - move to start of line
        KeyCode::Char('a') | KeyCode::Char('A') if modifiers.contains(KeyModifiers::CONTROL) => {
            footer.input_mut().move_to_start_of_line();
            footer.render_input_only(&mut stdout())?;
            return Ok(false);
        }

        // Ctrl+E - move to end of line
        KeyCode::Char('e') | KeyCode::Char('E') if modifiers.contains(KeyModifiers::CONTROL) => {
            footer.input_mut().move_to_end_of_line();
            footer.render_input_only(&mut stdout())?;
            return Ok(false);
        }

        // Ctrl+K - delete to end of line
        KeyCode::Char('k') | KeyCode::Char('K') if modifiers.contains(KeyModifiers::CONTROL) => {
            footer.input_mut().delete_to_end_of_line();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Ctrl+U - delete entire line
        KeyCode::Char('u') | KeyCode::Char('U') if modifiers.contains(KeyModifiers::CONTROL) => {
            footer.input_mut().delete_line();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Ctrl+W - delete word backward (standard Unix keybinding)
        KeyCode::Char('w') | KeyCode::Char('W') if modifiers.contains(KeyModifiers::CONTROL) => {
            footer.input_mut().delete_word();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Alt+Backspace - delete word backward (macOS/modern editor keybinding)
        KeyCode::Backspace if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().delete_word();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Alt+Delete - delete word forward
        KeyCode::Delete if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().delete_word_forward();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // Alt+Left - move cursor word left
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().move_cursor_word_left();
            footer.render_input_only(&mut stdout())?;
            return Ok(false);
        }

        // Alt+Right - move cursor word right
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().move_cursor_word_right();
            footer.render_input_only(&mut stdout())?;
            return Ok(false);
        }

        // Alt+b (common terminal sequence for Option+Left on macOS) - move cursor word left
        KeyCode::Char('b') if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().move_cursor_word_left();
            footer.render_input_only(&mut stdout())?;
            return Ok(false);
        }

        // Alt+f (common terminal sequence for Option+Right on macOS) - move cursor word right
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().move_cursor_word_right();
            footer.render_input_only(&mut stdout())?;
            return Ok(false);
        }

        // Alt+d (common terminal sequence for Option+Delete on macOS) - delete word forward
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::ALT) => {
            footer.input_mut().delete_word_forward();
            update_slash_suggestions_and_render(app, footer, &mut stdout())?;
            return Ok(false);
        }

        // E key - toggle expand all (if not typing)
        KeyCode::Char('e') | KeyCode::Char('E')
            if !modifiers.contains(KeyModifiers::CONTROL) && footer.input().text().is_empty() =>
        {
            app.toggle_expand_all();
            update_ui_from_state(app, state, footer).await;
            footer.render(&mut stdout())?;
            return Ok(false);
        }

        _ => {}
    }

    // Try to handle with InputState
    if footer.input_mut().handle_key(key_code, modifiers) {
        update_slash_suggestions_and_render(app, footer, &mut stdout())?;
        return Ok(false);
    }

    Ok(false)
}

/// Navigate to previous command in history
fn navigate_history_previous(app: &mut App, footer: &mut StickyFooter) {
    if app.command_history.is_empty() {
        return;
    }

    let input = footer.input_mut();
    match app.history_position {
        None => {
            // Starting history navigation - save current input
            let current = input.text();
            if !current.is_empty() {
                app.history_temp_input = Some(current);
            }
            // Go to most recent command
            let pos = app.command_history.len() - 1;
            app.history_position = Some(pos);
            *input = InputState::from_lines(
                app.command_history[pos]
                    .lines()
                    .map(|s| s.to_string())
                    .collect(),
            );
            input.move_to_top();
        }
        Some(pos) if pos > 0 => {
            // Go to older command
            let new_pos = pos - 1;
            app.history_position = Some(new_pos);
            *input = InputState::from_lines(
                app.command_history[new_pos]
                    .lines()
                    .map(|s| s.to_string())
                    .collect(),
            );
            input.move_to_top();
        }
        _ => {
            // Already at oldest command, do nothing
        }
    }
}

/// Navigate to next command in history
fn navigate_history_next(app: &mut App, footer: &mut StickyFooter) {
    let input = footer.input_mut();
    match app.history_position {
        Some(pos) if pos < app.command_history.len() - 1 => {
            // Go to newer command
            let new_pos = pos + 1;
            app.history_position = Some(new_pos);
            *input = InputState::from_lines(
                app.command_history[new_pos]
                    .lines()
                    .map(|s| s.to_string())
                    .collect(),
            );
            input.move_to_bottom();
        }
        Some(_) => {
            // At newest command, restore temp input or clear
            app.history_position = None;
            let temp = app.history_temp_input.take().unwrap_or_default();
            *input = InputState::from_lines(temp.lines().map(|s| s.to_string()).collect());
            input.move_to_bottom();
        }
        None => {
            // Not in history mode, do nothing
        }
    }
}

/// Update slash suggestions and render footer intelligently
/// Only re-renders full footer if suggestions actually changed
fn update_slash_suggestions_and_render(
    app: &mut App,
    footer: &mut StickyFooter,
    stdout: &mut impl Write,
) -> Result<()> {
    // Store old suggestions before updating
    let old_suggestions = app.slash_suggestions.clone();

    // Update suggestions based on current input
    app.update_slash_suggestions(&footer.input().text());

    // Check if suggestions actually changed
    if old_suggestions != app.slash_suggestions {
        // Update footer content based on new suggestions
        if app.slash_suggestions.is_empty() {
            footer.set_content(FooterContent::Normal {
                servers: app.servers.clone(),
                clients: app.clients.clone(),
                connections: app.connections.clone(),
                tasks: app.tasks.clone(),
                expand_all: app.expand_all_connections,
                conversations: app.conversations.clone(),
            });
        } else {
            footer.set_content(FooterContent::SlashCommands {
                suggestions: app.slash_suggestions.clone(),
            });
        }
        // Re-render entire footer (content changed)
        footer.render(stdout)?;
    } else {
        // Only re-render input line (suggestions unchanged)
        footer.render_input_only(stdout)?;
    }

    Ok(())
}

/// Update UI with current application state
async fn update_ui_from_state(app: &mut App, state: &AppState, footer: &mut StickyFooter) {
    use crate::ui::app::{ClientDisplayInfo, ConnectionDisplayInfo, ServerDisplayInfo};

    // Track old footer height BEFORE updating content
    let old_scroll_height = footer.scroll_region_height();
    let term_height = footer.terminal_height();
    let old_footer_height = term_height.saturating_sub(old_scroll_height);

    app.connection_info.mode = state.get_mode().await.to_string();
    app.connection_info.model = state
        .get_ollama_model()
        .await
        .unwrap_or_else(|| "None".to_string());

    // Update server list
    let servers = state.get_all_servers().await;
    app.servers = servers
        .iter()
        .map(|s| ServerDisplayInfo {
            id: format!("#{}", s.id.as_u32()),
            protocol: s.protocol_name.clone(),
            port: s.port,
            status: s.status.to_string(),
            connections: s.connections.len(),
        })
        .collect();

    // Update client list
    let clients = state.get_all_clients().await;
    app.clients = clients
        .iter()
        .map(|c| ClientDisplayInfo {
            id: format!("#{}", c.id.as_u32()),
            protocol: c.protocol_name.clone(),
            remote_addr: c.remote_addr.clone(),
            status: c.status.to_string(),
        })
        .collect();

    // Update connection list - collect into a temporary vec to avoid borrow issues
    let mut connections = Vec::new();
    for s in &servers {
        for conn in s.connections.values() {
            let network_conn_id = conn.id.to_string();
            let global_id = app.get_or_allocate_connection_id(network_conn_id);
            connections.push(ConnectionDisplayInfo {
                id: global_id,
                server_id: format!("#{}", s.id.as_u32()),
                address: conn.remote_addr.to_string(),
                state: match conn.status {
                    crate::state::server::ConnectionStatus::Active => "Active".to_string(),
                    crate::state::server::ConnectionStatus::Closed => "Closed".to_string(),
                },
            });
        }
    }
    app.connections = connections;

    // Fetch active conversations from state
    app.conversations = state.get_active_conversations().await;

    // Fetch scheduled tasks from state
    use crate::ui::app::TaskDisplayInfo;
    let all_tasks = state.get_all_tasks().await;
    app.tasks = all_tasks
        .iter()
        .map(|t| {
            let scope = match &t.scope {
                crate::state::task::TaskScope::Global => "Global".to_string(),
                crate::state::task::TaskScope::Server(sid) => format!("#{}", sid.as_u32()),
                crate::state::task::TaskScope::Connection(sid, cid) => {
                    format!("#{}:{}", sid.as_u32(), cid)
                }
                crate::state::task::TaskScope::Client(cid) => format!("Client #{}", cid.as_u32()),
            };
            let task_type = match &t.task_type {
                crate::state::task::TaskType::OneShot { delay_secs } => {
                    format!("OneShot({}s)", delay_secs)
                }
                crate::state::task::TaskType::Recurring {
                    interval_secs,
                    executions_count,
                    ..
                } => {
                    format!("Recurring({}s, {} runs)", interval_secs, executions_count)
                }
            };
            let status = match &t.status {
                crate::state::task::TaskStatus::Scheduled => "Scheduled".to_string(),
                crate::state::task::TaskStatus::Executing => "Executing".to_string(),
                crate::state::task::TaskStatus::Completed => "Completed".to_string(),
                crate::state::task::TaskStatus::Failed(err) => format!("Failed: {}", err),
            };
            TaskDisplayInfo {
                id: format!("T{}", t.id.as_u64()),
                name: t.name.clone(),
                scope,
                status,
                task_type,
            }
        })
        .collect();

    // Update footer content (this recalculates scroll region)
    if app.slash_suggestions.is_empty() {
        footer.set_content(FooterContent::Normal {
            servers: app.servers.clone(),
            clients: app.clients.clone(),
            connections: app.connections.clone(),
            tasks: app.tasks.clone(),
            expand_all: app.expand_all_connections,
            conversations: app.conversations.clone(),
        });
    } else {
        footer.set_content(FooterContent::SlashCommands {
            suggestions: app.slash_suggestions.clone(),
        });
    }

    // Update connection info
    if let Some(first_server) = servers.first() {
        app.connection_info.protocol = first_server.protocol_name.clone();
        if let Some(addr) = first_server.local_addr {
            app.connection_info.local_addr = Some(addr.to_string());
        }
    }

    let scripting_mode = state.get_selected_scripting_mode().await;
    let scripting_status = format_scripting_mode(scripting_mode);
    let web_search_mode = state.get_web_search_mode().await;
    let event_handler_mode = state.get_event_handler_mode().await;

    footer.set_connection_info(ConnectionInfo {
        model: app.connection_info.model.clone(),
        scripting_env: scripting_status,
        web_search_mode,
        event_handler_mode,
    });

    // CRITICAL: Handle footer size changes (expansion/shrinking)
    let new_scroll_height = footer.scroll_region_height();
    let new_footer_height = term_height.saturating_sub(new_scroll_height);

    if new_footer_height != old_footer_height {
        let term_width = footer.terminal_width();
        let old_footer_start = term_height.saturating_sub(old_footer_height);

        if new_footer_height > old_footer_height {
            // Footer is EXPANDING (e.g., connection added, causing footer to grow)
            let lines_to_add = new_footer_height - old_footer_height;

            // Try to consume from blank lines buffer first
            let consumed = footer.consume_blank_lines_buffer(lines_to_add);
            let lines_to_push = lines_to_add - consumed;

            // If buffer didn't have enough space, push content up BEFORE changing scroll region
            if lines_to_push > 0 {
                // Move cursor to bottom of the OLD scroll region (0-indexed)
                let last_old_scroll_line = old_scroll_height.saturating_sub(1);
                execute!(stdout(), cursor::MoveTo(0, last_old_scroll_line)).ok();

                // Print newlines to scroll content up within the OLD scroll region
                // This preserves all content by scrolling it up before we shrink the region
                for _ in 0..lines_to_push {
                    execute!(stdout(), Print("\n")).ok();
                }
                stdout().flush().ok();
            }

            // NOW set the new (smaller) scrolling region
            print!("\x1b[1;{}r", new_scroll_height);
            stdout().flush().ok();

            // Footer.render() will clear and draw the footer area
        } else if new_footer_height < old_footer_height {
            // Footer is SHRINKING (e.g., connection removed, causing footer to shrink)
            let lines_to_remove = old_footer_height - new_footer_height;

            // Add shrunk lines to blank lines buffer
            footer.add_to_blank_lines_buffer(lines_to_remove);

            // Clear the top N lines of the old footer
            let blank_line = " ".repeat(term_width as usize);
            for line_offset in 0..lines_to_remove {
                execute!(
                    stdout(),
                    cursor::MoveTo(0, old_footer_start + line_offset),
                    Print(&blank_line),
                )
                .ok();
            }
            stdout().flush().ok();

            // Update scrolling region to new height
            print!("\x1b[1;{}r", new_scroll_height);
            stdout().flush().ok();
        }
    }

    // NOTE: Callers are responsible for rendering the footer after this function
    // All call sites already do: update_ui_from_state() then footer.render()
}

/// Handle status/info commands
async fn handle_status_command(
    command: &UserCommand,
    app: &mut App,
    state: &AppState,
    event_handler: &mut EventHandler,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
    stats_monitor: &Arc<crate::system_stats::SystemStatsMonitor>,
) -> Result<()> {
    match command {
        UserCommand::Status => {
            print_output_line("=== Server Status ===", footer, palette)?;
            if app.servers.is_empty() {
                print_output_line("No servers running", footer, palette)?;
            } else {
                for server in &app.servers {
                    print_output_line(
                        &format!(
                            "Server {}: {} on port {} - {}",
                            server.id, server.protocol, server.port, server.status
                        ),
                        footer,
                        palette,
                    )?;
                }
            }
        }
        UserCommand::ShowModel => {
            let current_model = state
                .get_ollama_model()
                .await
                .unwrap_or_else(|| "None".to_string());
            print_output_line(
                &format!("Current model: {}", current_model),
                footer,
                palette,
            )?;
            print_output_line("", footer, palette)?;
            print_output_line("Fetching available models...", footer, palette)?;

            // Fetch model list from Ollama via event handler's LLM client
            match event_handler.list_models().await {
                Ok(models) => {
                    if models.is_empty() {
                        print_output_line(
                            "No models found. Please pull a model first.",
                            footer,
                            palette,
                        )?;
                        print_output_line("Example: ollama pull llama3.2", footer, palette)?;
                    } else {
                        print_output_line(
                            &format!("Available models ({}):", models.len()),
                            footer,
                            palette,
                        )?;
                        for model in &models {
                            if model == &current_model {
                                print_output_line(
                                    &format!("  * {} (current)", model),
                                    footer,
                                    palette,
                                )?;
                            } else {
                                print_output_line(&format!("    {}", model), footer, palette)?;
                            }
                        }
                        print_output_line("", footer, palette)?;
                        print_output_line("To change model, use: /model <name>", footer, palette)?;
                    }
                }
                Err(e) => {
                    print_output_line(&format!("Failed to fetch models: {}", e), footer, palette)?;
                    print_output_line("Make sure Ollama is running.", footer, palette)?;
                }
            }
        }
        UserCommand::ShowLogLevel => {
            print_output_line(
                &format!("Current log level: {}", app.log_level.as_str()),
                footer,
                palette,
            )?;
        }
        UserCommand::ShowWebSearch => {
            let mode = state.get_web_search_mode().await;
            let status = match mode {
                crate::state::app_state::WebSearchMode::On => "ON (always allowed)",
                crate::state::app_state::WebSearchMode::Ask => "ASK (requires approval)",
                crate::state::app_state::WebSearchMode::Off => "OFF (disabled)",
            };
            print_output_line(&format!("Web search mode: {}", status), footer, palette)?;
            print_output_line("", footer, palette)?;
            print_output_line(
                "To change, use: /web on, /web ask, or /web off",
                footer,
                palette,
            )?;
            print_output_line("Or press Ctrl+W to cycle through modes", footer, palette)?;
        }
        UserCommand::ShowEventHandler => {
            let mode = state.get_event_handler_mode().await;
            print_output_line(
                &format!("Current event handler mode: {}", mode),
                footer,
                palette,
            )?;
            print_output_line("", footer, palette)?;
            print_output_line(
                "To change, use: /handler any, /handler script, /handler static, or /handler llm",
                footer,
                palette,
            )?;
            print_output_line("Or press Ctrl+H to cycle through modes", footer, palette)?;
        }
        UserCommand::ShowStability => {
            let min = state.get_min_stability().await;
            for line in crate::protocol::stability_report(min) {
                print_output_line(&line, footer, palette)?;
            }
        }
        UserCommand::ShowEnvironment => {
            print_output_line("=== Environment Information ===", footer, palette)?;
            print_output_line(
                &format!("Platform: {}", std::env::consts::OS),
                footer,
                palette,
            )?;
            print_output_line(
                &format!("Architecture: {}", std::env::consts::ARCH),
                footer,
                palette,
            )?;
            if let Ok(cwd) = std::env::current_dir() {
                print_output_line(
                    &format!("Working directory: {}", cwd.display()),
                    footer,
                    palette,
                )?;
            }
            print_output_line(
                &format!(
                    "Model: {}",
                    state
                        .get_ollama_model()
                        .await
                        .unwrap_or_else(|| "None".to_string())
                ),
                footer,
                palette,
            )?;

            // List the protocols excluded from this run and WHY — the footer's
            // "N excluded (/env)" hint points here, so actually explain it.
            let caps = state.get_system_capabilities().await;
            let server_excluded =
                crate::protocol::server_registry::registry().get_excluded_protocols(&caps);
            let client_excluded = crate::protocol::CLIENT_REGISTRY.get_excluded_protocols(&caps);
            let total = server_excluded.len() + client_excluded.len();

            if total == 0 {
                print_output_line(
                    "Excluded protocols: none (all dependencies met)",
                    footer,
                    palette,
                )?;
            } else {
                print_output_line(
                    &format!(
                        "Excluded protocols: {} ({} server, {} client) — hidden from the model \
                         because a dependency is missing on this machine:",
                        total,
                        server_excluded.len(),
                        client_excluded.len()
                    ),
                    footer,
                    palette,
                )?;
                for (label, excluded) in
                    [("server", &server_excluded), ("client", &client_excluded)]
                {
                    let mut names: Vec<&String> = excluded.keys().collect();
                    names.sort();
                    for name in names {
                        // Deduplicate reasons (a protocol may miss several deps).
                        let reasons: Vec<String> = excluded[name]
                            .iter()
                            .map(|d| d.name())
                            .collect::<std::collections::BTreeSet<_>>()
                            .into_iter()
                            .collect();
                        print_output_line(
                            &format!("  [{}] {} — needs {}", label, name, reasons.join(", ")),
                            footer,
                            palette,
                        )?;
                    }
                }
            }

            // Privileged *default* ports are advice, not exclusion. These protocols
            // are fully available — they just cannot use their well-known port
            // without privilege, so name the port and move on. They used to be
            // reported above as excluded and hidden from the model, which meant a
            // non-root netget could not serve DNS, HTTP, SMTP or SSH at all, on any
            // port.
            let privileged_defaults =
                crate::protocol::server_registry::registry().privileged_default_ports(&caps);
            if !privileged_defaults.is_empty() {
                print_output_line(
                    &format!(
                        "Available, but not on their default port ({}): this process cannot bind \
                         below 1024, so pass a port >= 1024 (or run as root).",
                        privileged_defaults.len()
                    ),
                    footer,
                    palette,
                )?;
                for (name, port) in &privileged_defaults {
                    // +8000 keeps the well-known number readable and is always
                    // unprivileged (the largest privileged port is 1023, so the
                    // result is 8001..=9023). It also lands on the conventional
                    // choice for the common cases: 80 -> 8080, 443 -> 8443,
                    // 853 -> 8853.
                    print_output_line(
                        &format!(
                            "  [server] {} — defaults to {}; try {}",
                            name,
                            port,
                            port + 8000
                        ),
                        footer,
                        palette,
                    )?;
                }
            }
        }
        UserCommand::ShowUsage => {
            // Toggle usage stats display
            app.toggle_usage_stats();
            if app.show_usage_stats {
                print_output_line("Usage stats section enabled.", footer, palette)?;
                print_output_line(
                    "The usage panel is now visible in the footer.",
                    footer,
                    palette,
                )?;

                // Get current stats and update footer immediately
                let system_stats = stats_monitor.get_stats().await;
                let (input_tokens, output_tokens, llm_calls) = state.get_llm_stats().await;

                app.system_stats = system_stats.clone();
                footer.set_show_usage_stats(true);
                footer.set_system_stats(system_stats);
                footer.set_llm_stats(input_tokens, output_tokens, llm_calls);

                // Render footer to show stats immediately
                footer.render(&mut std::io::stdout())?;
            } else {
                print_output_line("Usage stats section disabled.", footer, palette)?;

                // Hide stats and re-render footer
                footer.set_show_usage_stats(false);
                footer.render(&mut std::io::stdout())?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `/manage` — list running servers and clients with their ids, and print the
/// create/update command shape.
///
/// This is the management view's list-and-shape surface. A full interactive
/// create/update form (prefilled from declared startup params) is a documented
/// follow-up; the data model (`cli::management::ServerForm`/`ClientForm`) and the
/// update executors it would drive are already in place, and are reachable today
/// via `/update`, the `update_server`/`update_client` actions, and the MCP tools.
async fn handle_manage(
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    print_output_line("=== Manage: running instances ===", footer, palette)?;

    let servers = state.get_all_servers().await;
    if servers.is_empty() {
        print_output_line("Servers: (none)", footer, palette)?;
    } else {
        print_output_line("Servers:", footer, palette)?;
        for s in &servers {
            print_output_line(
                &format!(
                    "  #{}  {}  port {}  [{}]",
                    s.id.as_u32(),
                    s.protocol_name,
                    s.port,
                    s.status
                ),
                footer,
                palette,
            )?;
        }
    }

    let clients = state.get_all_clients().await;
    if clients.is_empty() {
        print_output_line("Clients: (none)", footer, palette)?;
    } else {
        print_output_line("Clients:", footer, palette)?;
        for c in &clients {
            print_output_line(
                &format!(
                    "  #{}  {}  -> {}  [{}]",
                    c.id.as_u32(),
                    c.protocol_name,
                    c.remote_addr,
                    c.status
                ),
                footer,
                palette,
            )?;
        }
    }

    print_output_line("", footer, palette)?;
    print_output_line(
        "Create:  /create <protocol> [--client]   interactive prefilled form, or",
        footer,
        palette,
    )?;
    print_output_line(
        "         describe it in plain language, /simple <protocol>, or /load <file>.",
        footer,
        palette,
    )?;
    print_output_line(
        "Update:  /edit <id>   interactive form prefilled with the current config, or",
        footer,
        palette,
    )?;
    print_output_line(
        "         /update <id> <new instruction>   (hot instruction change), or the",
        footer,
        palette,
    )?;
    print_output_line(
        "         update_server / update_client action & MCP tool for full-form changes.",
        footer,
        palette,
    )?;

    Ok(())
}

/// `/update <id> <instruction>` — update a running server or client's instruction
/// in place, by unified id (server first, then client), via the shared management
/// update executor. Keeps live connections.
async fn handle_update(
    id: u32,
    instruction: String,
    state: &AppState,
    llm: &OllamaClient,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::cli::management::{self, ClientForm, ServerForm};
    use crate::state::{ClientId, ServerId};

    let (status_tx, _status_rx) = mpsc::unbounded_channel::<String>();

    // Server takes precedence when the id resolves to one.
    if state.get_server(ServerId::new(id)).await.is_some() {
        let form = ServerForm {
            instruction: Some(instruction),
            ..Default::default()
        };
        match management::update_server(state, ServerId::new(id), form, status_tx).await {
            Ok(outcome) => print_output_line(&outcome.summary, footer, palette)?,
            Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
        }
        return Ok(());
    }

    if state.get_client(ClientId::new(id)).await.is_some() {
        let form = ClientForm {
            instruction: Some(instruction),
            ..Default::default()
        };
        match management::update_client(state, ClientId::new(id), form, llm.clone(), status_tx)
            .await
        {
            Ok(outcome) => print_output_line(&outcome.summary, footer, palette)?,
            Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
        }
        return Ok(());
    }

    print_output_line(
        &format!("No server or client found with ID #{}", id),
        footer,
        palette,
    )?;
    Ok(())
}

// ===========================================================================
// Interactive create/update form driving (`/create`, `/edit`)
// ===========================================================================

/// What an operator asked to open with `/create` or `/edit`.
enum FormStart {
    /// `/create <protocol> [--client]`
    Create { protocol: String, is_client: bool },
    /// `/edit <id>`
    Edit { id: u32 },
}

/// Recognise the two form-entry slash commands. Returns `None` for anything else
/// (so normal command parsing continues). A malformed `/edit` (non-numeric id)
/// falls through to `None` and is reported as an unknown command.
fn parse_form_start(text: &str) -> Option<FormStart> {
    let t = text.trim();
    let lower = t.to_lowercase();

    if lower == "/create" || lower.starts_with("/create ") {
        let rest = t[7..].trim();
        let mut is_client = false;
        let mut protocol = String::new();
        for tok in rest.split_whitespace() {
            match tok {
                "--client" | "-c" | "client" => is_client = true,
                other if protocol.is_empty() => protocol = other.to_string(),
                _ => {}
            }
        }
        return Some(FormStart::Create {
            protocol,
            is_client,
        });
    }

    if lower == "/edit" || lower.starts_with("/edit ") {
        let rest = t[5..].trim();
        if let Ok(id) = rest.parse::<u32>() {
            return Some(FormStart::Edit { id });
        }
    }

    None
}

/// Replace the footer input contents with `text` (used to prefill each field).
fn set_input_text(footer: &mut StickyFooter, text: &str) {
    footer.input_mut().clear();
    for c in text.chars() {
        if c == '\n' {
            footer.input_mut().insert_newline();
        } else {
            footer.input_mut().insert_char(c);
        }
    }
}

/// Print the form title + the first field's prompt, prefill the input, and store
/// the form as active.
fn begin_form(
    app: &mut App,
    form: crate::cli::management::InteractiveForm,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    print_output_line(
        &format!(
            "=== {} — Enter submits each field, Esc cancels ===",
            form.title()
        ),
        footer,
        palette,
    )?;
    let prompt = form.prompt();
    let prefill = form.current_prefill();
    app.active_form = Some(form);
    print_output_line(&prompt, footer, palette)?;
    set_input_text(footer, &prefill);
    Ok(())
}

/// Open a create or update form. Create forms read the protocol's declared startup
/// params; edit forms prefill from the running instance's current config.
async fn start_form(
    fs: FormStart,
    app: &mut App,
    state: &AppState,
    _event_handler: &mut EventHandler,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::cli::management::{
        client_declared_params, server_declared_params, ClientPrefill, InteractiveForm,
        ServerPrefill,
    };
    use crate::state::{ClientId, ServerId};

    match fs {
        FormStart::Create {
            protocol,
            is_client,
        } => {
            if protocol.is_empty() {
                print_output_line("Usage: /create <protocol> [--client]", footer, palette)?;
                return Ok(());
            }
            let form = if is_client {
                match client_declared_params(&protocol) {
                    Some(schema) => InteractiveForm::create_client(&protocol, &schema),
                    None => {
                        print_output_line(
                            &format!(
                                "[ERROR] Unknown or unavailable client protocol '{}'",
                                protocol
                            ),
                            footer,
                            palette,
                        )?;
                        return Ok(());
                    }
                }
            } else {
                match server_declared_params(&protocol) {
                    Some(schema) => InteractiveForm::create_server(&protocol, &schema),
                    None => {
                        print_output_line(
                            &format!(
                                "[ERROR] Unknown or unavailable server protocol '{}'",
                                protocol
                            ),
                            footer,
                            palette,
                        )?;
                        return Ok(());
                    }
                }
            };
            begin_form(app, form, footer, palette)?;
        }
        FormStart::Edit { id } => {
            if let Some(s) = state.get_server(ServerId::new(id)).await {
                let schema = server_declared_params(&s.protocol_name).unwrap_or_default();
                let prefill = ServerPrefill::from_server(&s);
                let form = InteractiveForm::update_server(id, &s.protocol_name, &schema, &prefill);
                begin_form(app, form, footer, palette)?;
                return Ok(());
            }
            if let Some(c) = state.get_client(ClientId::new(id)).await {
                let schema = client_declared_params(&c.protocol_name).unwrap_or_default();
                let prefill = ClientPrefill {
                    remote_addr: c.remote_addr.clone(),
                    instruction: c.instruction.clone(),
                    memory: c.memory.clone(),
                    startup_params: c.startup_params.clone(),
                    event_handlers: c.event_handler_config.as_ref().and_then(|cc| {
                        serde_json::to_value(&cc.handlers)
                            .ok()
                            .and_then(|v| v.as_array().cloned())
                    }),
                    feedback_instructions: c.feedback_instructions.clone(),
                };
                let form = InteractiveForm::update_client(id, &c.protocol_name, &schema, &prefill);
                begin_form(app, form, footer, palette)?;
                return Ok(());
            }
            print_output_line(
                &format!("No server or client found with ID #{}", id),
                footer,
                palette,
            )?;
        }
    }
    Ok(())
}

/// Submit the current field (the footer input's text) and either advance to the
/// next field or, when complete, build the form and run create/update.
async fn advance_form(
    app: &mut App,
    state: &AppState,
    event_handler: &mut EventHandler,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    let raw = footer.input().text();

    // Refuse to advance past a required field left blank.
    if let Some(form) = app.active_form.as_ref() {
        if let Some(f) = form.current_field() {
            if f.required && raw.trim().is_empty() {
                print_output_line(
                    &format!("[WARN] '{}' is required — please enter a value.", f.name),
                    footer,
                    palette,
                )?;
                return Ok(());
            }
        }
    }

    let complete = {
        let form = app.active_form.as_mut().expect("form active");
        form.submit_current(&raw);
        form.is_complete()
    };

    if complete {
        finish_form(app, state, event_handler, footer, palette).await?;
    } else {
        let (prompt, prefill) = {
            let form = app.active_form.as_ref().expect("form active");
            (form.prompt(), form.current_prefill())
        };
        print_output_line(&prompt, footer, palette)?;
        set_input_text(footer, &prefill);
    }
    Ok(())
}

/// Cancel the active form.
fn cancel_form(app: &mut App, footer: &mut StickyFooter, palette: &ColorPalette) -> Result<()> {
    app.active_form = None;
    footer.input_mut().clear();
    print_output_line("Form cancelled.", footer, palette)?;
    Ok(())
}

/// Build the completed form into a `ServerForm`/`ClientForm` and drive it through
/// the shared create / `update_server` / `update_client` executors.
async fn finish_form(
    app: &mut App,
    state: &AppState,
    event_handler: &mut EventHandler,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::cli::management::{self, FormTarget};
    use crate::state::{ClientId, ServerId};

    let form = app.active_form.take().expect("form active");
    footer.input_mut().clear();
    let (status_tx, _rx) = mpsc::unbounded_channel::<String>();

    match form.target {
        FormTarget::CreateServer => match form.into_server_form() {
            Ok(sf) => {
                let proto = sf.protocol.clone();
                match sf.create(state, status_tx).await {
                    Ok(id) => print_output_line(
                        &format!("[SERVER] Created {} server #{}", proto, id.as_u32()),
                        footer,
                        palette,
                    )?,
                    Err(e) => print_output_line(
                        &format!("[ERROR] Create failed: {}", e),
                        footer,
                        palette,
                    )?,
                }
            }
            Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
        },
        FormTarget::CreateClient => {
            let llm = event_handler.get_llm_client();
            match form.into_client_form() {
                Ok(cf) => {
                    let proto = cf.protocol.clone();
                    match cf.create(state, llm, status_tx).await {
                        Ok(id) => print_output_line(
                            &format!("[CLIENT] Created {} client #{}", proto, id.as_u32()),
                            footer,
                            palette,
                        )?,
                        Err(e) => print_output_line(
                            &format!("[ERROR] Create failed: {}", e),
                            footer,
                            palette,
                        )?,
                    }
                }
                Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
            }
        }
        FormTarget::UpdateServer(id) => match form.into_server_form() {
            Ok(sf) => {
                match management::update_server(state, ServerId::new(id), sf, status_tx).await {
                    Ok(o) => print_output_line(&o.summary, footer, palette)?,
                    Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
                }
            }
            Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
        },
        FormTarget::UpdateClient(id) => {
            let llm = event_handler.get_llm_client();
            match form.into_client_form() {
                Ok(cf) => {
                    match management::update_client(state, ClientId::new(id), cf, llm, status_tx)
                        .await
                    {
                        Ok(o) => print_output_line(&o.summary, footer, palette)?,
                        Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
                    }
                }
                Err(e) => print_output_line(&format!("[ERROR] {}", e), footer, palette)?,
            }
        }
    }
    Ok(())
}

async fn handle_stop_all(
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::state::client::ClientStatus;
    use crate::state::server::ServerStatus;

    print_output_line(
        "Stopping all servers, connections, and clients...",
        footer,
        palette,
    )?;

    // Stop all servers
    let server_ids: Vec<_> = state.get_all_server_ids().await;
    for server_id in server_ids {
        state
            .update_server_status(server_id, ServerStatus::Stopped)
            .await;
        state.cleanup_server_tasks(server_id).await;
        print_output_line(
            &format!("[SERVER] Stopped server #{}", server_id.as_u32()),
            footer,
            palette,
        )?;
    }

    // Stop all clients
    let client_ids: Vec<_> = state.get_all_client_ids().await;
    for client_id in client_ids {
        state
            .update_client_status(client_id, ClientStatus::Disconnected)
            .await;
        state.cleanup_client_tasks(client_id).await;
        print_output_line(
            &format!("[CLIENT] Stopped client #{}", client_id.as_u32()),
            footer,
            palette,
        )?;
    }

    print_output_line("All servers and clients stopped.", footer, palette)?;
    Ok(())
}

async fn handle_stop_by_id(
    id: u32,
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::server::connection::ConnectionId;
    use crate::state::client::{ClientId, ClientStatus};
    use crate::state::server::{ServerId, ServerStatus};

    // Try to find what type of entity this ID corresponds to
    let mut found = false;

    // Check if it's a server
    let server_id = ServerId::new(id);
    if state.get_server(server_id).await.is_some() {
        state
            .update_server_status(server_id, ServerStatus::Stopped)
            .await;
        state.cleanup_server_tasks(server_id).await;
        print_output_line(&format!("[SERVER] Stopped server #{}", id), footer, palette)?;
        found = true;
    }

    // Check if it's a client
    let client_id = ClientId::new(id);
    if state.get_client(client_id).await.is_some() {
        state
            .update_client_status(client_id, ClientStatus::Disconnected)
            .await;
        state.cleanup_client_tasks(client_id).await;
        print_output_line(&format!("[CLIENT] Stopped client #{}", id), footer, palette)?;
        found = true;
    }

    // Check if it's a connection
    let connection_id = ConnectionId::new(id);
    let all_servers = state.get_all_servers().await;
    for server in all_servers {
        if server.connections.contains_key(&connection_id) {
            state
                .close_connection_on_server(server.id, connection_id)
                .await;
            print_output_line(
                &format!(
                    "[CONNECTION] Closed connection #{} on server #{}",
                    id,
                    server.id.as_u32()
                ),
                footer,
                palette,
            )?;
            found = true;
            break;
        }
    }

    if !found {
        print_output_line(
            &format!("No server, client, or connection found with ID #{}", id),
            footer,
            palette,
        )?;
    }

    Ok(())
}

async fn handle_save(
    name: String,
    id: Option<u32>,
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::state::client::ClientId;
    use crate::state::server::ServerId;
    use crate::utils::save_load;

    let path = if let Some(id_val) = id {
        // Save specific server or client by ID
        // Try server first
        let server_id = ServerId::new(id_val);
        if state.get_server(server_id).await.is_some() {
            match save_load::save_server(state, server_id, &name).await {
                Ok(path) => {
                    print_output_line(
                        &format!("[SAVE] Saved server #{} to: {}", id_val, path.display()),
                        footer,
                        palette,
                    )?;
                    path
                }
                Err(e) => {
                    print_output_line(
                        &format!("[ERROR] Failed to save server #{}: {}", id_val, e),
                        footer,
                        palette,
                    )?;
                    return Ok(());
                }
            }
        } else {
            // Try client
            let client_id = ClientId::new(id_val);
            if state.get_client(client_id).await.is_some() {
                match save_load::save_client(state, client_id, &name).await {
                    Ok(path) => {
                        print_output_line(
                            &format!("[SAVE] Saved client #{} to: {}", id_val, path.display()),
                            footer,
                            palette,
                        )?;
                        path
                    }
                    Err(e) => {
                        print_output_line(
                            &format!("[ERROR] Failed to save client #{}: {}", id_val, e),
                            footer,
                            palette,
                        )?;
                        return Ok(());
                    }
                }
            } else {
                print_output_line(
                    &format!("[ERROR] No server or client found with ID #{}", id_val),
                    footer,
                    palette,
                )?;
                return Ok(());
            }
        }
    } else {
        // Save all servers and clients
        match save_load::save_all(state, &name).await {
            Ok(path) => {
                let servers = state.get_all_servers().await;
                let clients = state.get_all_clients().await;
                print_output_line(
                    &format!(
                        "[SAVE] Saved {} server(s) and {} client(s) to: {}",
                        servers.len(),
                        clients.len(),
                        path.display()
                    ),
                    footer,
                    palette,
                )?;
                path
            }
            Err(e) => {
                print_output_line(
                    &format!("[ERROR] Failed to save configuration: {}", e),
                    footer,
                    palette,
                )?;
                return Ok(());
            }
        }
    };

    print_output_line(
        &format!(
            "[INFO] Use '/load {}' to restore this configuration",
            path.display()
        ),
        footer,
        palette,
    )?;
    Ok(())
}

async fn handle_load(
    name: String,
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
    llm: &crate::llm::OllamaClient,
) -> Result<()> {
    use crate::cli::{client_startup, server_startup};
    use crate::utils::save_load;

    // Load actions from file
    let actions = match save_load::load_actions(&name).await {
        Ok(actions) => actions,
        Err(e) => {
            print_output_line(
                &format!("[ERROR] Failed to load file '{}': {}", name, e),
                footer,
                palette,
            )?;
            return Ok(());
        }
    };

    if actions.is_empty() {
        print_output_line(
            &format!("[WARN] File '{}' contains no actions", name),
            footer,
            palette,
        )?;
        return Ok(());
    }

    print_output_line(
        &format!(
            "[LOAD] Loading {} action(s) from: {}",
            actions.len(),
            save_load::normalize_filename(&name)
        ),
        footer,
        palette,
    )?;

    // Execute each action
    for (i, action) in actions.iter().enumerate() {
        // Try to parse as common action
        if let Ok(common_action) = crate::llm::actions::common::CommonAction::from_json(action) {
            use crate::llm::actions::common::CommonAction;

            match common_action {
                CommonAction::OpenServer {
                    mac_address,
                    interface,
                    host,
                    port,
                    protocol,
                    send_first,
                    initial_memory,
                    instruction,
                    startup_params,
                    event_handlers,
                    scheduled_tasks,
                    feedback_instructions,
                } => {
                    // Create status channel for server startup messages
                    // Messages will be logged via tracing macros in the spawn method
                    let (status_tx, _status_rx) = tokio::sync::mpsc::unbounded_channel();

                    // Execute open_server action via server startup
                    match server_startup::start_server_from_action(
                        state,
                        mac_address,
                        interface.clone(),
                        host,
                        port,
                        &protocol,
                        send_first,
                        initial_memory,
                        instruction.clone(),
                        startup_params,
                        event_handlers,
                        scheduled_tasks,
                        feedback_instructions,
                        status_tx,
                    )
                    .await
                    {
                        Ok(server_id) => {
                            let binding_desc = if let Some(iface) = &interface {
                                format!("interface {} ({})", iface, protocol)
                            } else if let Some(p) = port {
                                format!("port {} ({})", p, protocol)
                            } else {
                                format!("({})", protocol)
                            };
                            print_output_line(
                                &format!(
                                    "[LOAD] Opened server #{} on {}",
                                    server_id.as_u32(),
                                    binding_desc
                                ),
                                footer,
                                palette,
                            )?;
                        }
                        Err(e) => {
                            print_output_line(
                                &format!("[ERROR] Failed to open server (action {}): {}", i + 1, e),
                                footer,
                                palette,
                            )?;
                        }
                    }
                }
                CommonAction::OpenClient {
                    protocol,
                    remote_addr,
                    instruction,
                    startup_params,
                    initial_memory,
                    event_handlers,
                    scheduled_tasks,
                    feedback_instructions,
                } => {
                    // Execute open_client action via client startup
                    match client_startup::start_client_from_action(
                        state,
                        &protocol,
                        &remote_addr,
                        instruction.clone(),
                        startup_params,
                        initial_memory,
                        event_handlers,
                        scheduled_tasks,
                        feedback_instructions,
                        llm.clone(),
                        None, // status_tx: legacy TUI keeps the drain-to-tracing behavior
                    )
                    .await
                    {
                        Ok(client_id) => {
                            print_output_line(
                                &format!(
                                    "[LOAD] Opened client #{} to {} ({})",
                                    client_id.as_u32(),
                                    remote_addr,
                                    protocol
                                ),
                                footer,
                                palette,
                            )?;
                        }
                        Err(e) => {
                            print_output_line(
                                &format!("[ERROR] Failed to open client (action {}): {}", i + 1, e),
                                footer,
                                palette,
                            )?;
                        }
                    }
                }
                CommonAction::ShowMessage { message } => {
                    print_output_line(&format!("[{}] {}", i + 1, message), footer, palette)?;
                }
                _ => {
                    print_output_line(
                        &format!("[WARN] Skipping unsupported action type (action {})", i + 1),
                        footer,
                        palette,
                    )?;
                }
            }
        } else {
            print_output_line(
                &format!("[WARN] Skipping invalid action (action {})", i + 1),
                footer,
                palette,
            )?;
        }
    }

    print_output_line("[LOAD] Configuration loaded successfully", footer, palette)?;
    Ok(())
}

#[cfg(feature = "sqlite")]
async fn handle_sqlite(
    db_id: Option<u32>,
    query: Option<String>,
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
) -> Result<()> {
    use crate::state::sqlite::DatabaseId;

    match (db_id, query) {
        (None, None) => {
            // List all databases
            let databases = state.get_all_databases().await;
            if databases.is_empty() {
                print_output_line("[SQLITE] No databases found", footer, palette)?;
            } else {
                print_output_line(
                    &format!("[SQLITE] Found {} database(s):", databases.len()),
                    footer,
                    palette,
                )?;
                for db in databases {
                    print_output_line(
                        &format!(
                            "  {} - {} ({}) - {} table(s), {} queries",
                            db.id,
                            db.name,
                            db.owner,
                            db.tables.len(),
                            db.query_count
                        ),
                        footer,
                        palette,
                    )?;
                }
            }
        }
        (Some(id), None) => {
            // Show schema for specific database
            let db_id = DatabaseId::new(id);
            if let Some(db) = state.get_database(db_id).await {
                print_output_line(
                    &format!(
                        "[SQLITE] Database: {} ({}) - {} ({} table(s))",
                        db.id,
                        db.name,
                        db.owner,
                        db.tables.len()
                    ),
                    footer,
                    palette,
                )?;
                if db.tables.is_empty() {
                    print_output_line("  No tables", footer, palette)?;
                } else {
                    for table in &db.tables {
                        print_output_line(
                            &format!("  Table: {} ({} rows)", table.name, table.row_count),
                            footer,
                            palette,
                        )?;
                        for column in &table.columns {
                            print_output_line(&format!("    {}", column), footer, palette)?;
                        }
                    }
                }
            } else {
                print_output_line(
                    &format!("[ERROR] Database {} not found", id),
                    footer,
                    palette,
                )?;
            }
        }
        (Some(id), Some(sql)) => {
            // Execute query on specific database
            let db_id = DatabaseId::new(id);
            match state.execute_sql(db_id, &sql).await {
                Ok(result) => {
                    print_output_line(
                        &format!("[SQLITE] Query executed on {}:", db_id),
                        footer,
                        palette,
                    )?;
                    // Format and display result
                    let formatted = result.format();
                    for line in formatted.lines() {
                        print_output_line(&format!("  {}", line), footer, palette)?;
                    }
                }
                Err(e) => {
                    print_output_line(&format!("[ERROR] Query failed: {}", e), footer, palette)?;
                }
            }
        }
        (None, Some(sql)) => {
            // Execute query on first database
            let databases = state.get_all_databases().await;
            if let Some(db) = databases.first() {
                let db_id = db.id;
                match state.execute_sql(db_id, &sql).await {
                    Ok(result) => {
                        print_output_line(
                            &format!("[SQLITE] Query executed on {}:", db_id),
                            footer,
                            palette,
                        )?;
                        // Format and display result
                        let formatted = result.format();
                        for line in formatted.lines() {
                            print_output_line(&format!("  {}", line), footer, palette)?;
                        }
                    }
                    Err(e) => {
                        print_output_line(
                            &format!("[ERROR] Query failed: {}", e),
                            footer,
                            palette,
                        )?;
                    }
                }
            } else {
                print_output_line("[ERROR] No databases found", footer, palette)?;
            }
        }
    }

    Ok(())
}

/// Handle /simple command - list available simple protocols
fn handle_list_simple(footer: &mut StickyFooter, palette: &ColorPalette) -> Result<()> {
    use crate::protocol::EASY_REGISTRY;

    print_output_line("Available simple protocols:", footer, palette)?;
    print_output_line("", footer, palette)?;

    let protocols = EASY_REGISTRY.get_all_names();
    if protocols.is_empty() {
        print_output_line(
            "  No simple protocols available (check compiled features)",
            footer,
            palette,
        )?;
    } else {
        for name in protocols {
            print_output_line(&format!("  - {}", name), footer, palette)?;
        }
    }

    print_output_line("", footer, palette)?;
    print_output_line("Usage: /simple <protocol>", footer, palette)?;
    print_output_line("Example: /simple http", footer, palette)?;

    Ok(())
}

/// Handle /simple <protocol> command - start a simple protocol server
async fn handle_start_simple(
    protocol: &str,
    state: &AppState,
    footer: &mut StickyFooter,
    palette: &ColorPalette,
    llm: &crate::llm::OllamaClient,
    _status_tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<()> {
    use crate::protocol::EASY_REGISTRY;

    // Check if protocol exists
    if EASY_REGISTRY.get_by_name(protocol).is_none() {
        print_output_line(
            &format!("[ERROR] Unknown simple protocol: {}", protocol),
            footer,
            palette,
        )?;
        print_output_line("Use /simple to list available protocols", footer, palette)?;
        return Ok(());
    }

    print_output_line(
        &format!("[SIMPLE] Starting simple protocol: {}", protocol),
        footer,
        palette,
    )?;

    // Start the easy protocol
    match crate::cli::easy_startup::start_easy_protocol(
        protocol,
        None, // user_instruction - could be extended later
        None, // port - could be extended later
        std::sync::Arc::new(state.clone()),
        std::sync::Arc::new(llm.clone()),
    )
    .await
    {
        Ok(easy_id) => {
            print_output_line(
                &format!(
                    "[SIMPLE] Started {} (easy instance #{})",
                    protocol,
                    easy_id.as_u32()
                ),
                footer,
                palette,
            )?;
        }
        Err(e) => {
            print_output_line(
                &format!("[ERROR] Failed to start {}: {}", protocol, e),
                footer,
                palette,
            )?;
        }
    }

    Ok(())
}
