//! StarStack CLI, web server and Model Context Protocol entry points.
mod ai;
mod auth;
mod chat_settings;
mod chunking;
mod eval;
mod firecrawl;
mod index;
mod index_worker;
mod library;
mod lists;
mod mcp;
mod model_hub;
mod model_preferences;
mod models;
mod native_embedding;
mod ollama_settings;
mod organize;
mod organizer;
mod proxy;
mod quota;
mod repo_code;
mod repo_commits;
mod retry;
mod store;
mod sync;
#[cfg(test)]
mod test_http;
mod web;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use library::{Format, Library, Query};
use std::{
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
};
use store::{Filter, Repo, Sort};

#[derive(Parser)]
#[command(
    version,
    about = "星栈 · 检索和批注自己的 GitHub 收藏",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// 启动包含登录页和管理后台的 Web 服务
    Serve {
        #[arg(long, env = "STARSTACK_BIND", default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
    },
    /// 连接 GitHub 账号
    Auth {
        #[command(subcommand)]
        action: Auth,
    },
    /// 同步收藏并增量更新向量索引
    Sync {
        /// 只拉取仓库元数据和 GitHub 分类，不抓取 README 或调用模型
        #[arg(long)]
        catalog_only: bool,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        approve: Option<String>,
    },
    /// 索引新增内容；--rebuild 更换模型或修复索引
    Index {
        #[arg(long)]
        rebuild: bool,
    },
    #[command(hide = true)]
    IndexWorker {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        owner: String,
        #[arg(long, value_enum)]
        profile: index_worker::Profile,
    },
    /// 管理本机模型、下载来源与检索后端
    Models {
        #[command(subcommand)]
        action: Option<ModelAction>,
    },
    /// 用 JSON Lines 标注问题衡量检索召回率
    Eval {
        #[arg(long)]
        queries: PathBuf,
    },
    /// 一句话召回收藏
    Search {
        query: String,
        #[command(flatten)]
        filter: Filter,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// 从自己的收藏中获取带引用的 AI 推荐
    Ask {
        query: String,
        #[command(flatten)]
        filter: Filter,
    },
    /// 查看收藏列表，可按时间或 star 数排序
    List {
        #[command(flatten)]
        filter: Filter,
        #[arg(long, value_enum, default_value = "time")]
        sort: Sort,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// 查看仓库及私人批注
    Show { repo: String },
    /// 添加或移除私人标签
    Tag {
        repo: String,
        tag: String,
        #[arg(long)]
        remove: bool,
    },
    /// 写入私人备注，空字符串可清除备注
    Note { repo: String, text: String },
    /// 置顶或取消置顶
    Pin {
        repo: String,
        #[arg(long)]
        remove: bool,
    },
    /// 查看自己在 GitHub 维护的分类及数量
    Lists {
        #[arg(long)]
        refresh: bool,
    },
    /// 列出标签及数量
    Tags,
    /// 收藏统计与最近同步时间
    Stats,
    /// 导出 Markdown 或 JSON 到文件或标准输出
    Export {
        #[arg(long, value_enum, default_value = "markdown")]
        format: Format,
        #[command(flatten)]
        filter: Filter,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 启动 stdio MCP 工具
    Mcp,
}
#[derive(Subcommand)]
enum ModelAction {
    /// 列出内置模型和当前账号的选择
    List,
    /// 下载并校验内置模型（支持重复执行或切换来源）
    Download {
        #[arg(default_value = models::DEFAULT_ID)]
        id: String,
        #[arg(long, default_value = "huggingface", value_parser = ["huggingface", "modelscope"])]
        source: String,
    },
    /// 选择 builtin 或 ollama；远程地址须由管理员加入允许列表
    Select {
        #[arg(value_parser = ["builtin", "ollama"])]
        backend: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        url: Option<String>,
    },
    /// 移除不再使用的内置模型
    Remove {
        #[arg(default_value = models::DEFAULT_ID)]
        id: String,
    },
}
#[derive(Subcommand)]
enum Auth {
    /// 在浏览器输入 GitHub 设备码授权
    Login {
        /// 使用当前操作系统账号已登录的 GitHub CLI，按需读取凭证
        #[arg(long)]
        gh: bool,
    },
}
fn display(r: &Repo) {
    println!(
        "{}{}  {}  ☆ {}  {}",
        if r.pinned { "★ " } else { "" },
        r.full_name,
        r.language.as_deref().unwrap_or("—"),
        r.stargazers_count,
        r.starred_at.get(..10).unwrap_or(&r.starred_at)
    );
    if let Some(d) = &r.description {
        println!("  {d}");
    }
    if !r.tags.is_empty() {
        println!("  #{}", r.tags.join(" #"));
    }
    if !r.note.is_empty() {
        println!("  备注　{}", r.note);
    }
    println!(
        "  {}{}",
        r.html_url,
        r.score
            .map_or(String::new(), |s| format!("  相关度 {s:.3}"))
    );
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Command::IndexWorker {
        data_dir,
        owner,
        profile,
    } = &cli.command
    {
        return index_worker::run(data_dir, owner, *profile);
    }
    let progress = progress_reporter();
    let lib = Library {
        dir: store::home()?,
        user: store::PRIMARY.into(),
    };
    let mut db = lib.open()?;
    match cli.command {
        Command::Serve { bind } => {
            auth::bootstrap(&mut db)?;
            drop(db);
            runtime()?.block_on(web::serve(lib, bind))?;
        }
        Command::Mcp => {
            drop(db);
            runtime()?.block_on(mcp::stdio(lib))?;
        }
        Command::Auth {
            action: Auth::Login { gh: true },
        } => {
            auth::use_gh(&mut db)?;
            println!("初始资料库已连接本机 gh，可运行 starstack sync");
        }
        Command::Auth {
            action: Auth::Login { gh: false },
        } => {
            let mut device = auth::Device::start(&mut db)?;
            eprintln!(
                "打开 {} 并输入 {}",
                device.verification_uri, device.user_code
            );
            loop {
                std::thread::sleep(std::time::Duration::from_secs(device.interval));
                if device.poll(&mut db)? {
                    break;
                }
            }
            println!("GitHub 已连接，可以运行 starstack sync");
        }
        Command::Sync {
            full,
            approve,
            catalog_only,
        } => {
            if let Some(id) = approve {
                library::approve(&mut db, &id)?;
            }
            if catalog_only {
                let catalog = sync::catalog(&mut db, full, &progress)?;
                println!("{catalog}");
                println!("{}", lists::refresh(&mut db, &progress)?);
                ensure!(catalog.failed == 0, "部分仓库未同步，请重试");
                return Ok(());
            }
            let report = library::refresh(&mut db, full, &progress)?;
            println!("{report}");
            ensure!(report.sync.failed == 0, "部分仓库同步失败，请重新运行 sync");
            ensure!(report.index_ready, "请按提示运行 starstack index");
        }
        Command::IndexWorker { .. } => {
            unreachable!("worker command handled before opening CLI database")
        }
        Command::Index { rebuild } => println!("{}", index::build(&mut db, rebuild, &progress)?),
        Command::Models { action } => match action.unwrap_or(ModelAction::List) {
            ModelAction::List => println!(
                "{}",
                serde_json::to_string_pretty(&model_preferences::panel(&mut db)?)?
            ),
            ModelAction::Download { id, source } => {
                models::install(&mut db, &id, &source, &progress)?;
                println!("模型已准备就绪：{id}");
            }
            ModelAction::Remove { id } => {
                models::remove(&mut db, &id)?;
                println!("模型已移除：{id}");
            }
            ModelAction::Select {
                backend,
                model,
                url,
            } => {
                model_preferences::select(
                    &mut db,
                    model_preferences::Selection {
                        backend,
                        model,
                        url,
                    },
                )?;
                println!("设置已保存");
                if index::needs_rebuild(&mut db)? {
                    println!("请运行 starstack index --rebuild");
                }
            }
        },
        Command::Eval { queries } => eval::run(&mut db, &queries)?,
        Command::Search {
            query,
            filter,
            limit,
        } => {
            let rows = library::search(
                &mut db,
                &Query {
                    query,
                    filter,
                    limit: Some(limit),
                },
            )?;
            if rows.is_empty() {
                println!("当前筛选条件下没有收藏");
            }
            for r in rows {
                display(&r);
            }
        }
        Command::Ask { query, filter } => {
            let rows = library::search(
                &mut db,
                &Query {
                    query: query.clone(),
                    filter,
                    limit: Some(5),
                },
            )?;
            println!("{}", runtime()?.block_on(ai::ask(&query, &rows))?);
            for (i, r) in rows.iter().enumerate() {
                println!("[{}] {}  {}", i + 1, r.full_name, r.html_url);
            }
        }
        Command::List {
            filter,
            sort,
            limit,
            offset,
        } => {
            let (total, rows) = store::page(&mut db, &filter, sort, offset, limit)?;
            println!("共 {total} 个仓库");
            for r in &rows {
                display(r);
            }
        }
        Command::Show { repo } => println!(
            "{}",
            serde_json::to_string_pretty(&store::by_name(&mut db, &repo)?)?
        ),
        Command::Tag { repo, tag, remove } => {
            ensure!(!tag.trim().is_empty(), "请输入标签");
            db.immediate_transaction::<_, anyhow::Error, _>(|db| {
                let mut r = store::by_name(db, &repo)?;
                if remove {
                    r.tags.retain(|s| !s.eq_ignore_ascii_case(tag.trim()));
                } else {
                    r.tags.push(tag);
                }
                store::edit_in_transaction(
                    db,
                    store::Edit {
                        id: r.id,
                        tags: Some(r.tags),
                        ..Default::default()
                    },
                )?;
                Ok(())
            })?;
            println!("标签已保存");
        }
        Command::Note { repo, text } => {
            let id = store::by_name(&mut db, &repo)?.id;
            display(&store::edit(
                &mut db,
                store::Edit {
                    id,
                    note: Some(text),
                    ..Default::default()
                },
            )?);
        }
        Command::Pin { repo, remove } => {
            let id = store::by_name(&mut db, &repo)?.id;
            display(&store::edit(
                &mut db,
                store::Edit {
                    id,
                    pinned: Some(!remove),
                    ..Default::default()
                },
            )?);
        }
        Command::Lists { refresh } => {
            if refresh {
                eprintln!("{}", lists::refresh(&mut db, &progress)?);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&lists::counts(&mut db)?)?
            );
        }
        Command::Tags => println!(
            "{}",
            serde_json::to_string_pretty(&store::stats(&mut db)?.tags)?
        ),
        Command::Stats => println!("{}", serde_json::to_string_pretty(&store::stats(&mut db)?)?),
        Command::Export {
            format,
            filter,
            output,
        } => {
            let rows = store::list(&mut db, &filter, Sort::Time)?;
            let text = library::export(&rows, format)?;
            if let Some(file) = output {
                let dir = file
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
                let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
                tmp.write_all(text.as_bytes())?;
                tmp.persist(&file).context("无法写入导出文件")?;
                println!("已导出 {} 个仓库到 {}", rows.len(), file.display());
            } else {
                io::stdout().lock().write_all(text.as_bytes())?;
            }
        }
    }
    Ok(())
}
fn progress_reporter() -> impl Fn(&str) {
    use std::{
        cell::Cell,
        io::IsTerminal,
        time::{Duration, Instant},
    };
    let last = Cell::new(None::<Instant>);
    move |message| {
        let terminal = io::stderr().is_terminal();
        let interval = if terminal {
            Duration::from_millis(350)
        } else {
            Duration::from_secs(5)
        };
        if last.get().is_some_and(|t| t.elapsed() < interval) {
            return;
        }
        last.set(Some(Instant::now()));
        if terminal {
            eprint!("\r\x1b[2K{message}\r");
        } else {
            eprintln!("{message}");
        }
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("星栈：{e:#}");
            ExitCode::FAILURE
        }
    }
}
