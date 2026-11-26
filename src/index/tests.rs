use super::*;
use crate::test_http::{Reply, Server};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn repo(id: i64, year: i32, description: &str, language: &str) -> Repo {
    Repo {
        id,
        full_name: format!("sample/repo{id}"),
        description: Some(description.into()),
        language: Some(language.into()),
        topics: Vec::new(),
        stargazers_count: 12,
        starred_at: format!("{year}-03-01T00:00:00Z"),
        html_url: format!("https://github.com/sample/repo{id}"),
        archived: false,
        private: Some(false),
        tags: Vec::new(),
        note: String::new(),
        pinned: false,
        score: None,
        readme: String::new(),
        license: None,
        pushed_at: None,
        default_branch: None,
    }
}

#[test]
fn incremental_projection_recovers_and_filters_before_ranking() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let year = Utc::now().year();
    let mut a = repo(103, year - 1, "network proxy", "Rust");
    let b = repo(205, year - 1, "editor", "Rust");
    let c = repo(306, year, "network proxy", "Python");
    for r in [&a, &b, &c] {
        store::upsert(&mut db, r)?;
    }
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Server::new(move |request| {
        assert_eq!(request.target, "/api/embed");
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let input = body["input"].as_array().unwrap();
        let mut embeddings = Vec::new();
        for value in input {
            let s = value.as_str().unwrap();
            if s.starts_with("Repository:") {
                requests.fetch_add(1, Ordering::Relaxed);
            }
            embeddings.push(if s.contains("proxy") || s.contains("代理") {
                vec![1.0, 0.0, 0.0, 0.0]
            } else {
                vec![0.0, 1.0, 0.0, 0.0]
            });
        }
        Reply::json(json!({"embeddings":embeddings}))
    });
    let ollama = Ollama {
        client: Client::builder().no_proxy().build()?,
        url: format!("{}/api/embed", server.url),
        model: DEFAULT_MODEL.into(),
    };
    let build = |db: &mut Db, rebuild| build_with(db, temp.path(), rebuild, &ollama, &|_| {});
    build(&mut db, false)?;
    build(&mut db, false)?;
    assert_eq!(count.load(Ordering::Relaxed), 3);
    let matches = search_with(
        &mut db,
        temp.path(),
        "去年 star 的 Rust 代理工具",
        &Filter::default(),
        4,
        &ollama,
    )?;
    assert_eq!(
        matches.iter().map(|r| r.id).collect::<Vec<_>>(),
        [a.id, b.id]
    );
    assert_eq!(matches[0].score, Some(1.0));

    let filter = Filter {
        language: Some("Python".into()),
        ..Filter::default()
    };
    assert_eq!(
        search_with(&mut db, temp.path(), "Rust proxy", &filter, 1, &ollama)?[0].id,
        c.id
    );
    a.readme = "Detailed usage".into();
    store::upsert(&mut db, &a)?;
    assert!(
        search_with(
            &mut db,
            temp.path(),
            "proxy",
            &Filter::default(),
            1,
            &ollama
        )
        .is_err()
    );
    build(&mut db, false)?;
    assert_eq!(count.load(Ordering::Relaxed), 4);

    let file = artifact_path(temp.path(), &store::owner(&mut db)?, generation(&mut db)?);
    fs::write(file, b"corrupt")?;
    build(&mut db, false)?;
    assert_eq!(count.load(Ordering::Relaxed), 4);
    store::deactivate_missing(&mut db, &[b.id, c.id].into_iter().collect())?;
    build(&mut db, false)?;
    let ids = embeddings::table
        .select(embeddings::id)
        .order(embeddings::id)
        .load::<i64>(&mut db)?;
    assert_eq!(ids, [b.id, c.id]);
    build(&mut db, true)?;
    assert_eq!(count.load(Ordering::Relaxed), 6);

    // The same GitHub ID can have different private descriptions and vectors for two owners.
    let mut other = store::connect_as(temp.path(), "another-user")?;
    let private = repo(b.id, year, "private proxy", "Rust");
    store::upsert(&mut other, &private)?;
    build(&mut other, true)?;
    assert_eq!(
        search_with(
            &mut other,
            temp.path(),
            "proxy",
            &Filter::default(),
            5,
            &ollama
        )?[0]
            .description,
        private.description
    );
    let original = search_with(
        &mut db,
        temp.path(),
        "proxy",
        &Filter::default(),
        5,
        &ollama,
    )?;
    assert_eq!(original.len(), 2);
    assert!(
        original
            .iter()
            .all(|r| r.description != private.description)
    );
    let current = generation(&mut db)?;
    let editing = std::cell::RefCell::new(store::connect(temp.path())?);
    let error = build_with(&mut db, temp.path(), true, &ollama, &|stage| {
        if stage.starts_with("正在写入") {
            store::edit(
                &mut editing.borrow_mut(),
                store::Edit {
                    id: b.id,
                    note: Some("A new use case".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
    })
    .unwrap_err();
    assert!(error.to_string().contains("收藏资料已更新"));
    assert_eq!(generation(&mut db)?, current);
    build(&mut db, false)?;
    Ok(())
}

#[test]
fn switching_models_requires_rebuild_and_invalid_embeddings_preserve_index() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let mut unknown = repo(1, 2025, "proxy", "unused");
    unknown.language = None;
    store::upsert(&mut db, &unknown)?;
    store::upsert(&mut db, &repo(2, 2024, "test editor", "Python"))?;
    let mode = Arc::new(AtomicUsize::new(0));
    let switch = mode.clone();
    let server = Server::new(move |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut vectors: Vec<_> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|_| vec![1.0, 0.0])
            .collect();
        let mut model = body["model"].as_str().unwrap();
        match switch.load(Ordering::Relaxed) {
            1 => vectors[0].push(0.0),
            2 => vectors.clear(),
            3 => vectors[0] = vec![0.0, 0.0],
            4 => model = "unexpected-model",
            _ => {}
        }
        Reply::json(json!({"model":model,"embeddings":vectors}))
    });
    let mut ollama = Ollama {
        client: Client::builder().no_proxy().build()?,
        url: format!("{}/api/embed", server.url),
        model: DEFAULT_MODEL.into(),
    };
    build_with(&mut db, temp.path(), false, &ollama, &|_| {})?;
    let current = generation(&mut db)?;
    let filter = Filter {
        language: Some("未标注".into()),
        ..Filter::default()
    };
    assert_eq!(
        search_with(&mut db, temp.path(), "工具", &filter, 10, &ollama)?[0].id,
        unknown.id
    );
    for invalid in 1..=4 {
        mode.store(invalid, Ordering::Relaxed);
        assert!(build_with(&mut db, temp.path(), true, &ollama, &|_| {}).is_err());
        assert_eq!(generation(&mut db)?, current);
    }
    mode.store(1, Ordering::Relaxed);
    assert!(
        search_with(&mut db, temp.path(), "query", &filter, 10, &ollama)
            .unwrap_err()
            .to_string()
            .contains("--rebuild")
    );
    mode.store(0, Ordering::Relaxed);
    ollama.model = "replacement".into();
    assert!(
        build_with(&mut db, temp.path(), false, &ollama, &|_| {})
            .unwrap_err()
            .to_string()
            .contains("--rebuild")
    );
    build_with(&mut db, temp.path(), true, &ollama, &|_| {})?;
    assert_eq!(
        store::meta(&mut db, "index_model")?.as_deref(),
        Some("replacement")
    );
    assert_eq!(
        search_with(&mut db, temp.path(), "query", &filter, 1, &ollama)?.len(),
        1
    );
    Ok(())
}

#[test]
fn semantic_candidates_use_current_owners_list_members_before_ranking() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    for id in 1..=3 {
        store::upsert(&mut db, &repo(id, 2025, "test", "Rust"))?;
    }
    db.batch_execute("INSERT INTO github_lists VALUES ('primary','list-id','Research','1','1',3),('other','list-id','Research','1','1',1);
        INSERT INTO github_list_members VALUES ('primary','list-id',1),('primary','list-id',3),('primary','list-id',99),('other','list-id',2)")?;
    store::deactivate_missing(&mut db, &[1, 2].into_iter().collect())?;
    let filter = Filter {
        list: Some("research".into()),
        ..Filter::default()
    };
    assert_eq!(allowed_ids(&mut db, "primary", &filter)?, [1]);
    Ok(())
}
