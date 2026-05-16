use anyhow::{Context, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::document::Value;
use tantivy::schema::{
    DateOptions, Field, IndexRecordOption, NumericOptions, Schema, SchemaBuilder, INDEXED, STORED,
    STRING, TEXT,
};
use tantivy::{
    DateTime as TantivyDateTime, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument,
    Term,
};

use crate::parse::IndexableEvent;

/// Tantivy schema fields, held together for ergonomic access.
#[derive(Clone)]
pub struct Fields {
    pub session_id: Field,
    pub project: Field,
    pub event_role: Field,
    pub timestamp: Field,
    pub cwd: Field,
    pub git_branch: Field,
    pub file_path: Field,
    pub event_index: Field,
    pub text: Field,
}

pub fn build_schema() -> (Schema, Fields) {
    let mut sb = SchemaBuilder::default();
    let date_opts = DateOptions::default()
        .set_stored()
        .set_indexed()
        .set_fast();
    let u64_opts: NumericOptions = NumericOptions::default().set_stored().set_fast();
    let session_id = sb.add_text_field("session_id", STRING | STORED);
    let project = sb.add_text_field("project", STRING | STORED);
    let event_role = sb.add_text_field("event_role", STRING | STORED);
    let timestamp = sb.add_date_field("timestamp", date_opts);
    let cwd = sb.add_text_field("cwd", STRING | STORED);
    let git_branch = sb.add_text_field("git_branch", STRING | STORED);
    let file_path = sb.add_text_field("file_path", STRING | STORED);
    let event_index = sb.add_u64_field("event_index", u64_opts | INDEXED);
    let text = sb.add_text_field("text", TEXT | STORED);
    let schema = sb.build();
    (
        schema,
        Fields {
            session_id,
            project,
            event_role,
            timestamp,
            cwd,
            git_branch,
            file_path,
            event_index,
            text,
        },
    )
}

/// Resolve the on-disk index directory. Defaults to `~/.sonar/index/`.
pub fn default_index_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".sonar").join("index"))
}

/// Open an existing index, or create it on disk if missing.
///
/// MmapDirectory is selected explicitly: index reads happen via
/// memory-mapped files — that's the whole point of the project.
pub fn open_or_create_index(index_path: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(index_path)
        .with_context(|| format!("creating index dir {}", index_path.display()))?;
    let (schema, fields) = build_schema();
    let dir = MmapDirectory::open(index_path)
        .with_context(|| format!("opening MmapDirectory at {}", index_path.display()))?;
    let index = Index::open_or_create(dir, schema)?;
    Ok((index, fields))
}

/// Wrapper around tantivy's IndexWriter for adding events.
pub struct EventWriter {
    writer: IndexWriter,
    fields: Fields,
}

impl EventWriter {
    pub fn new(index: &Index, fields: Fields, heap_bytes: usize) -> Result<Self> {
        let writer = index.writer(heap_bytes)?;
        Ok(Self { writer, fields })
    }

    pub fn add(&mut self, ev: &IndexableEvent) -> Result<()> {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.session_id, &ev.session_id);
        doc.add_text(self.fields.project, &ev.project);
        doc.add_text(self.fields.event_role, &ev.event_role);
        if let Some(ts) = ev.timestamp {
            let micros = ts.timestamp_micros();
            doc.add_date(
                self.fields.timestamp,
                TantivyDateTime::from_timestamp_micros(micros),
            );
        }
        if let Some(c) = &ev.cwd {
            doc.add_text(self.fields.cwd, c);
        }
        if let Some(b) = &ev.git_branch {
            doc.add_text(self.fields.git_branch, b);
        }
        doc.add_text(self.fields.file_path, &ev.file_path);
        doc.add_u64(self.fields.event_index, ev.event_index);
        doc.add_text(self.fields.text, &ev.text);
        self.writer.add_document(doc)?;
        Ok(())
    }

    /// Delete all events previously indexed from a specific file. Used by
    /// the daemon when a transcript file is rewritten so we don't double-
    /// index identical content.
    pub fn delete_file(&mut self, file_path: &str) {
        let term = Term::from_field_text(self.fields.file_path, file_path);
        self.writer.delete_term(term);
    }

    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }
}

/// Reader-side handle. Holds an IndexReader configured to reload on
/// commit, so the daemon's writes show up here without restarting.
pub struct EventSearcher {
    reader: IndexReader,
    pub fields: Fields,
}

impl EventSearcher {
    pub fn new(index: &Index, fields: Fields) -> Result<Self> {
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self { reader, fields })
    }

    /// Run a query and return the top `limit` matches.
    pub fn search(&self, args: SearchArgs) -> Result<Vec<SearchHit>> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let qp = QueryParser::for_index(searcher.index(), vec![self.fields.text]);
        let text_query: Box<dyn Query> =
            qp.parse_query(&args.query).context("parsing query")?;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];

        if let Some(since) = args.since {
            // tantivy 0.26's Term-based RangeQuery silently returns 0 hits on
            // Date fields (verified empirically: even an epoch-to-MAX range
            // matches nothing). Parsing the range through QueryParser against
            // the timestamp field works correctly — same primitive, different
            // construction path. See examples/since_debug.rs for the probe.
            let iso = since.to_rfc3339_opts(SecondsFormat::Secs, true);
            let qstr = format!("timestamp:[{} TO *]", iso);
            let range_query = qp
                .parse_query(&qstr)
                .context("parsing date range")?;
            clauses.push((Occur::Must, range_query));
        }

        if let Some(project) = &args.project {
            let term = Term::from_field_text(self.fields.project, project);
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        let combined = BooleanQuery::from(clauses);

        let limit = args.limit.unwrap_or(10).clamp(1, 100);
        let docs = searcher.search(&combined, &TopDocs::with_limit(limit).order_by_score())?;

        let mut hits = Vec::with_capacity(docs.len());
        for (score, addr) in docs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            hits.push(SearchHit::from_doc(&doc, &self.fields, score, &args.query));
        }
        Ok(hits)
    }

    /// True if any document is currently indexed. Useful for diagnostics.
    pub fn has_any_docs(&self) -> Result<bool> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        Ok(searcher.num_docs() > 0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchArgs {
    pub query: String,
    pub since: Option<DateTime<Utc>>,
    pub project: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub project: String,
    pub event_role: String,
    pub timestamp: Option<String>,
    pub file_path: String,
    pub event_index: u64,
    pub snippet: String,
    pub score: f32,
}

impl SearchHit {
    fn from_doc(doc: &TantivyDocument, fields: &Fields, score: f32, query: &str) -> Self {
        let text = first_text(doc, fields.text).unwrap_or_default();
        let snippet = make_snippet(&text, query, 140);
        let timestamp = first_date(doc, fields.timestamp).and_then(|d| {
            let secs = d.into_timestamp_secs();
            DateTime::<Utc>::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
        });
        Self {
            session_id: first_text(doc, fields.session_id).unwrap_or_default(),
            project: first_text(doc, fields.project).unwrap_or_default(),
            event_role: first_text(doc, fields.event_role).unwrap_or_default(),
            timestamp,
            file_path: first_text(doc, fields.file_path).unwrap_or_default(),
            event_index: first_u64(doc, fields.event_index).unwrap_or(0),
            snippet,
            score,
        }
    }
}

/// Parse a `since` argument that can be either an ISO date or a relative
/// shorthand like "3d", "2w", "5h".
pub fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Ok(d.with_timezone(&Utc));
    }
    if s.len() < 2 {
        anyhow::bail!("invalid --since value: {}", s);
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num
        .parse()
        .with_context(|| format!("invalid --since value: {}", s))?;
    let dur = match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => anyhow::bail!("invalid time unit in --since: {}", s),
    };
    Ok(Utc::now() - dur)
}

fn first_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    doc.get_first(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn first_u64(doc: &TantivyDocument, field: Field) -> Option<u64> {
    doc.get_first(field).and_then(|v| v.as_u64())
}

fn first_date(doc: &TantivyDocument, field: Field) -> Option<TantivyDateTime> {
    doc.get_first(field).and_then(|v| v.as_datetime())
}

/// Show a ~`max_chars` snippet of `text`, centered on the first occurrence
/// of any query term if possible.
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
