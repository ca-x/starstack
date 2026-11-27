//! Local GitHub README extraction. Networking uses the account proxy; parsing stays in-process.
use crate::proxy::ProxyConfig;
use anyhow::Result;

#[cfg(not(feature = "firecrawl-local"))]
pub(crate) fn readmes(names: &[String], _: &ProxyConfig) -> Vec<Result<String>> {
    names
        .iter()
        .map(|_| Err(anyhow::anyhow!("此构建未启用 Firecrawl Local")))
        .collect()
}
#[cfg(feature = "firecrawl-local")]
pub(crate) fn readmes(names: &[String], proxy: &ProxyConfig) -> Vec<Result<String>> {
    use futures::{StreamExt, stream};
    let setup = (|| -> Result<_> {
        let client = proxy
            .async_builder()?
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(35))
            .user_agent("StarStack README reader")
            .build()?;
        Ok((
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client,
        ))
    })();
    match setup {
        Err(e) => names.iter().map(|_| Err(anyhow::anyhow!("{e}"))).collect(),
        Ok((runtime, client)) => runtime.block_on(async {
            stream::iter(names.iter().map(|name| fetch(&client, name)))
                .buffered(4)
                .collect()
                .await
        }),
    }
}
#[cfg(feature = "firecrawl-local")]
async fn fetch(client: &reqwest::Client, name: &str) -> Result<String> {
    use anyhow::{Context, ensure};
    let (owner, repo) = name.split_once('/').context("无效的 GitHub 仓库名")?;
    let part = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    ensure!(part(owner) && part(repo), "无效的 GitHub 仓库路径");
    let url = format!("https://github.com/{owner}/{repo}");
    let mut response = client.get(url).send().await?.error_for_status()?;
    ensure!(
        response.status().is_success(),
        "GitHub 页面地址已变更，请刷新仓库元数据"
    );
    let mut html = Vec::new();
    while let Some(part) = response.chunk().await? {
        ensure!(
            html.len() + part.len() <= 4 * 1024 * 1024,
            "GitHub 页面超过 4 MiB 限额"
        );
        html.extend_from_slice(&part);
    }
    tokio::task::spawn_blocking(move || extract(&String::from_utf8_lossy(&html))).await?
}
#[cfg(feature = "firecrawl-local")]
fn extract(html: &str) -> Result<String> {
    use anyhow::ensure;
    use kuchikiki::traits::TendrilSink;
    let doc = kuchikiki::parse_html().one(html).document_node;
    if let Ok(article) = doc.select_first("article.markdown-body") {
        let markdown = firecrawl_local::markdown::document_to_markdown(article.as_node());
        let end = markdown.floor_char_boundary(markdown.len().min(65536));
        return Ok(markdown[..end].trim().to_owned());
    }
    ensure!(
        doc.select_first("meta[name='octolytics-dimension-repository_nwo']")
            .is_ok(),
        "未识别 GitHub 仓库页面，将保留已有 README 并重试"
    );
    Ok(String::new())
}
#[cfg(all(test, feature = "firecrawl-local"))]
mod tests {
    use super::*;
    #[test]
    fn extracts_only_the_readme_with_the_native_library() {
        let md=extract("<html><nav>Dashboard navigation</nav><article class='markdown-body'><h2>Example</h2><p>A library for diagrams.</p></article></html>").unwrap();
        assert!(
            md.contains("## Example")
                && md.contains("library for diagrams")
                && !md.contains("navigation")
        );
        assert!(extract("<h1>Sign in</h1>").is_err());
    }
    #[test]
    #[ignore = "uses the live public GitHub website"]
    fn public_github_readme_with_native_firecrawl() {
        let repos = vec!["clap-rs/clap".to_owned()];
        let mut results = readmes(&repos, &ProxyConfig::System);
        let text = results
            .pop()
            .expect("one request returns one result")
            .expect("public GitHub README");
        assert!(
            text.len() > 256 && text.to_lowercase().contains("clap"),
            "expected the project README"
        );
        println!(
            "clap-rs/clap: extracted {} README bytes in-process",
            text.len()
        );
    }
}
