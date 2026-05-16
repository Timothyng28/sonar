# sonar

![rust](https://img.shields.io/badge/rust-2021-orange?logo=rust)
![tests](https://img.shields.io/badge/tests-10%2F10%20passing-success)
![storage](https://img.shields.io/badge/storage-mmap-blue)
![ranking](https://img.shields.io/badge/ranking-BM25-blueviolet)
![protocol](https://img.shields.io/badge/protocol-MCP-purple)
![license](https://img.shields.io/badge/license-MIT-green)

Inverted-index search over your Claude Code conversation history, served from a memory-mapped tantivy index. Exposed as an MCP tool so any agent can answer *"which session did I work on X?"* in microseconds.

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

## What it does

Claude Code writes every session you have to a `.jsonl` file under `~/.claude/projects/`. Over time you accumulate hundreds of conversations spanning thousands of turns and millions of words. They're sitting on disk, unread.

`sonar` indexes all of that into a [tantivy](https://github.com/quickwit-oss/tantivy) full-text inverted index, exposes one MCP tool over stdio, and answers:

> *"Which session did I work on the app xyz frontend?"*

…in ~5 ms end-to-end, ~250 µs of which is the actual search.

## How it actually works (the speed isn't really mmap)

Both sonar and ripgrep use `mmap`. The real difference is **what each tool has to look at on every query.**

Sonar builds an **inverted index** at index time. Conceptually it's a phone book: for every word that appears in any transcript, sonar stores a list of which session events contain it.

```
"alembic"    → [event_3, event_17, event_42, …]    (posting list)
"migration"  → [event_3, event_17, event_88, …]
```

A query for `"alembic migration"` becomes: look up each term in the dictionary (microseconds), intersect the posting lists (microseconds), fetch the top-N matches' stored snippets (microseconds). **The cost is proportional to the number of matches, not the size of the corpus.** Doubling your transcript archive barely moves the query latency.

That index is what's mmap'd. The term dictionary (a compressed Finite State Transducer), the posting lists, and the stored fields all live in segment files under `~/.sonar/index/`, and tantivy memory-maps them. **Mmap is how the index gets read efficiently across process boundaries** — the indexer writer + the MCP server reader + the CLI all share pages via the OS page cache. It's the transport, not the secret sauce. The inverted-index data structure is.

Tantivy provides the inverted-index infrastructure (FST, posting lists, BM25 scoring, multi-segment commits, crash-safe reads); sonar feeds Claude transcripts in and queries them.

## Sonar vs ripgrep — different tools, different jobs

Ripgrep is genuinely well-engineered. It uses mmap too, plus SIMD `memchr`, parallel walks, and Aho-Corasick for multi-pattern matching. The reason it doesn't build an inverted index isn't oversight — it's a deliberate trade-off.

| | ripgrep | sonar |
|---|---|---|
| **Setup cost** | Zero — point at any directory and go | Index build (~2 s / 146 k events) needed first |
| **Best when source is…** | constantly edited (a working directory) | append-mostly (Claude transcripts, log archives, mail) |
| **Query types** | full regex including PCRE | tokenized literal-term queries (BM25) |
| **Per-query work** | **O(corpus bytes)** — scan everything | **O(matching docs)** — look up posting lists |
| **Scaling** | linear with corpus size | sub-linear; grows with match count, not corpus |
| **Ranking** | none — file paths in walk order | BM25 relevance, deterministic across runs |
| **Output** | file paths + matching lines | structured: `{session_id, snippet, score, timestamp, …}` |
| **On-disk artifact** | none | index folder (~6% of corpus size) |
| **Stays in sync if files change** | trivially | needs a hook or daemon to reindex |

**Use ripgrep when:**

- You're grepping a working directory of code
- You need regex
- Files change every few seconds and you don't want to maintain an index
- It's a one-off search and you don't want infrastructure

**Use sonar when:**

- You're searching Claude Code conversation history (or any append-mostly archive)
- An agent is going to query the same corpus many times in a session
- You want ranked results, not just "files containing X"
- You want sub-millisecond latency that doesn't degrade as the archive grows
- You want structured output an agent can consume without re-reading files

They're not competing — they're for different workloads. Sonar leans on the [Lucene / Elasticsearch / Solr / Postgres-FTS / SQLite-FTS5](https://en.wikipedia.org/wiki/Search_engine_indexing) tradition (pre-built inverted index, query is a lookup). Ripgrep leans on the [grep / ack / ag](https://github.com/BurntSushi/ripgrep) tradition (no state, scan-on-demand, optimized to the metal).

## Measured: sonar vs ripgrep vs grep

Same query (`"alembic migration"`), same machine, `hyperfine --warmup 3 --runs 30`. Three tools side by side.

### 1× corpus (1.0 GB, 1,389 sessions)

| Tool | Mean | Range | vs sonar |
|---|---|---|---|
| **sonar** | **5.3 ms ± 0.3** | 5.0 – 5.9 ms | — |
| ripgrep | 10.4 ms ± 1.2 | 9.2 – 14.2 ms | **1.97× slower** |
| BSD `grep -rl` | **4.78 s ± 0.04** | 4.71 – 4.85 s | **900× slower** |

### 10× UNIQUE corpus (10 GB, 13,890 sessions, real byte-copies — no clonefile / no shared inodes)

| Tool | Mean | Range | vs sonar |
|---|---|---|---|
| **sonar** | **5.0 ms ± 0.2** | 4.8 – 6.0 ms | — |
| ripgrep | 28.7 ms ± 14.1 | 17.1 – 91.0 ms | **5.76× slower** |
| BSD `grep -rl` | **6.65 s ± 0.05** | 6.57 – 6.79 s | **1,334× slower** |

### What changes with scale

- **Sonar stays flat** — 5.3 → 5.0 ms across 10× more data. The inverted-index lookup is O(matches), not O(corpus). Doubling, tripling, 10×-ing your transcript archive barely moves query latency.
- **Ripgrep scales linearly** — 1.97× → 5.76× slower than sonar. The gap *tripled* when corpus grew 10×. Ripgrep's SIMD scan engine is genuinely fast, but it still has to walk every byte of every file on every query.
- **`grep -rl` is dominated by file-open overhead** — 1,389 files × ~200 µs `open()` syscalls ≈ several seconds before any bytes get scanned. Even with a hot page cache, opening tens of thousands of small files costs more than the actual matching.

### Query-only latency (no startup, what the MCP server actually pays per call)

| Corpus | min | median | p95 | growth vs 1× |
|---|---|---|---|---|
| 1× (146 k events) | 248 µs | **279 µs** | 315 µs | — |
| 10× unique (1.47 M events) | 195 µs | **293 µs** | 315 µs | within noise |

**10× more data → essentially no change in query latency.** That's the headline: the inverted index decouples query cost from corpus size. Through the MCP, every tool call Claude makes is ~300 µs of search + ~2 ms of JSON-RPC framing, regardless of how much history you've accumulated.

> **Reproduce on your machine:**
> ```bash
> # 1× scale, all three tools
> hyperfine --warmup 3 --runs 30 \
>   --command-name sonar     "sonar search 'YOUR QUERY' --limit 10" \
>   --command-name ripgrep   "rg -l -F 'YOUR QUERY' ~/.claude/projects/ | head -10" \
>   --command-name 'grep -rl' "/usr/bin/grep -rl --include='*.jsonl' 'YOUR QUERY' ~/.claude/projects/ | head -10"
> ```

## Beyond speed

Independent of latency, sonar gives an agent a few things ripgrep can't:

1. **Deterministic ranking.** Ripgrep's parallel filesystem walk returns matches in arbitrary order — the "top 5" can change run-to-run because `head -5` slices a different prefix each time. Sonar's BM25 score is stable: same query, same ranking, every time.

2. **Content-aware matching.** Grep tools match on the literal substring; if a session about *feature X* is filed in a directory whose path doesn't say "feature X", a path-name heuristic misses it. Sonar's BM25 ranks by *content relevance*, not where the file happens to live.

3. **Structured filters.** `--since 7d`, `--project foo`, `--role assistant` — one flag each. Ripgrep would need a shell pipeline of `find` + `xargs` + manual JSON parsing to do the same.

4. **Agent-ready output.** Sonar returns `{session_id, project, timestamp, file_path, event_index, snippet, score}` as JSON. An MCP-connected Claude can act on the result directly. Ripgrep returns paths; Claude would have to `Read` each one to learn anything.

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
