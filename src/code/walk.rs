use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use std::path::{Path, PathBuf};

/// One source file discovered during a repo walk.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Absolute path on disk.
    pub abs_path: PathBuf,
    /// Path relative to the repo root — what we store as `file_path`.
    pub rel_path: String,
    /// Inferred language label (rust, python, ts, …, or "other").
    pub language: String,
    /// File size in bytes (used for the stored field; also gates large files).
    pub size: u64,
}

/// Skip files bigger than this. Most legit source files are <100 KB;
/// anything bigger is usually generated code, lockfiles, vendored data,
/// or model checkpoints. Indexing them wastes space without helping
/// search quality.
pub const MAX_FILE_BYTES: u64 = 1_000_000;

/// Walk `root` respecting `.gitignore`, `.ignore`, hidden-file rules, etc.
/// (Same rules ripgrep uses — via the `ignore` crate.)
///
/// Returns one `SourceFile` per indexable file. Binary files and files
/// over `MAX_FILE_BYTES` are filtered out.
pub fn walk_repo(root: &Path) -> Result<Vec<SourceFile>> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", root.display()))?;

    let mut out = Vec::new();
    let walker = WalkBuilder::new(&root)
        .hidden(true) // skip dotfiles / dotdirs (incl. .git)
        .ignore(true) // honor .ignore
        .git_ignore(true) // honor .gitignore
        .git_exclude(true) // honor .git/info/exclude
        .git_global(true) // honor global ~/.gitignore
        .parents(true) // walk up parent dirs to find .gitignore
        .build();

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !is_file(&entry) {
            continue;
        }
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        if size == 0 || size > MAX_FILE_BYTES {
            continue;
        }
        if is_probably_binary(path) {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let language = language_for_extension(path);

        out.push(SourceFile {
            abs_path: path.to_path_buf(),
            rel_path: rel,
            language,
            size,
        });
    }
    Ok(out)
}

fn is_file(e: &DirEntry) -> bool {
    e.file_type().map(|ft| ft.is_file()).unwrap_or(false)
}

/// Cheap binary sniff. Extension-based: we skip well-known binary types
/// without reading content. Truly unknown extensions get indexed —
/// tantivy's tokenizer handles arbitrary UTF-8 fine. If it's actually
/// binary garbage, the tokenizer just produces zero terms.
fn is_probably_binary(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png" | "jpg"
            | "jpeg"
            | "gif"
            | "ico"
            | "webp"
            | "bmp"
            | "tiff"
            | "pdf"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "a"
            | "o"
            | "wasm"
            | "class"
            | "jar"
            | "pyc"
            | "pyo"
            | "mp3"
            | "mp4"
            | "mov"
            | "avi"
            | "webm"
            | "wav"
            | "flac"
            | "ogg"
            | "ttf"
            | "otf"
            | "woff"
            | "woff2"
            | "eot"
            | "psd"
            | "ai"
            | "sketch"
            | "fig"
            | "db"
            | "sqlite"
            | "parquet"
            | "arrow"
            | "lock"
            | "bin"
    )
}

/// Lightweight extension → language label. Used as a filter facet and a
/// display hint; doesn't drive tokenization. Defaults to `"other"`.
pub fn language_for_extension(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let lang = match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "scala" | "sc" => "scala",
        "clj" | "cljs" | "cljc" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "lua" => "lua",
        "r" => "r",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "vue" => "vue",
        "svelte" => "svelte",
        "md" | "markdown" => "markdown",
        "yml" | "yaml" => "yaml",
        "toml" => "toml",
        "json" | "jsonc" => "json",
        "xml" => "xml",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "tf" | "hcl" => "terraform",
        "dockerfile" => "dockerfile",
        "makefile" | "mk" => "make",
        "" => match path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "dockerfile" => "dockerfile",
            "makefile" => "make",
            "justfile" => "just",
            _ => "other",
        },
        _ => "other",
    };
    lang.to_string()
}
