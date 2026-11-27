//! Bounded, read-only access to code from the saved repository's default branch.
use crate::{
    auth, proxy,
    quota::{Policy, Resource, Window},
    store::{self, Db, Repo},
};
use anyhow::{Context, Result, bail, ensure};
use reqwest::{StatusCode, Url, blocking::Client, header::ACCEPT};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io::Read, time::Duration};

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
    #[serde(default)]
    truncated: bool,
}
#[derive(Deserialize)]
struct Entry {
    path: String,
    sha: String,
    size: Option<u64>,
    mode: String,
}
#[derive(Serialize)]
pub(crate) struct Source {
    pub path: String,
    pub text: String,
}
pub(crate) struct Code {
    pub sources: Vec<Source>,
    pub truncated: bool,
}
impl Code {
    pub fn index_text(&self) -> String {
        self.sources
            .iter()
            .map(|file| format!("{}\n{}", file.path, file.text))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

struct Github<'a> {
    client: Client,
    base: Url,
    token: String,
    name: [&'a str; 2],
    quota: Window,
    policy: Policy,
}
impl Github<'_> {
    fn get(
        &mut self,
        path: &[&str],
        query: &[(&str, &str)],
        raw: bool,
        limit: u64,
    ) -> Result<Vec<u8>> {
        self.quota.check()?;
        ensure!(
            self.quota.has_headroom(self.policy.reserve_percent),
            "GitHub API 配额已到预留阈值，请在重置后重试"
        );
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("GitHub API 地址无效"))?
            .pop_if_empty()
            .extend(["repos"])
            .extend(self.name)
            .extend(path.iter().copied());
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header(
                ACCEPT,
                if raw {
                    "application/vnd.github.raw+json"
                } else {
                    "application/vnd.github+json"
                },
            )
            .send()
            .context("无法连接 GitHub，请检查网络或代理后重试")?;
        self.quota.observe(response.headers());
        match response.status() {
            StatusCode::UNAUTHORIZED => bail!("GitHub 授权已失效，请重新授权"),
            StatusCode::NOT_FOUND => bail!("GitHub 找不到仓库或文件，请检查当前账号是否有访问权限"),
            ref status if !status.is_success() => {
                bail!("GitHub 读取源代码失败（HTTP {}）", status.as_u16())
            }
            _ => {}
        }
        let mut bytes = Vec::new();
        response
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .context("读取 GitHub 响应失败")?;
        ensure!(
            bytes.len() as u64 <= limit,
            "GitHub 响应过大，请缩小代码范围"
        );
        Ok(bytes)
    }
}

pub(crate) fn collect(db: &mut Db, repo: &Repo, question: &str) -> Result<Code> {
    with_github(db, repo, |github| excerpts(github, repo, question))
}

fn with_github<T>(
    db: &mut Db,
    repo: &Repo,
    read: impl FnOnce(&mut Github<'_>) -> Result<T>,
) -> Result<T> {
    let (owner, name) = repo
        .full_name
        .split_once('/')
        .context("GitHub 仓库名称无效")?;
    ensure!(
        [owner, name].into_iter().all(|part| !part.is_empty()
            && !matches!(part, "." | "..")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "GitHub 仓库名称无效"
    );
    let endpoint = std::env::var("STARSTACK_GITHUB_API_URL")
        .unwrap_or_else(|_| "https://api.github.com".into());
    if let Some(origin) = store::meta(db, "github_api")? {
        ensure!(
            origin.trim_end_matches('/') == endpoint.trim_end_matches('/'),
            "当前资料库已绑定其他 GitHub API 地址，请恢复原地址"
        );
    }
    let mut github = Github {
        client: proxy::builder(db)?
            .user_agent("starstack")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(6))
            .build()?,
        name: [owner, name],
        base: Url::parse(&endpoint).context("GitHub API 地址无效")?,
        token: auth::github_token(db)?,
        quota: Window::load(db, Resource::Rest)?,
        policy: Policy::load(db)?,
    };
    let result = read(&mut github);
    github.quota.save(db, Resource::Rest)?;
    result
}

#[derive(Deserialize)]
pub(crate) struct RemoteCommit {
    pub sha: String,
    pub commit: CommitData,
}
#[derive(Deserialize)]
pub(crate) struct CommitData {
    pub message: String,
    pub committer: CommitTime,
}
#[derive(Deserialize)]
pub(crate) struct CommitTime {
    pub date: String,
}

pub(crate) fn commits(
    db: &mut Db,
    repo: &Repo,
    anchor: Option<&str>,
    page: usize,
) -> Result<Vec<RemoteCommit>> {
    with_github(db, repo, |github| {
        let page = page.to_string();
        let mut query = vec![("per_page", "100"), ("page", page.as_str())];
        if let Some(anchor) = anchor {
            query.push(("sha", anchor));
        }
        let bytes = github.get(&["commits"], &query, false, 8 * 1024 * 1024)?;
        serde_json::from_slice(&bytes).context("无法读取 GitHub 提交记录")
    })
}

pub(crate) fn compare_commits(db: &mut Db, repo: &Repo, old: &str, new: &str) -> Result<bool> {
    with_github(db, repo, |github| {
        #[derive(Deserialize)]
        struct Compare {
            status: String,
        }
        let range = format!("{old}...{new}");
        let response = github.get(
            &["compare", &range],
            &[("per_page", "1")],
            false,
            2 * 1024 * 1024,
        )?;
        let comparison: Compare = serde_json::from_slice(&response).context("无法核对提交基线")?;
        Ok(matches!(comparison.status.as_str(), "ahead" | "identical"))
    })
}

fn excerpts(github: &mut Github<'_>, repo: &Repo, question: &str) -> Result<Code> {
    #[derive(Deserialize)]
    struct Metadata {
        default_branch: String,
    }
    let branch = match repo.default_branch.as_deref() {
        Some(branch) if !branch.is_empty() => branch.to_owned(),
        _ => {
            serde_json::from_slice::<Metadata>(&github.get(&[], &[], false, 1024 * 1024)?)?
                .default_branch
        }
    };
    let tree: Tree = serde_json::from_slice(&github.get(
        &["git", "trees", &branch],
        &[("recursive", "1")],
        false,
        24 * 1024 * 1024,
    )?)
    .context("无法读取 GitHub 文件清单")?;
    let terms = question.to_ascii_lowercase();
    let terms: BTreeSet<_> = terms
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| {
            word.len() >= 3
                && !matches!(
                    *word,
                    "the" | "and" | "how" | "does" | "what" | "this" | "work" | "using"
                )
        })
        .collect();
    let mut files: Vec<_> = tree.tree.into_iter().filter(is_source).collect();
    files.sort_by_cached_key(|file| {
        let path = file.path.to_ascii_lowercase();
        let basename = path.rsplit('/').next().unwrap_or(&path);
        let score = terms.iter().filter(|term| path.contains(**term)).count() * 20
            + usize::from(matches!(
                basename,
                "lib.rs"
                    | "main.rs"
                    | "main.go"
                    | "index.ts"
                    | "index.js"
                    | "app.py"
                    | "__init__.py"
            )) * 8
            + usize::from(path.starts_with("src/")) * 2;
        (std::cmp::Reverse(score), path.matches('/').count(), path)
    });
    let mut sources = Vec::new();
    for file in files.into_iter().take(4) {
        let bytes = github.get(&["git", "blobs", &file.sha], &[], true, 96 * 1024)?;
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if text.contains('\0') || text.trim().is_empty() {
            continue;
        }
        sources.push(Source {
            path: file.path,
            text: text.chars().take(2800).collect(),
        });
    }
    ensure!(
        !sources.is_empty(),
        "未在 GitHub 文件清单中找到可读取的源代码"
    );
    Ok(Code {
        sources,
        truncated: tree.truncated,
    })
}

fn is_source(file: &Entry) -> bool {
    matches!(file.mode.as_str(), "100644" | "100755")
        && file.size.is_some_and(|size| size > 0 && size <= 96 * 1024)
        && matches!(file.sha.len(), 40 | 64)
        && file.sha.bytes().all(|c| c.is_ascii_hexdigit())
        && file.path.len() <= 350
        && !file.path.chars().any(char::is_control)
        && !file.path.split('/').any(|part| {
            part.starts_with('.')
                || matches!(
                    part,
                    "vendor" | "node_modules" | "dist" | "target" | "fixtures"
                )
        })
        && file.path.rsplit_once('.').is_some_and(|(_, ext)| {
            matches!(
                ext,
                "rs" | "py"
                    | "go"
                    | "ts"
                    | "tsx"
                    | "js"
                    | "jsx"
                    | "java"
                    | "kt"
                    | "swift"
                    | "rb"
                    | "php"
                    | "c"
                    | "cc"
                    | "cpp"
                    | "h"
                    | "cs"
                    | "zig"
                    | "scala"
                    | "ex"
                    | "exs"
                    | "lua"
                    | "dart"
                    | "svelte"
                    | "vue"
            )
        })
}
