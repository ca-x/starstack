//! Exercise the shipped binary and both official MCP SDK transports.
use anyhow::{Context, Result, ensure};
use reqwest::{
    blocking::Client,
    header::{ACCEPT, COOKIE, SET_COOKIE},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_starstack"));
    cmd.env("STARSTACK_HOME", dir)
        .env("STARSTACK_SYNC_INTERVAL_MINUTES", "0")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("STARSTACK_AI_PROVIDER")
        .env_remove("STARSTACK_MODEL")
        .env_remove("STARSTACK_OLLAMA_URL")
        .env_remove("STARSTACK_OLLAMA_DEFAULT_URL")
        .env_remove("STARSTACK_MCP_HOSTS");
    cmd
}
fn initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"starstack-test","version":"1"}}})
}
#[test]
fn stdio_tools_discover_and_read_the_local_library() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut server = Process(
        command(dir.path())
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut input = server.0.stdin.take().context("missing stdin")?;
    let mut output = BufReader::new(server.0.stdout.take().context("missing stdout")?);
    let mut read = || -> Result<Value> {
        let mut line = String::new();
        output.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    };
    writeln!(input, "{}", initialize())?;
    ensure!(
        read()?["result"]["capabilities"]["tools"].is_object(),
        "missing tool capability"
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )?;
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
    )?;
    let catalog = read()?;
    let tools = catalog["result"]["tools"]
        .as_array()
        .context("missing tools")?;
    assert_eq!(tools.len(), 5);
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_stats","arguments":{}}})
    )?;
    assert_eq!(read()?["result"]["structuredContent"]["total"], 0);
    Ok(())
}
#[test]
fn http_accounts_confirmation_and_revocation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut server = Process(
        command(dir.path())
            .env("STARSTACK_ADMIN_USER", "admin")
            .env("STARSTACK_ADMIN_PASSWORD", "fixture-admin-123")
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut startup = String::new();
    BufReader::new(server.0.stderr.take().context("no startup message")?)
        .read_line(&mut startup)?;
    let base = startup
        .split_whitespace()
        .last()
        .and_then(|s| s.strip_suffix("/login"))
        .context("server address")?;
    let c = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let post = |path: &str, cookie: &str, v: Value| {
        c.post(format!("{base}/api/{path}"))
            .header("X-StarStack", "1")
            .header(COOKIE, cookie)
            .json(&v)
            .send()
    };
    let login = |user: &str, password: &str| -> Result<String> {
        let res =
            post("login", "", json!({"username":user,"password":password}))?.error_for_status()?;
        Ok(res.headers()[SET_COOKIE]
            .to_str()?
            .split(';')
            .next()
            .context("missing cookie")?
            .to_string())
    };
    let admin = login("admin", "fixture-admin-123")?;
    post(
        "users",
        &admin,
        json!({"username":"reader","password":"fixture-reader-123"}),
    )?
    .error_for_status()?;
    let reader = login("reader", "fixture-reader-123")?;
    {
        use diesel::{
            Connection, RunQueryDsl, SqliteConnection,
            sql_types::{BigInt, Text},
        };
        let database = dir.path().join("starstack.sqlite3");
        let mut db = SqliteConnection::establish(database.to_str().unwrap())?;
        for id in [1_i64, 2, 3] {
            let metadata = json!({"id":id, "full_name":format!("sample/repo{id}"),
                "description":"pagination fixture","language":"Rust","topics":[],
                "stargazers_count":1,"starred_at":"2025-01-01","html_url":"https://github.com/sample",
                "archived":false});
            diesel::sql_query(
                "INSERT INTO repositories(owner_id,id,metadata) VALUES ('primary',?,?)",
            )
            .bind::<BigInt, _>(id)
            .bind::<Text, _>(metadata.to_string())
            .execute(&mut db)?;
        }
    }
    let first: Value = post("search", &admin, json!({"query":"pagination","limit":2}))?
        .error_for_status()?
        .json()?;
    assert_eq!(first.get("answer"), Some(&Value::Null));
    assert_eq!(first.get("answer_error"), Some(&Value::Null));
    assert_eq!(first["has_more"], true);
    assert_eq!(first["keyword_fallback"], true);
    let shown: Vec<_> = first["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(shown, [1, 2]);
    let second: Value = post(
        "search",
        &admin,
        json!({"query":"pagination","seen":shown,"limit":2,"continuation_mode":"keyword"}),
    )?
    .error_for_status()?
    .json()?;
    assert_eq!(second["has_more"], false);
    assert_eq!(second["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(second["repositories"][0]["id"], 3);
    let other: Value = post(
        "search",
        &reader,
        json!({"query":"pagination","seen":[1],"limit":2}),
    )?
    .error_for_status()?
    .json()?;
    assert_eq!(other["repositories"], json!([]));
    assert_eq!(
        post(
            "search",
            &reader,
            json!({"query":"x","continuation_mode":"semantic"})
        )?
        .status(),
        503
    );
    assert_eq!(
        post(
            "search",
            &reader,
            json!({"query":"x","continuation_mode":"unknown"})
        )?
        .status(),
        400
    );
    for invalid in [0, 101] {
        assert_eq!(
            post(
                "search",
                &reader,
                json!({"query":"x","limit":invalid,"continuation_mode":"semantic"})
            )?
            .status(),
            400
        );
    }
    for invalid in [json!([-1]), json!(["1"]), json!([u64::MAX])] {
        assert_eq!(
            post(
                "search",
                &admin,
                json!({"query":"pagination","seen":invalid})
            )?
            .status(),
            400
        );
    }

    assert_eq!(
        post(
            "users",
            &reader,
            json!({"username":"denied","password":"fixture-password"})
        )?
        .status(),
        403
    );
    post("settings", &reader, json!({"sync_interval_minutes":13}))?.error_for_status()?;
    let settings = |cookie: &str| -> Result<Value> {
        Ok(c.get(format!("{base}/api/settings"))
            .header("X-StarStack", "1")
            .header(COOKIE, cookie)
            .send()?
            .error_for_status()?
            .json()?)
    };
    assert_eq!(settings(&admin)?["sync_interval_minutes"], 0);
    assert_eq!(settings(&reader)?["sync_interval_minutes"], 13);
    post(
        "settings",
        &reader,
        json!({"quota_reserve_percent":17,"readme_budget":4,"readme_interval_minutes":0,"initial_readme_boost":true}),
    )?
    .error_for_status()?;
    let policy = settings(&reader)?;
    assert_eq!(policy["sync_interval_minutes"], 13);
    assert_eq!(policy["quota_reserve_percent"], 17);
    assert_eq!(policy["readme_budget"], 4);
    assert_eq!(policy["initial_readme_boost"], true);
    assert_eq!(policy["readme_interval_minutes"], 0);
    assert_eq!(settings(&admin)?["quota_reserve_percent"], 5);
    assert_eq!(settings(&admin)?["initial_readme_boost"], false);
    let accepted: Value = post("readmes", &reader, json!({}))?
        .error_for_status()?
        .json()?;
    assert!(
        accepted["message"]
            .as_str()
            .is_some_and(|s| s.contains("README"))
    );
    let models = |cookie: &str| -> Result<Value> {
        Ok(c.get(format!("{base}/api/models"))
            .header("X-StarStack", "1")
            .header(COOKIE, cookie)
            .send()?
            .error_for_status()?
            .json()?)
    };
    let chat = |cookie: &str| -> Result<Value> {
        Ok(c.get(format!("{base}/api/chat"))
            .header("X-StarStack", "1")
            .header(COOKIE, cookie)
            .send()?
            .error_for_status()?
            .json()?)
    };
    assert_eq!(chat(&admin)?["configured"], false);
    let config = json!({"provider":"ollama","base_url":"http://127.0.0.1:11333/v1","model":"chat:test","api_key":"fixture-secret"});
    assert_eq!(post("chat", &reader, config.clone())?.status(), 403);
    assert_eq!(post("chat/test", &reader, config.clone())?.status(), 403);
    assert_eq!(chat(&admin)?["api_type"], "auto");
    post("chat", &admin, config.clone())?.error_for_status()?;
    assert_eq!(chat(&admin)?["has_key"], true);
    assert!(!chat(&admin)?.to_string().contains("fixture-secret"));
    assert!(chat(&reader)?["base_url"].is_null());
    post(
        "chat",
        &admin,
        json!({"provider":"ollama","base_url":"http://127.0.0.1:11334/v1","model":"chat:test"}),
    )?
    .error_for_status()?;
    assert_eq!(chat(&admin)?["has_key"], false);
    post("chat", &admin, json!({"provider":""}))?.error_for_status()?;
    assert_eq!(chat(&reader)?["configured"], false);
    let initial = models(&reader)?;
    assert_eq!(initial["backend"], "builtin");
    assert_eq!(initial["models"][0]["status"], "missing");
    let source = json!({"id":"multilingual-minilm-l12-v2","source":"modelscope"});
    assert_eq!(post("models/download", &reader, source)?.status(), 403);
    assert_eq!(
        post(
            "models/delete",
            &reader,
            json!({"id":"multilingual-minilm-l12-v2"})
        )?
        .status(),
        403
    );
    let endpoints = json!({"urls":["http://127.0.0.1:11434","https://ollama.example.test"]});
    assert_eq!(
        post("models/endpoints", &reader, endpoints.clone())?.status(),
        403
    );
    post("models/endpoints", &admin, endpoints)?.error_for_status()?;
    assert_eq!(
        post(
            "models/select",
            &reader,
            json!({"backend":"ollama","url":"http://unapproved.example.test","model":"x"})
        )?
        .status(),
        400
    );
    post(
        "models/select",
        &reader,
        json!({"backend":"ollama","url":"https://ollama.example.test","model":"remote:test"}),
    )?
    .error_for_status()?;
    let saved = models(&reader)?;
    assert_eq!(saved["ollama_url"], "https://ollama.example.test");
    assert_eq!(saved["model"], "remote:test");
    assert_eq!(models(&admin)?["backend"], "builtin");
    assert_eq!(
        post(
            "models/chunking",
            &reader,
            json!({"strategy":"window","size":128,"overlap":65})
        )?
        .status(),
        400
    );
    let windows = json!({"preset":"short"});
    post("models/chunking", &reader, windows.clone())?.error_for_status()?;
    assert_eq!(models(&reader)?["chunking"], windows);
    assert_eq!(models(&admin)?["chunking"]["preset"], "balanced");
    let token = post("tokens", &reader, json!({"name":"desktop"}))?
        .error_for_status()?
        .json::<Value>()?;
    let token = token["token"].as_str().context("missing issued token")?;
    let mcp = |v: Value| {
        c.post(format!("{base}/mcp"))
            .bearer_auth(token)
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .json(&v)
            .send()
    };
    mcp(initialize())?.error_for_status()?;
    let operation = json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"request_sync","arguments":{}}});
    let response = mcp(operation.clone())?
        .error_for_status()?
        .json::<Value>()?;
    let pending = &response["result"]["structuredContent"];
    assert_eq!(
        pending["status"], "awaiting_user_confirmation",
        "SDK reply: {response}"
    );
    assert!(pending["local_command"].is_null());
    assert_eq!(settings(&reader)?["pending_sync"], pending["request_id"]);
    assert!(settings(&admin)?["pending_sync"].is_null());
    let token_list = |cookie: &str| -> Result<Value> {
        Ok(c.get(format!("{base}/api/tokens"))
            .header("X-StarStack", "1")
            .header(COOKIE, cookie)
            .send()?
            .error_for_status()?
            .json()?)
    };
    let tokens = token_list(&reader)?;
    assert_eq!(tokens["tokens"][0]["requests"], 2);
    assert!(tokens["tokens"][0]["last_used_at"].is_string());
    assert!(tokens["tokens"][0]["digest"].is_null());
    assert_eq!(token_list(&admin)?["tokens"], json!([]));

    post(
        "users/update",
        &admin,
        json!({"username":"reader","password":"new-fixture-reader"}),
    )?
    .error_for_status()?;
    assert_eq!(
        post("tokens", &reader, json!({"name":"expired session"}))?.status(),
        401
    );
    assert_eq!(mcp(operation)?.status(), 401);
    let reader = login("reader", "new-fixture-reader")?;
    assert_eq!(token_list(&reader)?["tokens"], json!([]));
    Ok(())
}
