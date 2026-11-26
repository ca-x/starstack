//! Exercise automatic organization through the server and real Aimux HTTP adapter.
#[path = "../src/test_http.rs"]
mod test_http;

use anyhow::{Context, Result, ensure};
use diesel::{
    connection::SimpleConnection,
    prelude::*,
    sql_types::{BigInt, Text},
};
use reqwest::{
    blocking::{Client, Response},
    header::{COOKIE, SET_COOKIE},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use test_http::{Reply, Server};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Api {
    client: Client,
    base: String,
    cookie: String,
}
impl Api {
    fn post(&self, route: &str, body: Value) -> Result<Response> {
        Ok(self
            .client
            .post(format!("{}/api/{route}", self.base))
            .header("X-StarStack", "1")
            .header(COOKIE, &self.cookie)
            .json(&body)
            .send()?)
    }
    fn ok(&self, route: &str, body: Value) -> Result<Value> {
        let r = self.post(route, body)?;
        let status = r.status();
        let body = r.text()?;
        ensure!(status.is_success(), "{route} {status} {body}");
        Ok(serde_json::from_str(&body)?)
    }
    fn get(&self, route: &str) -> Result<Value> {
        Ok(self
            .client
            .get(format!("{}/api/{route}", self.base))
            .header("X-StarStack", "1")
            .header(COOKIE, &self.cookie)
            .send()?
            .error_for_status()?
            .json()?)
    }
    fn wait_for(&self, expected: &str) -> Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let job = self.get("organize/job")?;
            if job["status"] == expected {
                return Ok(job);
            }
            ensure!(
                job["status"] != "failed" && Instant::now() < deadline,
                "expected {expected}, got {job}"
            );
            thread::sleep(Duration::from_millis(30));
        }
    }
}

fn seed(db: &mut SqliteConnection) -> Result<()> {
    for id in 1..=23i64 {
        let data=json!({"id":id,"full_name":format!("fixture/item{id}"),"html_url":format!("https://example.invalid/item{id}"),
            "description":"Synthetic test repository","language":"Rust","topics":["demo"],"readme":"PRIVATE-README-FIXTURE",
            "archived":false,"stargazers_count":1,"starred_at":"2024-01-01T00:00:00Z"}).to_string();
        diesel::sql_query("INSERT INTO repositories (owner_id,id,metadata,note) VALUES ('primary',?,?,'PRIVATE-NOTE-FIXTURE')")
            .bind::<BigInt,_>(id).bind::<Text,_>(data).execute(db)?;
    }
    db.batch_execute("INSERT INTO github_lists VALUES ('primary','L','Existing List','2020-01-01T00:00:00Z','2020-01-01T00:00:00Z',0)")?;
    Ok(())
}

#[test]
fn configured_chat_organizes_across_batches_pauses_and_rejects_invented_items() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let calls = Arc::new(Mutex::new(Vec::<Vec<i64>>::new()));
    let requests = calls.clone();
    let pause = Arc::new(AtomicBool::new(false));
    let pause_batch = pause.clone();
    let (entered, waiting) = mpsc::channel();
    let (release, unblock) = mpsc::channel();
    let chat = Server::new(move |request| {
        if request.target != "/v1/chat/completions" {
            return Reply::status(404);
        }
        assert!(!request.headers.contains("github-fixture"));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "organizer-fixture");
        let message = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "user")
            .unwrap();
        let text = message["content"].as_str().unwrap();
        assert!(!text.contains("PRIVATE-"));
        let input: Value = serde_json::from_str(text).unwrap();
        let ids: Vec<_> = input["repositories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        assert!(!ids.is_empty() && ids.len() <= 20);
        requests.lock().unwrap().push(ids.clone());
        if ids.first() == Some(&21) && pause_batch.swap(false, Ordering::SeqCst) {
            entered.send(()).unwrap();
            unblock.recv_timeout(Duration::from_secs(20)).unwrap();
        }
        let tag = if input["target"] == "github_lists" {
            "Existing List"
        } else {
            "Test tag"
        };
        let mut answers: Vec<_> = ids
            .iter()
            .map(|id| json!({"id":id,"tags":[tag,tag],"reason":"fixture"}))
            .collect();
        if input["target"] == "github_lists" {
            answers[0]["tags"] = json!(["Made-up list", tag, tag]);
        }
        answers.push(json!({"id":900001,"tags":[tag],"reason":"unknown id"}));
        Reply::json(
            json!({"id":"fixture","object":"chat.completion","created":1,"model":"organizer-fixture",
            "choices":[{"index":0,"message":{"role":"assistant","content":serde_json::to_string(&answers).unwrap()},"finish_reason":"stop"}]}),
        )
    });
    let mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writes = mutations.clone();
    let github = Server::new(move |request| {
        assert_eq!(request.target, "/graphql");
        assert!(
            request
                .headers
                .contains("authorization: bearer github-fixture")
        );
        let b: Value = serde_json::from_slice(&request.body).unwrap();
        let q = b["query"].as_str().unwrap();
        let assigned = writes.load(Ordering::Relaxed) > 0;
        let stamp = "2020-01-01T00:00:00Z";
        let node = |n| json!({"id":"L","name":"Existing List","updatedAt":stamp,"lastAddedAt":stamp,"items":{"totalCount":n}});
        let data = if q.contains("StarStackListRepository") {
            json!({"viewer":{"databaseId":123},"repository":{"id":"R","fullDatabaseId":"1","viewerHasStarred":true}})
        } else if q.contains("StarStackLists(") {
            json!({"viewer":{"databaseId":123,"lists":{"totalCount":1,"nodes":[node(usize::from(assigned))],"pageInfo":{"hasNextPage":false,"endCursor":null}}}})
        } else if q.contains("StarStackListItems(") {
            json!({"nodes":[{"id":"L","updatedAt":stamp,"lastAddedAt":stamp,"items":{"totalCount":1,"nodes":[{"fullDatabaseId":"1"}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}]})
        } else {
            assert!(q.contains("mutation StarStackAppendLists"));
            assert_eq!(
                b["variables"]["input"],
                json!({"itemId":"R","listIds":["L"]})
            );
            writes.fetch_add(1, Ordering::Relaxed);
            json!({"updateUserListsForItem":{"item":{"id":"R"},"user":{"databaseId":123},"lists":[node(1)]}})
        };
        Reply::json(json!({"data":data}))
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_starstack"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("STARSTACK_") {
            cmd.env_remove(key);
        }
    }
    let mut server = Process(
        cmd.env("STARSTACK_HOME", dir.path())
            .env("STARSTACK_ADMIN_USER", "admin")
            .env("STARSTACK_ADMIN_PASSWORD", "local-test-only")
            .env("STARSTACK_SYNC_INTERVAL_MINUTES", "0")
            .env("STARSTACK_OLLAMA_URL", &chat.url)
            .env("STARSTACK_GITHUB_API_URL", &github.url)
            .env("GITHUB_TOKEN", "github-fixture")
            .env_remove("GH_TOKEN")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut startup = String::new();
    BufReader::new(server.0.stderr.take().context("server stderr")?).read_line(&mut startup)?;
    let base = startup
        .split_whitespace()
        .last()
        .and_then(|s| s.strip_suffix("/login"))
        .context("server URL")?
        .to_owned();
    let mut api = Api {
        client: Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()?,
        base,
        cookie: String::new(),
    };
    let login = api
        .post(
            "login",
            json!({"username":"admin","password":"local-test-only"}),
        )?
        .error_for_status()?;
    api.cookie = login.headers()[SET_COOKIE]
        .to_str()?
        .split(';')
        .next()
        .context("session cookie")?
        .to_owned();
    let mut db =
        SqliteConnection::establish(dir.path().join("starstack.sqlite3").to_str().unwrap())?;
    seed(&mut db)?;

    assert_eq!(api.get("chat")?["configured"], false);
    let missing = api.post("organize/preview", json!({"target":"tags","limit":1}))?;
    assert_eq!(missing.status(), 400);
    assert!(missing.text()?.contains("尚未配置 AI 聊天"));
    assert!(calls.lock().unwrap().is_empty());
    api.ok("chat",json!({"provider":"ollama","model":"organizer-fixture","base_url":format!("{}/v1",chat.url)}))?;
    let preview = api.ok(
        "organize/preview",
        json!({"target":"github_lists","limit":1}),
    )?;
    let candidates = preview["suggestions"].as_array().context("preview rows")?;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["id"], 1);
    assert_eq!(candidates[0]["tags"], json!(["Existing List"]));
    assert_eq!(preview["next_offset"], 1);
    assert_eq!(mutations.load(Ordering::Relaxed), 0);
    let lists = json!({"target":"github_lists","items":[{"id":1,"tags":["Existing List"]}]});
    assert_eq!(api.ok("organize/apply", lists.clone())?["added"], 1);
    assert_eq!(api.ok("organize/apply", lists)?["added"], 0);
    assert_eq!(mutations.load(Ordering::Relaxed), 1);

    let job = json!({"target":"tags","confirmed":true,"filter":{}});
    assert_eq!(
        api.post("organize/start", json!({"target":"tags","confirmed":false}))?
            .status(),
        400
    );
    pause.store(true, Ordering::SeqCst);
    api.ok("organize/start", job.clone())?;
    waiting.recv_timeout(Duration::from_secs(15))?;
    let progress = api.get("organize/job")?;
    assert_eq!(progress["processed"], 20);
    api.ok("organize/pause", json!({}))?;
    release.send(())?;
    let paused = api.wait_for("paused")?;
    assert_eq!(paused["processed"], 20);
    api.ok("organize/resume", json!({}))?;
    let done = api.wait_for("done")?;
    assert_eq!(
        (
            done["total"].as_u64(),
            done["processed"].as_u64(),
            done["added"].as_u64()
        ),
        (Some(23), Some(23), Some(23))
    );
    api.ok("organize/start", job)?;
    assert_eq!(api.wait_for("done")?["added"], 0);
    assert_eq!(
        api.ok(
            "organize/apply",
            json!({"target":"tags","items":[{"id":1,"tags":["Test tag"]}]})
        )?["added"],
        0
    );
    assert_eq!(api.post("organize/apply",json!({"target":"tags","items":[{"id":1,"tags":["should rollback"]},{"id":900001,"tags":["x"]}]}))?.status(),400);
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type=Text)]
        tags: String,
    }
    let rows = diesel::sql_query("SELECT tags FROM repositories WHERE owner_id='primary'")
        .load::<Row>(&mut db)?;
    assert_eq!(rows.len(), 23);
    assert!(rows.iter().all(|r| r.tags == "[\"Test tag\"]"));
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(Vec::len).collect::<Vec<_>>(),
        [1, 20, 3, 3, 20, 3]
    );
    Ok(())
}
