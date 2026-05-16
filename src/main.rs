use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

use sonar::daemon::{default_transcripts_root, run_daemon, WRITER_HEAP_BYTES};
use sonar::index::{
    default_index_path, open_or_create_index, parse_since, EventSearcher, EventWriter, SearchArgs,
};

#[derive(Parser, Debug)]
#[command(
    name = "sonar",
    version,
    about = "Memory-mapped, instant search across your Claude Code conversation history."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Override the index directory. Defaults to ~/.sonar/index/.
    #[arg(long, global = true)]
    index_path: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Bootstrap the index from existing transcripts under ~/.claude/projects.
    ///
    /// With --file, re-indexes a single transcript file (the hook path).
    Index {
        /// Override the transcripts root. Defaults to ~/.claude/projects.
        #[arg(long, conflicts_with = "file")]
        root: Option<PathBuf>,
        /// Re-index a single transcript file. Use this from Claude Code hooks.
        /// Idempotent: prior records for the same path are replaced.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Run the long-lived watcher daemon: bootstrap + watch for new lines.
    Daemon {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Run the MCP server over stdio. Invoked by Claude Code via .mcp.json.
    Mcp,
    /// Print a JSON snippet to paste into your `.mcp.json`.
    McpConfig,
    /// Print a Stop-hook snippet for ~/.claude/settings.json that reindexes
    /// the active transcript at the end of every Claude turn.
    HookConfig,
    /// Wire sonar into ~/.claude/settings.json (SessionEnd hook) and the
    /// chosen MCP config. Both files are backed up to *.pre-sonar before
    /// any change. Idempotent.
    Install {
        /// Skip the hook install (only register the MCP server).
        #[arg(long)]
        no_hook: bool,
        /// Skip the MCP install (only install the hook).
        #[arg(long)]
        no_mcp: bool,
        /// Install MCP into a project-scoped .mcp.json under this dir
        /// instead of the global ~/.claude.json.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Show what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore *.pre-sonar backups. Mirror of `install`.
    Uninstall {
        /// Uninstall from the project-scoped .mcp.json under this dir
        /// instead of the global ~/.claude.json.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Search the index from the command line. Handy for sanity checks.
    Search {
        query: String,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Repeat the query N times and print min/median/max query latency.
        /// Excludes process startup and index-open cost.
        #[arg(long, default_value_t = 1)]
        bench: usize,
    },
    /// Print index status.
    Stats,
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let index_path = match cli.index_path {
        Some(p) => p,
        None => default_index_path()?,
    };

    match cli.cmd {
        Cmd::Index { root, file } => {
            init_tracing();
            let (index, fields) = open_or_create_index(&index_path)?;
            let mut writer = EventWriter::new(&index, fields.clone(), WRITER_HEAP_BYTES)?;
            let t = std::time::Instant::now();

            if let Some(file) = file {
                if !file.exists() {
                    anyhow::bail!("transcript file does not exist: {}", file.display());
                }
                let n = sonar::daemon::index_file(&mut writer, &file)?;
                writer.commit()?;
                tracing::info!(
                    "reindexed {} events from {} in {:?}",
                    n,
                    file.display(),
                    t.elapsed()
                );
            } else {
                let root = root.map(Ok).unwrap_or_else(default_transcripts_root)?;
                let (files, events) =
                    sonar::daemon::bootstrap_index(&mut writer, &fields, &root)?;
                tracing::info!(
                    "indexed {} events from {} files in {:?}",
                    events,
                    files,
                    t.elapsed()
                );
            }
            Ok(())
        }
        Cmd::Daemon { root } => {
            init_tracing();
            let root = root.map(Ok).unwrap_or_else(default_transcripts_root)?;
            let (index, fields) = open_or_create_index(&index_path)?;
            let mut writer = EventWriter::new(&index, fields.clone(), WRITER_HEAP_BYTES)?;
            run_daemon(&mut writer, &fields, &root)
        }
        Cmd::Mcp => {
            init_tracing();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(sonar::mcp::serve_stdio(index_path))
        }
        Cmd::McpConfig => sonar::mcp::print_mcp_config_snippet(),
        Cmd::HookConfig => sonar::mcp::print_hook_config_snippet(),
        Cmd::Install {
            no_hook,
            no_mcp,
            project,
            dry_run,
        } => sonar::install::install(sonar::install::InstallOpts {
            no_hook,
            no_mcp,
            project_mcp: project,
            dry_run,
        }),
        Cmd::Uninstall { project } => sonar::install::uninstall(project),
        Cmd::Search {
            query,
            since,
            project,
            limit,
            bench,
        } => {
            init_tracing();
            let (index, fields) = open_or_create_index(&index_path)?;
            let searcher = EventSearcher::new(&index, fields)?;
            let since = since.as_deref().map(parse_since).transpose()?;
            let args = SearchArgs {
                query: query.clone(),
                since,
                project,
                limit: Some(limit),
            };

            if bench > 1 {
                let mut times_us: Vec<u128> = Vec::with_capacity(bench);
                let mut last_hits = Vec::new();
                for _ in 0..bench {
                    let t0 = std::time::Instant::now();
                    last_hits = searcher.search(args.clone())?;
                    times_us.push(t0.elapsed().as_micros());
                }
                times_us.sort();
                let n = times_us.len();
                let median = times_us[n / 2];
                let min = times_us[0];
                let max = times_us[n - 1];
                let p95 = times_us[(n as f32 * 0.95) as usize];
                eprintln!(
                    "bench: {} runs of query={:?} | min={}µs median={}µs p95={}µs max={}µs | {} hits",
                    bench,
                    query,
                    min,
                    median,
                    p95,
                    max,
                    last_hits.len()
                );
                return Ok(());
            }

            let hits = searcher.search(args)?;
            for (i, h) in hits.iter().enumerate() {
                println!(
                    "{:>2}. [{}] {} {} ({})",
                    i + 1,
                    h.timestamp.as_deref().unwrap_or("—"),
                    h.project,
                    h.session_id,
                    h.event_role
                );
                println!("    {}", h.snippet);
                println!("    {}", h.file_path);
            }
            Ok(())
        }
        Cmd::Stats => {
            init_tracing();
            let (index, fields) = open_or_create_index(&index_path)?;
            let searcher = EventSearcher::new(&index, fields)?;
            let any = searcher.has_any_docs()?;
            println!("index path: {}", index_path.display());
            println!("has events: {}", any);
            Ok(())
        }
    }
}
