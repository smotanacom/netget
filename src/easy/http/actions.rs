use anyhow::{Context, Result};
use serde_json::{json, Value as JsonValue};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::llm::actions::Easy;
use crate::llm::template_engine::{TemplateDataBuilder, TEMPLATE_ENGINE};
use crate::llm::OllamaClient;
use crate::protocol::Event;
use crate::state::AppState;

/// HTTP-easy protocol - simplified HTTP server for "dumb models"
///
/// This protocol wraps the standard HTTP server and provides a simplified
/// interface where the LLM responds in Markdown instead of JSON actions.
///
/// Flow:
/// 1. HTTP-easy generates open_server action for HTTP protocol
/// 2. HTTP server receives request and fires http_request event
/// 3. EventHandler routes event to HTTP-easy handler (via server_to_easy mapping)
/// 4. HTTP-easy transforms request into conversational prompt
/// 5. LLM responds in Markdown
/// 6. HTTP-easy converts Markdown to HTML
/// 7. HTTP-easy generates send_http_response action
/// 8. HTTP server sends HTML response
pub struct HttpEasyProtocol;

impl Easy for HttpEasyProtocol {
    fn protocol_name(&self) -> &'static str {
        "http"
    }

    fn underlying_protocol(&self) -> &'static str {
        "HTTP"
    }

    fn default_port(&self) -> Option<u16> {
        Some(8080)
    }

    fn generate_startup_action(
        &self,
        user_instruction: Option<String>,
        port: Option<u16>,
    ) -> Result<JsonValue> {
        let port = port.unwrap_or_else(|| self.default_port().unwrap());
        let instruction = user_instruction.unwrap_or_else(|| {
            "You are a helpful HTTP server. Respond to requests with useful information."
                .to_string()
        });

        // Generate open_server action for HTTP protocol
        Ok(json!({
            "type": "open_server",
            "protocol": "HTTP",
            "port": port,
            "instruction": instruction,
        }))
    }

    fn handle_event(
        &self,
        event: Event,
        user_instruction: Option<String>,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<JsonValue>>> + Send>> {
        Box::pin(async move {
            let event_type = &event.event_type;

            // Only handle http_request events
            if event_type.id != "http_request" {
                return Ok(vec![]);
            }

            // Extract request details from event data
            let method = event
                .data
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET");
            let uri = event
                .data
                .get("uri")
                .and_then(|v| v.as_str())
                .unwrap_or("/");
            let body = event
                .data
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // Convert headers object to array for template
            let headers_array: Vec<serde_json::Value> = event
                .data
                .get("headers")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .map(|(k, v)| {
                            json!({
                                "name": k,
                                "value": v.as_str().unwrap_or("")
                            })
                        })
                        .collect()
                })
                .unwrap_or_else(Vec::new);

            // Parse query string from URI if present
            let (path, query_string) = if let Some(idx) = uri.find('?') {
                (uri[..idx].to_string(), Some(uri[idx + 1..].to_string()))
            } else {
                (uri.to_string(), None)
            };

            // Build prompt using template engine
            let mut data_builder = TemplateDataBuilder::new()
                .field("protocol_name", "HTTP")
                .field("output_format", "HTML")
                .field("method", method)
                .field("uri", path)
                .field("headers", headers_array);

            if let Some(qs) = query_string {
                data_builder = data_builder.field("query_string", qs);
            }

            if !body.is_empty() {
                data_builder = data_builder.field("body", body);
            }

            if let Some(instruction) = user_instruction {
                data_builder = data_builder.field("user_instruction", instruction);
            }

            let template_data = data_builder.build();

            // Render prompt template
            let prompt = TEMPLATE_ENGINE
                .render_json("easy_request/http", &template_data)
                .context("Failed to render HTTP-easy prompt template")?;

            // Get model name from app_state
            let model = app_state
                .get_ollama_model()
                .await
                .unwrap_or_else(|| "qwen2.5-coder:7b".to_string());

            // Call LLM expecting Markdown response
            let response = llm_client
                .generate(&model, &prompt)
                .await
                .context("Failed to call LLM for HTTP-easy response")?;

            // Convert Markdown to HTML
            let html = markdown_to_html(&response.text);

            // Generate send_http_response action
            let action = json!({
                "type": "send_http_response",
                "status": 200,
                "headers": {
                    "Content-Type": "text/html; charset=utf-8",
                    "X-Powered-By": "NetGet HTTP-Easy",
                },
                "body": html,
            });

            Ok(vec![action])
        })
    }

    fn get_handled_event_type_ids(&self) -> Vec<&'static str> {
        vec!["http_request"]
    }

    fn description(&self) -> &'static str {
        "Simplified HTTP server where LLM responds in Markdown (converted to HTML)"
    }
}

/// Convert Markdown to HTML
///
/// For now, this is a simple implementation. In the future, we could use
/// a proper Markdown library like `pulldown-cmark` for full Markdown support.
pub fn markdown_to_html(markdown: &str) -> String {
    let mut html = String::from("<!DOCTYPE html>\n<html>\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n");
    html.push_str("<style>\n");
    html.push_str("body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, Cantarell, sans-serif; ");
    html.push_str(
        "max-width: 800px; margin: 2em auto; padding: 0 1em; line-height: 1.6; color: #333; }\n",
    );
    html.push_str("h1, h2, h3 { color: #111; margin-top: 1.5em; }\n");
    html.push_str("h1 { border-bottom: 2px solid #eee; padding-bottom: 0.3em; }\n");
    html.push_str("code { background: #f4f4f4; padding: 0.2em 0.4em; border-radius: 3px; font-size: 0.9em; }\n");
    html.push_str(
        "pre { background: #f4f4f4; padding: 1em; border-radius: 5px; overflow-x: auto; }\n",
    );
    html.push_str("pre code { background: none; padding: 0; }\n");
    html.push_str("blockquote { border-left: 4px solid #ddd; padding-left: 1em; margin-left: 0; color: #666; }\n");
    html.push_str("ul, ol { padding-left: 2em; }\n");
    html.push_str("a { color: #0366d6; text-decoration: none; }\n");
    html.push_str("a:hover { text-decoration: underline; }\n");
    html.push_str("</style>\n");
    html.push_str("</head>\n<body>\n");

    // Simple Markdown-to-HTML conversion (basic implementation)
    let lines = markdown.lines();
    let mut in_code_block = false;
    let mut in_list = false;

    for line in lines {
        let trimmed = line.trim();

        // Code blocks (```)
        if trimmed.starts_with("```") {
            if in_code_block {
                html.push_str("</code></pre>\n");
                in_code_block = false;
            } else {
                if in_list {
                    html.push_str("</ul>\n");
                    in_list = false;
                }
                html.push_str("<pre><code>");
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            html.push_str(&html_escape(line));
            html.push('\n');
            continue;
        }

        // Headings
        if let Some(heading) = parse_heading(trimmed) {
            if in_list {
                html.push_str("</ul>\n");
                in_list = false;
            }
            html.push_str(&heading);
            html.push('\n');
            continue;
        }

        // Lists
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            if !in_list {
                html.push_str("<ul>\n");
                in_list = true;
            }
            let content = &trimmed[2..];
            html.push_str(&format!("<li>{}</li>\n", process_inline_markdown(content)));
            continue;
        }

        // End list if needed
        if in_list && !trimmed.is_empty() {
            html.push_str("</ul>\n");
            in_list = false;
        }

        // Paragraphs
        if !trimmed.is_empty() {
            html.push_str(&format!("<p>{}</p>\n", process_inline_markdown(trimmed)));
        } else if in_list {
            html.push_str("</ul>\n");
            in_list = false;
        }
    }

    // Close any open tags
    if in_code_block {
        html.push_str("</code></pre>\n");
    }
    if in_list {
        html.push_str("</ul>\n");
    }

    html.push_str("</body>\n</html>");
    html
}

/// Parse heading markdown (# Heading)
fn parse_heading(line: &str) -> Option<String> {
    if line.starts_with("# ") {
        Some(format!("<h1>{}</h1>", html_escape(&line[2..])))
    } else if line.starts_with("## ") {
        Some(format!("<h2>{}</h2>", html_escape(&line[3..])))
    } else if line.starts_with("### ") {
        Some(format!("<h3>{}</h3>", html_escape(&line[4..])))
    } else if line.starts_with("#### ") {
        Some(format!("<h4>{}</h4>", html_escape(&line[5..])))
    } else {
        None
    }
}

/// Render the supported inline subset with paired tags. Unmatched delimiters stay
/// literal, and code spans are escaped without interpreting their contents as emphasis.
fn process_inline_markdown(text: &str) -> String {
    render_inline(text, 0)
}

fn render_inline(mut text: &str, depth: usize) -> String {
    // Nested emphasis is input-controlled; keep both recursion and work bounded.
    if depth == 32 {
        return html_escape(text);
    }
    let mut result = String::new();
    while !text.is_empty() {
        let Some(start) = text.find(['`', '*', '_']) else {
            result.push_str(&html_escape(text));
            break;
        };
        result.push_str(&html_escape(&text[..start]));
        let candidate = &text[start..];
        let (delimiter, tag) = if candidate.starts_with('`') {
            ("`", "code")
        } else if candidate.starts_with("**") {
            ("**", "strong")
        } else if candidate.starts_with("__") {
            ("__", "strong")
        } else if candidate.starts_with('*') {
            ("*", "em")
        } else {
            ("_", "em")
        };
        let rest = &candidate[delimiter.len()..];
        // Do not treat underscores within identifiers as Markdown emphasis.
        let in_word = delimiter.starts_with('_')
            && text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
        if !in_word {
            if let Some(end) = rest.find(delimiter).filter(|&end| end > 0) {
                result.push('<');
                result.push_str(tag);
                result.push('>');
                if tag == "code" {
                    result.push_str(&html_escape(&rest[..end]));
                } else {
                    result.push_str(&render_inline(&rest[..end], depth + 1));
                }
                result.push_str("</");
                result.push_str(tag);
                result.push('>');
                text = &rest[end + delimiter.len()..];
                continue;
            }
        }
        result.push_str(delimiter);
        text = rest;
    }
    result
}

/// Escape HTML special characters
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Generate HTTP-easy prompt for testing/snapshot purposes
///
/// This public function generates the same prompt that would be sent to the LLM,
/// useful for snapshot testing and debugging.
///
/// # Arguments
/// * `method` - HTTP method (GET, POST, etc.)
/// * `uri` - Request URI
/// * `headers` - Request headers as key-value pairs
/// * `body` - Optional request body
/// * `user_instruction` - Optional custom instruction
///
/// # Returns
/// The generated prompt string
pub fn generate_http_easy_prompt(
    method: &str,
    uri: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    user_instruction: Option<&str>,
) -> Result<String> {
    // Convert headers to array for template
    let headers_array: Vec<JsonValue> = headers
        .iter()
        .map(|(k, v)| {
            json!({
                "name": k,
                "value": v
            })
        })
        .collect();

    // Parse query string from URI if present
    let (path, query_string) = if let Some(idx) = uri.find('?') {
        (uri[..idx].to_string(), Some(uri[idx + 1..].to_string()))
    } else {
        (uri.to_string(), None)
    };

    // Build template data
    let mut data_builder = TemplateDataBuilder::new()
        .field("protocol_name", "HTTP")
        .field("output_format", "HTML")
        .field("method", method)
        .field("uri", path)
        .field("headers", headers_array);

    if let Some(qs) = query_string {
        data_builder = data_builder.field("query_string", qs);
    }

    if let Some(body_str) = body {
        data_builder = data_builder.field("body", body_str);
    }

    if let Some(instruction) = user_instruction {
        data_builder = data_builder.field("user_instruction", instruction);
    }

    let template_data = data_builder.build();

    // Render prompt template
    TEMPLATE_ENGINE
        .render_json("easy_request/http", &template_data)
        .context("Failed to render HTTP-easy prompt template")
}
