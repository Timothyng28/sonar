use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// A single indexable record extracted from a Claude Code session JSONL line.
///
/// Claude Code writes one JSON object per line. There are multiple shapes
/// (user, assistant, attachment, last-prompt, permission-mode, ...). We pull
/// out a uniform shape that's good for full-text search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexableEvent {
    pub session_id: String,
    pub project: String,
    pub event_role: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub file_path: String,
    pub event_index: u64,
    pub text: String,
}

/// Parse a single JSONL line into an IndexableEvent. Returns Ok(None) when
/// the line contains no searchable text (e.g. permission-mode, last-prompt).
pub fn parse_line(
    line: &str,
    file_path: &Path,
    event_index: u64,
) -> Result<Option<IndexableEvent>> {
    let v: Value = serde_json::from_str(line)?;

    let session_id = v
        .get("sessionId")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let event_role = v
        .get("type")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let timestamp = v
        .get("timestamp")
        .and_then(|x| x.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let cwd = v
        .get("cwd")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let git_branch = v
        .get("gitBranch")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    let text = extract_text(&v, &event_role);
    if text.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(IndexableEvent {
        session_id,
        project: project_from_path(file_path),
        event_role,
        timestamp,
        cwd,
        git_branch,
        file_path: file_path.to_string_lossy().to_string(),
        event_index,
        text,
    }))
}

/// Pull out a searchable text blob from a Claude Code JSONL record.
fn extract_text(v: &Value, event_role: &str) -> String {
    match event_role {
        "user" => extract_message_content(v.get("message")),
        "assistant" => extract_message_content(v.get("message")),
        "attachment" => {
            // attachment objects carry hook output, tool results, file content
            let att = v.get("attachment");
            let mut parts = Vec::new();
            if let Some(att) = att {
                if let Some(s) = att.get("content").and_then(|x| x.as_str()) {
                    parts.push(s.to_string());
                }
                if let Some(s) = att.get("stdout").and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                }
                if let Some(s) = att.get("stderr").and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        parts.push(s.to_string());
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Extract text content from a Claude Code message value. The `content`
/// field is either a plain string (user prompts) or an array of content
/// blocks (assistant turns, tool calls).
fn extract_message_content(msg: Option<&Value>) -> String {
    let Some(msg) = msg else {
        return String::new();
    };
    let Some(content) = msg.get("content") else {
        return String::new();
    };

    if let Some(s) = content.as_str() {
        return s.to_string();
    }

    if let Some(arr) = content.as_array() {
        let mut parts = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                parts.push(s.to_string());
                continue;
            }
            if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                parts.push(t.to_string());
            }
            if let Some(t) = item.get("thinking").and_then(|x| x.as_str()) {
                parts.push(t.to_string());
            }
            // tool_use content
            if let Some(name) = item.get("name").and_then(|x| x.as_str()) {
                parts.push(format!("[tool: {}]", name));
                if let Some(input) = item.get("input") {
                    parts.push(input.to_string());
                }
            }
        }
        return parts.join("\n");
    }

    String::new()
}

/// Convert a transcript file path to a human-friendly project label.
///
/// Claude Code names its project dirs by replacing slashes with dashes,
/// e.g. `-Users-you-Desktop-myproject`. Reverse that to read more
/// naturally.
pub fn project_from_path(path: &Path) -> String {
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    // turn "-Users-you-Desktop-myproject" → "Users/you/Desktop/myproject"
    let trimmed = parent.trim_start_matches('-');
    trimmed.replace('-', "/")
}
