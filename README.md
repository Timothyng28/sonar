# sonar

![rust](https://img.shields.io/badge/rust-2021-orange?logo=rust)
![tests](https://img.shields.io/badge/tests-10%2F10%20passing-success)
![storage](https://img.shields.io/badge/storage-mmap-blue)
![ranking](https://img.shields.io/badge/ranking-BM25-blueviolet)
![protocol](https://img.shields.io/badge/protocol-MCP-purple)
![license](https://img.shields.io/badge/license-MIT-green)

Memory-mapped, instant search across your Claude Code conversation history. Exposed as an MCP tool so any agent can answer *"which session did I work on X?"* in microseconds.

## At a glance

| Metric | Value |
|---|---|
| **Query latency (warm, in-process)** | **187 µs median · 253 µs p95** |
| End-to-end CLI (incl. process startup) | 4.8 ms mean |
| Throughput potential | ~5,000 queries/sec/core |
| Bootstrap | 2.0 s for 146,446 events |
| Index size on disk | 82 MB for the same corpus |
| Binary size | 6.8 MB, single static binary |
| External services | none (fully local) |
| Tests | 10/10 passing across 5 suites |

## Speed vs ripgrep

Same query, same machine, `hyperfine --warmup 3 --runs 30`:

| Corpus | sonar (mean) | ripgrep (mean) | sonar advantage |
|---|---|---|---|
| 1× (1,388 sessions, 1.0 GB) | **4.8 ms ± 0.5** | 11.6 ms ± 4.5 | **2.4× faster** |
| 10× (13,880 sessions, 8.9 GB) | **5.7 ms ± 0.3** | 15.5 ms ± 5.0 | **2.7× faster** |

End-to-end numbers above (binary startup + open + query + render). The actual **query work** is sub-millisecond:

| Corpus | Query-only latency (1000 runs, no startup) |
|---|---|
| 1× (146,446 events) | min 182µs · **median 202µs** · p95 225µs |
| 10× (1,466,540 events) | min 310µs · **median 347µs** · p95 378µs |

**Sonar's query latency grows by 72% when the corpus grows 10×.** ripgrep scales linearly with file count, so the gap *widens* as your transcript archive grows — sonar plateaus, ripgrep keeps slowing down.

## Speed isn't the only win

### 1. Deterministic ranking

Ripgrep's parallel filesystem walk returns matches in **arbitrary order** — run the same query twice, you can get a different "top 5" because `head -5` slices a different prefix. Sonar's BM25 score is stable: same query → same ranking, every time.

### 2. Content-aware over path-aware

A common failure mode for grep-based search: the directory containing a session may not match the topic. You might have built *feature X* in a parent repo session whose path doesn't mention *feature X* at all — the *content* does, but the *path* doesn't. Ripgrep matches on path or on a literal substring; if it finds a misleadingly-named directory first, it'll stop there. Sonar's BM25 ranking surfaces the session where the topic is actually *discussed*, regardless of which directory it lived in.

### 3. Structured filters

Filter by `--since 7d`, `--project ccai`, `--role assistant` in one flag. Ripgrep would need a shell pipeline of `find` + `xargs` + manual JSON parsing.

### 4. Output ready for an agent

Sonar returns structured hits — `{session_id, project, timestamp, file_path, event_index, snippet, score}` — so an MCP-connected Claude can act on the result directly. Ripgrep returns file paths; Claude has to `Read` each one to learn anything.

## What it does

Claude Code writes every session you have to a `.jsonl` file under `~/.claude/projects/`. Over time you accumulate hundreds of conversations spanning thousands of turns and millions of words. They're sitting on disk, unread.

`sonar` indexes all of that into a [tantivy](https://github.com/quickwit-oss/tantivy) full-text index backed by `MmapDirectory`, then exposes one MCP tool over stdio. Plug it into Claude Code and ask:

> *"Which session did I work on the app xyz frontend?"*

…and get an answer in ~5 ms.

## Why mmap

- **Sub-millisecond queries.** The index isn't loaded into RAM; the OS pages in only the bytes touched during a search.
- **Safe concurrent reads while writing.** The daemon writer, the MCP server, multiple Claude Code sessions, and the CLI all share the same memory-mapped index without locks.
- **Zero copies into heap.** Search hits read straight from page-cache-backed pages.
- **Survives crashes.** Tantivy's commit model writes new segments atomically; readers reload on commit.

## Install

```bash
git clone https://github.com/Timothyng28/sonar
cd sonar
cargo build --release
```

Then wire it into Claude Code in one command:

```bash
./target/release/sonar index            # bootstrap the search index
./target/release/sonar install          # add SessionEnd hook + register MCP server
```

`sonar install` is idempotent and safe:

- Adds a `SessionEnd` hook to `~/.claude/settings.json` so transcripts re-index automatically when you exit a session
- Registers `sonar` in `~/.claude.json` under `mcpServers` so the tool is available in every Claude Code session
- Backs up both files to `*.pre-sonar` before any change
- Preserves all existing hooks and MCP servers
- Skips writes if sonar is already installed
- `--dry-run` to preview, `--no-hook` / `--no-mcp` for partial installs, `--project <dir>` to scope MCP to a single repo
- Restart Claude Code after install so the MCP server loads. The hook works without restart.

```bash
./target/release/sonar uninstall        # restore from .pre-sonar backups
```

## Usage from inside Claude Code

After `sonar install` + a Claude Code restart, just ask:

> *"Find me the session where I figured out the alembic migration thing last week."*

Claude calls the `sonar` MCP tool, returns matching sessions with snippets and file paths, and either summarizes or uses the native `Read` tool to dig into a specific transcript.

## CLI subcommands

| Command | Purpose |
|---|---|
| `sonar index` | One-time bootstrap of existing transcripts. Use `--file <path>` to reindex a single transcript (what the hook does). |
| `sonar daemon` | Long-running watcher; keeps index fresh via FSEvents/inotify. Optional — the `SessionEnd` hook covers most users. |
| `sonar mcp` | Stdio MCP server (invoked by Claude Code) |
| `sonar mcp-config` | Print the JSON snippet for `.mcp.json` |
| `sonar hook-config` | Print the `SessionEnd` hook snippet for `~/.claude/settings.json` |
| `sonar install` / `sonar uninstall` | One-command wire-up with backups + idempotency |
| `sonar search <query>` | Search from the command line. `--since 7d`, `--project X`, `--limit N`, `--bench 100` for timing |
| `sonar stats` | Show index status |

## The single MCP tool

```
sonar(query, since?, project?, limit?)
```

- **query** — free-text BM25 query. Supports phrase quoting and AND/OR.
- **since** — ISO date *or* relative shorthand (`3d`, `2w`, `5h`).
- **project** — filter by project label.
- **limit** — max results (default 10, capped 100).

Returns a JSON array of `{session_id, project, event_role, timestamp, file_path, event_index, snippet, score}`.

## Architecture

```
~/.claude/projects/*/sessions/*.jsonl       Claude Code writes these
              │
              ▼
        SessionEnd hook (or FSEvents)
              │
              ▼
      sonar index --file <path>            (one process, owns IndexWriter)
              │
              ▼
   tantivy MmapDirectory  ◄────────────────  sonar mcp  (read-only Searcher)
   ~/.sonar/index/                          spawned per Claude Code session
              ▲
              │
        sonar search (CLI)                  (also read-only)
```

One writer, many readers, all coordinating through a memory-mapped index. tantivy's `IndexReader::OnCommitWithDelay` policy makes new commits visible to readers without restart.

## Layout

```
src/
├── main.rs          CLI dispatch
├── lib.rs           Library entry point
├── parse.rs         Claude Code JSONL → IndexableEvent
├── index.rs         tantivy schema + MmapDirectory + writer/searcher
├── daemon.rs        notify watcher + incremental reindex
├── install.rs       'sonar install' / 'sonar uninstall'
└── mcp.rs           rmcp server exposing one tool
tests/
├── parse_test.rs    JSONL parser unit tests
└── install_test.rs  Install/uninstall integration tests
```

## Development

```bash
cargo build              # debug
cargo build --release    # production binary
cargo test               # all tests (10 currently)
```

Reproduce the benchmarks yourself:

```bash
# Latency (in-process, no startup)
sonar search "your query" --bench 1000

# End-to-end vs ripgrep
hyperfine --warmup 3 --runs 30 \
  "sonar search 'your query' --limit 10" \
  "rg -l --no-config -F 'your query' ~/.claude/projects/ | head -10"
```

## License

MIT.
