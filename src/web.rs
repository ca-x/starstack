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
use serde::{Deserialize, Serialize};
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
#[derive(Default, Clone, Serialize)]
struct JobReport {
    message: String,
    warnings: Vec<String>,
    details: Vec<String>,
}
impl JobReport {
    fn from_sync(summary: &crate::sync::Summary, catalog: bool) -> Self {
        let (message, warnings, details) = summary.web_report(catalog);
        Self {
            message,
            warnings,
            details,
        }
    }
    fn add_step(&mut self, result: Result<String>, label: &str) {
        match result {
            Ok(summary) => self.details.push(summary),
            Err(error) => {
                self.warnings.push(format!("{label}：{error}"));
                self.details
                    .extend(error.chain().skip(1).map(ToString::to_string));
            }
        }
    }
}
#[derive(Default)]
struct Job {
    message: String,
    running: bool,
    report: Option<JobReport>,
}
struct App {
    base: Library,
    sessions: Mutex<HashMap<String, Session>>,
    jobs: Mutex<HashMap<String, Job>>,
    model_download: Mutex<HashMap<String, Job>>,
    devices: Mutex<HashMap<String, PendingDevice>>,
    logins: Mutex<HashMap<String, (u32, Instant)>>,
}
struct PendingDevice {
    code: String,
    device: Option<auth::Device>,
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
    let a = app
        .base
        .run(move |db| auth::account(db, &id))
        .await
        .map_err(|e| {
            if matches!(
                e.downcast_ref::<diesel::result::Error>(),
                Some(diesel::result::Error::NotFound)
            ) {
                unauthorized()
            } else {
                e.into()
            }
        })?;
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
async fn setup_status(State(app): State<Shared>, h: HeaderMap) -> Api<Json<Value>> {
    check_header(&h)?;
    Ok(Json(
        json!({"required": app.base.run(auth::needs_setup).await?}),
    ))
}
async fn setup(
    State(app): State<Shared>,
    h: HeaderMap,
    Json(req): Json<Login>,
) -> Api<Json<Value>> {
    check_header(&h)?;
    app.base
        .run(move |db| auth::setup(db, &req.username, &req.password))
        .await?;
    Ok(Json(json!({"message": "管理员已创建"})))
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
#[derive(Deserialize)]
struct UploadQuery {
    model: String,
}

async fn upload(
    State(app): State<Shared>,
    h: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<UploadQuery>,
    mut files: axum::extract::Multipart,
) -> Api<Json<Value>> {
    use tokio::io::AsyncWriteExt;
    check_header(&h)?;
    let a = actor(&app, &h).await?;
    if !a.admin {
        return Err(ApiError(StatusCode::FORBIDDEN, "需要管理员权限".into()));
    }
    let custom = query.model == "custom";
    if !custom
        && !matches!(
            query.model.as_str(),
            crate::models::DEFAULT_ID | crate::models::QWEN_ID
        )
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "未知的上传目标模型".into(),
        ));
    }
    let lib = tenant(&app, &a.id);
    let stage = tempfile::Builder::new()
        .prefix(".starstack-upload-")
        .tempdir_in(&lib.dir)
        .context("无法创建临时上传目录")?;
    let mut names = std::collections::HashSet::new();
    while let Some(mut field) = files.next_field().await.context("上传表单读取失败")? {
        let filename = match (field.name(), field.file_name()) {
            (Some("pooling_config"), Some(_)) if custom => "1_Pooling/config.json",
            (
                Some("files"),
                Some(
                    name @ ("config.json"
                    | "tokenizer.json"
                    | "model.safetensors"
                    | "model.gguf"
                    | "Qwen3-Embedding-0.6B-q4_k_m.gguf"),
                ),
            ) => name,
            _ => {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "文件不在模型清单中".into(),
                ));
            }
        }
        .to_owned();
        if !names.insert(filename.clone()) {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "上传表单包含重复文件".into(),
            ));
        }
        let path = stage.path().join(&filename);
        if filename.starts_with("1_Pooling/") {
            tokio::fs::create_dir(stage.path().join("1_Pooling"))
                .await
                .context("无法创建池化配置目录")?;
        }
        let mut dest = tokio::fs::File::create(path)
            .await
            .context("无法写入上传文件")?;
        let limit = if filename == "tokenizer.json" {
            64 * 1024 * 1024
        } else if filename.ends_with(".json") {
            1024 * 1024
        } else {
            1_500_000_000
        };
        let mut size = 0usize;
        while let Some(part) = field.chunk().await.context("文件上传中断，请重试")? {
            size = size.saturating_add(part.len());
            if size > limit {
                return Err(ApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "文件超过模型导入大小限制".into(),
                ));
            }
            dest.write_all(&part)
                .await
                .context("无法写入模型，请检查磁盘空间")?;
        }
        dest.flush().await.context("模型临时文件写入失败")?;
    }
    let installed = lib
        .run(move |db| {
            verify_admin(db, &a)?;
            let model = if custom {
                crate::models::import_custom(db, stage.path(), &|_| {})?
            } else {
                crate::models::import_files(db, &query.model, stage.path(), &|_| {})?
            };
            Ok(json!({"message":"模型已校验并安装","id":model.id}))
        })
        .await?;
    Ok(Json(installed))
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
        "me" => lib.run(move|db| Ok(json!({
            "username":a.username,"admin":a.admin,"disabled":a.disabled,
            "nickname":store::meta(db,"nickname")?
        }))).await?,
        "mcp/settings" => lib.run(|db| Ok(json!({"use_chat":crate::mcp::chat_enabled(db)?}))).await?,
        "github" => lib.run(|db| Ok(json!({"source": auth::github_source(db)?}))).await?,
        "chat" => lib.run(move|db|crate::chat_settings::status(db,a.admin)).await?,
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
        "proxy" | "proxy/test"=>return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "organize/job" => lib.run(crate::organizer::progress).await?,
        "models" => {
            let mut panel = lib.run(crate::model_preferences::panel).await?;
            let jobs = app.model_download.lock().expect("model download lock poisoned");
            if let Some(models)=panel["models"].as_array_mut() {
                for model in models {
                    if let Some(job)=model["id"].as_str().and_then(|id|jobs.get(id)) && model["status"]!="ready" {
                        if job.running {
                            model["status"]=json!("downloading");
                            model["error"]=Value::Null;
                        } else if !job.message.is_empty() && model["status"]!="downloading" {
                            model["status"]=json!("failed");
                            model["error"]=json!(job.message);
                        }
                    }
                }
            }
            panel
        }
        "settings"=>lib.run(settings).await?,
        "progress" => {
            let jobs = app.jobs.lock().expect("jobs lock poisoned");
            let job = jobs.get(&a.id);
            json!({"message":job.map_or("", |j| j.message.as_str()), "running":job.is_some_and(|j|j.running), "report":job.and_then(|j|j.report.as_ref())})
        }
        _ => return Err(ApiError(StatusCode::NOT_FOUND,"没有此接口".into())),
    };
    Ok(Json(v))
}
fn verify_admin(db: &mut store::Db, actor: &auth::Account) -> Result<()> {
    let current = auth::account(db, &actor.id)?;
    ensure!(
        current.admin && !current.disabled && current.revision == actor.revision,
        "需要有效的管理员会话"
    );
    Ok(())
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
) -> Result<tokio::task::JoinHandle<Result<JobReport>>> {
    {
        let mut jobs = app.jobs.lock().expect("jobs lock poisoned");
        let item = jobs.entry(lib.user.clone()).or_default();
        ensure!(!item.running, "同步或索引正在运行，请查看进度");
        item.running = true;
        item.message = "准备中".into();
        item.report = None;
    }
    let op = operation.to_owned();
    Ok(tokio::spawn(async move { work(app, lib, op, input).await }))
}
async fn work(app: Shared, lib: Library, operation: String, input: Run) -> Result<JobReport> {
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
                "index" => Ok(JobReport {
                    message: index::build(db, input.rebuild, &progress)?,
                    ..JobReport::default()
                }),
                "readme" => {
                    store::set_meta(
                        db,
                        "last_readme_attempt",
                        &chrono::Utc::now().timestamp().to_string(),
                    )?;
                    let mut report =
                        JobReport::from_sync(&crate::sync::enrich(db, &progress)?, false);
                    report.add_step(index::build(db, false, &progress), "索引待更新");
                    Ok(report)
                }
                _ => {
                    let mut report = JobReport::from_sync(
                        &crate::sync::catalog(db, input.full, &progress)?,
                        true,
                    );
                    report.add_step(crate::lists::refresh(db, &progress), "分类待刷新");
                    Ok(report)
                }
            }
        })
        .await;
    {
        let mut jobs = app.jobs.lock().expect("jobs lock poisoned");
        let item = jobs.entry(lib.user.clone()).or_default();
        item.running = false;
        let report = match &result {
            Ok(report) => report.clone(),
            Err(error) => JobReport {
                message: "任务未完成".into(),
                warnings: vec![error.to_string()],
                details: error.chain().skip(1).map(ToString::to_string).collect(),
            },
        };
        item.message.clone_from(&report.message);
        item.report = Some(report);
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
        "repository/commits" => {
            #[derive(Deserialize)]
            struct CommitInput { id: i64, #[serde(default)] offset: u32, enabled: Option<bool>, limit: Option<usize>, #[serde(default)] refresh: bool }
            let input:CommitInput=value(body)?;
            lib.run(move|db|{
                if let Some(enabled)=input.enabled {
                    crate::repo_commits::configure(db,input.id,enabled,input.limit.unwrap_or(20))?;
                }
                if input.refresh { crate::repo_commits::refresh(db,input.id)?; }
                crate::repo_commits::status(db,input.id,input.offset)
            }).await?
        }
        "repository/summary" => {
            #[derive(Deserialize)]
            struct SummaryInput { id: i64, language: Option<String> }
            let input:SummaryInput=value(body)?;
            let repo=lib.run(move|db|store::by_id(db,input.id)).await?;
            let language=if input.language.as_deref()==Some("en") {"English"} else {"Chinese"};
            let chat=ai::RepositoryChat::configured().await?;
            json!({"summary":chat.summarize(&repo,language).await?})
        }
        "repository/context" => {
            #[derive(Deserialize)]
            struct ContextInput { id: i64, code: String }
            let input:ContextInput=value(body)?;
            lib.run(move|db|store::save_repository_code(db,input.id,&input.code)).await?;
            json!({"message":"已保存到本地"})
        }
        "repository/ask" => {
            #[derive(Deserialize)]
            struct RepositoryQuestion { id: i64, question: String, #[serde(default)] history: Vec<ai::RepositoryTurn> }
            let input:RepositoryQuestion=value(body)?;
            let question=input.question.trim();
            if question.is_empty() || question.len()>4096 || input.history.len()>4 ||
                input.history.iter().any(|turn|turn.question.len()>4096 || turn.answer.len()>16*1024) {
                return Err(ApiError(StatusCode::BAD_REQUEST,"请缩短问题或重新开始仓库对话".into()));
            }
            let chat=ai::RepositoryChat::configured().await?;
            let mut terms=question.to_owned();
            if let Some(previous)=input.history.last() {
                terms.push(' ');
                terms.push_str(&previous.question);
                terms.push(' ');
                terms.extend(previous.answer.chars().take(512));
            }
            let (repo,code)=lib.run(move|db|{
                let repo=store::by_id(db,input.id)?;
                let code=crate::repo_code::collect(db,&repo,&terms)?;
                Ok((repo,code))
            }).await?;
            let answer=chat.question(&repo,question,&input.history,&code).await?;
            json!({"answer":answer,"source_paths":code.sources.iter().map(|file|&file.path).collect::<Vec<_>>(),
                "code":code.index_text(),"truncated":code.truncated})
        }
        "export" => {
            let q: Export = value(body)?;
            let text = lib.run(move|db| library::export(&store::list(db,&q.filter,Sort::Time)?,q.format)).await?;
            let mime = match q.format { Format::Json=>"application/json", Format::Markdown=>"text/markdown; charset=utf-8" };
            return Ok(([(header::CONTENT_TYPE,mime)],text).into_response());
        }
        "chat" | "chat/models" | "chat/test" if !a.admin=>return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "chat" => {
            let input=value(body)?;
            lib.run(move|db|db.immediate_transaction(|db|{verify_admin(db,&a)?;crate::chat_settings::update(db,input)})).await?;
            json!({"message":"AI 聊天服务设置已保存"})
        }
        "chat/models" | "chat/test" => {
            let input=value(body)?;
            let cfg=lib.run(move|db|{verify_admin(db,&a)?;crate::chat_settings::draft(db,input)}).await?;
            if path == "chat/test" {
                let start=Instant::now();
                ai::test_connection(cfg).await?;
                json!({"elapsed_ms":start.elapsed().as_millis()})
            } else {
                json!({"models":crate::chat_settings::models(cfg).await?})
            }
        }
        "models/search" => {let input=value(body)?;lib.run(move|db|crate::model_hub::search(db,input)).await?}
        "organize/preview" => crate::organizer::preview(lib,body).await?,
        "organize/apply" => lib.run(move|db|crate::organizer::manual(db,body)).await?,
        "organize/start" => crate::organizer::start(lib,Some(body)).await?,
        "organize/pause" => lib.run(crate::organizer::pause).await?,
        "organize/resume" => crate::organizer::start(lib,None).await?,
        "models/chunking" => {
            let input=value(body)?;
            lib.run(move|db|crate::chunking::save(db,input)).await?;
            json!({"message":"分块设置已保存"})
        }
        "models/select" => {
            let selection=value(body)?;
            lib.run(move|db| crate::model_preferences::select(db,selection)).await?;
            json!({"message":"Embedding 设置已保存"})
        }
        "models/endpoints" | "models/download" | "models/delete" | "models/hub-install" if !a.admin =>
            return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
        "models/endpoints" => {
            #[derive(Deserialize)]
            struct Endpoints { urls: Vec<String> }
            let input:Endpoints=value(body)?;
            lib.run(move|db|db.immediate_transaction(|db|{
                verify_admin(db,&a)?;
                crate::ollama_settings::set_endpoints(db,&input.urls)
            })).await?;
            json!({"message":"Ollama 服务地址已保存"})
        }
        "models/hub-install" => {
            let source=string(&body,"source")?;
            let repository=string(&body,"repository")?;
            lib.run(move|db|{verify_admin(db,&a)?;let model=crate::models::download_custom(db,&source,&repository,&|_|{})?;Ok(json!({"message":"模型已安装","id":model.id}))}).await?
        }
        "models/delete" => {
            let id=string(&body,"id")?;
            let deleted=id.clone();
            lib.run(move|db| {
                verify_admin(db,&a)?;
                crate::models::remove(db,&id)?;
                Ok(())
            }).await?;
            app.model_download.lock().expect("model download lock poisoned").remove(&deleted);
            json!({"message":"模型已移除"})
        }
        "models/download" => {
            let id=string(&body,"id")?;
            let source=string(&body,"source")?;
            if !matches!(id.as_str(),crate::models::DEFAULT_ID|crate::models::QWEN_ID) || !matches!(source.as_str(),"modelscope"|"huggingface"){
                return Err(ApiError(StatusCode::BAD_REQUEST,"模型或下载来源无效".into()));
            }
            {
                let mut jobs=app.model_download.lock().expect("model download lock poisoned");
                let job=jobs.entry(id.clone()).or_default();
                if job.running {return Err(ApiError(StatusCode::CONFLICT,"模型下载正在进行".into()));}
                job.running=true;
                job.message.clear();
            }
            tokio::spawn(async move{
                let downloaded=id.clone();
                let result=lib.run(move|db| {
                    verify_admin(db,&a)?;
                    crate::models::install(db,&id,&source,&|_|{})?;
                    Ok(())
                }).await;
                let mut jobs=app.model_download.lock().expect("model download lock poisoned");
                let job=jobs.entry(downloaded).or_default();
                job.running=false;
                job.message=result.err().map(|e|format!("{e:#}")).unwrap_or_default();
            });
            return Ok((StatusCode::ACCEPTED,Json(json!({"message":"模型下载已启动"}))).into_response());
        }
        "readmes" => {
            let _background=start_job(app,lib,"readme",Run::default())?;
            json!({"message":"README 补全任务已启动"})
        }
        "sync" | "index" => {
            let input = value(body)?;
            let task = start_job(app,lib,&path,input)?;
            let result = task.await.context("任务中断")?;
            match result {
                Ok(report) => json!(report),
                Err(error) => return Err(ApiError(StatusCode::BAD_REQUEST, error.to_string())),
            }
        }
        "proxy/test" if a.admin=> {
            let candidate=body.get("candidate").cloned().unwrap_or(Value::Null);
            lib.run(move|db|crate::proxy::test_connection(db,candidate,a.revision)).await?
        }
        "proxy" if a.admin=>lib.run(move|db|crate::proxy::save_settings(db,body,a.revision)).await?,
        "proxy" | "proxy/test"=>return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
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
        "me" => {
            let nickname=string(&body,"nickname")?.trim().to_owned();
            lib.run(move|db| {
                ensure!(nickname.chars().count()<=48 && !nickname.chars().any(char::is_control),"昵称最多 48 个字符，不能包含控制字符");
                if nickname.is_empty() { store::delete_meta(db,"nickname")?; }
                else { store::set_meta(db,"nickname",&nickname)?; }
                Ok(json!({"username":a.username,"admin":a.admin,"disabled":a.disabled,
                    "nickname":(!nickname.is_empty()).then_some(nickname)}))
            }).await?
        }
        "mcp/settings" => {
            #[derive(Deserialize)]
            struct McpSettings { use_chat: bool }
            let input:McpSettings=value(body)?;
            lib.run(move|db|store::set_meta(db,crate::mcp::USE_CHAT,if input.use_chat {"1"}else{"0"})).await?;
            json!({"use_chat":input.use_chat})
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
        "users" | "users/update" | "users/delete" if !a.admin => return Err(ApiError(StatusCode::FORBIDDEN,"需要管理员权限".into())),
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
        "users/delete" => {
            let username=string(&body,"username")?;
            lib.run(move|db|auth::delete(db,&a,&username)).await?;
            json!({"message":"用户已删除"})
        }
        "github/disconnect" => lib.run(move|db| {
            let mut devices=app.devices.lock().expect("device lock poisoned");
            auth::disconnect_github(db)?;
            devices.remove(&a.id);
            Ok(json!({"source":null}))
        }).await?,
        "device/start" => lib.run(move|db| {
            ensure!(auth::github_source(db)? != Some("environment"), "请从部署环境移除 GitHub 令牌");
            let d=auth::Device::start(db)?;
            let code=auth::secret();
            let result=json!({"code":code,"user_code":d.user_code,"verification_uri":d.verification_uri,"interval":d.interval,"expires_in":d.expires_in});
            let mut devices=app.devices.lock().expect("device lock poisoned");
            devices.retain(|_,pending|pending.device.as_ref().is_none_or(|d|!d.expired()));
            devices.insert(a.id,PendingDevice { code, device:Some(d) });
            Ok(result)
        }).await?,
        "device/cancel" => {
            let code=string(&body,"code")?;
            let mut devices=app.devices.lock().expect("device lock poisoned");
            if devices.get(&a.id).is_some_and(|pending|pending.code==code) {
                devices.remove(&a.id);
            }
            json!({})
        }
        "device/poll" => {
            let code=string(&body,"code")?;
            lib.run(move|db|{
                let mut d=app.devices.lock().expect("device lock poisoned")
                    .get_mut(&a.id).filter(|pending|pending.code==code)
                    .and_then(|pending|pending.device.take()).context("设备授权无效，请重新开始")?;
                let token=d.poll_token();
                let mut devices=app.devices.lock().expect("device lock poisoned");
                let pending=devices.get_mut(&a.id).filter(|pending|pending.code==code).context("设备授权无效，请重新开始")?;
                let interval=d.interval;
                match token {
                    Ok(None) => {
                        pending.device=Some(d);
                        Ok(json!({"status":"pending","interval":interval}))
                    }
                    result => {
                        devices.remove(&a.id);
                        let token=result?.context("设备授权无效，请重新开始")?;
                        db.immediate_transaction(|db| {
                            let current=auth::account(db,&a.id)?;
                            ensure!(!current.disabled && current.revision==a.revision, "设备授权无效，请重新开始");
                            auth::save_github_token(db,&token)
                        })?;
                        Ok(json!({"status":"complete","interval":interval}))
                    }
                }
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
        model_download: Mutex::default(),
        devices: Mutex::default(),
        logins: Mutex::default(),
    });
    let router = Router::new()
        .route(
            "/api/models/upload",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(1_610_612_736)),
        )
        .route("/api/login", post(login))
        .route("/api/setup", get(setup_status).post(setup))
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
