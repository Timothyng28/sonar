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
use std::path::PathBuf;
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
            None => match default_code_repo() {
                Some(r) => r,
                None => {
                    return Err(McpError::invalid_params(
                        "no code repos indexed. run `sonar code index --repo <path>` first."
                            .to_string(),
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

/// Pick the only indexed code repo if exactly one is set up. Used to make
/// `sonar_code` work without specifying `repo` for single-repo users.
fn default_code_repo() -> Option<String> {
    let home = dirs::home_dir()?;
    let dir = home.join(".sonar").join("code");
    let mut entries = std::fs::read_dir(&dir).ok()?.filter_map(|e| e.ok()).filter(|e| {
        e.file_type().map(|t| t.is_dir()).unwrap_or(false)
    });
    let first = entries.next()?;
    if entries.next().is_some() {
        // More than one — caller must specify which.
        return None;
    }
    Some(first.file_name().to_string_lossy().to_string())
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
