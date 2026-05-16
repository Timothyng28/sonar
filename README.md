# sonar

![rust](https://img.shields.io/badge/rust-2021-orange?logo=rust)
![tests](https://img.shields.io/badge/tests-21%2F21%20passing-success)
![storage](https://img.shields.io/badge/storage-mmap-blue)
![ranking](https://img.shields.io/badge/ranking-BM25-blueviolet)
![protocol](https://img.shields.io/badge/protocol-MCP-purple)
![license](https://img.shields.io/badge/license-MIT-green)

Inverted-index search over **your Claude Code conversation history** *and* **the canonical state of your source code**, served from memory-mapped tantivy indexes. Two MCP tools, one binary, one server process. Any agent can answer *"which session did I work on X?"* or *"where is X implemented in this codebase?"* in microseconds.

## Two tools, one server

| Tool | Searches | Indexed when |
|---|---|---|
| **`sonar(query, since?, project?, limit?)`** | past Claude Code session transcripts (`~/.claude/projects/*.jsonl`) | `SessionEnd` hook fires on session close |
| **`sonar_code(query, repo?, language?, limit?)`** | canonical source code from indexed repos (typically `development`) | `post-merge` git hook fires after `git pull` |

Both share the same MCP server process — one `sonar mcp` child per Claude Code session serves both indexes. Both deliberately index "the past after it canonicalizes," never the live present:

- Live conversation? Already in Claude's context. Don't need to search for it.
- Live code in your worktree? Use native `Read` / `Grep`. The agent can see it.
- **Past sessions and merged code** — *that's* what sonar is for.

## How it actually works

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

## Real-world head-to-head

Two parallel agent sessions, same backend monorepo, same machine. One uses sonar; the other uses ripgrep + standard agent tools (`Read`/`Glob`/`git`/etc.). I ran two rounds — the *shape of the question* matters more than the tools.

### Round 1 — a commit-shaped question

> *"Find what implements this recent merged PR"* (the prompt named the commit SHA)

| | sonar | ripgrep + git |
|---|---|---|
| Wall time | 42 s | **18 s** |
| Strategy | multiple BM25 queries against the index | `git show --stat <sha>` + read the design-decision doc |

**git wins, decisively.** When the question explicitly references a commit, the commit object itself is a perfect index for *what changed where* — no content-search engine can beat that. Sonar wasn't designed to. This round was my own fault for picking a question that handed git a built-in shortcut.

### Round 2 — a content-shaped question

> *"Find the migration that adds these columns, the model class that has them, and the decision logic that reads them"* — no commit, no PR, no file-path hint

| | sonar | ripgrep |
|---|---|---|
| **Wall time** | **20 s** | 31 s |
| Queries / commands | 5 × `sonar code search` | 5+ × `rg` + `ls` + `Read` |
| Coverage breadth (downstream consumers, UIs) | ✓ surfaced | ✗ missed |
| Coverage depth (every callsite with line numbers, worker registration, internal client types) | partial | ✓ exhaustive |
| Answer quality | correct, broad | correct, deep |

**sonar wins on time + breadth.** Both produced enough for Claude to give a correct answer; they emphasized different facets. Sonar's BM25 ranking surfaced files where the concept is *thematically central* (including downstream consumers and UI surfaces that grep didn't bubble up); ripgrep's literal-substring scan surfaced every textual occurrence including line-numbered callsites.

### What this tells you

Three honest takeaways from running both:

| Question shape | Best tool |
|---|---|
| *"What changed in commit/PR X?"* | **git** (`git show --stat`, `git log -p`) — perfect built-in index |
| *"Find every callsite of `foo()`, with line numbers"* | **ripgrep** — literal exhaustiveness is its job |
| *"Find files *about* topic X, ranked by relevance"* | **sonar** — that's what an inverted index with BM25 is for |
| *"Find the function that vaguely does Y, described loosely"* | **sonar** — content-relevance ranking wins on fuzzy queries |
| *"Find the canonical implementation across migration / model / consumer / UI"* | **sonar** — surfaces breadth + downstream consequence |

**Neither tool replaces the other.** A well-equipped agent reaches for `git` first when the question is about history, `ripgrep` first when the question is about exact strings, and `sonar` first when the question is about content semantics. The right answer for the agent is *"have all three available."*

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

## Adding source code

Sonar also indexes source code — typically the canonical state of one branch per repo. The hook fires after `git pull` does a merge / fast-forward, so the index always tracks what's actually merged. Worktrees with in-progress changes are intentionally NOT indexed — for those, the agent uses `Read`/`Grep` directly.

```bash
cd ~/Desktop/myrepo

# 1. one-time bootstrap
~/Desktop/sonar/target/release/sonar code index --repo .

# 2. install the post-merge hook (defaults to --branch development)
~/Desktop/sonar/target/release/sonar code install --repo .

# or pick a different tracked branch:
~/Desktop/sonar/target/release/sonar code install --repo . --branch main
~/Desktop/sonar/target/release/sonar code install --repo . --branch trunk
```

After install, every `git pull` that updates the tracked branch re-indexes the repo in the background (~few seconds). The same MCP server now answers code questions too:

> *"Where do we set up the auth middleware in this repo?"*
> *"Find the migration that added the new permission columns."*

Claude picks `sonar_code` for "where in the code" questions and `sonar` for "what session" questions.

### How the hook handles branches & worktrees

The hook is installed at `.git/hooks/post-merge`, which fires after **any** successful `git pull` / `git merge` in the repo *or any of its worktrees* (git worktrees share `.git/hooks/`). The hook itself contains a branch check — it only reindexes if the merging worktree is on the tracked branch:

```bash
# .git/hooks/post-merge (generated)
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$BRANCH" = "development" ]; then
  sonar code index --repo /path/to/repo --branch development &
fi
```

So in a worktree setup:

| Action | Hook fires? | Reindex? |
|---|---|---|
| `git pull` in main repo (on `development`) | ✓ | ✓ |
| `git pull` in `.worktrees/feature-foo` (on `feature/foo`) | ✓ | ✗ (branch check fails — correctly, feature branches aren't canonical) |
| `git merge feature/foo` into `development` in main repo | ✓ | ✓ |

This is why the hook tracks **one branch per repo** — the canonical one. If you have multiple long-lived branches you want indexed separately (rare), install the hook multiple times with different `--label` and `--branch`, and they'll write to separate index dirs under `~/.sonar/code/`.

### Storage stays bounded

Each reindex calls `delete_by_branch` on the writer first, then re-adds every file. Tantivy tombstones the old segment(s) and merges them out in the background. The on-disk index stays at roughly **one corpus's worth** (~7% of source size) regardless of how many times you reindex. Verified empirically: 5 back-to-back reindexes of a small repo went from 256 KB → 264 KB, not 256 KB × 5.

## Usage from inside Claude Code

After `sonar install` + a Claude Code restart, just ask either kind of question:

> *"Find me the session where I figured out the alembic migration thing last week."*  → `sonar` (transcripts)
>
> *"Where in the codebase does the request handler call into the worker pool?"*  → `sonar_code` (source)

Claude picks the right tool. Both return ranked structured hits with snippets; Claude either summarizes or uses native `Read` to dig further.

## CLI subcommands

### Transcripts (sessions)

| Command | Purpose |
|---|---|
| `sonar index` | One-time bootstrap of existing transcripts. Use `--file <path>` to reindex one (what the hook does). |
| `sonar daemon` | Long-running watcher; keeps index fresh via FSEvents/inotify. Optional — `SessionEnd` hook covers most users. |
| `sonar mcp` | Stdio MCP server (invoked by Claude Code). Serves both `sonar` and `sonar_code` tools. |
| `sonar mcp-config` / `sonar hook-config` | Print JSON snippets for `.mcp.json` / `~/.claude/settings.json` |
| `sonar install` / `sonar uninstall` | One-command wire-up with backups + idempotency |
| `sonar search <query>` | CLI search. `--since 7d`, `--project X`, `--limit N`, `--bench 100` |
| `sonar stats` | Show transcript index status |

### Source code (NEW)

| Command | Purpose |
|---|---|
| `sonar code index --repo <path>` | Bootstrap or re-index a repo (typically the current `development` checkout). `--label`, `--branch` optional. |
| `sonar code install --repo <path>` | Writes `.git/hooks/post-merge` so `git pull` triggers a re-index. `--branch development` by default. |
| `sonar code search <query>` | CLI search. `--repo`, `--language python`, `--limit N`, `--bench 100` |
| `sonar code stats --repo <label>` | Show code index status |

## The two MCP tools

```
sonar(query, since?, project?, limit?)            # transcripts
sonar_code(query, repo?, language?, limit?)        # source code
```

- **query** — free-text BM25 query. Supports phrase quoting and AND/OR.
- **since** (transcripts only) — ISO date *or* relative shorthand (`3d`, `2w`, `5h`).
- **project** (transcripts only) — filter by project label.
- **repo** (code only) — repo label (defaults to the only one indexed, if there's just one).
- **language** (code only) — filter by `rust`, `python`, `typescript`, `go`, `java`, `cpp`, `sql`, `yaml`, …
- **limit** — max results (default 10, capped 100).

Returns a JSON array of structured hits ready for an agent to act on.

## Architecture

```
TRANSCRIPTS                                CODE
~/.claude/projects/*.jsonl                 ~/Desktop/<repo>/  (working tree)
        │                                          │
        ▼                                          ▼
  SessionEnd hook                            post-merge git hook
        │                                          │
        ▼                                          ▼
  sonar index --file <transcript>            sonar code index --repo <path>
        │                                          │
        ▼                                          ▼
  ~/.sonar/index/  (mmap)                    ~/.sonar/code/<repo>/  (mmap)
        │                                          │
        └────────────┬─────────────────────────────┘
                     ▼
              one `sonar mcp` server
              (spawned per Claude session)
              exposes two tools:
              ┌──────────────┐  ┌─────────────────┐
              │  sonar(...)  │  │ sonar_code(...) │
              └──────────────┘  └─────────────────┘
                     ▲
                     │
              Claude Code session
```

Two indexes, two hooks, one MCP server. Each index gets its own tantivy `MmapDirectory`. The MCP server lazily opens a `CodeSearcher` per repo on first query — so even with many tracked repos, startup is fast and only-touched indexes are paged in.

## Layout

```
src/
├── main.rs          CLI dispatch (transcript + code subcommands)
├── lib.rs           Library entry point
├── parse.rs         Claude Code JSONL → IndexableEvent (transcripts)
├── index.rs         transcript schema + MmapDirectory + writer/searcher
├── daemon.rs        notify watcher + incremental reindex (transcripts)
├── code/
│   ├── mod.rs       code-search module entry
│   ├── walk.rs      gitignore-respecting repo walker (via `ignore` crate)
│   ├── parse.rs     UTF-8 read + camelCase identifier expansion
│   ├── index.rs     code schema + MmapDirectory + writer/searcher
│   └── install.rs   `post-merge` git hook installer
├── install.rs       'sonar install' / 'sonar uninstall' (transcripts wire-up)
└── mcp.rs           rmcp server exposing BOTH tools: sonar + sonar_code
tests/
├── parse_test.rs    JSONL parser unit tests
├── install_test.rs  install/uninstall integration tests
└── code_test.rs     walker + tokenization + code index round-trip
```

## Development

```bash
cargo build              # debug
cargo build --release    # production binary
cargo test               # all tests (21 currently across 6 suites)
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
