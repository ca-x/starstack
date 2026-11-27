//! Evaluate ranked repository retrieval against labeled JSON Lines queries.
use crate::{library, store::Db};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    time::Instant,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    query: String,
    expect: Vec<String>,
}

fn score<'a>(ranked: impl Iterator<Item = &'a str>, expected: &HashSet<String>) -> (f64, f64) {
    let mut hits = 0;
    let mut reciprocal = 0.0;
    for (rank, name) in ranked.take(100).enumerate() {
        if expected.contains(&name.to_ascii_lowercase()) {
            hits += usize::from(rank < 5);
            if reciprocal == 0.0 {
                reciprocal = 1.0 / (rank + 1) as f64;
            }
        }
    }
    (hits as f64 / expected.len() as f64, reciprocal)
}

pub(crate) fn run(db: &mut Db, path: &Path) -> Result<()> {
    let reader = BufReader::new(File::open(path).context("无法打开评测问题文件")?);
    let mut cases = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let case: Case =
            serde_json::from_str(&line).with_context(|| format!("评测文件第 {} 行", i + 1))?;
        let query = library::Query {
            query: case.query,
            filter: Default::default(),
            limit: Some(100),
        };
        query
            .validate()
            .with_context(|| format!("评测文件第 {} 行", i + 1))?;
        let expected = case
            .expect
            .into_iter()
            .map(|name| name.trim().to_ascii_lowercase())
            .collect::<HashSet<_>>();
        ensure!(
            !expected.is_empty()
                && expected.iter().all(|name| {
                    name.split_once('/').is_some_and(|(owner, repo)| {
                        !owner.is_empty() && !repo.is_empty() && !repo.contains('/')
                    })
                }),
            "评测文件第 {} 行需要至少一个 owner/repo",
            i + 1
        );
        cases.push((query, expected));
    }
    ensure!(!cases.is_empty(), "评测文件没有问题");
    let mut recall = 0.0;
    let mut reciprocal = 0.0;
    let mut latencies = Vec::new();
    for (query, expected) in &cases {
        let started = Instant::now();
        let rows = library::search_published(db, query)
            .with_context(|| format!("评测检索失败（{}）", query.query))?;
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        let (hits, rank) = score(rows.iter().map(|r| r.full_name.as_str()), expected);
        recall += hits;
        reciprocal += rank;
    }
    latencies.sort_by(f64::total_cmp);
    let count = cases.len();
    println!(
        "{}",
        serde_json::json!({
            "queries": count, "recall_at_5": recall / count as f64, "mrr_at_100": reciprocal / count as f64,
            "latency_p50_ms": latencies[count.div_ceil(2) - 1],
            "latency_p95_ms": latencies[(count * 95).div_ceil(100) - 1]
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_all_expected_repositories_and_caps_reciprocal_rank() {
        let expected = HashSet::from(["owner/one".to_owned(), "owner/two".to_owned()]);
        let ranked = ["a/a", "OWNER/ONE", "b/b", "c/c", "d/d", "owner/two"];
        assert_eq!(score(ranked.into_iter(), &expected), (0.5, 0.5));
        let late = std::iter::repeat_n("miss/miss", 100).chain(["owner/one"]);
        assert_eq!(score(late, &expected), (0.0, 0.0));
    }
}
