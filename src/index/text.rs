use super::Repo;
use kuchikiki::{NodeRef, traits::TendrilSink};
use pulldown_cmark::{Options, Parser, html};

pub(super) fn metadata(repo: &Repo) -> String {
    let mut text = format!(
        "Repository: {}\nDescription: {}\nLanguage: {}\nTopics: {}\nLicense: {}\nTags: {}\nNote: {}",
        repo.full_name,
        repo.description.as_deref().unwrap_or(""),
        repo.language.as_deref().unwrap_or(""),
        repo.topics.join(", "),
        repo.license.as_deref().unwrap_or(""),
        repo.tags.join(", "),
        repo.note
    );
    if !repo.summary.is_empty() {
        text.push_str("\nSaved summary: ");
        text.push_str(&repo.summary);
    }
    text
}

pub(super) fn body(repo: &Repo, cap: usize) -> String {
    let mut text = clean_readme(&repo.readme, cap);
    for (label, content) in [
        ("Saved code", &repo.code_context),
        ("Recent commits", &repo.commit_index),
    ] {
        if !content.trim().is_empty() {
            text.push_str(&format!("\n{label}: {content}"));
        }
    }
    text
}

fn omitted_heading(text: &str) -> bool {
    let heading = text.trim().to_lowercase();
    let exact = [
        "toc",
        "contents",
        "getting installed",
        "licenses",
        "licence",
        "contribution",
        "credits",
        "目录",
        "目錄",
        "變更日誌",
    ];
    exact.contains(&heading.as_str())
        || [
            "table of contents",
            "installation",
            "install",
            "license",
            "licence",
            "contributing",
            "contribute",
            "sponsor",
            "changelog",
            "change log",
            "release notes",
            "安装",
            "安裝",
            "许可证",
            "授權",
            "贡献",
            "貢獻",
            "赞助",
            "贊助",
            "更新日志",
            "变更日志",
        ]
        .into_iter()
        .any(|prefix| {
            heading.strip_prefix(prefix).is_some_and(|tail| {
                !prefix.is_ascii()
                    || tail.is_empty()
                    || tail.starts_with(|c: char| !c.is_alphanumeric())
            })
        })
}

/// Navigation tables often contain nothing but links. Keep tables with descriptive cells.
fn link_directory(table: &NodeRef) -> bool {
    let Ok(cells) = table.select("td") else {
        return false;
    };
    let mut has_link = false;
    for cell in cells {
        let cell = cell.as_node();
        for node in cell.descendants() {
            if node
                .as_element()
                .is_some_and(|e| e.name.local.as_ref() == "a")
            {
                has_link = true;
            }
            if let Some(text) = node.as_text()
                && text.borrow().chars().any(char::is_alphanumeric)
                && !node
                    .ancestors()
                    .take_while(|p| p != cell)
                    .any(|p| p.as_element().is_some_and(|e| e.name.local.as_ref() == "a"))
            {
                return false;
            }
        }
    }
    has_link
}

fn clean_readme(readme: &str, cap: usize) -> String {
    if cap == 0 || readme.trim().is_empty() {
        return String::new();
    }
    let mut markup = String::new();
    html::push_html(&mut markup, Parser::new_ext(readme, Options::ENABLE_TABLES));
    let root = kuchikiki::parse_html().one(markup).document_node;
    let mut nodes = vec![root];
    let mut output = String::new();
    let mut omitted = None;
    while let Some(node) = nodes.pop() {
        if let Some(element) = node.as_element() {
            let name = element.name.local.as_ref();
            if matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                let level = name.as_bytes()[1];
                if omitted.is_some_and(|outer| level <= outer) {
                    omitted = None;
                }
                if omitted.is_none() && omitted_heading(&node.text_contents()) {
                    omitted = Some(level);
                }
                output.push(' ');
            }
            if matches!(
                name,
                "img" | "svg" | "head" | "script" | "style" | "pre" | "nav"
            ) || (name == "table" && link_directory(&node))
            {
                continue;
            }
            if matches!(name, "br" | "p" | "div" | "li" | "td" | "th") {
                output.push(' ');
            }
        } else if let Some(text) = node.as_text()
            && omitted.is_none()
        {
            output.push_str(&text.borrow());
        }
        nodes.extend(node.children().rev());
    }
    let normalized = output.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = normalized
        .char_indices()
        .nth(cap)
        .map_or(normalized.len(), |(i, _)| i);
    normalized[..end].trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keeps_html_descriptions_and_skips_link_directories() {
        let raw = "<div>A <b>small</b> parser.</div>\n\n| Packages |\n| --- |\n| [wheel](https://example.com) |\n\n| Format | Purpose |\n| --- | --- |\n| [JSON](url) | config |";
        assert_eq!(
            clean_readme(raw, 100),
            "A small parser. Format Purpose JSON config"
        );
    }

    #[test]
    fn drops_boilerplate_and_preserves_unicode_and_link_text() {
        let raw = "[![badge](b)](a)\n\n<!-- hidden -->\n## 目录\n[使用](#usage)\n## 安装方式\n通用安装说明\n### 依赖\n依赖说明\n## 使用\n[项目](https://example.com) 与 **图谱**\n\n~~~sh\ncommand\n~~~\n\n## License\nMIT";
        assert_eq!(clean_readme(raw, 4000), "使用 项目 与 图谱");
        assert_eq!(clean_readme(raw, 5), "使用 项目");
    }
}
