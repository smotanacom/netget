//! XML Reference Parser for Large Content Blocks
//!
//! Allows LLMs to write large text blocks (scripts, configs, etc.) outside JSON
//! using simple XML-style tags, avoiding JSON string escaping issues.
//!
//! # Format
//!
//! ```text
//! {"actions": [{"code": "<script001>"}]}
//!
//! <script001>
//! import json
//! # No escaping needed!
//! </script001>
//! ```
//!
//! Supports both standard XML (`</tag>`) and simplified (`<tag>`) closing tags.
//! Tags can appear before, after, or mixed with JSON.

use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use tracing::{debug, trace};

static REFERENCE_TAG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"<([a-zA-Z]+[0-9]+[a-zA-Z0-9_]*)>").expect("valid reference tag pattern")
});

/// Extract XML-style references from LLM response
///
/// Extracts content from tags like `<script001>content</script001>` or `<script001>content<script001>`
/// and returns the cleaned response (with tags removed) plus a map of tag_name -> content.
///
/// # Arguments
/// * `text` - The full LLM response text
///
/// # Returns
/// * Tuple of (cleaned_text, references_map)
///   - cleaned_text: Response with all XML blocks removed (leaving just JSON)
///   - references_map: HashMap mapping tag names to their content
pub fn extract_references(text: &str) -> Result<(String, HashMap<String, String>)> {
    let mut refs = HashMap::new();
    let mut cleaned = text.to_string();

    // Find both opening <tag> and closing </tag> or <tag> patterns
    // IMPORTANT: Only match tags that contain at least one digit to avoid matching
    // HTML tags like <body>, <html>, <div> in response content. This feature is
    // designed for tags like <script001>, <config1>, etc.
    let opening_regex = &*REFERENCE_TAG;

    // Collect blocks to extract and remove
    let mut blocks_to_remove: Vec<(usize, usize)> = Vec::new();
    let mut scan_position = 0;
    let mut json_depth = 0usize;
    let mut in_json_string = false;
    let mut escaped = false;

    // Find all opening tags
    for opening_cap in opening_regex.captures_iter(text) {
        let tag_name = &opening_cap[1];
        let opening_match = opening_cap.get(0).unwrap();
        let opening_start = opening_match.start();
        let opening_end = opening_match.end();

        // A reference body is opaque text, including any tag-like script/HTML content.
        // Extracting nested ranges as independent references made the reverse removals
        // overlap: removing the inner range invalidated the outer byte offsets and
        // could panic on model-supplied text.
        if blocks_to_remove
            .last()
            .is_some_and(|&(_, end)| opening_start < end)
        {
            continue;
        }

        // Skip if already extracted (duplicate tag)
        if refs.contains_key(tag_name) {
            continue;
        }

        // Track JSON strings across the original text, skipping opaque blocks once
        // extracted. Checking only the preceding quote corrupted embedded placeholders
        // such as `"prefix <script001> suffix"` and mishandled escaped quotes.
        for &byte in &text.as_bytes()[scan_position..opening_start] {
            if in_json_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_json_string = false;
                }
            } else {
                match byte {
                    b'{' | b'[' => json_depth += 1,
                    b'}' | b']' => json_depth = json_depth.saturating_sub(1),
                    b'"' if json_depth > 0 => in_json_string = true,
                    _ => {}
                }
            }
        }
        scan_position = opening_start;
        if in_json_string {
            continue;
        }

        // Look for matching closing tag (standard format: </tag>)
        let closing_pattern = format!("</{}>", tag_name);
        if let Some(closing_pos) = text[opening_end..].find(&closing_pattern) {
            let abs_closing_pos = opening_end + closing_pos;
            let closing_end = abs_closing_pos + closing_pattern.len();

            // Extract content between tags
            let content = text[opening_end..abs_closing_pos].trim().to_string();

            debug!(
                "Extracted reference (standard): <{}> ({} chars)",
                tag_name,
                content.len()
            );
            refs.insert(tag_name.to_string(), content);
            blocks_to_remove.push((opening_start, closing_end));
            scan_position = closing_end;
            continue;
        }

        // If no standard closing found, try simplified format: <tag>
        let simplified_pattern = format!("<{}>", tag_name);
        if let Some(closing_pos) = text[opening_end..].find(&simplified_pattern) {
            let abs_closing_pos = opening_end + closing_pos;
            let closing_end = abs_closing_pos + simplified_pattern.len();

            // Extract content between tags
            let content = text[opening_end..abs_closing_pos].trim().to_string();

            debug!(
                "Extracted reference (simplified): <{}> ({} chars)",
                tag_name,
                content.len()
            );
            refs.insert(tag_name.to_string(), content);
            blocks_to_remove.push((opening_start, closing_end));
            scan_position = closing_end;
        }
    }

    // Remove blocks in reverse order (to preserve indices)
    blocks_to_remove.sort_by(|a, b| b.0.cmp(&a.0));
    for (start, end) in blocks_to_remove {
        cleaned.replace_range(start..end, "");
    }

    // Trim extra whitespace
    cleaned = cleaned.trim().to_string();

    trace!(
        "Reference extraction complete: {} refs extracted, cleaned text {} chars",
        refs.len(),
        cleaned.len()
    );

    Ok((cleaned, refs))
}

/// Resolve XML reference placeholders in JSON string
///
/// Replaces placeholders like `"<script001>"` with the actual content from references map.
///
/// # Arguments
/// * `json_str` - JSON string potentially containing reference placeholders
/// * `refs` - Map of tag names to their content
///
/// # Returns
/// * JSON string with all references resolved
pub fn resolve_references(json_str: &str, refs: &HashMap<String, String>) -> String {
    // Resolve only placeholders in the original input. Iterating the HashMap and
    // repeatedly replacing in the result re-interpreted tags inside inserted scripts,
    // making their contents depend on the map's randomized iteration order.
    REFERENCE_TAG
        .replace_all(json_str, |capture: &regex::Captures<'_>| {
            refs.get(&capture[1])
                .map(|content| escape_json_string(content))
                .unwrap_or_else(|| capture[0].to_string())
        })
        .into_owned()
}

/// Escape string content for JSON
///
/// Handles common escape sequences: quotes, newlines, backslashes, etc.
fn escape_json_string(s: &str) -> String {
    // String serialization also escapes NUL and the other ASCII control characters;
    // replacing only quotes/newlines produced invalid JSON for those payloads.
    let quoted = serde_json::to_string(s).expect("serializing a string cannot fail");
    quoted[1..quoted.len() - 1].to_string()
}

/// Check if a string contains XML reference placeholders
/// Only matches tags containing at least one digit (like <script001>, <config1>)
pub fn contains_references(text: &str) -> bool {
    REFERENCE_TAG.is_match(text)
}
