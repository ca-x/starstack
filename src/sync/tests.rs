use super::*;
use crate::test_http::{Reply, Server};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn star(id: i64, day: u8) -> serde_json::Value {
    json!({"starred_at": format!("2025-03-{day:02}T12:00:00Z"),"repo":{
        "id":id,"private":false,"full_name":format!("sample/r{id}"), "description":"a sample", "language":"Rust",
        "stargazers_count":2,"archived":false,"html_url":format!("https://github.com/sample/r{id}"),
        "pushed_at": "2025-01-01T00:00:00Z", "default_branch": "main",
        "license":{"spdx_id":"MIT","name":"MIT License"}
    }})
}

#[test]
fn synchronization_retries_keeps_cursor_on_failure_and_reconciles_removals() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let round = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let server = {
        let (round, reads, attempts) = (round.clone(), reads.clone(), attempts.clone());
        Server::new(move |request| {
            assert!(request.headers.contains("authorization: bearer test-only"));
            let round = round.load(Ordering::Relaxed);
            match request.target.as_str() {
                "/user" => Reply::json(json!({"id":if round == 5 { 42 } else { 1 }})),
                target if target.starts_with("/repos/") => {
                    reads.fetch_add(1, Ordering::Relaxed);
                    assert!(request.headers.contains("application/vnd.github.raw+json"));
                    Reply {
                        status: 200,
                        headers: "",
                        body: "A README".into(),
                    }
                }
                target if target.ends_with("page=1") => {
                    assert!(request.headers.contains("application/vnd.github.star+json"));
                    if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                        return Reply {
                            headers: "Retry-After: 0\r\n",
                            ..Reply::status(429)
                        };
                    }
                    let page = match round {
                        0 | 1 => json!([star(9, 9), star(8, 8)]),
                        2 => json!([star(10,10), {"bad":"record"}, star(9,9)]),
                        3 => json!([star(11, 10), star(10, 10), star(9, 9)]),
                        4 => json!([star(9, 9)]),
                        6 => json!([star(7, 12), star(9, 9)]),
                        _ => panic!("unexpected collection fetch on account mismatch"),
                    };
                    let mut response = Reply::json(page);
                    if round < 2 || round == 3 {
                        response.headers =
                            "Link: <https://example.invalid/page2>; rel=\"next\"\r\n";
                    }
                    response
                }
                target if target.ends_with("page=2") => match round {
                    0 => Reply::json(json!([star(7, 7)])),
                    3 => Reply::json(json!([star(12, 9), star(8, 8)])),
                    _ => panic!("incremental sync must stop after crossing cursor"),
                },
                other => panic!("unexpected URL {other}"),
            }
        })
    };
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        token: "test-only",
        base: &server.url,
    };
    let first = synchronize(&mut db, true, Mode::Readmes, &gh, &|_| {})?;
    assert_eq!(first.added, 3);
    let last_full = store::meta(&mut db, "last_full_sync")?;
    assert!(last_full.is_some());
    let repo = &store::repos(&mut db)?[0];
    assert_eq!(repo.readme, "A README");
    assert_eq!(repo.license.as_deref(), Some("MIT"));
    store::edit(
        &mut db,
        store::Edit {
            id: 7,
            note: Some("keep this note".into()),
            summary: Some("keep this summary".into()),
            ..Default::default()
        },
    )?;
    store::save_repository_code(&mut db, 7, "keep this snippet")?;
    round.store(1, Ordering::Relaxed);
    assert_eq!(
        synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {})?.skipped,
        2
    );
    assert_eq!(reads.load(Ordering::Relaxed), 3);
    let repo = store::by_id(&mut db, 7)?;
    assert_eq!(repo.summary, "keep this summary");
    assert_eq!(repo.code_context, "keep this snippet");
    let mut other = store::connect_as(temp.path(), "other")?;
    assert!(store::save_repository_code(&mut other, 7, "forbidden").is_err());
    let cursor = store::meta(&mut db, "star_cursor")?;
    round.store(2, Ordering::Relaxed);
    let partial = synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {})?;
    assert_eq!((partial.added, partial.failed), (1, 1));
    assert_eq!(store::meta(&mut db, "star_cursor")?, cursor);
    assert_eq!(store::meta(&mut db, "last_full_sync")?, last_full);
    assert!(partial.to_string().contains("starstack sync --full"));
    let saved: serde_json::Value =
        serde_json::from_str(&store::meta(&mut db, "last_sync_summary")?.unwrap())?;
    assert_eq!(saved["failure_examples"].as_array().unwrap().len(), 1);
    round.store(3, Ordering::Relaxed);
    assert_eq!(
        synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {})?.added,
        2
    );
    round.store(4, Ordering::Relaxed);
    assert_eq!(
        synchronize(&mut db, true, Mode::Readmes, &gh, &|_| {})?.removed,
        5
    );
    assert_eq!(store::repos(&mut db)?.len(), 1);
    round.store(5, Ordering::Relaxed);
    assert!(
        synchronize(&mut db, true, Mode::Readmes, &gh, &|_| {})
            .unwrap_err()
            .to_string()
            .contains("已绑定其他 GitHub 账户")
    );
    round.store(6, Ordering::Relaxed);
    assert_eq!(
        synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {})?.updated,
        1
    );
    assert_eq!(store::by_id(&mut db, 7)?.note, "keep this note");
    Ok(())
}

#[test]
fn exhausted_quota_persists_all_metadata_and_stops_readme_requests() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let server = Server::new(move |request| match request.target.as_str() {
        "/user" => Reply::json(json!({"id":1})),
        target if target.starts_with("/user/starred?") => {
            Reply::json(json!([star(3, 3), star(2, 2), star(1, 1)]))
        }
        "/repos/sample/r2/readme" => Reply {
            headers: "Retry-After: 60\r\n",
            ..Reply::status(429)
        },
        "/repos/sample/r3/readme" => {
            panic!("no further README requests while rate limited")
        }
        _ => Reply {
            status: 200,
            headers: "",
            body: "previous README".into(),
        },
    });
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        token: "test-only",
        base: &server.url,
    };
    let partial = synchronize(&mut db, true, Mode::Readmes, &gh, &|_| {})?;
    assert_eq!(
        (
            partial.added,
            partial.failed,
            partial.pending_readmes,
            partial.retry_after_seconds
        ),
        (3, 0, 2, Some(60))
    );
    assert!(partial.to_string().contains("60 秒"));
    assert!(store::meta(&mut db, "last_full_sync")?.is_some());
    assert!(store::meta(&mut db, "star_cursor")?.is_some());
    assert_eq!(store::repos(&mut db)?.len(), 3);
    assert_eq!(store::by_id(&mut db, 1)?.readme, "previous README");
    // The persisted cooldown prevents probing /user again immediately.
    assert!(Window::load(&mut db, Resource::Rest)?.check().is_err());
    assert!(synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {}).is_err());
    Ok(())
}

#[test]
fn failed_middle_page_commits_usable_pages_without_advancing_cursor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let old = serde_json::from_value::<Star>(star(8, 8))?.into_repo();
    store::upsert(&mut db, &old)?;
    let server = Server::new(|request| match request.target.as_str() {
        "/user" => Reply::json(json!({"id":1})),
        target if target.ends_with("page=1") => Reply {
            headers: "Link: <https://example.invalid/?page=2>; rel=\"next\", <https://example.invalid/?page=3>; rel=\"last\"\r\n",
            ..Reply::json(json!([star(3, 3)]))
        },
        target if target.ends_with("page=2") => Reply {
            headers: "Retry-After: 0\r\n",
            ..Reply::status(503)
        },
        target if target.ends_with("page=3") => Reply::json(json!([star(1, 1)])),
        target if target.ends_with("/readme") => Reply::status(404),
        other => panic!("unexpected request {other}"),
    });
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        token: "test-only",
        base: &server.url,
    };
    let partial = synchronize(&mut db, true, Mode::Readmes, &gh, &|_| {})?;
    assert_eq!((partial.added, partial.failed), (2, 1));
    assert!(partial.failure_examples[0].contains("第 2 页"));
    assert!(store::meta(&mut db, "last_full_sync")?.is_none());
    assert!(store::meta(&mut db, "star_cursor")?.is_none());
    assert!(store::meta(&mut db, "retry_full_sync")?.is_some());
    assert!(
        store::by_id(&mut db, 8).is_ok(),
        "partial membership cannot remove an existing star"
    );
    Ok(())
}

#[test]
fn catalog_reconciles_all_pages_then_readmes_resume_beyond_the_cursor() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let phase = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let phase = phase.clone();
        let requests = requests.clone();
        Server::new(move |request| {
            let phase = phase.load(Ordering::Relaxed);
            match request.target.as_str() {
                "/user" => Reply::json(json!({"id":1})),
                target if target.ends_with("page=1") => {
                    assert!(
                        phase < 3,
                        "standalone enrichment must not request the catalogue"
                    );
                    Reply {
                        headers: "Link: <https://example.invalid/?page=2>; rel=\"next\"\r\n",
                        ..Reply::json(json!([star(3, 3), star(2, 2)]))
                    }
                }
                target if target.ends_with("page=2") => {
                    assert!(matches!(phase, 0 | 2));
                    let mut item = star(1, 1);
                    if phase == 2 {
                        item["repo"]["pushed_at"] = json!("2025-04-10T00:00:00Z");
                        item["repo"]["description"] = json!("new metadata");
                    }
                    Reply::json(json!([item]))
                }
                target if target.ends_with("/readme") => {
                    assert!(matches!(phase, 1 | 3));
                    requests.fetch_add(1, Ordering::Relaxed);
                    if phase == 1 && target == "/repos/sample/r1/readme" {
                        return Reply::status(403);
                    }
                    Reply {
                        status: 200,
                        headers: "",
                        body: "updated text".into(),
                    }
                }
                other => panic!("unexpected {other}"),
            }
        })
    };
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        token: "test-only",
        base: &server.url,
    };
    let catalog = synchronize(&mut db, false, Mode::Catalog, &gh, &|_| {})?;
    assert_eq!(
        (catalog.added, catalog.pending_readmes, catalog.failed),
        (3, 3, 0)
    );
    assert_eq!(requests.load(Ordering::Relaxed), 0);
    store::edit(
        &mut db,
        store::Edit {
            id: 1,
            note: Some("private".into()),
            pinned: Some(true),
            ..Default::default()
        },
    )?;
    phase.store(1, Ordering::Relaxed);
    let partial = synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {})?;
    assert_eq!(
        (
            partial.readmes_fetched,
            partial.readme_failures,
            partial.pending_readmes
        ),
        (2, 1, 1)
    );
    // An old README stays accessible while a newer version is pending.
    let mut prior = store::by_id(&mut db, 1)?;
    prior.readme = "older snapshot".into();
    store::upsert(&mut db, &prior)?;
    phase.store(2, Ordering::Relaxed);
    let changed = synchronize(&mut db, true, Mode::Catalog, &gh, &|_| {})?;
    assert_eq!((changed.updated, changed.pending_readmes), (1, 1));
    let saved = store::by_id(&mut db, 1)?;
    assert_eq!(saved.readme, "older snapshot");
    assert_eq!(saved.description.as_deref(), Some("new metadata"));
    assert!(saved.pinned);
    assert_eq!(saved.note, "private");
    phase.store(3, Ordering::Relaxed);
    let resumed = synchronize(&mut db, false, Mode::PendingOnly, &gh, &|_| {})?;
    assert_eq!((resumed.readmes_fetched, resumed.pending_readmes), (1, 0));
    assert_eq!(store::by_id(&mut db, 1)?.readme, "updated text");
    assert_eq!(requests.load(Ordering::Relaxed), 4);
    Ok(())
}

#[test]
fn remaining_quota_is_reserved_for_membership_queries() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let headers = "X-RateLimit-Resource: core\r\nX-RateLimit-Limit: 5000\r\nX-RateLimit-Remaining: 1\r\nX-RateLimit-Reset: 4102444800\r\n";
    let server = Server::new(move |request| match request.target.as_str() {
        "/user" => Reply::json(json!({"id":1})),
        target if target.ends_with("page=1") => Reply {
            headers,
            ..Reply::json(json!([star(1, 1)]))
        },
        other => panic!("budget reserve should prevent {other}"),
    });
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        token: "test-only",
        base: &server.url,
    };
    let summary = synchronize(&mut db, false, Mode::Readmes, &gh, &|_| {})?;
    assert_eq!(
        (summary.added, summary.failed, summary.pending_readmes),
        (1, 0, 1)
    );
    assert!(summary.to_string().contains("预留"));
    let catalog = synchronize(&mut db, false, Mode::Catalog, &gh, &|_| {})?;
    assert_eq!((catalog.skipped, catalog.pending_readmes), (1, 1));
    Ok(())
}

#[test]
fn readme_budget_rotates_past_a_failing_repository_and_honors_private_visibility() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    QuotaPolicy {
        readme_budget: 1,
        ..Default::default()
    }
    .save(&mut db)?;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let server = {
        let calls = calls.clone();
        Server::new(move |r| match r.target.as_str() {
            "/user" => Reply::json(json!({"id":1})),
            target if target.ends_with("page=1") => {
                let mut first = star(1, 1);
                first["repo"]["private"] = json!(true);
                let mut second = star(2, 2);
                second["repo"].as_object_mut().unwrap().remove("private");
                Reply::json(json!([second, first]))
            }
            target if target.ends_with("/readme") => {
                calls.lock().unwrap().push(target.to_owned());
                if target.contains("/r1/") {
                    Reply::status(403)
                } else {
                    Reply::status(404)
                }
            }
            other => panic!("unexpected {other}"),
        })
    };
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        token: "test-only",
        base: &server.url,
    };
    synchronize(&mut db, true, Mode::Catalog, &gh, &|_| {})?;
    assert_eq!(store::by_id(&mut db, 1)?.private, Some(true));
    assert_eq!(store::by_id(&mut db, 2)?.private, None);
    // An explicitly selected local scraper must still use GitHub API for private or unclassified rows.
    store::set_meta(
        &mut db,
        "quota_policy",
        &serde_json::to_string(&QuotaPolicy {
            readme_source: ReadmeSource::Firecrawl,
            readme_budget: 1,
            ..Default::default()
        })?,
    )?;
    let first = synchronize(&mut db, false, Mode::PendingOnly, &gh, &|_| {})?;
    assert_eq!(
        (first.failed, first.readme_failures, first.pending_readmes),
        (0, 1, 2)
    );
    let second = synchronize(&mut db, false, Mode::PendingOnly, &gh, &|_| {})?;
    assert_eq!(
        (
            second.failed,
            second.readmes_fetched,
            second.pending_readmes
        ),
        (0, 1, 1)
    );
    assert_eq!(
        *calls.lock().unwrap(),
        ["/repos/sample/r1/readme", "/repos/sample/r2/readme"]
    );
    Ok(())
}
