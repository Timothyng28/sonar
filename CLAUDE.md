# sonar

Memory-mapped, instant search over two things: your Claude Code **conversation transcripts** and indexed **source code**. Backed by [tantivy](https://github.com/quickwit-oss/tantivy) (a Rust full-text search engine), exposed both as a CLI and as an MCP server that Claude Code talks to over stdio.

A single Rust crate: `src/lib.rs` holds all the logic, `src/main.rs` is a thin `clap` CLI over it.

## Build / test / lint

```bash
cargo build                 # debug build
cargo test                  # full suite (integration tests in tests/ + inline #[cfg(test)] mods)
cargo test --lib mcp::tests # one module's unit tests
cargo clippy --all-targets  # lint — keep this clean, it's the bar for merge
cargo build --release       # optimized binary (thin LTO, stripped)
```

There is no separate formatter step beyond `cargo fmt` defaults. No CI config lives in the repo yet — `cargo test` + `cargo clippy` green is the merge bar.

## Two indexes, two halves

The codebase is split down the middle by what it searches. Both halves mirror the same shape (index → search → MCP tool → CLI subcommand).

| Concern | Transcripts half | Code half |
|---------|------------------|-----------|
| On-disk index | `~/.sonar/index/` (single) | `~/.sonar/code/<label>/` (one dir per repo) |
| Indexing + watch | `src/daemon.rs`, `src/index.rs`, `src/parse.rs` | `src/code/index.rs`, `src/code/walk.rs`, `src/code/parse.rs` |
| MCP tool | `sonar(query, since?, project?, limit?)` | `sonar_code(query, repo?, language?, limit?)` |
| CLI | `sonar index` / `daemon` / `search` / `stats` | `sonar code index` / `search` / `stats` / `install` |

`src/mcp.rs` is the MCP server (`sonar mcp` over stdio) exposing both tools. `src/install.rs` and `src/code/install.rs` wire sonar into `~/.claude` (SessionEnd hook + MCP registration) and into a repo (`post-merge` git hook that re-indexes on `git pull`).

## Conventions

- **Logic in `lib.rs` modules, CLI stays thin.** Anything testable belongs in a module under `src/`, not in `main.rs`. `main.rs` parses args and calls in.
- **Pull filesystem-dependent logic into a pure helper that takes a path**, so tests can point it at a `tempfile::TempDir` instead of mutating `$HOME`. See `classify_code_repos(&Path)` in `src/mcp.rs` — `resolve_default_code_repo()` resolves the real `~/.sonar/code/` and delegates to it.
- **Errors are user-facing.** When a tool/CLI can't proceed, the message should tell the caller *how to fix it* (which arg to pass, which command to run), and must distinguish genuinely-empty state from ambiguous-input state — they need different responses from the caller.
- `Result`/`Option` are unwrapped with `.unwrap_or(...)` / `.expect("why this can't fail")`, never bare `.unwrap()` on fallible runtime state.

## Default-repo resolution (the multi-repo gotcha)

When `repo` is omitted, sonar picks a default from `~/.sonar/code/`. The **MCP** path (`resolve_default_code_repo` in `src/mcp.rs`) distinguishes three cases: none indexed (error: "no code repos indexed"), exactly one (use it), many (error listing the labels so the caller retries with `repo:`).

Known quirk: the **CLI** path (`pick_first_code_repo` in `src/main.rs`) does *not* yet make this distinction — with multiple repos and no `--repo`, it silently grabs the first directory. If you touch repo resolution, consider aligning the CLI with the MCP behavior.
