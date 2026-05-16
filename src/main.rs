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
    /// Code search: index a repo (typically `development`) and search it.
    /// Mirrors the transcript flow but for source files. See subcommand help.
    Code {
        #[command(subcommand)]
        cmd: CodeCmd,
    },
}

#[derive(Subcommand, Debug)]
enum CodeCmd {
    /// Index a repo's working tree. Use after `git fetch origin development`.
    Index {
        /// Repo root to index. Defaults to the current directory.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Repo label stored in the index. Defaults to the repo dir's name.
        #[arg(long)]
        label: Option<String>,
        /// Branch name stored in the index. Defaults to the repo's current
        /// branch (auto-detected via git).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Search the code index.
    Search {
        query: String,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Repeat the query N times for timing.
        #[arg(long, default_value_t = 1)]
        bench: usize,
    },
    /// Print code-index status.
    Stats {
        /// Repo label whose index to inspect.
        #[arg(long)]
        repo: String,
    },
    /// Install a `post-merge` git hook that re-indexes when development
    /// changes via `git pull` / `git merge`.
    Install {
        /// Repo root to install the hook into. Defaults to current dir.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Only fire the hook on this branch. Defaults to `development`.
        #[arg(long, default_value = "development")]
        branch: String,
    },
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
        Cmd::Code { cmd } => run_code(cmd),
    }
}

fn run_code(cmd: CodeCmd) -> Result<()> {
    use sonar::code::index::{
        index_repo, open_or_create_code_index, CodeSearchArgs, CodeSearcher, CodeWriter,
        CODE_WRITER_HEAP_BYTES,
    };

    match cmd {
        CodeCmd::Index { repo, label, branch } => {
            init_tracing();
            let repo = repo.canonicalize()?;
            let label = label.unwrap_or_else(|| {
                repo.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("repo")
                    .to_string()
            });
            let branch = branch.unwrap_or_else(|| detect_branch(&repo).unwrap_or_else(|| "unknown".into()));
            let commit_sha = detect_commit_sha(&repo).unwrap_or_else(|| "unknown".into());

            let (index, fields, idx_path) = open_or_create_code_index(&label)?;
            let mut writer = CodeWriter::new(&index, fields, CODE_WRITER_HEAP_BYTES)?;
            let t = std::time::Instant::now();
            let (files, bytes) = index_repo(&mut writer, &label, &branch, &commit_sha, &repo)?;
            tracing::info!(
                "indexed {} files ({:.1} MB) from repo={} branch={} sha={} in {:?} → {}",
                files,
                bytes as f64 / 1_048_576.0,
                label,
                branch,
                &commit_sha[..commit_sha.len().min(8)],
                t.elapsed(),
                idx_path.display(),
            );
            Ok(())
        }
        CodeCmd::Search {
            query,
            repo,
            language,
            limit,
            bench,
        } => {
            init_tracing();
            // For ad-hoc CLI search we need a repo label — use the first
            // index dir if --repo isn't given.
            let label = match &repo {
                Some(r) => r.clone(),
                None => pick_first_code_repo()?,
            };
            let (index, fields, _) = open_or_create_code_index(&label)?;
            let searcher = CodeSearcher::new(&index, fields)?;
            let args = CodeSearchArgs {
                query: query.clone(),
                repo: Some(label.clone()),
                language,
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
                eprintln!(
                    "bench: {} runs of code query={:?} | min={}µs median={}µs p95={}µs max={}µs | {} hits",
                    bench,
                    query,
                    times_us[0],
                    times_us[n / 2],
                    times_us[(n as f32 * 0.95) as usize],
                    times_us[n - 1],
                    last_hits.len()
                );
                return Ok(());
            }
            let hits = searcher.search(args)?;
            for (i, h) in hits.iter().enumerate() {
                println!(
                    "{:>2}. [{}/{}] {} ({})",
                    i + 1,
                    h.repo,
                    h.branch,
                    h.file_path,
                    h.language
                );
                println!("    {}", h.snippet);
            }
            Ok(())
        }
        CodeCmd::Stats { repo } => {
            init_tracing();
            let (index, fields, idx_path) = open_or_create_code_index(&repo)?;
            let searcher = CodeSearcher::new(&index, fields)?;
            println!("index path: {}", idx_path.display());
            println!("has files: {}", searcher.has_any_docs()?);
            Ok(())
        }
        CodeCmd::Install { repo, branch } => {
            init_tracing();
            sonar::code::install::install_post_merge_hook(&repo, &branch)
        }
    }
}

fn detect_branch(repo: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn detect_commit_sha(repo: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// If no --repo is given to `sonar code search`, fall back to whatever
/// repo is sitting in ~/.sonar/code/. This is just a convenience for
/// single-repo users.
fn pick_first_code_repo() -> Result<String> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".sonar").join("code");
    if !dir.exists() {
        anyhow::bail!(
            "no code indexes found at {}. run `sonar code index --repo <path>` first.",
            dir.display()
        );
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            return Ok(entry.file_name().to_string_lossy().to_string());
        }
    }
    anyhow::bail!("no code indexes found at {}", dir.display())
}
