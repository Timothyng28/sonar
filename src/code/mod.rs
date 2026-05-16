//! `sonar code` — inverted-index search over source code.
//!
//! Mirrors the architecture of the transcript pipeline (parse → index →
//! search) but indexes whole repo trees instead of JSONL session files.
//! The model is "canonical-state-only": you index `development` on `git
//! fetch`, and the agent searches that. Worktree-local working copies
//! aren't tracked — the agent already sees those via `Read`/`Grep`.

pub mod index;
pub mod install;
pub mod parse;
pub mod walk;
