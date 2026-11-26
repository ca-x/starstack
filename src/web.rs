//! Cookie-authenticated web API and token-authenticated MCP HTTP transport.
use crate::{
    ai, auth, index,
    library::{self, Format, Library, Query},
    mcp::Mcp,
    store::{self, Filter, Sort},
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, Path, Request, State},
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Redirect, Response},
    routing::{any, get, post},
};
use include_dir::{Dir, include_dir};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tower::ServiceExt;
static WEB: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
struct Session {
    user: String,
    revision: i32,
    expires: Instant,
}
#[derive(Default)]
struct Job {
    message: String,
    running: bool,
}
struct App {
    base: Library,
    sessions: Mutex<HashMap<String, Session>>,
    jobs: Mutex<HashMap<String, Job>>,
    devices: Mutex<HashMap<(String, String), auth::Device>>,
    logins: Mutex<HashMap<String, (u32, Instant)>>,
}
type Shared = Arc<App>;
struct ApiError(StatusCode, String);
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
type Api<T> = std::result::Result<T, ApiError>;
fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "请先登录，或检查访问凭证".into())
}
fn check_header(h: &HeaderMap) -> Api<()> {
    if h.get("x-starstack").and_then(|v| v.to_str().ok()) == Some("1") {
        Ok(())
    } else {
        Err(ApiError(StatusCode::FORBIDDEN, "缺少客户端请求标识".into()))
    }
}
fn cookie(h: &HeaderMap) -> Option<&str> {
    h.get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|s| s.strip_prefix("starstack_session="))
}
async fn actor(app: &App, h: &HeaderMap) -> Api<auth::Account> {
    let (id, rev) = {
        let sessions = app.sessions.lock().expect("session lock poisoned");
        let s = sessions
            .get(cookie(h).ok_or_else(unauthorized)?)
            .filter(|s| s.expires > Instant::now())
            .ok_or_else(unauthorized)?;
        (s.user.clone(), s.revision)
    };
    let a = app.base.run(move |db| auth::account(db, &id)).await?;
    if a.disabled || a.revision != rev {
        return Err(unauthorized());
    }
    Ok(a)
}
#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}
async fn login(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    h: HeaderMap,
    Json(req): Json<Login>,
) -> Api<Response> {
    check_header(&h)?;
    if req.username.len() > 128 || req.password.len() > 1024 {
        return Err(unauthorized());
    }
    {
        let mut logins = app.logins.lock().expect("login lock poisoned");
        logins.retain(|_, (_, until)| *until > Instant::now());
        let key = peer.ip().to_string();
        if logins.len() >= 4096
            && !logins.contains_key(&key)
            && let Some(old) = logins
                .iter()
                .min_by_key(|(_, (_, end))| end)
                .map(|(k, _)| k.clone())
        {
            logins.remove(&old);
        }
        let (tries, _) = logins
            .entry(key)
            .or_insert((0, Instant::now() + Duration::from_secs(60)));
        if *tries >= 10 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "尝试次数过多，请一分钟后再试".into(),
            ));
        }
        *tries += 1;
    }
    let a = app
        .base
        .run(move |db| auth::verify(db, &req.username, &req.password))
        .await
        .map_err(|_| unauthorized())?;
    app.logins
        .lock()
        .expect("login lock poisoned")
        .remove(&peer.ip().to_string());
    let token = auth::secret();
    let mut sessions = app.sessions.lock().expect("session lock poisoned");
    sessions.retain(|_, s| s.expires > Instant::now());
    sessions.insert(
        token.clone(),
        Session {
            user: a.id,
            revision: a.revision,
            expires: Instant::now() + SESSION_TTL,
        },
    );
    let secure = if std::env::var("STARSTACK_SECURE_COOKIE").as_deref() == Ok("1") {
        "; Secure"
    } else {
        ""
    };
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "starstack_session={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
                SESSION_TTL.as_secs(),
                secure
            ),
        )],
        Json(json!({"message":"已登录"})),
    )
        .into_response())
}
async fn logout(State(app): State<Shared>, h: HeaderMap) -> Api<Response> {
    check_header(&h)?;
    if let Some(token) = cookie(&h) {
        app.sessions
            .lock()
            .expect("session lock poisoned")
            .remove(token);
    }
    Ok((
        [(
            header::SET_COOKIE,
            "starstack_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        )],
        Json(json!({"message":"已退出"})),
    )
        .into_response())
}
#[derive(Default, Deserialize)]
struct Listing {
    #[serde(flatten)]
    filter: Filter,
    #[serde(default)]
    sort: Sort,
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}
#[derive(Deserialize)]
struct Export {
    format: Format,
    #[serde(flatten)]
    filter: Filter,
}
#[derive(Default, Deserialize)]
struct Run {
    #[serde(default)]
    full: bool,
    #[serde(default)]
    rebuild: bool,
    request_id: Option<String>,
}
#[derive(Deserialize)]
struct Settings {
    sync_interval_minutes: Option<u64>,
    readme_interval_minutes: Option<u64>,
    quota_reserve_percent: Option<u8>,
    readme_budget: Option<usize>,
    readme_source: Option<crate::quota::ReadmeSource>,
}
fn value<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).context("参数格式有误")
}
fn string(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("缺少参数 {key}"))
}
fn tenant(app: &App, user: &str) -> Library {
    Library {
        dir: app.base.dir.clone(),
        user: user.into(),
    }
}
async fn read(
    State(app): State<Shared>,
    h: HeaderMap,
    Path(path): Path<String>,
) -> Api<Json<Value>> {
    check_header(&h)?;
    let a = actor(&app, &h).await?;
    let lib = tenant(&app, &a.id);
    let v = match path.as_str() {
        "me" => json!(a),
        "stats" => lib.run(|db| Ok(json!(store::stats(db)?))).await?,
        "tokens" => lib.run(|db| Ok(json!({"tokens": auth::tokens(db)?}))).await?,
        "users" if a.admin => lib.run(|db| {
            let users = auth::users(db)?.into_iter().map(|user| {
                json!({"username":user.username, "admin":user.admin, "disabled":user.disabled, "primary":user.id==store::PRIMARY})
            }).collect::<Vec<_>>();
            Ok(json!({"users": users}))
        }).await?,
        "users" => return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "proxy" if a.admin=>lib.run(crate::proxy::read_settings).await?,
        "proxy"=>return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "settings"=>lib.run(settings).await?,
        "progress" => {
            let jobs = app.jobs.lock().expect("jobs lock poisoned");
            let job = jobs.get(&a.id);
            json!({"message":job.map_or("", |j| j.message.as_str()), "running":job.is_some_and(|j|j.running)})
        }
        _ => return Err(ApiError(StatusCode::NOT_FOUND,"没有此接口".into())),
    };
    Ok(Json(v))
}
fn settings(db: &mut store::Db) -> Result<Value> {
    use crate::quota::{self, Policy, Resource, Window};
    let p = Policy::load(db)?;
    Ok(json!({"sync_interval_minutes":library::interval(db)?,
        "readme_interval_minutes":quota::readme_interval(db)?,"pending_sync":store::meta(db,"pending_sync")?,
        "quota_reserve_percent":p.reserve_percent,"readme_budget":p.readme_budget,"readme_source":p.readme_source,
        "firecrawl_available":cfg!(feature="firecrawl-local"), "pending_readmes":quota::pending(db)?,
        "quotas":{"rest":Window::load(db,Resource::Rest)?,"graphql":Window::load(db,Resource::Graphql)?}}))
}
fn start_job(
    app: Shared,
    lib: Library,
    operation: &str,
    input: Run,
) -> Result<tokio::task::JoinHandle<Result<String>>> {
    {
        let mut jobs = app.jobs.lock().expect("jobs lock poisoned");
        let item = jobs.entry(lib.user.clone()).or_default();
        ensure!(!item.running, "同步或索引正在运行，请查看进度");
        item.running = true;
        item.message = "准备中".into();
    }
    let op = operation.to_owned();
    Ok(tokio::spawn(async move { work(app, lib, op, input).await }))
}
async fn work(app: Shared, lib: Library, operation: String, input: Run) -> Result<String> {
    let catalog = operation == "sync";
    let state = app.clone();
    let user = lib.user.clone();
    let result = lib
        .run(move |db| {
            if let Some(id) = input.request_id {
                library::approve(db, &id)?;
            }
            let progress = |msg: &str| {
                state
                    .jobs
                    .lock()
                    .expect("jobs lock poisoned")
                    .entry(user.clone())
                    .or_default()
                    .message = msg.into();
            };
            match operation.as_str() {
                "index" => index::build(db, input.rebuild, &progress),
                "readme" => {
                    store::set_meta(
                        db,
                        "last_readme_attempt",
                        &chrono::Utc::now().timestamp().to_string(),
                    )?;
                    let report = crate::sync::enrich(db, &progress)?;
                    let index = index::build(db, false, &progress)
                        .unwrap_or_else(|e| format!("索引待更新：{e:#}"));
                    Ok(format!("{report}。{index}"))
                }
                _ => {
                    let catalog = crate::sync::catalog(db, input.full, &progress)?;
                    let lists = crate::lists::refresh(db, &progress)
                        .unwrap_or_else(|e| format!("分类未刷新：{e:#}"));
                    Ok(format!("{catalog}。{lists}"))
                }
            }
        })
        .await;
    {
        let mut jobs = app.jobs.lock().expect("jobs lock poisoned");
        let item = jobs.entry(lib.user.clone()).or_default();
        item.running = false;
        item.message = match &result {
            Ok(s) => s.clone(),
            Err(e) => format!("{e:#}"),
        };
    }
    if catalog && result.is_ok() {
        let enrich = lib
            .run(|db| {
                Ok(crate::quota::readme_interval(db)? > 0
                    && crate::quota::Policy::load(db)?.readme_budget > 0
                    && crate::quota::pending(db)? > 0)
            })
            .await?;
        // The next stage is independent of the browser connection.
        let _background = start_job(
            app,
            lib,
            if enrich { "readme" } else { "index" },
            Run::default(),
        );
    }
    result
}
async fn write(
    State(app): State<Shared>,
    h: HeaderMap,
    Path(path): Path<String>,
    Json(body): Json<Value>,
) -> Api<Response> {
    check_header(&h)?;
    let a = actor(&app, &h).await?;
    let lib = tenant(&app, &a.id);
    let result = match path.as_str() {
        "list" => {
            let q: Listing = value(body)?;
            lib.run(move |db| {
                let (total,page)=store::page(db,&q.filter,q.sort,q.offset,q.limit.unwrap_or(50).min(500))?;
                Ok(json!({"total":total, "repositories":library::cards(page)}))
            }).await?
        }
        "search" | "ask" => {
            let mut q: Query = value(body)?;
            let ask = path == "ask";
            if ask { q.limit=Some(5); }
            let question = q.query.clone();
            let rows = lib.run(move|db| library::search(db,&q)).await?;
            let answer = if ask { Some(ai::ask(&question,&rows).await?) } else { None };
            json!({"repositories":library::cards(rows),"answer":answer})
        }
        "edit" => {
            let patch = value(body)?;
            lib.run(move|db| Ok(json!(store::edit(db,patch)?))).await?
        }
        "export" => {
            let q: Export = value(body)?;
            let text = lib.run(move|db| library::export(&store::list(db,&q.filter,Sort::Time)?,q.format)).await?;
            let mime = match q.format { Format::Json=>"application/json", Format::Markdown=>"text/markdown; charset=utf-8" };
            return Ok(([(header::CONTENT_TYPE,mime)],text).into_response());
        }
        "readmes" => {
            let _background=start_job(app,lib,"readme",Run::default())?;
            json!({"message":"README 补全任务已启动"})
        }
        "sync" | "index" => {
            let input = value(body)?;
            let task = start_job(app,lib,&path,input)?;
            let message = task.await.context("任务中断")??;
            json!({"message":message})
        }
        "proxy" if a.admin=>lib.run(move|db|crate::proxy::save_settings(db,body,a.revision)).await?,
        "proxy"=>return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "settings"=>{
            let q:Settings=value(body)?;
            lib.run(move|db|db.immediate_transaction(|db|{
                for (key,v) in [("sync_interval_minutes",q.sync_interval_minutes),("readme_interval_minutes",q.readme_interval_minutes)]{
                    if let Some(n)=v{ensure!(n<=10080,"周期范围为 0–10080 分钟");store::set_meta(db,key,&n.to_string())?;}
                }
                let mut p=crate::quota::Policy::load(db)?;
                if let Some(n)=q.quota_reserve_percent{p.reserve_percent=n;}
                if let Some(n)=q.readme_budget{p.readme_budget=n;}
                if let Some(source)=q.readme_source{p.readme_source=source;}
                p.save(db)?;
                settings(db)
            })).await?
        }
        "tokens" => {
            let name=string(&body,"name")?;
            let token=lib.run(move|db| auth::issue(db,&name,a.revision)).await?;
            json!({"token":token})
        }
        "tokens/revoke" => {
            let id=string(&body,"id")?;
            lib.run(move|db|auth::revoke(db,&id)).await?;
            json!({"message":"已撤销"})
        }
        "users" | "users/update" if !a.admin => return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "users" => {
            let q=value(body)?;
            lib.run(move|db|auth::create(db,&a.id,q)).await?;
            json!({"message":"用户已创建"})
        }
        "users/update" => {
            let q=value(body)?;
            lib.run(move|db|auth::update(db,&a.id,q)).await?;
            json!({"message":"用户已更新"})
        }
        "device/start" => lib.run(move|db| {
            let d=auth::Device::start(db)?;
            let code=auth::secret();
            let result=json!({"code":code,"user_code":d.user_code,"verification_uri":d.verification_uri,"interval":d.interval,"expires_in":d.expires_in});
            let mut devices=app.devices.lock().expect("device lock poisoned");
            devices.retain(|(u,_),d|*u!=a.id && !d.expired());
            devices.insert((a.id,code),d);
            Ok(result)
        }).await?,
        "device/poll" => {
            let code=string(&body,"code")?;
            lib.run(move|db|{
                let key=(a.id,code);
                let mut d=app.devices.lock().expect("device lock poisoned").remove(&key).context("设备授权无效，请重新开始")?;
                let done=d.poll(db)?;
                let interval=d.interval;
                if !done {app.devices.lock().expect("device lock poisoned").insert(key,d);}
                Ok(json!({"status":if done{"complete"}else{"pending"},"interval":interval}))
            }).await?
        }
        _ => return Err(ApiError(StatusCode::NOT_FOUND,"没有此接口".into())),
    };
    Ok(Json(result).into_response())
}
async fn mcp_http(State(app): State<Shared>, req: Request) -> Response {
    match serve_mcp_request(app, req).await {
        Ok(response) => response,
        Err(e) if e.0 == StatusCode::UNAUTHORIZED => {
            let mut response = e.into_response();
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer realm=\"StarStack\""),
            );
            response
        }
        Err(e) => e.into_response(),
    }
}
async fn serve_mcp_request(app: Shared, req: Request) -> Api<Response> {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?
        .to_owned();
    let user = app
        .base
        .run(move |db| auth::bearer_owner(db, &bearer))
        .await
        .map_err(|_| unauthorized())?;
    let library = tenant(&app, &user);
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_stateless_protocol_metadata_required(false)
        .with_json_response(true);
    if let Ok(hosts) = std::env::var("STARSTACK_MCP_HOSTS") {
        config = config.with_allowed_hosts(hosts.split(',').map(|s| s.trim().to_owned()));
    }
    let svc = StreamableHttpService::new(
        move || Ok(Mcp::new(library.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    Ok(svc
        .oneshot(req)
        .await
        .expect("infallible MCP HTTP service")
        .map(Body::new))
}
async fn assets(State(app): State<Shared>, uri: Uri, h: HeaderMap, method: Method) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path();
    if path == "/" {
        return Redirect::temporary("/admin").into_response();
    }
    let path = if path == "/admin" || path.starts_with("/admin/") {
        if actor(&app, &h).await.is_err() {
            return Redirect::temporary("/login").into_response();
        }
        "index.html"
    } else if path == "/login" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    match WEB.get_file(path) {
        Some(f) => (
            [(
                header::CONTENT_TYPE,
                mime_guess::from_path(path).first_or_octet_stream().as_ref(),
            )],
            f.contents(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
async fn schedule(app: Shared) {
    loop {
        // A persisted per-user attempt time prevents rapid retry loops across restarts.
        let users = app.base.run(auth::users).await.unwrap_or_default();
        for a in users.into_iter().filter(|a| !a.disabled) {
            let lib = tenant(&app, &a.id);
            if app
                .jobs
                .lock()
                .expect("jobs lock poisoned")
                .get(&a.id)
                .is_some_and(|j| j.running)
            {
                continue;
            }
            let due = lib
                .run(|db| {
                    db.immediate_transaction(|db| {
                        if !auth::has_github_credentials(db)? {
                            return Ok(None);
                        }
                        let now = chrono::Utc::now().timestamp();
                        let last = |db: &mut store::Db, key: &str| -> Result<i64> {
                            Ok(store::meta(db, key)?
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0))
                        };
                        let every = library::interval(db)?;
                        let full = store::meta(db, "last_full_sync")?
                            .and_then(|v| v.parse::<chrono::DateTime<chrono::Utc>>().ok())
                            .map_or(0, |v| v.timestamp());
                        let rest = crate::quota::Window::load(db, crate::quota::Resource::Rest)?;
                        if every > 0
                            && now - last(db, "last_scheduled")?.max(full) >= (every * 60) as i64
                            && rest.check().is_ok()
                        {
                            store::set_meta(db, "last_scheduled", &now.to_string())?;
                            return Ok(Some("sync"));
                        }
                        let policy = crate::quota::Policy::load(db)?;
                        let interval = crate::quota::readme_interval(db)?;
                        if interval > 0
                            && policy.readme_budget > 0
                            && now - last(db, "last_readme_attempt")? >= (interval * 60) as i64
                            && rest.check().is_ok()
                            && (policy.readme_source == crate::quota::ReadmeSource::Firecrawl
                                || rest.has_headroom(policy.reserve_percent))
                            && crate::quota::pending(db)? > 0
                        {
                            store::set_meta(db, "last_readme_attempt", &now.to_string())?;
                            return Ok(Some("readme"));
                        }
                        Ok(None)
                    })
                })
                .await;
            if let Ok(Some(operation)) = due {
                let app = app.clone();
                let _background = start_job(
                    app,
                    lib,
                    operation,
                    Run {
                        full: true,
                        ..Default::default()
                    },
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
pub(crate) async fn serve(base: Library, bind: SocketAddr) -> Result<()> {
    let app = Arc::new(App {
        base,
        sessions: Mutex::default(),
        jobs: Mutex::default(),
        devices: Mutex::default(),
        logins: Mutex::default(),
    });
    let router = Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/{*path}", get(read).post(write))
        .route("/mcp", any(mcp_http))
        .fallback(assets)
        .layer(axum::middleware::map_response(
            |mut r: Response| async move {
                let h = r.headers_mut();
                h.insert(
                    header::CACHE_CONTROL,
                    "no-store".parse().expect("static header"),
                );
                h.insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    "nosniff".parse().expect("static header"),
                );
                h.insert(
                    header::X_FRAME_OPTIONS,
                    "DENY".parse().expect("static header"),
                );
                r
            },
        ))
        .with_state(app.clone());
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("无法监听后台端口，请检查 --bind")?;
    eprintln!("星栈已启动 http://{}/login", listener.local_addr()?);
    tokio::spawn(schedule(app));
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
