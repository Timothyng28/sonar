// Standalone test — opens the real index and runs a date-range query four
// different ways, prints how many hits each returns. Whichever returns
// nonzero is the construction we should use in src/index.rs.

use anyhow::Result;
use chrono::Utc;
use sonar::index::{default_index_path, open_or_create_index};
use std::ops::Bound;
use tantivy::collector::Count;
use tantivy::query::{Query, QueryParser, RangeQuery};
use tantivy::{DateTime as TantivyDateTime, ReloadPolicy, Term};

fn main() -> Result<()> {
    let p = default_index_path()?;
    let (index, fields) = open_or_create_index(&p)?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let searcher = reader.searcher();
    println!("total docs in index: {}", searcher.num_docs());

    let now = Utc::now();
    for label in ["1d", "7d", "epoch"] {
        let lo_micros = match label {
            "1d" => now.timestamp_micros() - 86_400_000_000,
            "7d" => now.timestamp_micros() - 7 * 86_400_000_000,
            _ => 0,
        };
        let lo = TantivyDateTime::from_timestamp_micros(lo_micros);
        let hi = TantivyDateTime::from_timestamp_micros(i64::MAX / 2);

        for (variant_name, lo_term, hi_term) in [
            (
                "from_field_date_for_search",
                Term::from_field_date_for_search(fields.timestamp, lo),
                Term::from_field_date_for_search(fields.timestamp, hi),
            ),
            (
                "from_field_date (no truncate)",
                Term::from_field_date(fields.timestamp, lo),
                Term::from_field_date(fields.timestamp, hi),
            ),
        ] {
            let q = RangeQuery::new(Bound::Included(lo_term), Bound::Excluded(hi_term));
            let count = searcher.search(&q, &Count)?;
            println!("  [{}] {:>32}: {} hits", label, variant_name, count);
        }

        let qp = QueryParser::for_index(&index, vec![fields.text]);
        let dt_iso = chrono::DateTime::<Utc>::from_timestamp_micros(lo_micros)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let qstr = format!("timestamp:[{} TO *]", dt_iso);
        match qp.parse_query(&qstr) {
            Ok(q) => {
                let count = searcher.search(&q, &Count)?;
                println!(
                    "  [{}] {:>32}: {} hits  (qstr: {})",
                    label, "QueryParser", count, qstr
                );
            }
            Err(e) => println!("  [{}] QueryParser parse error: {}", label, e),
        }
        println!();
    }

    let qp = QueryParser::for_index(&index, vec![fields.text]);
    let q: Box<dyn Query> = qp.parse_query("sonar install")?;
    let n = searcher.search(&q, &Count)?;
    println!("sanity: 'sonar install' text-only matches: {} docs", n);

    Ok(())
}
