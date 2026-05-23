use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler};
use rmcp::{transport::stdio, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::code::index::{open_or_create_code_index, CodeSearchArgs, CodeSearcher};
use crate::index::{open_or_create_index, parse_since, EventSearcher, SearchArgs};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SonarArgs {
    /// Free-text BM25 search query. Supports phrase quoting and AND/OR.
    pub query: String,
    /// Optional time window. ISO-8601 date or relative ("3d", "2w", "5h").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Optional project filter (matches the project label as stored).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Maximum number of results to return. Defaults to 10, capped at 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SonarCodeArgs {
    /// Free-text BM25 query against indexed source code. Phrases and AND/OR
    /// supported. camelCase / PascalCase identifiers are split at index
    /// time, so a query for "order" matches a file containing
    /// `processOrderItem`.
    pub query: String,
    /// Optional repo label (the dir name of the repo as stored at index
    /// time). Required when more than one repo is indexed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Optional language filter (rust, python, typescript, javascript, go,
    /// java, swift, cpp, sql, yaml, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Max results. Default 10, capped 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Clone)]
pub struct SonarServer {
    transcripts: Arc<EventSearcher>,
    /// One CodeSearcher per indexed repo, opened lazily on first call so
    /// the MCP server starts fast even with many tracked repos.
    code: Arc<Mutex<HashMap<String, Arc<CodeSearcher>>>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<SonarServer>,
}

#[tool_router]
impl SonarServer {
    pub fn new(index_path: PathBuf) -> Result<Self> {
        let (index, fields) = open_or_create_index(&index_path)?;
        let transcripts = EventSearcher::new(&index, fields)?;
        Ok(Self {
            transcripts: Arc::new(transcripts),
            code: Arc::new(Mutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        description = "Search across every Claude Code session transcript on this machine using BM25 full-text search over a memory-mapped tantivy index. Returns matching sessions with snippets, file paths, timestamps, and relevance scores. Use this to answer questions like 'which session did I work on X?' or 'find the conversation where I figured out Y.'"
    )]
    async fn sonar(
        &self,
        Parameters(args): Parameters<SonarArgs>,
    ) -> Result<CallToolResult, McpError> {
        let since = match args.since.as_deref() {
            Some(s) => match parse_since(s) {
                Ok(d) => Some(d),
                Err(e) => {
                    return Err(McpError::invalid_params(
                        format!("invalid 'since': {}", e),
                        None,
                    ));
                }
            },
            None => None,
        };
        let search = SearchArgs {
            query: args.query,
            since,
            project: args.project,
            limit: args.limit,
        };
        let hits = self
            .transcripts
            .search(search)
            .map_err(|e| McpError::internal_error(format!("search failed: {}", e), None))?;
        let body = serde_json::to_string_pretty(&hits)
            .map_err(|e| McpError::internal_error(format!("encoding result: {}", e), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Search across indexed source code on this machine — typically the canonical state of a tracked repo's `development` branch. Returns matching files with snippets, file path, language, and relevance score. Use this to answer 'where is this implemented?', 'find usages of X', or 'how was Y done in this codebase?' Note: this indexes only canonicalized code (re-indexed on `git pull` of the tracked branch); the *current uncommitted state of any worktree* is not searchable here — use the native Read/Grep tools for that."
    )]
    async fn sonar_code(
        &self,
        Parameters(args): Parameters<SonarCodeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let label = match args.repo.clone() {
            Some(r) => r,
            None => match resolve_default_code_repo() {
                RepoResolution::One(r) => r,
                RepoResolution::None => {
                    return Err(McpError::invalid_params(
                        "no code repos indexed. run `sonar code index --repo <path>` first."
                            .to_string(),
                        None,
                    ));
                }
                RepoResolution::Many(labels) => {
                    return Err(McpError::invalid_params(
                        format!(
                            "multiple repos indexed ({}). specify which one with `repo: \"<label>\"`.",
                            labels.join(", ")
                        ),
                        None,
                    ));
                }
            },
        };

        let searcher = self.code_searcher_for(&label).await.map_err(|e| {
            McpError::internal_error(format!("opening code index {}: {}", label, e), None)
        })?;

        let search = CodeSearchArgs {
            query: args.query,
            repo: Some(label),
            language: args.language,
            limit: args.limit,
        };
        let hits = searcher
            .search(search)
            .map_err(|e| McpError::internal_error(format!("code search failed: {}", e), None))?;
        let body = serde_json::to_string_pretty(&hits)
            .map_err(|e| McpError::internal_error(format!("encoding result: {}", e), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    async fn code_searcher_for(&self, label: &str) -> Result<Arc<CodeSearcher>> {
        {
            let map = self.code.lock().await;
            if let Some(s) = map.get(label) {
                return Ok(s.clone());
            }
        }
        let (index, fields, _) = open_or_create_code_index(label)?;
        let searcher = Arc::new(CodeSearcher::new(&index, fields)?);
        let mut map = self.code.lock().await;
        map.insert(label.to_string(), searcher.clone());
        Ok(searcher)
    }
}

#[tool_handler]
impl ServerHandler for SonarServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "sonar: two tools for searching your past, both backed by memory-mapped \
                 tantivy indexes. \
                 \n  • `sonar(query, since?, project?, limit?)` — past Claude Code conversation \
                 transcripts. Use it to answer 'which session did I work on X?'. \
                 \n  • `sonar_code(query, repo?, language?, limit?)` — canonical source code \
                 from indexed repos (typically `development`). Use it to answer 'where is \
                 this implemented?' or 'find usages of X in the codebase'. Note: uncommitted \
                 code in the current worktree is NOT in this index; use native Read/Grep for \
                 that. Sonar's purpose is searching your past — sessions you've closed and \
                 code that has merged."
                    .to_string(),
            )
    }
}

/// Outcome of resolving the default code repo when `sonar_code` is called
/// without an explicit `repo`. The `Many` variant carries the labels so the
/// caller can tell the user which one to pick, instead of conflating an
/// ambiguous call with a genuinely empty index.
enum RepoResolution {
    None,
    One(String),
    Many(Vec<String>),
}

/// List code-repo labels (sub-dir names) under `dir`, sorted for stable output.
/// `dir` is the `~/.sonar/code/` index root.
fn code_repos_in(dir: &Path) -> Vec<String> {
    let mut labels: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    labels.sort();
    labels
}

/// Classify the indexed repos in `dir` into the resolution the caller acts on.
fn classify_code_repos(dir: &Path) -> RepoResolution {
    let mut labels = code_repos_in(dir);
    match labels.len() {
        0 => RepoResolution::None,
        1 => RepoResolution::One(labels.pop().expect("len checked")),
        _ => RepoResolution::Many(labels),
    }
}

/// Pick the only indexed code repo if exactly one is set up, so `sonar_code`
/// works without specifying `repo` for single-repo users. Distinguishes a
/// genuinely empty index from an ambiguous multi-repo state.
fn resolve_default_code_repo() -> RepoResolution {
    let Some(home) = dirs::home_dir() else {
        return RepoResolution::None;
    };
    classify_code_repos(&home.join(".sonar").join("code"))
}

/// Entrypoint for `sonar mcp`: start the stdio MCP server. Blocks until the
/// client disconnects.
pub async fn serve_stdio(index_path: PathBuf) -> Result<()> {
    let server = SonarServer::new(index_path)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Print a snippet the user can paste into their `.mcp.json`.
pub fn print_mcp_config_snippet() -> Result<()> {
    let bin = std::env::current_exe()?;
    println!(
        "{}",
        serde_json::json!({
            "mcpServers": {
                "sonar": {
                    "command": bin.to_string_lossy(),
                    "args": ["mcp"]
                }
            }
        })
    );
    Ok(())
}

/// Print a SessionEnd-hook snippet for ~/.claude/settings.json. The hook
/// fires once per session at close, reads the JSON payload from stdin to
/// pull out `transcript_path`, and runs `sonar index --file <path>` to
/// refresh that one transcript in the index. One indexing event per
/// session instead of per turn — much lower overhead, still keeps the
/// index fresh for cross-session search.
pub fn print_hook_config_snippet() -> Result<()> {
    let bin = std::env::current_exe()?;
    let bin_str = bin.to_string_lossy().to_string();
    let cmd = format!(
        r#"P=$(jq -r .transcript_path 2>/dev/null) && [ -n "$P" ] && "{}" index --file "$P" >/dev/null 2>&1 || true"#,
        bin_str
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "SessionEnd": [
                    {
                        "matcher": "",
                        "hooks": [
                            {
                                "type": "command",
                                "command": cmd
                            }
                        ]
                    }
                ]
            }
        }))?
    );
    Ok(())
}

// suppress dead-code warnings on McpError import path for older rmcp builds
#[allow(dead_code)]
fn _ctx_type_anchor(_c: &RequestContext<RoleServer>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mkrepo(root: &Path, label: &str) {
        std::fs::create_dir_all(root.join(label)).unwrap();
    }

    #[test]
    fn empty_index_resolves_to_none() {
        let tmp = TempDir::new().unwrap();
        assert!(matches!(classify_code_repos(tmp.path()), RepoResolution::None));
    }

    #[test]
    fn missing_dir_resolves_to_none() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(matches!(classify_code_repos(&missing), RepoResolution::None));
    }

    #[test]
    fn single_repo_resolves_to_that_repo() {
        let tmp = TempDir::new().unwrap();
        mkrepo(tmp.path(), "claritycare");
        match classify_code_repos(tmp.path()) {
            RepoResolution::One(r) => assert_eq!(r, "claritycare"),
            other => panic!("expected One, got {:?}", labels_of(&other)),
        }
    }

    #[test]
    fn multiple_repos_resolve_to_sorted_labels() {
        let tmp = TempDir::new().unwrap();
        mkrepo(tmp.path(), "sonar");
        mkrepo(tmp.path(), "claritycare");
        match classify_code_repos(tmp.path()) {
            RepoResolution::Many(labels) => assert_eq!(labels, vec!["claritycare", "sonar"]),
            other => panic!("expected Many, got {:?}", labels_of(&other)),
        }
    }

    fn labels_of(r: &RepoResolution) -> Vec<String> {
        match r {
            RepoResolution::None => vec![],
            RepoResolution::One(s) => vec![s.clone()],
            RepoResolution::Many(v) => v.clone(),
        }
    }
}
