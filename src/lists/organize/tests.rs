use super::*;
use crate::test_http::{Reply, Server};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    assigned: bool,
    fail: Option<u16>,
    star: bool,
    writes: usize,
    old_first_pages: usize,
    old_last_pages: usize,
    calls: usize,
}

fn server(state: Arc<Mutex<State>>) -> Server {
    Server::new(move |request| {
        let mut s = state.lock().unwrap();
        s.calls += 1;
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let q = body["query"].as_str().unwrap();
        let v = &body["variables"];
        let info = |more: bool| json!({"hasNextPage":more,"endCursor":more.then_some("next")});
        let stamp = "2020-01-01T00:00:00Z";
        let listing = |id: &str, name: &str, n: usize| json!({"id":id,"name":name,"updatedAt":stamp,"lastAddedAt":stamp,"items":{"totalCount":n}});
        let itempage = |id: &str, n: usize, rows: &[i64], more: bool| {
            json!({"id":id,"updatedAt":stamp,"lastAddedAt":stamp,"items":{
            "totalCount":n,"nodes":rows.iter().map(|v|json!({"fullDatabaseId":v.to_string()})).collect::<Vec<_>>(),"pageInfo":info(more)}})
        };
        let data = if q.contains("StarStackListRepository") {
            assert_eq!(v, &json!({"owner":"fixture","name":"sample"}));
            json!({"viewer":{"databaseId":123},"repository":{"id":"R","fullDatabaseId":"987","viewerHasStarred":s.star}})
        } else if q.contains("StarStackLists(") {
            let more = v["after"].is_null();
            let nodes = if more {
                vec![listing("X", "Original", 2)]
            } else {
                vec![
                    listing("Y", "Target", s.assigned as usize),
                    listing("Z", "Empty", 0),
                ]
            };
            json!({"viewer":{"databaseId":123,"lists":{"totalCount":3,"pageInfo":info(more),"nodes":nodes}}})
        } else if q.contains("StarStackListItems(") {
            let mut nodes = Vec::new();
            for id in v["ids"].as_array().unwrap() {
                match id.as_str().unwrap() {
                    "X" => {
                        s.old_first_pages += 1;
                        nodes.push(itempage("X", 2, &[7], true));
                    }
                    "Y" if s.assigned => nodes.push(itempage("Y", 1, &[987], false)),
                    _ => panic!("unexpected list id"),
                }
            }
            json!({"nodes":nodes})
        } else if q.contains("StarStackMoreListItems(") {
            assert_eq!(v, &json!({"id":"X","after":"next"}));
            s.old_last_pages += 1;
            json!({"node":itempage("X",2,&[987],false)})
        } else {
            assert!(q.starts_with("mutation StarStackAppendLists"));
            s.writes += 1;
            assert_eq!(v, &json!({"input":{"itemId":"R","listIds":["X","Y"]}}));
            if let Some(code) = s.fail {
                return if code == 429 {
                    Reply {
                        headers: "Retry-After: 60\r\n",
                        ..Reply::status(429)
                    }
                } else if code == 403 {
                    Reply::json(
                        json!({"errors":[{"type":"FORBIDDEN","message":"insufficient scope"}]}),
                    )
                } else {
                    Reply::status(code)
                };
            }
            s.assigned = true;
            json!({"updateUserListsForItem":{"item":{"id":"R"},"user":{"databaseId":123},"lists":[listing("X","Original",2),listing("Y","Target",1)]}})
        };
        Reply::json(json!({"data":data}))
    })
}

fn fixture(db: &mut Db) -> Result<()> {
    let repo = serde_json::from_value(
        json!({"id":987,"full_name":"fixture/sample","topics":[],"stargazers_count":4,"starred_at":"2020-01-01T00:00:00Z","html_url":"https://github.com/fixture/sample","archived":false}),
    )?;
    store::upsert(db, &repo)?;
    Ok(())
}

#[test]
fn appending_preserves_live_memberships_and_reuses_complete_unchanged_lists() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    fixture(&mut db)?;
    let state = Arc::new(Mutex::new(State {
        star: true,
        ..Default::default()
    }));
    let server = server(state.clone());
    let gh = GitHub {
        client: Client::builder().no_proxy().build()?,
        url: endpoint(&server.url)?,
        token: "test",
        legacy_ids: Default::default(),
    };
    let add = |db: &mut Db, names: &[&str]| {
        append_with(
            db,
            &server.url,
            &gh,
            987,
            &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    };
    assert_eq!(add(&mut db, &["Target", "Target"])?, 1);
    assert_eq!(for_repo(&mut db, 987)?, ["Original", "Target"]);
    assert_eq!(counts(&mut db)?["Target"], 1);
    assert_eq!(add(&mut db, &["Target"])?, 0);
    assert!(add(&mut db, &["New list"]).is_err());
    state.lock().unwrap().star = false;
    assert!(add(&mut db, &["Empty"]).is_err());
    let s = state.lock().unwrap();
    assert_eq!(s.writes, 1);
    assert_eq!((s.old_first_pages, s.old_last_pages), (1, 1));
    Ok(())
}

#[test]
fn failed_or_limited_mutation_is_never_replayed_and_keeps_local_membership() -> Result<()> {
    for status in [403, 429, 502] {
        let home = tempfile::tempdir()?;
        let mut db = store::connect(home.path())?;
        fixture(&mut db)?;
        let state = Arc::new(Mutex::new(State {
            star: true,
            fail: Some(status),
            ..Default::default()
        }));
        let server = server(state.clone());
        let gh = GitHub {
            client: Client::builder().no_proxy().build()?,
            url: endpoint(&server.url)?,
            token: "test",
            legacy_ids: Default::default(),
        };
        let add = |db: &mut Db| append_with(db, &server.url, &gh, 987, &["Target".to_owned()]);
        assert!(add(&mut db).is_err());
        assert_eq!(state.lock().unwrap().writes, 1);
        assert_eq!(for_repo(&mut db, 987)?, ["Original"]);
        assert!(dirty_lists(&mut db)?.contains("Y"));
        if status == 429 {
            let calls = state.lock().unwrap().calls;
            assert!(add(&mut db).is_err());
            assert_eq!(state.lock().unwrap().calls, calls);
        }
    }
    Ok(())
}
