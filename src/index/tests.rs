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
        summary: String::new(),
        code_context: String::new(),
        commit_index: String::new(),
        pinned: false,
        score: None,
        readme: String::new(),
        license: None,
        pushed_at: None,
        default_branch: None,
    }
}

#[test]
fn index_progress_reports_new_and_reused_embeddings() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut db = store::connect(temp.path())?;
    let year = Utc::now().year();
    let first = repo(101, year, "first", "Rust");
    let mut second = repo(202, year, "second", "Rust");
    store::upsert(&mut db, &first)?;

    let server = Server::new(move |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let input = body["input"].as_array().unwrap();
        let mut response = json!({"embeddings": vec![vec![1.0, 0.0, 0.0, 0.0]; input.len()]});
        if !input
            .iter()
            .any(|text| text.as_str().unwrap().contains("no token count"))
        {
            response["prompt_eval_count"] = json!(input.len() * 17);
        }
        Reply::json(response)
    });
    let engine = Engine::fixture(&server.url, DEFAULT_MODEL)?;
    let observe_build = |db: &mut Db| -> Result<Progress> {
        let last = std::cell::Cell::new(None);
        build_observed_with(db, temp.path(), false, &engine, &|_| {}, &|counts| {
            last.set(Some(counts))
        })?;
        last.get().context("missing index progress")
    };

    let initial = observe_build(&mut db)?;
    assert_eq!(
        (
            initial.repositories,
            initial.total,
            initial.reused,
            initial.chunks,
            initial.tokens
        ),
        (1, 1, 0, 1, Some(17))
    );
    let noop = observe_build(&mut db)?;
    assert_eq!(
        (
            noop.repositories,
            noop.total,
            noop.reused,
            noop.chunks,
            noop.tokens
        ),
        (0, 0, 1, 0, Some(0))
    );
    second.description = Some("no token count".into());
    store::upsert(&mut db, &second)?;
    store::set_meta(&mut db, "index_pending", "1")?;
    let partial = observe_build(&mut db)?;
    assert_eq!(
        (
            partial.repositories,
            partial.total,
            partial.reused,
            partial.chunks,
            partial.tokens
        ),
        (1, 1, 1, 1, None)
    );
    assert!(store::meta(&mut db, "index_pending")?.is_none());
    Ok(())
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
    let ollama = Engine::fixture(&server.url, DEFAULT_MODEL)?;
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
    let active_matches = search_with(
        &mut db,
        temp.path(),
        "proxy",
        &Filter::default(),
        10,
        &ollama,
    )?;
    assert!(!active_matches.iter().any(|hit| hit.id == a.id));
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
    assert!(error.is::<BuildConflict>());
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
    let mut ollama = Engine::fixture(&server.url, DEFAULT_MODEL)?;
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
    ollama = Engine::fixture(&server.url, "replacement")?;
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

#[test]
fn chunked_index_ranks_readme_suffix_reuses_vectors_and_rejects_changed_settings() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let calls = Arc::new(AtomicUsize::new(0));
    let received = calls.clone();
    let server = Server::new(move |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let inputs = body["input"].as_array().unwrap();
        assert!(inputs.len() <= BATCH);
        received.fetch_add(inputs.len(), Ordering::Relaxed);
        let rows: Vec<_> = inputs
            .iter()
            .map(|x| {
                if x.as_str().unwrap().contains("photographs") {
                    [0.0, 1.0]
                } else {
                    [1.0, 0.0]
                }
            })
            .collect();
        Reply::json(json!({"embeddings":rows}))
    });
    let other_endpoint = "http://127.0.0.1:18281";
    crate::ollama_settings::set_endpoints(&mut db, &[server.url.clone(), other_endpoint.into()])?;
    crate::ollama_settings::set(&mut db, Some(&server.url), Some("fixture"))?;
    set_backend(&mut db, "ollama")?;
    let mut a = repo(1, 2025, "library", "Rust");
    a.readme = "installation and quickstart ".repeat(170);
    let mut b = a.clone();
    b.id = 2;
    b.full_name = "sample/photos".into();
    b.readme.push_str("Find and deduplicate photographs.");
    store::upsert(&mut db, &a)?;
    store::upsert(&mut db, &b)?;
    build(&mut db, false, &|_| {})?;
    assert!(!needs_rebuild(&mut db)?);
    let n = calls.load(Ordering::Relaxed);
    assert!(n > 2);
    assert_eq!(embeddings::table.count().get_result::<i64>(&mut db)?, 2);
    build(&mut db, false, &|_| {})?;
    assert_eq!(calls.load(Ordering::Relaxed), n);
    let ranks = search(&mut db, "photographs", &Filter::default(), 2)?;
    assert_eq!(ranks[0].id, b.id);
    assert!(ranks[0].score > ranks[1].score);

    let generation_before = generation(&mut db)?;
    let writer = std::cell::RefCell::new(store::connect(home.path())?);
    let err = build(&mut db, true, &|stage| {
        if stage.starts_with("正在写入") {
            crate::ollama_settings::set(&mut writer.borrow_mut(), Some(other_endpoint), None)
                .unwrap();
        }
    })
    .unwrap_err();
    assert!(err.is::<BuildConflict>());
    assert_eq!(generation(&mut db)?, generation_before);
    assert!(needs_rebuild(&mut db)?);
    assert!(
        search(&mut db, "photographs", &Filter::default(), 2)
            .unwrap_err()
            .to_string()
            .contains("--rebuild")
    );
    crate::ollama_settings::set(&mut db, Some(&server.url), None)?;
    assert!(!needs_rebuild(&mut db)?);
    Ok(())
}

#[test]
fn legacy_ollama_identity_matches_only_its_default_endpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    let legacy = Selection {
        provider: Provider::Ollama {
            url: DEFAULT_OLLAMA_URL.to_owned(),
            model: "legacy-model".into(),
        },
        policy: crate::chunking::Policy::whole_document(),
    };
    store::set_meta(&mut db, "index_model", "legacy-model")?;
    assert!(!legacy.mismatches(&mut db)?);
    let remote = Selection {
        provider: Provider::Ollama {
            url: "https://models.example".into(),
            model: "legacy-model".into(),
        },
        policy: crate::chunking::Policy::whole_document(),
    };
    assert!(remote.mismatches(&mut db)?);
    Ok(())
}

#[test]
fn changing_chunk_policy_during_inference_stops_the_next_batch_and_rejects_search_results()
-> Result<()> {
    fn save_policy(db: &mut Db, preset: &str) -> Result<()> {
        crate::chunking::save(db, serde_json::from_value(json!({"preset":preset}))?)
    }
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let home_path = home.path().to_owned();
    let change = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let during_request = change.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let sent = count.clone();
    let server = Server::new(move |request| {
        sent.fetch_add(1, Ordering::SeqCst);
        if during_request.swap(false, Ordering::SeqCst) {
            let mut writer = store::connect(&home_path).unwrap();
            save_policy(&mut writer, "balanced").unwrap();
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        Reply::json(
            json!({"embeddings": vec![[1.0, 0.0]; body["input"].as_array().unwrap().len()]}),
        )
    });
    crate::ollama_settings::set_endpoints(&mut db, std::slice::from_ref(&server.url))?;
    crate::ollama_settings::set(&mut db, Some(&server.url), Some("fixture"))?;
    set_backend(&mut db, "ollama")?;
    let mut long = repo(17, 2025, "fixture", "Rust");
    long.readme = "0123456789".repeat(420);
    store::upsert(&mut db, &long)?;
    save_policy(&mut db, "short")?;
    build(&mut db, false, &|_| {})?;
    let original_generation = generation(&mut db)?;
    let original_identity = store::meta(&mut db, "index_identity")?;
    assert!(count.load(Ordering::SeqCst) >= 2);
    count.store(0, Ordering::SeqCst);
    change.store(true, Ordering::SeqCst);
    let err = build(&mut db, true, &|_| {}).unwrap_err();
    assert!(err.is::<BuildConflict>());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(generation(&mut db)?, original_generation);
    assert_eq!(store::meta(&mut db, "index_identity")?, original_identity);
    assert!(needs_rebuild(&mut db)?);
    assert!(
        build(&mut db, false, &|_| {})
            .unwrap_err()
            .to_string()
            .contains("--rebuild")
    );
    save_policy(&mut db, "short")?;
    assert!(!needs_rebuild(&mut db)?);
    change.store(true, Ordering::SeqCst);
    assert!(
        search(&mut db, "fixture", &Filter::default(), 1)
            .unwrap_err()
            .is::<BuildConflict>()
    );
    build(&mut db, true, &|_| {})?;
    assert_ne!(store::meta(&mut db, "index_identity")?, original_identity);
    assert_eq!(
        search(&mut db, "fixture", &Filter::default(), 1)?[0].id,
        long.id
    );
    Ok(())
}

#[test]
fn existing_ollama_projection_and_raw_vector_survive_an_incremental_upgrade() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let owner = store::owner(&mut db)?;
    let historical_model = std::env::var("STARSTACK_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "older-custom-embedding:v1".into());
    let item = repo(19, 2025, "legacy repository", "Rust");
    store::upsert(&mut db, &item)?;
    let blob: Vec<_> = [3.0_f32, 4.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect();
    db.batch_execute(SCHEMA)?;
    diesel::insert_into(embeddings::table)
        .values((
            embeddings::owner.eq(&owner),
            embeddings::id.eq(item.id),
            embeddings::fingerprint.eq(fingerprint(&historical_model, &document(&item))),
            embeddings::vector.eq(&blob),
            embeddings::checksum.eq(Sha256::digest(&blob).to_vec()),
        ))
        .execute(&mut db)?;
    let mut identity = ProjectionIdentity::new(1);
    identity.embedding_model = Some(historical_model.clone());
    identity.embedding_version = Some(owner.clone());
    let path = artifact_path(home.path(), &owner, 1);
    let mut writer = ProjectionWriter::create(
        &path,
        ProjectionBuildConfig::new(2, identity)
            .with_metric(ProjectionMetric::Cosine)
            .with_bit_width(RaBitQBitWidth::Four),
    )?;
    writer.push(item.id as u64, &[3.0, 4.0])?;
    writer.finish()?;
    store::set_meta(&mut db, "index_generation", "1")?;
    store::set_meta(&mut db, "index_dimension", "2")?;
    store::set_meta(&mut db, "index_model", &historical_model)?;
    let remote = "http://127.0.0.1:18281";
    crate::ollama_settings::set_endpoints(&mut db, &[DEFAULT_OLLAMA_URL.into(), remote.into()])?;
    crate::ollama_settings::set(&mut db, Some(DEFAULT_OLLAMA_URL), None)?;
    assert_eq!(backend(&mut db)?, "ollama");
    assert!(crate::chunking::selected(&mut db, "ollama")?.legacy());
    assert!(!needs_rebuild(&mut db)?);
    let initial_file = fs::read(&path)?;
    assert!(build(&mut db, false, &|_| {})?.contains("索引已是最新"));
    assert_eq!(generation(&mut db)?, 1);
    assert_eq!(fs::read(path)?, initial_file);
    assert_eq!(
        embeddings::table
            .find((&owner, item.id))
            .select(embeddings::vector)
            .first::<Vec<u8>>(&mut db)?,
        blob
    );

    crate::ollama_settings::set(&mut db, Some(remote), None)?;
    assert!(needs_rebuild(&mut db)?);
    assert!(
        search(&mut db, "fixture", &Filter::default(), 1)
            .unwrap_err()
            .to_string()
            .contains("--rebuild")
    );
    Ok(())
}

#[test]
fn selected_and_published_models_protect_only_the_referenced_artifacts() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut a = store::connect(home.path())?;
    let mut b = store::connect_as(home.path(), "another-user")?;
    let minilm = crate::models::DEFAULT_ID;
    let qwen = crate::models::QWEN_ID;
    store::set_meta(&mut a, "embedding_backend", "builtin")?;
    assert!(ensure_model_unused(&mut b, minilm).is_err());
    ensure_model_unused(&mut b, qwen)?;
    store::set_meta(&mut a, "embedding_builtin_model", qwen)?;
    ensure_model_unused(&mut b, minilm)?;
    assert!(ensure_model_unused(&mut b, qwen).is_err());
    store::set_meta(&mut a, "index_backend", "builtin")?;
    store::set_meta(&mut a, "index_model", minilm)?;
    assert!(ensure_model_unused(&mut b, minilm).is_err());
    store::set_meta(&mut a, "embedding_backend", "ollama")?;
    ensure_model_unused(&mut b, qwen)?;
    Ok(())
}

#[test]
fn repinned_model_keeps_its_name_and_invalidates_the_projection() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let mut selection = Selection {
        provider: Provider::Builtin {
            id: crate::models::DEFAULT_ID.into(),
            family: "minilm",
            fingerprint: crate::models::descriptor(home.path(), crate::models::DEFAULT_ID)?
                .fingerprint,
        },
        policy: crate::chunking::Policy::whole_document(),
    };
    assert_eq!(selection.identity(), crate::native_embedding::IDENTITY);
    store::set_meta(&mut db, "index_identity", &selection.identity())?;
    store::set_meta(&mut db, "index_backend", "builtin")?;
    store::set_meta(&mut db, "index_model", crate::models::DEFAULT_ID)?;
    assert!(!selection.mismatches(&mut db)?);
    if let Provider::Builtin { fingerprint, .. } = &mut selection.provider {
        *fingerprint = "different-tokenizer-checksum".into();
    }
    assert!(selection.mismatches(&mut db)?);
    Ok(())
}
