use std::path::Path;

use sonar::parse::{parse_line, project_from_path};

#[test]
fn parses_user_message() {
    let line = r#"{"type":"user","sessionId":"sess-1","timestamp":"2026-05-10T14:23:00Z","message":{"role":"user","content":"add the migration"},"cwd":"/Users/u/Desktop/demo","gitBranch":"main"}"#;
    let path = Path::new("/x/-Users-u-Desktop-demo/abc.jsonl");
    let ev = parse_line(line, path, 0).unwrap().unwrap();
    assert_eq!(ev.session_id, "sess-1");
    assert_eq!(ev.event_role, "user");
    assert!(ev.text.contains("migration"));
    assert_eq!(ev.git_branch.as_deref(), Some("main"));
    assert!(ev.timestamp.is_some());
    assert_eq!(ev.project, "Users/u/Desktop/demo");
}

#[test]
fn parses_assistant_message_with_content_blocks() {
    let line = r#"{"type":"assistant","sessionId":"sess-2","timestamp":"2026-05-10T14:24:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Looking at the file..."},{"type":"thinking","thinking":"hmm"}]}}"#;
    let path = Path::new("/x/-Users-u-Desktop-demo/abc.jsonl");
    let ev = parse_line(line, path, 1).unwrap().unwrap();
    assert_eq!(ev.event_role, "assistant");
    assert!(ev.text.contains("Looking"));
    assert!(ev.text.contains("hmm"));
}

#[test]
fn parses_attachment_with_hook_content() {
    let line = r#"{"type":"attachment","sessionId":"sess-3","timestamp":"2026-05-10T14:25:00Z","attachment":{"type":"hook_success","content":"OK","stdout":"OK\n","stderr":"","command":"echo hi"}}"#;
    let path = Path::new("/x/-Users-u-Desktop-demo/abc.jsonl");
    let ev = parse_line(line, path, 2).unwrap().unwrap();
    assert_eq!(ev.event_role, "attachment");
    assert!(ev.text.contains("OK"));
}

#[test]
fn empty_content_returns_none() {
    let line = r#"{"type":"last-prompt","sessionId":"sess-4","leafUuid":"abc"}"#;
    let path = Path::new("/x/-Users-u-Desktop-demo/abc.jsonl");
    let ev = parse_line(line, path, 3).unwrap();
    assert!(ev.is_none(), "no searchable text should return None");
}

#[test]
fn malformed_json_errors() {
    let line = "not json at all";
    let path = Path::new("/x/-Users-u-Desktop-demo/abc.jsonl");
    let err = parse_line(line, path, 4);
    assert!(err.is_err());
}

#[test]
fn project_label_decodes_path() {
    let path = Path::new(
        "/Users/u/.claude/projects/-Users-u-Desktop-myproject/abc.jsonl",
    );
    assert_eq!(project_from_path(path), "Users/u/Desktop/myproject");
}
