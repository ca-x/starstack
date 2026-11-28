use super::*;
use crate::test_http::{Reply, Server};

#[tokio::test]
async fn successful_device_poll_replaces_the_token_and_preserves_the_library() -> Result<()> {
    let server = Server::new(|r| match r.target.as_str() {
        "/device/code" => Reply::json(json!({
            "device_code":"mock-challenge", "user_code":"ABCD",
            "verification_uri":"https://example.invalid/device", "interval":1, "expires_in":300
        })),
        "/oauth/access_token" => Reply::json(json!({"access_token":"replacement-token"})),
        _ => Reply::status(404),
    });
    let url = server.url.clone();
    let device = tokio::task::spawn_blocking(move || {
        auth::Device::start_at(
            &url,
            "fixture".into(),
            "",
            reqwest::blocking::Client::builder().no_proxy(),
        )
    })
    .await??;
    let dir = tempfile::tempdir()?;
    let base = Library {
        dir: dir.path().into(),
        user: store::PRIMARY.into(),
    };
    let mut db = base.open()?;
    auth::setup(&mut db, "admin", "fixture-password")?;
    let account = auth::account(&mut db, store::PRIMARY)?;
    auth::save_github_token(&mut db, "old-token", None)?;
    let repo = serde_json::from_value(json!({
        "id":7, "full_name":"example/library", "topics":[], "stargazers_count":2,
        "starred_at":"2026-11-01T00:00:00Z", "html_url":"https://github.com/example/library", "archived":false
    }))?;
    store::upsert(&mut db, &repo)?;
    let annotated = store::edit(
        &mut db,
        store::Edit {
            id: 7,
            note: Some("keep this note".into()),
            pinned: Some(true),
            ..Default::default()
        },
    )?;
    let app = Arc::new(App {
        base,
        sessions: Mutex::new(HashMap::from([(
            "session".into(),
            Session {
                user: account.id.clone(),
                revision: account.revision,
                expires: Instant::now() + SESSION_TTL,
            },
        )])),
        devices: Mutex::new(HashMap::from([(
            account.id,
            PendingDevice {
                code: "fixture".into(),
                device: Some(device),
            },
        )])),
        jobs: Mutex::default(),
        model_download: Mutex::default(),
        logins: Mutex::default(),
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let request = Request::post("/api/device/poll")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, "starstack_session=session")
        .header("x-starstack", "1")
        .body(Body::from(r#"{"code":"fixture"}"#))?;
    let response = Router::new()
        .route("/api/{*path}", post(write))
        .with_state(app)
        .oneshot(request)
        .await?;
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 8192).await?)?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "complete");
    assert_eq!(
        store::meta(&mut db, "github_token")?.as_deref(),
        Some("replacement-token")
    );
    assert_eq!(store::by_id(&mut db, 7)?, annotated);
    Ok(())
}
