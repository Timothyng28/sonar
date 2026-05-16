//! sonar — memory-mapped, instant search across your Claude Code conversation
//! history. Library entry point. The binary in `src/main.rs` is a thin CLI
//! over these modules.

pub mod daemon;
pub mod index;
pub mod install;
pub mod mcp;
pub mod parse;
