//! Sticky footer rendering for rolling terminal
//!
//! Manages the fixed footer area at the bottom of the terminal that displays:
//! - Connections and servers (normal mode)
//! - Slash command suggestions (slash command mode)
//! - Input field (always)
//! - Status bar (always)

use anyhow::Result;
use crossterm::{
    cursor, execute,
    style::{Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use std::io::Write;
use tokio::sync::oneshot;
use tracing::debug;

use crate::state::app_state::{ConversationInfo, WebApprovalResponse, WebSearchMode};
use crate::ui::app::{
    ClientDisplayInfo, ConnectionDisplayInfo, LogLevel, PacketStats, ServerDisplayInfo,
};

use super::input_state::InputState;
use super::theme::ColorPalette;

// Layout constants for multi-column footer
const INPUTS_LEFT_MARGIN: u16 = 6;
const INPUTS_COLUMN_WIDTH: u16 = 22; // Reduced from 30 to fit three columns in 80-char terminal
const COLUMN_MARGIN: u16 = 2; // Reduced from 4 to save space

/// Pending web approval request
pub struct PendingApproval {
    pub url: String,
    pub response_tx: oneshot::Sender<WebApprovalResponse>,
}

/// Content mode for the sticky footer
#[derive(Debug, Clone)]
pub enum FooterContent {
    /// Normal mode: show servers, clients, and connections
    Normal {
        servers: Vec<ServerDisplayInfo>,
        clients: Vec<ClientDisplayInfo>,
        connections: Vec<ConnectionDisplayInfo>,
        tasks: Vec<crate::ui::app::TaskDisplayInfo>,
        expand_all: bool,
        conversations: Vec<ConversationInfo>,
    },
    /// Slash command mode: show command suggestions
    SlashCommands { suggestions: Vec<String> },
}

/// Connection info for the status bar
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub model: String,
    #[allow(dead_code)]
    pub scripting_env: String,
    pub web_search_mode: WebSearchMode,
    pub event_handler_mode: crate::state::app_state::EventHandlerMode,
}

impl Default for ConnectionInfo {
    fn default() -> Self {
        Self {
            model: String::new(),
            scripting_env: String::new(),
            web_search_mode: WebSearchMode::On,
            event_handler_mode: crate::state::app_state::EventHandlerMode::default(),
        }
    }
}

/// Sticky footer state and renderer
pub struct StickyFooter {
    /// Terminal width
    terminal_width: u16,
    /// Terminal height
    terminal_height: u16,
    /// Height of the scroll region (where output goes)
    scroll_region_height: u16,
    /// Current footer content mode
    content: FooterContent,
    /// Input state
    input: InputState,
    /// Connection info for status bar
    connection_info: ConnectionInfo,
    /// Packet statistics for status bar
    packet_stats: PacketStats,
    /// Current log level
    log_level: LogLevel,
    /// Custom status message (for testing footer height changes)
    custom_status: Option<String>,
    /// Number of blank lines at the top of scroll region (created by footer expansions)
    blank_lines_buffer: u16,
    /// Track last footer height to clear properly when shrinking
    last_footer_height: u16,
    /// Track last terminal width to detect resize and clear extra lines
    last_terminal_width: u16,
    /// Whether to show usage stats section
    show_usage_stats: bool,
    /// LLM token statistics (input, output, calls)
    llm_stats: (u64, u64, u64),
    /// System statistics (CPU, memory, GPU)
    system_stats: crate::system_stats::SystemStats,
    /// Track last scroll region height to clear from old position after resize
    last_scroll_region_height: u16,
    /// Pending web approval request (if any)
    pub pending_approval: Option<PendingApproval>,
    /// System capabilities (for privilege warnings in status bar)
    system_capabilities: crate::privilege::SystemCapabilities,
    /// Color palette for theming
    palette: ColorPalette,
}

impl StickyFooter {
    /// Create a new sticky footer
    pub fn new(
        width: u16,
        height: u16,
        system_capabilities: crate::privilege::SystemCapabilities,
        palette: ColorPalette,
    ) -> Result<Self> {
        let mut footer = Self {
            terminal_width: width,
            terminal_height: height,
            scroll_region_height: height.saturating_sub(10), // Initial guess
            content: FooterContent::Normal {
                servers: Vec::new(),
                clients: Vec::new(),
                connections: Vec::new(),
                tasks: Vec::new(),
                expand_all: false,
                conversations: Vec::new(),
            },
            input: InputState::new(),
            connection_info: ConnectionInfo::default(),
            packet_stats: PacketStats::default(),
            log_level: LogLevel::Info,
            custom_status: None,
            blank_lines_buffer: 0,
            last_footer_height: 0,
            last_terminal_width: width,
            show_usage_stats: false,
            llm_stats: (0, 0, 0),
            system_stats: crate::system_stats::SystemStats::default(),
            last_scroll_region_height: height.saturating_sub(10),
            pending_approval: None,
            system_capabilities,
            palette,
        };

        // Calculate actual footer height and set last_footer_height to match
        // This prevents the first render from adding blank lines
        footer.recalculate_scroll_region();
        footer.last_footer_height = footer.calculate_footer_height();
        Ok(footer)
    }

    /// Set footer content mode
    pub fn set_content(&mut self, content: FooterContent) {
        self.content = content;
        self.recalculate_scroll_region();
    }

    /// Get mutable reference to input state
    pub fn input_mut(&mut self) -> &mut InputState {
        &mut self.input
    }

    /// Get reference to input state
    pub fn input(&self) -> &InputState {
        &self.input
    }

    /// Set connection info
    pub fn set_connection_info(&mut self, info: ConnectionInfo) {
        self.connection_info = info;
    }

    /// Set packet stats
    pub fn set_packet_stats(&mut self, stats: PacketStats) {
        self.packet_stats = stats;
    }

    /// Set log level
    pub fn set_log_level(&mut self, level: LogLevel) {
        self.log_level = level;
    }

    /// Set custom status message (for testing footer height changes)
    pub fn set_custom_status(&mut self, status: Option<String>) {
        self.custom_status = status;
        self.recalculate_scroll_region();
    }

    /// Set usage stats visibility
    pub fn set_show_usage_stats(&mut self, show: bool) {
        self.show_usage_stats = show;
        self.recalculate_scroll_region();
    }

    /// Set LLM statistics
    pub fn set_llm_stats(&mut self, input_tokens: u64, output_tokens: u64, calls: u64) {
        self.llm_stats = (input_tokens, output_tokens, calls);
    }

    /// Set system statistics
    pub fn set_system_stats(&mut self, stats: crate::system_stats::SystemStats) {
        self.system_stats = stats;
    }

    /// Get scroll region height
    pub fn scroll_region_height(&self) -> u16 {
        self.scroll_region_height
    }

    pub fn terminal_height(&self) -> u16 {
        self.terminal_height
    }

    pub fn terminal_width(&self) -> u16 {
        self.terminal_width
    }

    pub fn blank_lines_buffer(&self) -> u16 {
        self.blank_lines_buffer
    }

    pub fn decrement_blank_lines_buffer(&mut self) {
        if self.blank_lines_buffer > 0 {
            self.blank_lines_buffer -= 1;
        }
    }

    pub fn add_to_blank_lines_buffer(&mut self, lines: u16) {
        self.blank_lines_buffer += lines;
    }

    pub fn consume_blank_lines_buffer(&mut self, lines: u16) -> u16 {
        let consumed = self.blank_lines_buffer.min(lines);
        self.blank_lines_buffer -= consumed;
        consumed
    }

    /// Handle terminal resize
    pub fn handle_resize(&mut self, width: u16, height: u16) {
        // Save the old scroll region height BEFORE recalculating
        // (This is needed for clearing wrapped content on resize)
        self.last_scroll_region_height = self.scroll_region_height;

        self.terminal_width = width;
        self.terminal_height = height;
        self.recalculate_scroll_region();
    }

    /// Calculate lines needed for normal content (two-column layout)
    fn calculate_normal_content_lines(
        &self,
        servers: &[ServerDisplayInfo],
        clients: &[ClientDisplayInfo],
        connections: &[ConnectionDisplayInfo],
        _tasks: &[crate::ui::app::TaskDisplayInfo],
        expand_all: bool,
        conversations: &[ConversationInfo],
    ) -> u16 {
        // If custom status is set, use its line count
        if let Some(ref custom) = self.custom_status {
            return custom.lines().count() as u16;
        }

        // Calculate inputs column height (All LLM conversations)
        let input_convs: Vec<_> = conversations.iter().collect();

        let mut inputs_height = 0u16;
        if !input_convs.is_empty() {
            inputs_height += 1; // Header line
            inputs_height += input_convs.len() as u16; // Each conversation is 1 line (truncated to fit column width)
        }

        // Calculate servers column height
        let mut servers_height = 0u16;
        if !servers.is_empty() {
            servers_height += 1; // Header line

            for server in servers.iter() {
                servers_height += 1; // Server line

                // Connection lines for this server
                let server_connections: Vec<_> = connections
                    .iter()
                    .filter(|c| c.server_id == server.id)
                    .collect();

                if !server_connections.is_empty() {
                    let max_to_show = if expand_all {
                        server_connections.len()
                    } else {
                        10
                    };

                    servers_height += max_to_show.min(server_connections.len()) as u16;

                    // Add conversation sub-items for each connection
                    for conn in server_connections
                        .iter()
                        .take(max_to_show.min(server_connections.len()))
                    {
                        // Count conversations for this specific connection
                        let conn_convs: Vec<_> = conversations
                            .iter()
                            .filter(|conv| {
                                matches!(&conv.source,
                                    crate::state::app_state::ConversationSource::Network { server_id, connection_id }
                                    if server_id.as_u32().to_string() == server.id && connection_id.map(|id| id.to_string()) == Some(conn.id.to_string())
                                )
                            })
                            .collect();
                        servers_height += conn_convs.len() as u16;
                    }

                    // "... N more" line if truncated
                    if !expand_all && server_connections.len() > 10 {
                        servers_height += 1;
                    }
                }
            }
        }

        // Calculate clients column height
        let mut clients_height = 0u16;
        if !clients.is_empty() {
            clients_height += 1; // Header line
            clients_height += clients.len() as u16; // Each client is 1 line
        }

        // Calculate usage stats height (header + 2 stat lines if enabled)
        let usage_height = if self.show_usage_stats { 3 } else { 0 };

        // Return the max of all columns (or 0 if all empty)
        inputs_height
            .max(servers_height)
            .max(clients_height)
            .max(usage_height)
    }

    /// Calculate lines needed for input (or approval prompt if pending)
    fn calculate_input_lines(&self) -> u16 {
        // If approval is pending, use 1 line for the approval prompt
        if self.pending_approval.is_some() {
            return 1;
        }

        let lines = self.input.lines();
        let mut total = 0;

        for line in lines {
            total += self.wrapped_line_count(line);
        }

        // Ensure at least 1 line, cap at 12
        total.clamp(1, 12)
    }

    /// Calculate how many visual lines a text string will take after wrapping
    fn wrapped_line_count(&self, text: &str) -> u16 {
        if text.is_empty() {
            return 1;
        }

        let width = self.terminal_width.saturating_sub(2) as usize; // -2 for padding
        if width == 0 {
            return 1;
        }

        // Use textwrap to calculate wrapped lines
        let wrapped = textwrap::wrap(text, width);
        wrapped.len().max(1) as u16
    }

    /// Recalculate scroll region height
    fn recalculate_scroll_region(&mut self) {
        let footer_height = self.calculate_footer_height();

        // Ensure scroll region is at least 5 lines
        self.scroll_region_height = self.terminal_height.saturating_sub(footer_height).max(5);
    }

    /// Render the sticky footer (overlay at bottom of terminal)
    pub fn render(&mut self, stdout: &mut impl Write) -> Result<()> {
        self.recalculate_scroll_region();

        let footer_height = self.calculate_footer_height();

        // If footer is expanding, push content up by printing newlines
        if footer_height > self.last_footer_height {
            let expansion = footer_height - self.last_footer_height;

            // Move to last line of scroll region and print newlines to push content up
            let scroll_height = self.scroll_region_height;
            let last_scroll_line = scroll_height.saturating_sub(1);

            execute!(stdout, cursor::MoveTo(0, last_scroll_line))?;
            for _ in 0..expansion {
                execute!(stdout, Print("\n"))?;
            }
        }

        // Clear footer area - use max of old and new height to clear remnants when shrinking
        // If width changed (resize), the old footer may have wrapped into many more lines
        // We clear from the start of output area to the bottom to ensure all artifacts are removed
        let width_changed = self.terminal_width != self.last_terminal_width;

        debug!(
            "Render: width_changed={}, terminal_width={}, last_terminal_width={}, scroll_region_height={}, terminal_height={}, footer_height={}",
            width_changed, self.terminal_width, self.last_terminal_width,
            self.scroll_region_height, self.terminal_height, footer_height
        );

        if width_changed {
            // Calculate how much wrapping could have occurred based on width ratio
            // If width shrunk from 292 to 146 (50%), each line could wrap into 2 lines
            // So a 4-line footer could become 8 lines
            let width_ratio = if self.terminal_width > 0 {
                self.last_terminal_width as f32 / self.terminal_width as f32
            } else {
                1.0
            };

            // Estimate how many lines the old footer took up after wrapping
            // Use last_footer_height (from before width change) and multiply by ratio
            let estimated_wrapped_lines =
                (self.last_footer_height as f32 * width_ratio).ceil() as u16;

            // Clear from terminal bottom up by this estimated amount
            let clear_start = self.terminal_height.saturating_sub(estimated_wrapped_lines);

            debug!(
                "CLEARING: Width changed {} -> {}, ratio {:.2}, clearing from line {} down (terminal_height={}, old_footer={}, estimated_wrapped={})",
                self.last_terminal_width, self.terminal_width, width_ratio, clear_start,
                self.terminal_height, self.last_footer_height, estimated_wrapped_lines
            );

            // Clear from the estimated start position down to bottom
            execute!(
                stdout,
                cursor::MoveTo(0, clear_start),
                Clear(ClearType::FromCursorDown),
            )?;
            stdout.flush()?;

            debug!("Clear complete");
        } else {
            // Normal case: clear max of old and new footer heights line by line
            let height_to_clear = footer_height.max(self.last_footer_height);
            let clear_start = self.terminal_height.saturating_sub(height_to_clear);

            for line_offset in 0..height_to_clear {
                execute!(
                    stdout,
                    cursor::MoveTo(0, clear_start + line_offset),
                    Clear(ClearType::CurrentLine),
                )?;
            }
        }

        // Update tracked dimensions for next render
        self.last_footer_height = footer_height;
        self.last_terminal_width = self.terminal_width;
        self.last_scroll_region_height = self.scroll_region_height;

        // Calculate fixed positions from bottom up
        // Status bar and input box (with borders) stay in fixed positions
        let status_line = self.terminal_height - 1;
        let input_box_bottom_line = status_line - 1;
        let input_lines = self.calculate_input_lines();
        let input_start = input_box_bottom_line - input_lines; // First line inside input box
        let input_box_top_line = input_start - 1; // Top border of input box (connects to content columns)

        // Content is positioned directly above the input box top border (no separator)
        let content_lines = match &self.content {
            FooterContent::Normal {
                servers,
                clients,
                connections,
                tasks,
                expand_all,
                conversations,
            } => self.calculate_normal_content_lines(
                servers,
                clients,
                connections,
                tasks,
                *expand_all,
                conversations,
            ),
            FooterContent::SlashCommands { suggestions } => suggestions.len().min(10) as u16,
        };

        // If we have content, render it directly above the input box
        if content_lines > 0 {
            let content_start = input_box_top_line - content_lines;

            // Render content
            match &self.content {
                FooterContent::Normal {
                    servers,
                    clients,
                    connections,
                    tasks,
                    expand_all,
                    conversations,
                } => self.render_normal_content(
                    stdout,
                    content_start,
                    servers,
                    clients,
                    connections,
                    tasks,
                    *expand_all,
                    conversations,
                )?,
                FooterContent::SlashCommands { suggestions } => {
                    // Slash commands still need a separator
                    let separator_before_content = content_start - 1;
                    self.render_separator(stdout, separator_before_content)?;
                    self.render_slash_commands(stdout, content_start, suggestions)?
                }
            };
        }

        // Render top border of input box
        if content_lines > 0 {
            self.render_input_box_top_with_columns(stdout, input_box_top_line, &self.content)?;
        } else {
            self.render_input_box_top(stdout, input_box_top_line)?;
        }

        // Render input or approval prompt (fixed position)
        let input_end_line = if let Some(ref approval) = self.pending_approval {
            // Render approval prompt instead of input
            let line = self.render_approval_prompt(stdout, input_start, &approval.url)?;
            // Hide cursor during approval
            execute!(stdout, cursor::Hide)?;
            line
        } else {
            // Render normal input
            self.render_input(stdout, input_start)?
        };

        // Render bottom border of input box
        self.render_input_box_bottom(stdout, input_end_line)?;

        // Render status bar (fixed position)
        self.render_status_bar(stdout, status_line)?;

        stdout.flush()?;
        Ok(())
    }

    /// Render only the input portion of the footer (for efficient keystroke handling)
    pub fn render_input_only(&mut self, stdout: &mut impl Write) -> Result<()> {
        // Calculate fixed positions from bottom up (same as render())
        let status_line = self.terminal_height - 1;
        let input_box_bottom_line = status_line - 1;
        let input_lines = self.calculate_input_lines();
        let input_box_borders = 2;
        let input_start = input_box_bottom_line - input_lines;
        let input_box_top_line = input_start - 1;

        // Clear input box area (including borders)
        for line_offset in 0..(input_lines + input_box_borders) {
            execute!(
                stdout,
                cursor::MoveTo(0, input_box_top_line + line_offset),
                Clear(ClearType::CurrentLine),
            )?;
        }

        // Render top border of input box
        self.render_input_box_top(stdout, input_box_top_line)?;

        // Render input or approval prompt and get the next line number
        let input_end_line = if let Some(ref approval) = self.pending_approval {
            // Render approval prompt
            let result = self.render_approval_prompt(stdout, input_start, &approval.url)?;
            execute!(stdout, cursor::Hide)?;
            result
        } else {
            // Render normal input
            self.render_input(stdout, input_start)?
        };

        // Render bottom border of input box
        self.render_input_box_bottom(stdout, input_end_line)?;

        // Clear and render status bar (directly below input box bottom border)
        execute!(
            stdout,
            cursor::MoveTo(0, status_line),
            Clear(ClearType::CurrentLine),
        )?;
        self.render_status_bar(stdout, status_line)?;

        // Position cursor (only if not in approval mode)
        if self.pending_approval.is_none() {
            self.position_cursor(stdout)?;
            execute!(stdout, cursor::Show)?;
        }

        stdout.flush()?;
        Ok(())
    }

    /// Get the calculated footer height (public for clearing)
    pub fn calculate_footer_height(&self) -> u16 {
        let content_lines = match &self.content {
            FooterContent::Normal {
                servers,
                clients,
                connections,
                tasks,
                expand_all,
                conversations,
            } => self.calculate_normal_content_lines(
                servers,
                clients,
                connections,
                tasks,
                *expand_all,
                conversations,
            ),
            FooterContent::SlashCommands { suggestions } => suggestions.len().min(10) as u16,
        };

        let input_lines = self.calculate_input_lines();
        let input_box_borders = 2; // Top and bottom borders of input box
        let status_lines = 1;

        // Add separator lines based on content type:
        // - Normal mode (servers/inputs): 0 separators (columns connect directly to input box top border)
        // - SlashCommands mode: 2 separators (one above content, one between content and input box)
        // - No content: 0 separators
        let separator_lines = match &self.content {
            FooterContent::Normal { .. } if content_lines > 0 => 0,
            FooterContent::SlashCommands { .. } if content_lines > 0 => 2,
            _ => 0,
        };
        content_lines + separator_lines + input_box_borders + input_lines + status_lines
    }

    /// Render normal content (two-column layout with floating headers)
    #[allow(clippy::too_many_arguments)]
    fn render_normal_content(
        &self,
        stdout: &mut impl Write,
        start_line: u16,
        servers: &[ServerDisplayInfo],
        clients: &[ClientDisplayInfo],
        connections: &[ConnectionDisplayInfo],
        tasks: &[crate::ui::app::TaskDisplayInfo],
        expand_all: bool,
        conversations: &[ConversationInfo],
    ) -> Result<u16> {
        // If custom status is set, render it (separator handled by main render)
        if let Some(ref custom) = self.custom_status {
            let mut current_line = start_line;
            for line in custom.lines() {
                execute!(
                    stdout,
                    cursor::MoveTo(INPUTS_LEFT_MARGIN, current_line),
                    SetForegroundColor(self.palette.dimmed),
                    Print(line),
                    ResetColor,
                )?;
                current_line += 1;
            }
            return Ok(current_line);
        }

        // All conversations shown in LLM column
        let input_convs: Vec<_> = conversations.iter().collect();

        // Calculate heights for each column
        let inputs_height = if input_convs.is_empty() {
            0
        } else {
            1 + input_convs.len() as u16
        };
        let mut servers_height = 0u16;
        if !servers.is_empty() || !tasks.is_empty() {
            servers_height = 1; // Header
            for server in servers {
                servers_height += 1; // Server line
                let server_conns: Vec<_> = connections
                    .iter()
                    .filter(|c| c.server_id == server.id)
                    .collect();
                let max_to_show = if expand_all {
                    server_conns.len()
                } else {
                    10.min(server_conns.len())
                };
                servers_height += max_to_show as u16;

                // Add conversation sub-items for each connection
                for conn in server_conns.iter().take(max_to_show) {
                    let conn_convs: Vec<_> = conversations
                        .iter()
                        .filter(|conv| {
                            matches!(&conv.source,
                                crate::state::app_state::ConversationSource::Network { server_id, connection_id }
                                if server_id.as_u32().to_string() == server.id && connection_id.map(|id| id.to_string()) == Some(conn.id.to_string())
                            )
                        })
                        .collect();
                    servers_height += conn_convs.len() as u16;
                }

                // Add tasks for this server
                let server_tasks: Vec<_> = tasks
                    .iter()
                    .filter(|t| t.scope.starts_with(&server.id))
                    .collect();
                servers_height += server_tasks.len() as u16;

                if !expand_all && server_conns.len() > 10 {
                    servers_height += 1;
                }
            }

            // Add global tasks
            let global_tasks: Vec<_> = tasks.iter().filter(|t| t.scope == "Global").collect();
            servers_height += global_tasks.len() as u16;
        }

        // Calculate clients height
        let clients_height = if clients.is_empty() {
            0
        } else {
            1 + clients.len() as u16
        };

        // Calculate usage stats height (header + 2 stat lines if enabled)
        let usage_height = if self.show_usage_stats { 3 } else { 0 };

        // If all columns are empty, don't render anything
        if inputs_height == 0 && servers_height == 0 && clients_height == 0 && usage_height == 0 {
            return Ok(start_line);
        }

        let total_height = inputs_height
            .max(servers_height)
            .max(clients_height)
            .max(usage_height);
        let servers_column_start = INPUTS_LEFT_MARGIN + INPUTS_COLUMN_WIDTH + COLUMN_MARGIN;
        let clients_column_start = servers_column_start + INPUTS_COLUMN_WIDTH + COLUMN_MARGIN;
        // Usage shares the same column position as clients (third column)
        let usage_column_start = clients_column_start;

        // Render line by line
        for line_offset in 0..total_height {
            let current_line = start_line + line_offset;

            // Determine if we should render inputs column content for this line
            let inputs_start_offset = total_height.saturating_sub(inputs_height);
            let render_inputs = line_offset >= inputs_start_offset;

            // Determine if we should render servers column content for this line
            let servers_start_offset = total_height.saturating_sub(servers_height);
            let render_servers = line_offset >= servers_start_offset;

            // Determine if we should render clients column content for this line
            let clients_start_offset = total_height.saturating_sub(clients_height);
            let render_clients = line_offset >= clients_start_offset;

            // Determine if we should render usage column content for this line
            let usage_start_offset = total_height.saturating_sub(usage_height);
            let render_usage = line_offset >= usage_start_offset;

            // Clear the line first
            execute!(
                stdout,
                cursor::MoveTo(0, current_line),
                Clear(ClearType::CurrentLine)
            )?;

            // Render inputs column
            if render_inputs {
                let inputs_line_idx = line_offset - inputs_start_offset;
                if inputs_line_idx == 0 {
                    // Header line - always use ┌
                    execute!(
                        stdout,
                        cursor::MoveTo(INPUTS_LEFT_MARGIN, current_line),
                        SetForegroundColor(self.palette.separator),
                        Print("┌──── "),
                        ResetColor,
                        Print("LLM")
                    )?;
                } else {
                    // Content line
                    let conv_idx = (inputs_line_idx - 1) as usize;
                    if conv_idx < input_convs.len() {
                        let conv = input_convs[conv_idx];
                        // Just show the details without the [User] prefix
                        let text = self.truncate_to_width(&conv.details, INPUTS_COLUMN_WIDTH - 2);
                        let is_completed = conv.end_time.is_some();
                        execute!(
                            stdout,
                            cursor::MoveTo(INPUTS_LEFT_MARGIN, current_line),
                            SetForegroundColor(self.palette.separator),
                            Print("│ "),
                            ResetColor,
                        )?;
                        if is_completed {
                            execute!(stdout, SetForegroundColor(self.palette.dimmed))?;
                        }
                        execute!(stdout, Print(&text), ResetColor)?;
                    }
                }
            }

            // Render column separator and servers column
            if render_servers {
                let servers_line_idx = line_offset - servers_start_offset;
                if servers_line_idx == 0 {
                    // Header line - always use ┌
                    execute!(
                        stdout,
                        cursor::MoveTo(servers_column_start, current_line),
                        SetForegroundColor(self.palette.separator),
                        Print("┌──── "),
                        ResetColor,
                        Print("Connections")
                    )?;
                } else {
                    // Content line - need to build server/connection content
                    let mut content_line_idx = servers_line_idx - 1;

                    for server in servers {
                        if content_line_idx == 0 {
                            // This is the server line
                            let text = format!(
                                "#{} {} :{} - {}",
                                server.id, server.protocol, server.port, server.status
                            );
                            let is_inactive =
                                server.status == "Stopped" || server.status.starts_with("Error:");
                            execute!(
                                stdout,
                                cursor::MoveTo(servers_column_start, current_line),
                                SetForegroundColor(self.palette.separator),
                                Print("│ "),
                                ResetColor,
                            )?;
                            if is_inactive {
                                execute!(stdout, SetForegroundColor(self.palette.dimmed))?;
                            }
                            execute!(stdout, Print(&text), ResetColor)?;
                            break;
                        }
                        content_line_idx -= 1;

                        // Check connections for this server
                        let server_conns: Vec<_> = connections
                            .iter()
                            .filter(|c| c.server_id == server.id)
                            .collect();
                        let max_to_show = if expand_all {
                            server_conns.len()
                        } else {
                            10.min(server_conns.len())
                        };

                        // Check if this is a connection line or a conversation sub-item
                        let mut found = false;
                        for (conn_idx, conn) in server_conns.iter().take(max_to_show).enumerate() {
                            if conn_idx as u16 == content_line_idx {
                                // This is the connection line
                                let text =
                                    format!("  #{} {} {}", conn.id, conn.address, conn.state);
                                let is_closed = conn.state == "Closed";
                                execute!(
                                    stdout,
                                    cursor::MoveTo(servers_column_start, current_line),
                                    SetForegroundColor(self.palette.separator),
                                    Print("│ "),
                                    ResetColor,
                                )?;
                                if is_closed {
                                    execute!(stdout, SetForegroundColor(self.palette.dimmed))?;
                                }
                                execute!(stdout, Print(&text), ResetColor)?;
                                found = true;
                                break;
                            }

                            // Skip past the connection line
                            if content_line_idx == 0 {
                                break;
                            }
                            content_line_idx -= 1;

                            // Check for conversation sub-items for this connection
                            let conn_convs: Vec<_> = conversations
                                .iter()
                                .filter(|conv| {
                                    matches!(&conv.source,
                                        crate::state::app_state::ConversationSource::Network { server_id, connection_id }
                                        if server_id.as_u32().to_string() == server.id && connection_id.map(|id| id.to_string()) == Some(conn.id.to_string())
                                    )
                                })
                                .collect();

                            if content_line_idx < conn_convs.len() as u16 {
                                // This is a conversation sub-item
                                let conv = conn_convs[content_line_idx as usize];
                                let is_completed = conv.end_time.is_some();
                                execute!(
                                    stdout,
                                    cursor::MoveTo(servers_column_start, current_line),
                                    SetForegroundColor(self.palette.separator),
                                    Print("│ "),
                                    ResetColor,
                                )?;
                                if is_completed {
                                    execute!(stdout, SetForegroundColor(self.palette.dimmed))?;
                                }
                                execute!(
                                    stdout,
                                    Print(format!("    {}", conv.details)),
                                    ResetColor
                                )?;
                                found = true;
                                break;
                            }
                            content_line_idx -= conn_convs.len() as u16;
                        }

                        if found {
                            break;
                        }

                        // Check for "... N more" line
                        if !expand_all && server_conns.len() > 10 {
                            if content_line_idx == 0 {
                                execute!(
                                    stdout,
                                    cursor::MoveTo(servers_column_start, current_line),
                                    SetForegroundColor(self.palette.separator),
                                    Print("│ "),
                                    ResetColor,
                                    SetForegroundColor(self.palette.dimmed),
                                    Print(format!("  ... ({} more)", server_conns.len() - 10)),
                                    ResetColor
                                )?;
                                break;
                            }
                            content_line_idx -= 1;
                        }

                        // Render tasks for this server
                        let server_tasks: Vec<_> = tasks
                            .iter()
                            .filter(|t| t.scope.starts_with(&server.id))
                            .collect();

                        if content_line_idx < server_tasks.len() as u16 {
                            let task = server_tasks[content_line_idx as usize];
                            let text = format!("  [T] {} - {}", task.name, task.status);
                            execute!(
                                stdout,
                                cursor::MoveTo(servers_column_start, current_line),
                                SetForegroundColor(self.palette.separator),
                                Print("│ "),
                                ResetColor,
                                SetForegroundColor(self.palette.dimmed),
                                Print(&text),
                                ResetColor
                            )?;
                            break;
                        }
                        content_line_idx -= server_tasks.len() as u16;
                    }

                    // After all servers, render global tasks
                    let global_tasks: Vec<_> =
                        tasks.iter().filter(|t| t.scope == "Global").collect();

                    if content_line_idx < global_tasks.len() as u16 {
                        let task = global_tasks[content_line_idx as usize];
                        let text = format!("[Global Task] {} - {}", task.name, task.status);
                        execute!(
                            stdout,
                            cursor::MoveTo(servers_column_start, current_line),
                            SetForegroundColor(self.palette.separator),
                            Print("│ "),
                            ResetColor,
                            SetForegroundColor(self.palette.dimmed),
                            Print(&text),
                            ResetColor
                        )?;
                    }
                }
            }

            // Render clients column
            if render_clients {
                let clients_line_idx = line_offset - clients_start_offset;
                if clients_line_idx == 0 {
                    // Header line
                    execute!(
                        stdout,
                        cursor::MoveTo(clients_column_start, current_line),
                        SetForegroundColor(self.palette.separator),
                        Print("┌──── "),
                        ResetColor,
                        Print("Clients")
                    )?;
                } else {
                    // Content line - render client
                    let client_idx = (clients_line_idx - 1) as usize;
                    if client_idx < clients.len() {
                        let client = &clients[client_idx];
                        let text = format!(
                            "{} {} → {} ({})",
                            client.id, client.protocol, client.remote_addr, client.status
                        );
                        let is_inactive =
                            client.status == "Disconnected" || client.status.starts_with("Error:");
                        execute!(
                            stdout,
                            cursor::MoveTo(clients_column_start, current_line),
                            SetForegroundColor(self.palette.separator),
                            Print("│ "),
                            ResetColor,
                        )?;
                        if is_inactive {
                            execute!(stdout, SetForegroundColor(self.palette.dimmed))?;
                        }
                        execute!(stdout, Print(&text), ResetColor)?;
                    }
                }
            }

            // Render usage stats column
            if render_usage {
                let usage_line_idx = line_offset - usage_start_offset;
                if usage_line_idx == 0 {
                    // Header line
                    execute!(
                        stdout,
                        cursor::MoveTo(usage_column_start, current_line),
                        SetForegroundColor(self.palette.separator),
                        Print("┌──── "),
                        ResetColor,
                        Print("Usage")
                    )?;
                } else if usage_line_idx == 1 {
                    // Line 1: CPU and Memory stats (compact format)
                    let cpu = format!("{:.0}%", self.system_stats.cpu_usage);
                    let mem_used = self.system_stats.memory_used_str();
                    let mem_total = self.system_stats.memory_total_str();

                    let gpu_display = if let Some(gpu_pct) = self.system_stats.gpu_usage {
                        format!(" G:{:.0}%", gpu_pct)
                    } else {
                        String::new()
                    };

                    let text = format!("C:{} M:{}/{}{}", cpu, mem_used, mem_total, gpu_display);
                    execute!(
                        stdout,
                        cursor::MoveTo(usage_column_start, current_line),
                        SetForegroundColor(self.palette.separator),
                        Print("│ "),
                        ResetColor,
                        SetForegroundColor(self.palette.dimmed),
                        Print(&text),
                        ResetColor
                    )?;
                } else if usage_line_idx == 2 {
                    // Line 2: LLM stats (compact format)
                    let (input_tokens, output_tokens, llm_calls) = self.llm_stats;
                    // Format: "L:5 12k/8k" (calls, in/out in thousands)
                    let format_tokens = |t: u64| {
                        if t >= 1_000_000 {
                            format!("{}m", t / 1_000_000)
                        } else if t >= 1_000 {
                            format!("{}k", t / 1_000)
                        } else {
                            format!("{}", t)
                        }
                    };
                    let text = format!(
                        "L:{} {}/{}",
                        llm_calls,
                        format_tokens(input_tokens),
                        format_tokens(output_tokens)
                    );
                    execute!(
                        stdout,
                        cursor::MoveTo(usage_column_start, current_line),
                        SetForegroundColor(self.palette.separator),
                        Print("│ "),
                        ResetColor,
                        SetForegroundColor(self.palette.dimmed),
                        Print(&text),
                        ResetColor
                    )?;
                }
            }
        }

        Ok(start_line + total_height)
    }

    /// Truncate text to fit within a given width
    fn truncate_to_width(&self, text: &str, max_width: u16) -> String {
        if text.len() <= max_width as usize {
            text.to_string()
        } else {
            let truncate_at = (max_width as usize).saturating_sub(3);
            crate::utils::truncate_for_log(text, truncate_at)
        }
    }

    /// Render slash command suggestions
    fn render_slash_commands(
        &self,
        stdout: &mut impl Write,
        start_line: u16,
        suggestions: &[String],
    ) -> Result<u16> {
        let mut current_line = start_line;

        let max_lines = suggestions.len().min(10);
        for suggestion in suggestions.iter().take(max_lines) {
            let wrapped = self.wrap_text(suggestion);
            for line in wrapped {
                execute!(
                    stdout,
                    cursor::MoveTo(0, current_line),
                    SetForegroundColor(self.palette.dimmed),
                    Print(&line),
                    ResetColor,
                )?;
                current_line += 1;
            }
        }

        Ok(current_line)
    }

    /// Render a separator line
    fn render_separator(&self, stdout: &mut impl Write, line: u16) -> Result<u16> {
        let separator = "─".repeat(self.terminal_width as usize);
        execute!(
            stdout,
            cursor::MoveTo(0, line),
            SetForegroundColor(self.palette.separator),
            Print(&separator),
            ResetColor,
        )?;
        Ok(line + 1)
    }

    /// Render top border of input box (┌─────┐)
    fn render_input_box_top(&self, stdout: &mut impl Write, line: u16) -> Result<u16> {
        execute!(
            stdout,
            cursor::MoveTo(0, line),
            SetForegroundColor(self.palette.separator),
            Print("┌"),
            Print("─".repeat((self.terminal_width - 2) as usize)),
            Print("┐"),
            ResetColor,
        )?;
        Ok(line + 1)
    }

    /// Render top border of input box with column connections (┌──┴──┐)
    fn render_input_box_top_with_columns(
        &self,
        stdout: &mut impl Write,
        line: u16,
        content: &FooterContent,
    ) -> Result<u16> {
        // Start with the left corner
        execute!(
            stdout,
            cursor::MoveTo(0, line),
            SetForegroundColor(self.palette.separator),
            Print("┌"),
        )?;

        // Determine where to place ┴ join characters based on content type
        let mut join_positions = Vec::new();

        if let FooterContent::Normal {
            servers,
            tasks,
            conversations,
            ..
        } = content
        {
            // All conversations shown in LLM column
            let input_convs: Vec<_> = conversations.iter().collect();

            // Add ┴ at inputs column position if inputs exist
            if !input_convs.is_empty() {
                join_positions.push(INPUTS_LEFT_MARGIN);
            }

            // Add ┴ at servers column position if servers or tasks exist
            if !servers.is_empty() || !tasks.is_empty() {
                let servers_column_start = INPUTS_LEFT_MARGIN + INPUTS_COLUMN_WIDTH + COLUMN_MARGIN;
                join_positions.push(servers_column_start);
            }

            // Add ┴ at usage/clients column position (third column) if either exists
            // Note: clients and usage share the same column position
            let servers_column_start = INPUTS_LEFT_MARGIN + INPUTS_COLUMN_WIDTH + COLUMN_MARGIN;
            let third_column_start = servers_column_start + INPUTS_COLUMN_WIDTH + COLUMN_MARGIN;

            if self.show_usage_stats {
                join_positions.push(third_column_start);
            }
        }

        // Draw the horizontal line with joins
        for col in 1..(self.terminal_width - 1) {
            if join_positions.contains(&col) {
                execute!(stdout, cursor::MoveTo(col, line), Print("┴"))?;
            } else {
                execute!(stdout, cursor::MoveTo(col, line), Print("─"))?;
            }
        }

        // End with the right corner
        execute!(
            stdout,
            cursor::MoveTo(self.terminal_width - 1, line),
            Print("┐"),
            ResetColor,
        )?;

        Ok(line + 1)
    }

    /// Render bottom border of input box (└─────┘)
    fn render_input_box_bottom(&self, stdout: &mut impl Write, line: u16) -> Result<u16> {
        execute!(
            stdout,
            cursor::MoveTo(0, line),
            SetForegroundColor(self.palette.separator),
            Print("└"),
            Print("─".repeat((self.terminal_width - 2) as usize)),
            Print("┘"),
            ResetColor,
        )?;
        Ok(line + 1)
    }

    /// Render input field with rectangular box
    fn render_input(&self, stdout: &mut impl Write, start_line: u16) -> Result<u16> {
        let mut current_line = start_line;

        let input_lines = self.input.lines();
        let max_input_lines = self.calculate_input_lines() as usize;

        for (idx, line) in input_lines.iter().enumerate() {
            if idx >= max_input_lines {
                break;
            }

            // Left border with green color
            execute!(
                stdout,
                cursor::MoveTo(0, current_line),
                SetForegroundColor(self.palette.separator),
                Print("│ "),
                ResetColor
            )?;

            // Add prefix and content
            let prefix = if idx == 0 { "> " } else { "  " };
            let text_with_prefix = format!("{}{}", prefix, line);

            let wrapped = self.wrap_text(&text_with_prefix);
            for wrapped_line in wrapped {
                // Print the wrapped line content
                execute!(stdout, Print(&wrapped_line))?;

                // Calculate padding needed to reach right border
                let content_width = wrapped_line.chars().count();
                let available_width = (self.terminal_width - 4) as usize; // -4 for "│ " on left and " │" on right
                let padding = available_width.saturating_sub(content_width);

                // Right border with padding and green color
                execute!(
                    stdout,
                    Print(" ".repeat(padding)),
                    Print(" "),
                    SetForegroundColor(self.palette.separator),
                    Print("│"),
                    ResetColor
                )?;

                current_line += 1;
            }
        }

        Ok(current_line)
    }

    /// Render status bar
    /// Get dependency status for the status bar
    /// Returns (status_text, color) - empty string if all protocols are available
    fn get_dependency_status(&self) -> (String, crossterm::style::Color) {
        use crate::protocol::{registry, CLIENT_REGISTRY};

        // Check how many protocols are excluded
        tracing::debug!("get_dependency_status: Accessing server registry...");
        let server_excluded = registry().get_excluded_protocols(&self.system_capabilities);
        tracing::debug!(
            "get_dependency_status: Server registry accessed, {} excluded",
            server_excluded.len()
        );

        tracing::debug!("get_dependency_status: Accessing client registry...");
        let client_excluded = CLIENT_REGISTRY.get_excluded_protocols(&self.system_capabilities);
        tracing::debug!(
            "get_dependency_status: Client registry accessed, {} excluded",
            client_excluded.len()
        );

        let total_excluded = server_excluded.len() + client_excluded.len();

        if total_excluded == 0 {
            ("".to_string(), self.palette.success)
        } else {
            (
                format!("{} excluded (/env)", total_excluded),
                self.palette.ask,
            )
        }
    }

    fn render_status_bar(&self, stdout: &mut impl Write, line: u16) -> Result<u16> {
        let (web_status, web_color) = match self.connection_info.web_search_mode {
            WebSearchMode::On => ("ON", self.palette.success),
            WebSearchMode::Off => ("OFF", self.palette.failure),
            WebSearchMode::Ask => ("ASK", self.palette.ask),
        };

        // Determine handler color: green for ANY, yellow for others
        let handler_color = match self.connection_info.event_handler_mode {
            crate::state::app_state::EventHandlerMode::Any => self.palette.success,
            _ => self.palette.ask,
        };

        execute!(stdout, cursor::MoveTo(0, line))?;

        // Print each segment with appropriate coloring
        execute!(
            stdout,
            SetForegroundColor(self.palette.dimmed),
            Print(" Model:"),
            ResetColor,
            Print(self.connection_info.model.to_string()),
            SetForegroundColor(self.palette.dimmed),
            Print(" | Log:"),
            ResetColor,
            SetForegroundColor(self.log_level.color()),
            Print(self.log_level.as_str().to_string()),
            ResetColor,
            SetForegroundColor(self.palette.dimmed),
            Print(" <^l>"),
            Print(" | WebSearch:"),
            ResetColor,
            SetForegroundColor(web_color),
            Print(web_status.to_string()),
            ResetColor,
            SetForegroundColor(self.palette.dimmed),
            Print(" <^w>"),
            Print(" | Handler:"),
            ResetColor,
            SetForegroundColor(handler_color),
            Print(self.connection_info.event_handler_mode.as_str().to_string()),
            ResetColor,
            SetForegroundColor(self.palette.dimmed),
            Print(" <^h>"),
            ResetColor,
        )?;

        // Add dependency status indicator - but only as much of it as fits.
        //
        // **The status bar is the last line of the terminal, so overflowing it corrupts it.**
        // Nothing here accounted for the width: the segments above are a fixed cost, and this
        // suffix is variable ("7 excluded (/env)" when seven scripting runtimes are missing),
        // so on a wide-enough build the line ran past column 80, the terminal wrapped it, and
        // the continuation overwrote the *start* of the same row - the bar rendered as
        // "(/env):None | Log:INFO ..." with `Model:` gone. It showed up at `--all-features`,
        // where more runtimes are reported missing, and `tests/terminal_snapshot` caught it.
        //
        // Widths are counted in characters rather than bytes: the segments are ASCII, but the
        // model name is arbitrary text and slicing it by byte could split a UTF-8 sequence.
        tracing::debug!("render_status_bar: Getting dependency status...");
        let (dep_status, dep_color) = self.get_dependency_status();
        tracing::debug!(
            "render_status_bar: Dependency status retrieved: '{}'",
            dep_status
        );
        if !dep_status.is_empty() {
            let used = " Model:".chars().count()
                + self.connection_info.model.chars().count()
                + " | Log:".chars().count()
                + self.log_level.as_str().chars().count()
                + " <^l>".chars().count()
                + " | WebSearch:".chars().count()
                + web_status.chars().count()
                + " <^w>".chars().count()
                + " | Handler:".chars().count()
                + self
                    .connection_info
                    .event_handler_mode
                    .as_str()
                    .chars()
                    .count()
                + " <^h>".chars().count();

            // " | " plus the text itself.
            let available = (self.terminal_width as usize).saturating_sub(used + 3);
            let shown: String = if dep_status.chars().count() <= available {
                dep_status
            } else if available > 1 {
                dep_status
                    .chars()
                    .take(available - 1)
                    .chain(['…'])
                    .collect()
            } else {
                String::new()
            };

            if !shown.is_empty() {
                execute!(
                    stdout,
                    SetForegroundColor(self.palette.dimmed),
                    Print(" |"),
                    ResetColor,
                    SetForegroundColor(dep_color),
                    Print(format!(" {}", shown)),
                    ResetColor,
                )?;
            }
        }

        Ok(line + 1)
    }

    /// Position the cursor in the input field
    fn position_cursor(&self, stdout: &mut impl Write) -> Result<()> {
        let (cursor_row, cursor_col) = self.input.cursor_position();

        // Calculate visual position considering wrapping, box borders, and "> " prefix
        let input_start_line = self.terminal_height - self.calculate_input_lines() - 2; // -2 for status bar and input_box_bottom

        let mut visual_row = input_start_line;
        let mut visual_col = 2; // Start at column 2 to account for "│ " left border

        let input_lines = self.input.lines();

        for (idx, line) in input_lines.iter().enumerate() {
            let prefix = if idx == 0 { "> " } else { "  " };

            if idx < cursor_row {
                // Count wrapped lines for previous rows
                let text_with_prefix = format!("{}{}", prefix, line);
                let wrapped = self.wrap_text(&text_with_prefix);
                visual_row += wrapped.len() as u16;
            } else if idx == cursor_row {
                // Handle empty input as a special case - cursor goes right after prefix
                if line.is_empty() && cursor_col == 0 {
                    visual_col += prefix.len() as u16;
                    break;
                }

                // Calculate position on current row
                // cursor_col is position within the actual text (not including prefix)
                // We need to add prefix length to get the visual column
                let cursor_in_line = prefix.len() + cursor_col;
                let text_with_prefix = format!("{}{}", prefix, line);
                let wrapped = self.wrap_text(&text_with_prefix);

                // Handle case where wrapped is empty
                if wrapped.is_empty() {
                    visual_col += prefix.len() as u16;
                } else {
                    let mut char_count = 0;
                    let mut found = false;
                    for (wrap_idx, wrapped_line) in wrapped.iter().enumerate() {
                        // `cursor_col` is a character index (see input_state's module
                        // docs), so the wrapped segments must be measured in characters
                        // too. `.len()` here counted bytes, which put the cursor several
                        // columns to the right of the text on any non-ASCII line.
                        let line_end = char_count + wrapped_line.chars().count();
                        if cursor_in_line <= line_end {
                            visual_row += wrap_idx as u16;
                            visual_col += (cursor_in_line - char_count) as u16;
                            found = true;
                            break;
                        }
                        char_count = line_end;
                        visual_row += 1;
                    }
                    // Fallback: if cursor position wasn't found in wrapped lines,
                    // place it at the end of the last line or after prefix
                    if !found {
                        visual_col += prefix.len() as u16;
                    }
                }
                break;
            }
        }

        execute!(stdout, cursor::MoveTo(visual_col, visual_row))?;
        Ok(())
    }

    /// Word-wrap text to terminal width
    fn wrap_text(&self, text: &str) -> Vec<String> {
        let width = self.terminal_width.saturating_sub(2) as usize; // -2 for safety
        if width == 0 || text.is_empty() {
            return vec![String::new()];
        }

        textwrap::wrap(text, width)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Render approval prompt when web search approval is pending
    fn render_approval_prompt(
        &self,
        stdout: &mut impl Write,
        start_line: u16,
        url: &str,
    ) -> Result<u16> {
        let mut current_line = start_line;

        // Parse URL to extract protocol, domain and path
        let (protocol, domain, path) = if url.starts_with("http://") || url.starts_with("https://")
        {
            // Parse as URL
            if let Ok(parsed_url) = url::Url::parse(url) {
                let protocol = parsed_url.scheme().to_string() + "://";
                let domain = parsed_url.host_str().unwrap_or("unknown").to_string();
                let path = parsed_url.path().to_string();
                (protocol, domain, path)
            } else {
                // Fallback: treat whole thing as domain
                (String::new(), url.to_string(), String::new())
            }
        } else {
            // Treat as search query
            (String::new(), "search".to_string(), url.to_string())
        };

        // Calculate available width for URL display
        // "Web Search Request: " = 20 chars
        // " | (Y)es | (N)o | (A)llow All" = 30 chars
        // Total prefix/suffix = 50 chars
        let available_width = self.terminal_width.saturating_sub(50) as usize;

        // Format the URL parts
        let protocol_part = protocol;
        let domain_part = domain;
        let path_part = if path.is_empty() {
            String::new()
        } else {
            // Calculate remaining width after protocol and domain
            let used_width = protocol_part.len() + domain_part.len();
            let remaining_width = available_width.saturating_sub(used_width);
            if path.len() <= remaining_width {
                path
            } else if remaining_width > 3 {
                crate::utils::truncate_for_log(&path, remaining_width.saturating_sub(3))
            } else {
                String::new()
            }
        };

        // Render the approval prompt
        execute!(stdout, cursor::MoveTo(0, current_line))?;
        execute!(
            stdout,
            Print(" Web Search Request: "),
            // Protocol in grey (de-emphasized)
            SetForegroundColor(self.palette.dimmed),
            Print(&protocol_part),
            // Domain in normal color (stands out)
            SetForegroundColor(self.palette.normal),
            Print(&domain_part),
            // Path in grey (de-emphasized)
            SetForegroundColor(self.palette.dimmed),
            Print(&path_part),
            ResetColor,
            Print(" | "),
            SetForegroundColor(self.palette.success),
            Print("(Y)"),
            ResetColor,
            Print("es | "),
            SetForegroundColor(self.palette.failure),
            Print("(N)"),
            ResetColor,
            Print("o | "),
            SetForegroundColor(self.palette.info),
            Print("(A)"),
            ResetColor,
            Print("llow All"),
        )?;

        current_line += 1;
        Ok(current_line)
    }
}
