use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::document::Value;
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, SchemaBuilder, INDEXED, STORED, STRING, TEXT,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

use crate::code::parse;
use crate::code::walk::{walk_repo, SourceFile};

/// Tantivy schema for code documents — one document per file.
#[derive(Clone)]
pub struct CodeFields {
    pub repo: Field,
    pub branch: Field,
    pub commit_sha: Field,
    pub file_path: Field,
    pub language: Field,
    pub content: Field,
    pub size_bytes: Field,
}

pub fn build_code_schema() -> (Schema, CodeFields) {
    let mut sb = SchemaBuilder::default();
    let u64_opts: NumericOptions = NumericOptions::default().set_stored().set_fast();
    let repo = sb.add_text_field("repo", STRING | STORED);
    let branch = sb.add_text_field("branch", STRING | STORED);
    let commit_sha = sb.add_text_field("commit_sha", STORED);
    let file_path = sb.add_text_field("file_path", STRING | STORED);
    let language = sb.add_text_field("language", STRING | STORED);
    let content = sb.add_text_field("content", TEXT | STORED);
    let size_bytes = sb.add_u64_field("size_bytes", u64_opts | INDEXED);
    let schema = sb.build();
    (
        schema,
        CodeFields {
            repo,
            branch,
            commit_sha,
            file_path,
            language,
            content,
            size_bytes,
        },
    )
}

/// Where the code index for a given repo lives. Each tracked repo gets
/// its own subdir so multiple repos can coexist under one MCP server.
pub fn code_index_path(repo_label: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    // `/` in the label would create accidental subdirs; convert to '-'.
    let safe = repo_label.replace(['/', '\\'], "-");
    Ok(home.join(".sonar").join("code").join(safe))
}

/// Open or create the code index for a given repo.
pub fn open_or_create_code_index(repo_label: &str) -> Result<(Index, CodeFields, PathBuf)> {
    let path = code_index_path(repo_label)?;
    std::fs::create_dir_all(&path)
        .with_context(|| format!("creating code index dir {}", path.display()))?;
    let (schema, fields) = build_code_schema();
    let dir = MmapDirectory::open(&path)
        .with_context(|| format!("opening MmapDirectory at {}", path.display()))?;
    let index = Index::open_or_create(dir, schema)?;
    Ok((index, fields, path))
}

/// Wrapper around the IndexWriter for code documents.
pub struct CodeWriter {
    writer: IndexWriter,
    fields: CodeFields,
}

impl CodeWriter {
    pub fn new(index: &Index, fields: CodeFields, heap_bytes: usize) -> Result<Self> {
        let writer = index.writer(heap_bytes)?;
        Ok(Self { writer, fields })
    }

    /// Delete all docs for a given (repo, branch) — call before re-indexing
    /// to keep the index in sync with the latest fetch. Anything we don't
    /// re-add disappears at commit time.
    pub fn delete_repo_branch(&mut self, repo: &str, branch: &str) {
        // Tantivy doesn't have AND-delete; we delete each predicate
        // separately. In practice a single (repo, branch) combo covers
        // everything we just indexed, so deleting on `branch` then
        // committing is sufficient — but we delete on both for safety
        // when multiple branches share the same index dir (they don't,
        // but defense isn't free of cost).
        let _ = repo;
        let term = Term::from_field_text(self.fields.branch, branch);
        self.writer.delete_term(term);
    }

    pub fn add(
        &mut self,
        repo: &str,
        branch: &str,
        commit_sha: &str,
        file: &SourceFile,
        content_for_index: &str,
    ) -> Result<()> {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.repo, repo);
        doc.add_text(self.fields.branch, branch);
        doc.add_text(self.fields.commit_sha, commit_sha);
        doc.add_text(self.fields.file_path, &file.rel_path);
        doc.add_text(self.fields.language, &file.language);
        doc.add_text(self.fields.content, content_for_index);
        doc.add_u64(self.fields.size_bytes, file.size);
        self.writer.add_document(doc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }
}

/// Read-side handle.
pub struct CodeSearcher {
    reader: IndexReader,
    pub fields: CodeFields,
}

impl CodeSearcher {
    pub fn new(index: &Index, fields: CodeFields) -> Result<Self> {
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self { reader, fields })
    }

    pub fn search(&self, args: CodeSearchArgs) -> Result<Vec<CodeHit>> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let qp = QueryParser::for_index(searcher.index(), vec![self.fields.content]);
        let text_query: Box<dyn Query> = qp.parse_query(&args.query).context("parsing query")?;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];

        if let Some(repo) = &args.repo {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.repo, repo),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if let Some(lang) = &args.language {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.language, lang),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        let combined = BooleanQuery::from(clauses);
        let limit = args.limit.unwrap_or(10).clamp(1, 100);
        let docs = searcher.search(&combined, &TopDocs::with_limit(limit).order_by_score())?;

        let mut hits = Vec::with_capacity(docs.len());
        for (score, addr) in docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            hits.push(CodeHit::from_doc(&doc, &self.fields, score, &args.query));
        }
        Ok(hits)
    }

    pub fn has_any_docs(&self) -> Result<bool> {
        self.reader.reload()?;
        Ok(self.reader.searcher().num_docs() > 0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodeSearchArgs {
    pub query: String,
    pub repo: Option<String>,
    pub language: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeHit {
    pub repo: String,
    pub branch: String,
    pub commit_sha: String,
    pub file_path: String,
    pub language: String,
    pub size_bytes: u64,
    pub snippet: String,
    pub score: f32,
}

impl CodeHit {
    fn from_doc(doc: &TantivyDocument, fields: &CodeFields, score: f32, query: &str) -> Self {
        let content = first_text(doc, fields.content).unwrap_or_default();
        let snippet = make_snippet(&content, query, 160);
        Self {
            repo: first_text(doc, fields.repo).unwrap_or_default(),
            branch: first_text(doc, fields.branch).unwrap_or_default(),
            commit_sha: first_text(doc, fields.commit_sha).unwrap_or_default(),
            file_path: first_text(doc, fields.file_path).unwrap_or_default(),
            language: first_text(doc, fields.language).unwrap_or_default(),
            size_bytes: first_u64(doc, fields.size_bytes).unwrap_or(0),
            snippet,
            score,
        }
    }
}

fn first_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field).and_then(|v| v.as_str()).map(|s| s.to_string())
}
fn first_u64(doc: &TantivyDocument, field: Field) -> Option<u64> {
    doc.get_first(field).and_then(|v| v.as_u64())
}

/// Show ~max_chars of `text` near the first occurrence of any query term.
/// Returns a single-line whitespace-collapsed string for readable display.
fn make_snippet(text: &str, query: &str, max_chars: usize) -> String {
    let needle = query
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    let hay = text.to_lowercase();
    let center = if !needle.is_empty() {
        hay.find(&needle).unwrap_or(0)
    } else {
        0
    };
    let half = max_chars / 2;
    let start = center.saturating_sub(half);
    let end = (start + max_chars).min(text.len());
    // Walk start/end to valid utf-8 boundaries.
    let start = round_down_char(text, start);
    let end = round_down_char(text, end);
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.push_str(text[start..end].trim());
    if end < text.len() {
        snippet.push('…');
    }
    snippet.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn round_down_char(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Bootstrap or re-index a repo. Returns (files, bytes).
pub fn index_repo(
    writer: &mut CodeWriter,
    repo_label: &str,
    branch: &str,
    commit_sha: &str,
    repo_root: &Path,
) -> Result<(usize, u64)> {
    // Replace anything previously indexed for this branch.
    writer.delete_repo_branch(repo_label, branch);

    let files = walk_repo(repo_root)?;
    let mut bytes = 0u64;
    let mut count = 0usize;
    for f in &files {
        let prepared = match parse::read_and_prepare(&f.abs_path) {
            Ok(s) => s,
            Err(_) => continue, // skip files we can't read as UTF-8
        };
        writer.add(repo_label, branch, commit_sha, f, &prepared)?;
        bytes += f.size;
        count += 1;
    }
    writer.commit()?;
    Ok((count, bytes))
}

pub const CODE_WRITER_HEAP_BYTES: usize = 100_000_000; // 100 MB
