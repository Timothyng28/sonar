use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

use crate::index::{EventWriter, Fields};
use crate::parse::parse_line;

/// Tantivy IndexWriter heap size. 50 MB is what the tantivy docs recommend
/// as a sensible default for moderate workloads.
pub const WRITER_HEAP_BYTES: usize = 50_000_000;

/// Default location of Claude Code's transcript files.
pub fn default_transcripts_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".claude").join("projects"))
}

/// Walk `root` and index every `.jsonl` file found. Returns (files, events).
pub fn bootstrap_index(
    writer: &mut EventWriter,
    fields: &Fields,
    root: &Path,
) -> Result<(usize, usize)> {
    let _ = fields; // kept for symmetry / future field-aware indexing
    let mut files = 0usize;
    let mut events = 0usize;
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        files += 1;
        events += index_file(writer, entry.path())?;
    }
    writer.commit()?;
    Ok((files, events))
}

/// (Re)index a single JSONL transcript. Deletes any prior records keyed on
/// this file_path first so reindexing is idempotent.
pub fn index_file(writer: &mut EventWriter, path: &Path) -> Result<usize> {
    let path_str = path.to_string_lossy().to_string();
    writer.delete_file(&path_str);

    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0usize;
    for (i, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line, path, i as u64) {
            Ok(Some(ev)) => {
                writer.add(&ev)?;
                count += 1;
            }
            Ok(None) => {}
            Err(_) => {
                // skip malformed lines silently — Claude Code occasionally
                // writes partial lines while flushing, and we'll pick them
                // up on the next watcher tick anyway.
            }
        }
    }
    Ok(count)
}

/// Run the daemon: walk once, then watch for changes forever.
pub fn run_daemon(writer: &mut EventWriter, fields: &Fields, root: &Path) -> Result<()> {
    tracing::info!("bootstrapping index from {}", root.display());
    let (files, events) = bootstrap_index(writer, fields, root)?;
    tracing::info!("indexed {} events across {} files", events, files);

    // Set up the file watcher. notify will send raw events through a
    // channel; we then debounce per-file so a series of writes coalesces
    // into one reindex of that file.
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(tx)?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    tracing::info!("watching for changes…");

    let debounce = Duration::from_millis(750);
    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();

    loop {
        // Wait up to `debounce` for the next event.
        match rx.recv_timeout(debounce) {
            Ok(Ok(event)) => {
                if !is_interesting(&event) {
                    continue;
                }
                for p in event.paths {
                    if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                        continue;
                    }
                    pending.insert(p, Instant::now());
                }
            }
            Ok(Err(e)) => tracing::warn!("watch error: {}", e),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("file watcher channel disconnected");
            }
        }

        // Drain anything whose last-event timestamp is older than `debounce`.
        let now = Instant::now();
        let due: Vec<PathBuf> = pending
            .iter()
            .filter(|(_, t)| now.duration_since(**t) >= debounce)
            .map(|(p, _)| p.clone())
            .collect();
        if due.is_empty() {
            continue;
        }
        for p in &due {
            pending.remove(p);
            if !p.exists() {
                let s = p.to_string_lossy().to_string();
                writer.delete_file(&s);
                tracing::info!("removed: {}", s);
                continue;
            }
            match index_file(writer, p) {
                Ok(n) => tracing::info!("indexed {} events from {}", n, p.display()),
                Err(e) => tracing::warn!("failed to index {}: {}", p.display(), e),
            }
        }
        writer.commit()?;
    }
}

fn is_interesting(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}
