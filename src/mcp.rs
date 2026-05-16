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
use std::path::PathBuf;
use std::sync::Arc;

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

#[derive(Clone)]
pub struct SonarServer {
    searcher: Arc<EventSearcher>,
    #[allow(dead_code)]
    tool_router: ToolRouter<SonarServer>,
}

#[tool_router]
impl SonarServer {
    pub fn new(index_path: PathBuf) -> Result<Self> {
        let (index, fields) = open_or_create_index(&index_path)?;
        let searcher = EventSearcher::new(&index, fields)?;
        Ok(Self {
            searcher: Arc::new(searcher),
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
        let hits = self.searcher.search(search).map_err(|e| {
            McpError::internal_error(format!("search failed: {}", e), None)
        })?;
        let body = serde_json::to_string_pretty(&hits)
            .map_err(|e| McpError::internal_error(format!("encoding result: {}", e), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

#[tool_handler]
impl ServerHandler for SonarServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "sonar: full-text search across Claude Code session transcripts. \
                 One tool, `sonar(query, since?, project?, limit?)`. Use it whenever \
                 the user asks which past session contained some topic, prompt, or file."
                    .to_string(),
            )
    }
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
