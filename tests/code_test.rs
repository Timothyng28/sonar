use std::fs;
use std::path::Path;
use tempfile::TempDir;

use sonar::code::index::{
    index_repo, open_or_create_code_index, CodeSearchArgs, CodeSearcher, CodeWriter,
    CODE_WRITER_HEAP_BYTES,
};
use sonar::code::walk::{language_for_extension, walk_repo};

fn write_file(dir: &Path, rel: &str, body: &str) {
    let full = dir.join(rel);
    if let Some(p) = full.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(full, body).unwrap();
}

#[test]
fn walker_respects_gitignore_and_skips_binaries() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    // .gitignore only applies inside a git repo. The `ignore` crate
    // requires a `.git` directory to honor `.gitignore` (matching real
    // ripgrep / git behavior). Init one so the test exercises the
    // realistic code path.
    fs::create_dir_all(root.join(".git")).unwrap();
    write_file(root, ".gitignore", "secret.txt\nnode_modules/\n");
    write_file(root, "src/main.rs", "fn main() { let x = 42; }");
    write_file(root, "src/util.py", "def hello(): pass");
    write_file(root, "secret.txt", "DON'T INDEX ME");
    write_file(root, "node_modules/lib.js", "module.exports = {}");
    // 0-byte and oversized files filtered:
    write_file(root, "empty.rs", "");
    // pretend-binary by extension:
    write_file(root, "image.png", "fakepng");

    let files = walk_repo(root).unwrap();
    let names: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
    assert!(names.iter().any(|n| n.ends_with("main.rs")), "should index main.rs");
    assert!(names.iter().any(|n| n.ends_with("util.py")), "should index util.py");
    assert!(!names.iter().any(|n| n.contains("secret")), "gitignore should hide secret.txt");
    assert!(!names.iter().any(|n| n.contains("node_modules")), "gitignore should hide node_modules");
    assert!(!names.iter().any(|n| n.ends_with("empty.rs")), "empty files skipped");
    assert!(!names.iter().any(|n| n.ends_with("image.png")), "binary by extension skipped");
}

#[test]
fn language_detection_basics() {
    assert_eq!(language_for_extension(Path::new("foo.rs")), "rust");
    assert_eq!(language_for_extension(Path::new("foo.py")), "python");
    assert_eq!(language_for_extension(Path::new("foo.tsx")), "typescript");
    assert_eq!(language_for_extension(Path::new("Makefile")), "make");
    assert_eq!(language_for_extension(Path::new("Dockerfile")), "dockerfile");
    assert_eq!(language_for_extension(Path::new("README")), "other");
}

#[test]
fn index_and_search_round_trip() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    write_file(repo, "src/auth.rs", "fn loginUser(name: &str) -> Result<()> { Ok(()) }");
    write_file(repo, "src/db.py", "def connect_database(): return None");
    write_file(repo, "README.md", "# myproject\n\nA demo for sonar code search.");

    // Use an isolated $HOME for the index dir.
    let home = TempDir::new().unwrap();
    std::env::set_var("HOME", home.path());

    let (index, fields, _) = open_or_create_code_index("myproject").unwrap();
    let mut writer = CodeWriter::new(&index, fields.clone(), CODE_WRITER_HEAP_BYTES).unwrap();
    let (files, _bytes) = index_repo(&mut writer, "myproject", "development", "abc1234", repo).unwrap();
    assert_eq!(files, 3, "all three files should index");

    let searcher = CodeSearcher::new(&index, fields).unwrap();
    let hits = searcher
        .search(CodeSearchArgs {
            query: "loginUser".into(),
            repo: Some("myproject".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(!hits.is_empty(), "phrase match on identifier should hit auth.rs");
    assert!(hits[0].file_path.ends_with("auth.rs"));
    assert_eq!(hits[0].language, "rust");
    assert_eq!(hits[0].branch, "development");

    // Identifier-splitting: the same file should also match "login" alone.
    let hits = searcher
        .search(CodeSearchArgs {
            query: "login".into(),
            repo: Some("myproject".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(hits.iter().any(|h| h.file_path.ends_with("auth.rs")));

    // Language filter narrows correctly.
    let hits = searcher
        .search(CodeSearchArgs {
            query: "database".into(),
            repo: Some("myproject".into()),
            language: Some("python".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(hits.iter().all(|h| h.language == "python"));
    assert!(hits.iter().any(|h| h.file_path.ends_with("db.py")));
}

#[test]
fn reindex_replaces_old_docs() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();
    write_file(repo, "src/a.rs", "fn old_function() {}");

    let home = TempDir::new().unwrap();
    std::env::set_var("HOME", home.path());

    let (index, fields, _) = open_or_create_code_index("rep").unwrap();
    let mut writer = CodeWriter::new(&index, fields.clone(), CODE_WRITER_HEAP_BYTES).unwrap();
    index_repo(&mut writer, "rep", "development", "sha1", repo).unwrap();

    // Replace the file content and re-index.
    fs::write(repo.join("src/a.rs"), "fn new_function() {}").unwrap();
    index_repo(&mut writer, "rep", "development", "sha2", repo).unwrap();

    let searcher = CodeSearcher::new(&index, fields).unwrap();
    let old = searcher
        .search(CodeSearchArgs {
            query: "old_function".into(),
            repo: Some("rep".into()),
            ..Default::default()
        })
        .unwrap();
    let new = searcher
        .search(CodeSearchArgs {
            query: "new_function".into(),
            repo: Some("rep".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(old.is_empty(), "old content should be gone after reindex");
    assert!(!new.is_empty(), "new content should be present");
}
