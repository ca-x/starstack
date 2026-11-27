//! Opportunistic root README lookup during the first backfill.
use super::*;
use serde_json::{Value, json};
use std::fmt::Write;

const RESPONSE_LIMIT: u64 = 1024 * 1024;

pub(super) fn read(
    db: &mut Db,
    gh: &mut Requests<'_>,
    repos: &[Repo],
    enabled: &mut bool,
) -> Result<BTreeMap<i64, String>> {
    let mut found = BTreeMap::new();
    let mut window = Window::load(db, Resource::Graphql)?;
    if !window.has_headroom(gh.policy.reserve_percent)
        || !gh.quota.has_headroom(gh.policy.reserve_percent)
    {
        return Ok(found);
    }
    let candidates: Vec<_> = repos
        .iter()
        .filter(|repo| repo.private == Some(false))
        .filter_map(|repo| repo.full_name.split_once('/').map(|name| (repo.id, name)))
        .take(3)
        .collect();
    if candidates.len() < 2 || gh.budget_left < 2 {
        return Ok(found);
    }
    let mut fields = String::new();
    let mut declarations = Vec::new();
    let mut variables = serde_json::Map::new();
    for (i, (_, (owner, name))) in candidates.iter().enumerate() {
        declarations.push(format!("$o{i}:String!,$n{i}:String!"));
        variables.insert(format!("o{i}"), json!(owner));
        variables.insert(format!("n{i}"), json!(name));
        write!(
            fields,
            " r{i}:repository(owner:$o{i},name:$n{i}){{dot:object(expression:\"HEAD:.github\"){{... on Tree{{entries{{name}}}}}} object(expression:\"HEAD:README.md\"){{... on Blob{{byteSize isBinary isTruncated text}}}}}}"
        )?;
    }
    let payload = json!({"query":format!("query({}){{{fields}}}", declarations.join(",")), "variables":variables});
    let endpoint = crate::lists::endpoint(gh.gh.base)?;
    gh.budget_left -= 1;
    let Ok(response) = gh
        .gh
        .client
        .post(endpoint)
        .bearer_auth(gh.gh.token)
        .json(&payload)
        .send()
    else {
        *enabled = false;
        return Ok(found);
    };
    let status = response.status();
    let observed = window.observe(response.headers());
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let exhausted = response
        .headers()
        .get("x-ratelimit-remaining")
        .is_some_and(|v| v == "0");
    let mut bytes = Vec::new();
    let body: Option<Value> = if response
        .take(RESPONSE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes.len() as u64 <= RESPONSE_LIMIT
    {
        serde_json::from_slice(&bytes).ok()
    } else {
        None
    };
    let limited = |value: &Value| {
        value.get("type").and_then(Value::as_str) == Some("RATE_LIMITED")
            || value.pointer("/extensions/code").and_then(Value::as_str) == Some("RATE_LIMITED")
            || value
                .get("message")
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_ascii_lowercase().contains("rate limit"))
    };
    let reported = body.as_ref().is_some_and(|b| {
        limited(b)
            || b["errors"]
                .as_array()
                .is_some_and(|es| es.iter().any(limited))
    });
    if status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && (exhausted || retry_after.is_some()))
        || reported
    {
        let reset_delay = window
            .reset_at
            .filter(|_| exhausted)
            .map(|reset| reset.saturating_sub(Utc::now().timestamp()).max(1) as u64);
        let delay = retry_after.or(reset_delay).unwrap_or(60).max(1);
        window.defer(delay);
        if !exhausted || retry_after.is_some() {
            gh.quota.defer(delay);
            gh.quota.save(db, Resource::Rest)?;
        }
        *enabled = false;
    }
    if !observed
        || window
            .observed_headroom(gh.policy.reserve_percent)
            .is_none()
    {
        *enabled = false;
    }
    window.save(db, Resource::Graphql)?;
    let Some(data) = body
        .as_ref()
        .and_then(|b| b.get("data"))
        .and_then(Value::as_object)
        .filter(|_| status.is_success())
    else {
        *enabled = false;
        return Ok(found);
    };
    for (i, (id, _)) in candidates.into_iter().enumerate() {
        let alias = format!("r{i}");
        let errored = body
            .as_ref()
            .and_then(|b| b["errors"].as_array())
            .is_some_and(|es| {
                es.iter().any(|e| {
                    e.pointer("/path/0")
                        .and_then(Value::as_str)
                        .is_none_or(|path| path == alias)
                })
            });
        let Some(repo) = data.get(&alias) else {
            continue;
        };
        // GitHub gives README files in .github/ precedence over the root.
        let root_wins = repo.get("dot").is_some_and(|dot| {
            dot.is_null()
                || dot["entries"].as_array().is_some_and(|entries| {
                    entries.iter().all(|e| {
                        e["name"]
                            .as_str()
                            .is_some_and(|name| !name.to_ascii_lowercase().starts_with("readme"))
                    })
                })
        });
        if !errored
            && root_wins
            && let Some(blob) = repo.get("object")
            && blob["isBinary"] == false
            && blob["isTruncated"] == false
            && let Some(text) = blob["text"].as_str()
            && text.len() <= 64 * 1024
            && blob["byteSize"].as_u64() == Some(text.len() as u64)
        {
            found.insert(id, text.to_owned());
        }
    }
    Ok(found)
}
