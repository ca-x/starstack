use super::*;
use crate::test_http::{Reply, Server};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn info(cursor: Option<&str>) -> Value {
    json!({"hasNextPage":cursor.is_some(),"endCursor":cursor})
}

fn list(id: &str, name: &str, version: &str, n: usize) -> Value {
    json!({"id":id,"name":name,"updatedAt":version,"lastAddedAt":version,"items":{"totalCount":n}})
}

fn items(id: &str, version: &str, count: usize, ids: &[i64], cursor: Option<&str>) -> Value {
    json!({"id":id,"updatedAt":version,"lastAddedAt":version,"items":{
        "totalCount":count,"nodes":ids.iter().map(|&id|json!({"fullDatabaseId":id.to_string()})).collect::<Vec<_>>(),
        "pageInfo":info(cursor)}})
}

#[test]
fn refresh_stages_pages_preserves_previous_members_and_reuses_unchanged_lists() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    let phase = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let service = {
        let (phase, calls, requests) = (phase.clone(), calls.clone(), requests.clone());
        Server::new(move |request| {
            assert_eq!(request.target, "/graphql");
            requests.fetch_add(1, Ordering::Relaxed);
            let phase = phase.load(Ordering::Relaxed);
            let request_body: Value = serde_json::from_slice(&request.body).unwrap();
            let query = request_body["query"].as_str().unwrap();
            let vars = &request_body["variables"];
            let other = request.headers.contains("authorization: bearer secondary");
            if phase == 3 && !other && query.contains("StarStackListItems(") {
                return Reply::json(
                    json!({"data":{"nodes":[]},"errors":[{"type":"FORBIDDEN","message":"access denied"}]}),
                );
            }
            if phase == 4 {
                return Reply {
                    headers: "Retry-After: 120\r\n",
                    ..Reply::json(json!({"errors":[{"type":"RATE_LIMITED","message":"quota"}]}))
                };
            }
            let value = if query.contains("StarStackLists(") {
                let (total, nodes, next) = if other {
                    (1, vec![list("L", "Tools", "secondary", 1)], None)
                } else if vars["after"] == "more-lists" {
                    (
                        2,
                        vec![list(if phase < 2 { "E" } else { "F" }, "Empty", "v1", 0)],
                        None,
                    )
                } else {
                    (
                        2,
                        vec![list(
                            "L",
                            if phase < 2 { "Tools" } else { "Research" },
                            if phase < 2 {
                                "v1"
                            } else if phase == 2 {
                                "v2"
                            } else {
                                "v3"
                            },
                            2,
                        )],
                        Some("more-lists"),
                    )
                };
                json!({"viewer":{"databaseId":if other{202}else{101},"lists":{"totalCount":total,"nodes":nodes,"pageInfo":info(next)}}})
            } else if query.contains("StarStackListItems(") {
                calls.fetch_add(1, Ordering::Relaxed);
                assert_eq!(vars["ids"], json!(["L"]));
                let node = if other {
                    items("L", "secondary", 1, &[77], None)
                } else if phase < 2 {
                    items("L", "v1", 2, &[5], Some("more-items"))
                } else {
                    items("L", "v2", 2, &[9, 42], None)
                };
                json!({"nodes":[node]})
            } else {
                assert!(query.contains("StarStackMoreListItems("));
                assert_eq!(vars["after"], "more-items");
                json!({"node":items("L","v1",2,&[9],None)})
            };
            Reply::json(json!({"data":value}))
        })
    };
    let client = Client::builder().no_proxy().build()?;
    let gh = GitHub {
        client: client.clone(),
        url: endpoint(&service.url)?,
        token: "primary",
        legacy_ids: Default::default(),
    };
    let update = |db: &mut Db| refresh_with(db, &service.url, &gh, &|_| {});
    update(&mut db)?;
    assert_eq!(
        counts(&mut db)?,
        [("Empty".into(), 0), ("Tools".into(), 2)]
            .into_iter()
            .collect()
    );
    assert_eq!(members(&mut db, "tools")?, [5, 9].into_iter().collect());
    assert_eq!(for_repo(&mut db, 5)?, ["Tools"]);
    let rev = store::meta(&mut db, "lists_revision")?;
    phase.store(1, Ordering::Relaxed);
    update(&mut db)?;
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(rev, store::meta(&mut db, "lists_revision")?);
    phase.store(2, Ordering::Relaxed);
    update(&mut db)?;
    assert!(members(&mut db, "Tools")?.is_empty());
    assert!(for_repo(&mut db, 5)?.is_empty());
    assert_eq!(members(&mut db, "Research")?, [9, 42].into_iter().collect());

    let mut second = store::connect_as(dir.path(), "secondary")?;
    let other = GitHub {
        client,
        url: gh.url.clone(),
        token: "secondary",
        legacy_ids: Default::default(),
    };
    refresh_with(&mut second, &service.url, &other, &|_| {})?;
    assert_eq!(members(&mut second, "Tools")?, [77].into_iter().collect());
    assert!(for_repo(&mut db, 77)?.is_empty());

    phase.store(3, Ordering::Relaxed);
    let before = store::meta(&mut db, "lists_revision")?;
    assert!(update(&mut db).is_err());
    assert_eq!(store::meta(&mut db, "lists_revision")?, before);
    assert_eq!(members(&mut db, "Research")?, [9, 42].into_iter().collect());
    phase.store(4, Ordering::Relaxed);
    assert!(update(&mut db).is_err());
    let exhausted = requests.load(Ordering::Relaxed);
    assert!(update(&mut db).is_err());
    assert_eq!(exhausted, requests.load(Ordering::Relaxed));
    assert_eq!(members(&mut db, "Research")?, [9, 42].into_iter().collect());
    Ok(())
}

#[test]
fn full_database_id_preserves_large_ids_and_prefers_the_64_bit_field() -> Result<()> {
    let large = 5_632_000_123_i64;
    let numeric = serde_json::from_value::<Item>(json!({"fullDatabaseId":large,"databaseId":17}))?;
    let string = serde_json::from_value::<Item>(
        json!({"fullDatabaseId":large.to_string(),"databaseId":null}),
    )?;
    assert_eq!(numeric.id()?, large);
    assert_eq!(string.id()?, large);
    assert_eq!(
        serde_json::from_value::<Item>(json!({"databaseId":17}))?.id()?,
        17
    );
    Ok(())
}

#[test]
fn graphql_never_changes_the_configured_rest_origin() -> Result<()> {
    assert_eq!(
        endpoint("https://api.github.com")?.as_str(),
        "https://api.github.com/graphql"
    );
    assert_eq!(
        endpoint("https://example.test/api/v3/")?.as_str(),
        "https://example.test/api/graphql"
    );
    assert_eq!(
        endpoint("http://127.0.0.1:8000/mock")?.as_str(),
        "http://127.0.0.1:8000/mock/graphql"
    );
    assert!(endpoint("https://user@example.test/api").is_err());
    Ok(())
}

#[test]
fn same_second_membership_swap_is_rechecked_without_rewriting_an_unchanged_list() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    let row = Arc::new(AtomicUsize::new(11));
    let member = row.clone();
    let stamp = "2025-01-01T00:00:00Z";
    let server = Server::new(move |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["query"].as_str().unwrap().contains("StarStackLists(") {
            Reply::json(json!({"data":{"viewer":{"databaseId":1,"lists":{
                "totalCount":1,"nodes":[list("L","Tools",stamp,1)],"pageInfo":info(None)}}}}))
        } else {
            Reply::json(
                json!({"data":{"nodes":[items("L",stamp,1,&[member.load(Ordering::Relaxed) as i64],None)]}}),
            )
        }
    });
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        url: endpoint(&server.url)?,
        token: "fixture",
        legacy_ids: Default::default(),
    };
    let refresh = |db: &mut Db| refresh_with(db, &server.url, &gh, &|_| {});
    refresh(&mut db)?;
    assert_eq!(members(&mut db, "Tools")?, [11].into_iter().collect());
    store::set_meta(&mut db, "lists_observed_at", "1735689600")?;
    row.store(22, Ordering::Relaxed);
    refresh(&mut db)?;
    assert_eq!(members(&mut db, "Tools")?, [22].into_iter().collect());
    let revision = store::meta(&mut db, "lists_revision")?;
    store::set_meta(&mut db, "lists_observed_at", "1735689600")?;
    refresh(&mut db)?;
    assert_eq!(revision, store::meta(&mut db, "lists_revision")?);
    Ok(())
}

#[test]
fn first_viewer_page_carries_quota_to_the_bound_github_account() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    let requests = Arc::new(AtomicUsize::new(0));
    let calls = requests.clone();
    let server = Server::new(move |request| {
        calls.fetch_add(1, Ordering::Relaxed);
        let b: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(b["query"].as_str().unwrap().contains("StarStackLists("));
        Reply {
            headers: "X-RateLimit-Resource: graphql\r\nX-RateLimit-Limit: 5000\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 4102444800\r\n",
            ..Reply::json(
                json!({"data":{"viewer":{"databaseId":88,"lists":{"totalCount":1,
                "nodes":[list("L","Tools","v1",1)],"pageInfo":info(None)}}}}),
            )
        }
    });
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        url: endpoint(&server.url)?,
        token: "fixture",
        legacy_ids: Default::default(),
    };
    for _ in 0..2 {
        assert!(refresh_with(&mut db, &server.url, &gh, &|_| {}).is_err());
    }
    assert_eq!(requests.load(Ordering::Relaxed), 1);
    assert_eq!(Window::load(&mut db, Resource::Graphql)?.remaining, Some(0));
    assert!(counts(&mut db)?.is_empty());
    Ok(())
}

#[test]
fn gateway_failures_retry_but_graphql_permissions_do_not() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    let requests = Arc::new(AtomicUsize::new(0));
    let calls = requests.clone();
    let server = Server::new(move |_| match calls.fetch_add(1, Ordering::Relaxed) {
        0 => Reply::status(502),
        1 => Reply::status(504),
        2 => Reply::json(
            json!({"data":{"viewer":{"databaseId":109,"lists":{"totalCount":0,"nodes":[],"pageInfo":info(None)}}}}),
        ),
        _ => Reply::json(json!({"errors":[{"type":"FORBIDDEN","message":"not authorized"}]})),
    });
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        url: endpoint(&server.url)?,
        token: "fixture",
        legacy_ids: Default::default(),
    };
    refresh_with(&mut db, &server.url, &gh, &|_| {})?;
    assert_eq!(requests.load(Ordering::Relaxed), 3);
    assert!(refresh_with(&mut db, &server.url, &gh, &|_| {}).is_err());
    assert_eq!(requests.load(Ordering::Relaxed), 4);
    Ok(())
}
