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

// Give interruption and configuration-race fixtures deterministic request boundaries.
fn build_small_batches(db: &mut Db, rebuild: bool, phase: &dyn Fn(&str)) -> Result<String> {
    let rebuild = prepare_build(db, rebuild)?;
    let mut engine = Engine::configured(db, Duration::from_secs(10))?;
    let Runtime::Ollama(_, batches) = &mut engine.runtime else {
        unreachable!()
    };
    *batches = RemoteBatches {
        size: BATCH,
        concurrent: 1,
    };
    let dir = store::data_dir(db)?;
    build_observed_with(db, &dir, rebuild, &engine, phase, &|_| {})
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
    a.readme = "installation and quickstart ".repeat(110);
    let mut b = a.clone();
    b.id = 2;
    b.full_name = "sample/photos".into();
    b.readme.push_str("Find and deduplicate photographs.");
    store::upsert(&mut db, &a)?;
    store::upsert(&mut db, &b)?;
    build_small_batches(&mut db, false, &|_| {})?;
    assert!(!needs_rebuild(&mut db)?);
    let n = calls.load(Ordering::Relaxed);
    assert!(n > 2);
    assert_eq!(embeddings::table.count().get_result::<i64>(&mut db)?, 2);
    build_small_batches(&mut db, false, &|_| {})?;
    assert_eq!(calls.load(Ordering::Relaxed), n);
    let ranks = search(&mut db, "photographs", &Filter::default(), 2)?;
    assert_eq!(ranks[0].id, b.id);
    assert!(ranks[0].score > ranks[1].score);

    let generation_before = generation(&mut db)?;
    let writer = std::cell::RefCell::new(store::connect(home.path())?);
    let err = build_small_batches(&mut db, true, &|stage| {
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
    build_small_batches(&mut db, false, &|_| {})?;
    let original_generation = generation(&mut db)?;
    let original_identity = store::meta(&mut db, "index_identity")?;
    assert!(count.load(Ordering::SeqCst) >= 2);
    count.store(0, Ordering::SeqCst);
    change.store(true, Ordering::SeqCst);
    let err = build_small_batches(&mut db, true, &|_| {}).unwrap_err();
    assert!(err.is::<BuildConflict>());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(generation(&mut db)?, original_generation);
    assert_eq!(store::meta(&mut db, "index_identity")?, original_identity);
    assert!(needs_rebuild(&mut db)?);
    assert!(
        build_small_batches(&mut db, false, &|_| {})
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
    build_small_batches(&mut db, true, &|_| {})?;
    assert_ne!(store::meta(&mut db, "index_identity")?, original_identity);
    assert_eq!(
        search(&mut db, "fixture", &Filter::default(), 1)?[0].id,
        long.id
    );
    Ok(())
}

#[test]
fn old_ollama_index_requires_structured_rebuild_and_retains_searchable_annotations() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let owner = store::owner(&mut db)?;
    let historical_model = std::env::var("STARSTACK_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "older-custom-embedding:v1".into());
    let item = repo(19, 2025, "legacy repository", "Rust");
    store::upsert(&mut db, &item)?;
    let item = store::edit(
        &mut db,
        store::Edit {
            id: item.id,
            note: Some("archival fixture".into()),
            pinned: Some(true),
            ..Default::default()
        },
    )?;
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
    assert!(needs_rebuild(&mut db)?);
    let initial_file = fs::read(&path)?;
    assert!(
        build(&mut db, false, &|_| {})
            .unwrap_err()
            .to_string()
            .contains("--rebuild")
    );
    let query = crate::library::Query {
        query: "archival".into(),
        filter: Filter::default(),
        limit: Some(1),
    };
    let (rows, keyword) = crate::library::interactive_search(&mut db, &query)?;
    assert!(keyword);
    assert_eq!(
        serde_json::to_value(status::read(&mut db)?)?["state"],
        "needs_attention"
    );
    assert_eq!(rows.as_slice(), std::slice::from_ref(&item));
    assert_eq!(generation(&mut db)?, 1);
    assert_eq!(fs::read(path)?, initial_file);
    let (raw, reference) = embeddings::table
        .find((&owner, item.id))
        .select((embeddings::vector, embeddings::checksum))
        .first::<(Vec<u8>, Vec<u8>)>(&mut db)?;
    assert!(raw.is_empty());
    assert_eq!(
        Payloads::new(home.path(), &owner).read(raw, &reference)?,
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

#[test]
fn rebuild_resumes_only_verified_checkpoints_after_interruption() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let server = Server::new(move |request| {
        let attempt = requests.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let entries = body["input"].as_array().unwrap().len();
        if attempt == 1 {
            Reply::status(500)
        } else {
            Reply::json(
                json!({"embeddings": vec![[1.0, 0.0]; entries], "prompt_eval_count": entries * 10}),
            )
        }
    });
    crate::ollama_settings::set_endpoints(&mut db, std::slice::from_ref(&server.url))?;
    crate::ollama_settings::set(&mut db, Some(&server.url), Some(DEFAULT_MODEL))?;
    set_backend(&mut db, "ollama")?;
    for id in 1..=20 {
        store::upsert(&mut db, &repo(id, 2025, "fixture", "Rust"))?;
    }
    assert!(build_small_batches(&mut db, true, &|_| {}).is_err());
    assert_eq!(generation(&mut db)?, 0);
    let partial = serde_json::to_value(status::read(&mut db)?)?;
    assert_eq!(partial["projection_bytes"], 0);
    assert_eq!(partial["legacy_payload_bytes"], 0);
    assert!(partial["vector_payload_bytes"].as_u64().unwrap_or_default() > 0);
    assert_eq!(partial["checkpoints"], 16);

    assert_eq!(
        embedding_checkpoints::table
            .count()
            .get_result::<i64>(&mut db)?,
        16
    );
    // Simulate upgrading a v0.2.6 account that stopped after its first SQLite checkpoint batch.
    let legacy = vector_bytes(&[1.0, 0.0]);
    diesel::update(embedding_checkpoints::table)
        .set((
            embedding_checkpoints::vector.eq(&legacy),
            embedding_checkpoints::checksum.eq(Sha256::digest(&legacy).to_vec()),
        ))
        .execute(&mut db)?;
    let owner = store::owner(&mut db)?;
    let legacy_dir = home
        .path()
        .join(format!("vector-payloads-{:x}", Sha256::digest(&owner)));
    fs::remove_dir_all(legacy_dir)?;
    // These versions predate independently persisted metadata/body checkpoints.
    diesel::delete(parts::embedding_parts::table).execute(&mut db)?;
    store::delete_meta(&mut db, "index_payload_storage")?;
    drop(db);

    let mut reopened = store::connect(home.path())?;
    assert!(pending_rebuild(&mut reopened)?);
    // A modified document and a corrupted legacy checkpoint must both be recomputed.
    store::upsert(&mut reopened, &repo(1, 2025, "changed", "Rust"))?;
    diesel::update(embedding_checkpoints::table.filter(embedding_checkpoints::id.eq(2)))
        .set(embedding_checkpoints::checksum.eq(vec![0]))
        .execute(&mut reopened)?;
    let last = std::cell::Cell::new(None);
    crate::index_worker::build(&mut reopened, false, &|_| {}, &|counts| {
        last.set(Some(counts))
    })?;
    let progress = last.get().context("missing resumed progress")?;
    assert_eq!(
        (
            progress.resumed,
            progress.repositories,
            progress.chunks,
            progress.tokens
        ),
        (14, 6, 6, Some(60))
    );
    assert_eq!(generation(&mut reopened)?, 1);
    let status = serde_json::to_value(status::read(&mut reopened)?)?;
    assert_eq!(status["state"], "ready");
    assert_eq!(status["indexed"], 20);
    assert!(status["projection_bytes"].as_u64().unwrap_or_default() > 0);
    assert!(status["vector_payload_bytes"].as_u64().unwrap_or_default() > 0);
    assert_eq!(status["legacy_payload_bytes"], 0);
    assert!(status["last_published_at"].is_string());

    assert_eq!(
        embeddings::table.count().get_result::<i64>(&mut reopened)?,
        20
    );
    assert_eq!(
        embedding_checkpoints::table
            .count()
            .get_result::<i64>(&mut reopened)?,
        0
    );
    assert!(store::meta(&mut reopened, "index_pending")?.is_none());
    assert!(!pending_rebuild(&mut reopened)?);
    assert_eq!(count.load(Ordering::SeqCst), 3);
    Ok(())
}

#[test]
fn collection_is_searchable_with_filters_before_any_semantic_index() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut account = store::connect(home.path())?;
    let mut other = store::connect_as(home.path(), "other")?;
    let rust = repo(1, 2025, "Rust source parser", "Rust");
    let private_note = repo(2, 2025, "Source explorer", "Go");
    store::upsert(&mut account, &rust)?;
    store::upsert(&mut account, &private_note)?;
    store::upsert(&mut other, &repo(3, 2025, "Rust source toolkit", "Rust"))?;
    store::edit(
        &mut account,
        store::Edit {
            id: private_note.id,
            note: Some("Rust fixture note".into()),
            ..Default::default()
        },
    )?;

    let q = crate::library::Query {
        query: "rust".into(),
        limit: Some(10),
        filter: Filter::default(),
    };
    let (matches, keyword) = crate::library::interactive_search(&mut account, &q)?;
    assert!(keyword);
    assert_eq!(matches.iter().map(|r| r.id).collect::<Vec<_>>(), [1]);
    let q = crate::library::Query {
        filter: Filter {
            language: Some("Go".into()),
            ..Default::default()
        },
        ..q
    };
    assert_eq!(
        crate::library::interactive_search(&mut account, &q)?.0[0].id,
        2
    );
    let fresh = serde_json::to_value(status::read(&mut account)?)?;
    assert_eq!(fresh["state"], "unbuilt");
    assert_eq!(fresh["repositories"], 2);
    assert_eq!(fresh["indexed"], 0);
    Ok(())
}

#[test]
fn v3_migration_indexes_live_rows_and_preserves_each_owners_annotations() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("starstack.sqlite3");
    let mut old = Db::establish(path.to_str().unwrap())?;
    old.batch_execute(include_str!("../schema.sql"))?;
    old.batch_execute(include_str!("../lists_schema.sql"))?;
    old.batch_execute(include_str!("../commits_schema.sql"))?;
    old.batch_execute(
        "ALTER TABLE repositories ADD COLUMN summary TEXT NOT NULL DEFAULT '';
        ALTER TABLE repositories ADD COLUMN code_context TEXT NOT NULL DEFAULT '';
        ALTER TABLE repositories ADD COLUMN commit_index TEXT NOT NULL DEFAULT '';
        PRAGMA user_version=3",
    )?;
    for (owner, id, note, active) in [
        ("primary", 8, "archival parser", true),
        ("other", 8, "private compiler", true),
        ("primary", 9, "archival removed", false),
    ] {
        use store::repositories as r;
        diesel::insert_into(r::table)
            .values((
                r::owner_id.eq(owner),
                r::id.eq(id),
                r::active.eq(active),
                r::metadata.eq(serde_json::to_string(&repo(id, 2025, "example", "Rust"))?),
                r::note.eq(note),
                r::pinned.eq(true),
            ))
            .execute(&mut old)?;
    }
    drop(old);
    let mut db = store::connect(home.path())?;
    #[derive(QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let indexed = |db: &mut Db, word: &str| -> Result<i64> {
        Ok(diesel::sql_query("SELECT count(*) AS n FROM repository_fts WHERE repository_fts MATCH ? AND owner='primary'")
            .bind::<diesel::sql_types::Text, _>(word).get_result::<Count>(db)?.n)
    };
    assert_eq!(indexed(&mut db, "archival")?, 1);
    assert_eq!(indexed(&mut db, "compiler")?, 0);
    let original = store::by_id(&mut db, 8)?;
    assert!(original.pinned);
    assert_eq!(original.note, "archival parser");
    store::edit(
        &mut db,
        store::Edit {
            id: 8,
            note: Some("local handbook".into()),
            ..Default::default()
        },
    )?;
    assert_eq!(indexed(&mut db, "archival")?, 0);
    assert_eq!(indexed(&mut db, "handbook")?, 1);
    let mut other = store::connect_as(home.path(), "other")?;
    assert_eq!(store::by_id(&mut other, 8)?.note, "private compiler");
    Ok(())
}

#[test]
fn structured_parts_reuse_only_unchanged_inputs_and_drop_removed_bodies() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let captured = requests.clone();
    let server = Server::new(move |request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let inputs = body["input"].as_array().unwrap();
        captured
            .lock()
            .unwrap()
            .extend(inputs.iter().map(|v| v.as_str().unwrap().to_owned()));
        Reply::json(json!({"embeddings": vec![[1.0, 0.0]; inputs.len()]}))
    });
    crate::ollama_settings::set_endpoints(&mut db, std::slice::from_ref(&server.url))?;
    crate::ollama_settings::set(&mut db, Some(&server.url), Some("fixture"))?;
    set_backend(&mut db, "ollama")?;
    let mut item = repo(1, 2025, "parser", "Rust");
    item.readme = "Original guide".into();
    store::upsert(&mut db, &item)?;
    let build_inputs = |db: &mut Db, rebuild| -> Result<Vec<String>> {
        build(db, rebuild, &|_| {})?;
        Ok(std::mem::take(&mut *requests.lock().unwrap()))
    };
    assert_eq!(build_inputs(&mut db, false)?.len(), 2);
    store::edit(
        &mut db,
        store::Edit {
            id: item.id,
            note: Some("my bookmark".into()),
            ..Default::default()
        },
    )?;
    let q = crate::library::Query {
        query: "bookmark".into(),
        filter: Filter::default(),
        limit: Some(2),
    };
    assert!(
        crate::library::search_published(&mut db, &q)
            .unwrap_err()
            .is::<Stale>()
    );
    let changed = build_inputs(&mut db, false)?;
    assert_eq!(changed.len(), 1);
    assert!(changed[0].contains("my bookmark"));
    item.readme = "Revised guide".into();
    store::upsert(&mut db, &item)?;
    assert_eq!(build_inputs(&mut db, false)?, ["Revised guide"]);
    assert_eq!(build_inputs(&mut db, true)?.len(), 2);
    item.readme.clear();
    store::upsert(&mut db, &item)?;
    assert!(build_inputs(&mut db, false)?.is_empty());
    use parts::embedding_parts as t;
    assert_eq!(t::table.select(t::part).load::<i32>(&mut db)?, [0]);
    Ok(())
}

#[test]
fn metadata_generation_is_searchable_while_prioritized_bodies_resume_after_edits() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let body_batches = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
    let received = body_batches.clone();
    let server = Server::new(move |request| {
        let data: Value = serde_json::from_slice(&request.body).unwrap();
        let inputs = data["input"].as_array().unwrap();
        let bodies: Vec<_> = inputs
            .iter()
            .filter_map(Value::as_str)
            .filter(|text| text.starts_with("guide-"))
            .map(str::to_owned)
            .collect();
        if !bodies.is_empty() {
            let mut batches = received.lock().unwrap();
            batches.push(bodies);
            if batches.len() == 2 {
                return Reply::status(500);
            }
        }
        let rows: Vec<_> = inputs
            .iter()
            .filter_map(Value::as_str)
            .map(|input| {
                if input.starts_with("Repository:") {
                    [1.0, 0.0]
                } else {
                    [0.0, 1.0]
                }
            })
            .collect();
        Reply::json(json!({"embeddings": rows}))
    });
    crate::ollama_settings::set_endpoints(&mut db, std::slice::from_ref(&server.url))?;
    crate::ollama_settings::set(&mut db, Some(&server.url), Some("fixture"))?;
    set_backend(&mut db, "ollama")?;
    let item = |id: i64| {
        let mut repo = repo(id, 2000 + id as i32, "example", "Rust");
        repo.readme = format!("guide-{id}");
        repo
    };
    for id in 1..=18 {
        store::upsert(&mut db, &item(id))?;
    }
    store::edit(
        &mut db,
        store::Edit {
            id: 1,
            pinned: Some(true),
            ..Default::default()
        },
    )?;
    assert!(build_small_batches(&mut db, false, &|_| {}).is_err());
    assert_eq!(generation(&mut db)?, 1);
    assert_eq!(
        embedding_checkpoints::table
            .count()
            .get_result::<i64>(&mut db)?,
        16
    );
    assert!(store::meta(&mut db, "index_pending")?.is_some());
    assert_eq!(
        serde_json::to_value(status::read(&mut db)?)?["body_pending"],
        true
    );
    let q = crate::library::Query {
        query: "probe".into(),
        filter: Filter::default(),
        limit: Some(50),
    };
    let first = crate::library::search_published(&mut db, &q)?;
    assert_eq!(first.len(), 18);
    assert!(first.iter().all(|row| row.score == Some(0.0)));
    assert_eq!(
        body_batches.lock().unwrap()[0][..3],
        ["guide-1", "guide-18", "guide-17"]
    );
    drop(db);

    let mut db = store::connect(home.path())?;
    store::deactivate_missing(&mut db, &(2..=18).collect())?;
    store::edit(
        &mut db,
        store::Edit {
            id: 18,
            note: Some("edited".into()),
            ..Default::default()
        },
    )?;
    store::edit(
        &mut db,
        store::Edit {
            id: 3,
            pinned: Some(true),
            ..Default::default()
        },
    )?;
    let mut changed = item(17);
    changed.readme.push_str("-revised");
    store::upsert(&mut db, &changed)?;
    store::upsert(&mut db, &item(19))?;
    build_small_batches(&mut db, false, &|_| {})?;
    assert_eq!(generation(&mut db)?, 2);
    assert_eq!(
        serde_json::to_value(status::read(&mut db)?)?["body_pending"],
        false
    );
    assert!(store::meta(&mut db, "index_pending")?.is_none());
    let batches = body_batches.lock().unwrap();
    let resumed: Vec<_> = batches[2..].iter().flatten().map(String::as_str).collect();
    assert_eq!(
        resumed,
        ["guide-3", "guide-19", "guide-17-revised", "guide-2"]
    );
    drop(batches);
    let complete = crate::library::search_published(&mut db, &q)?;
    assert_eq!(complete.len(), 18);
    assert!(
        complete
            .iter()
            .all(|row| row.id != 1 && row.score.is_some_and(|score| score > 0.6))
    );
    assert_eq!(
        complete.iter().find(|row| row.id == 18).unwrap().note,
        "edited"
    );
    Ok(())
}

#[test]
fn provider_progress_retains_submillisecond_work_and_reads_old_worker_events() -> Result<()> {
    let mut counts: Progress = serde_json::from_value(json!({
        "repositories":0, "total":2, "reused":0, "resumed":0, "chunks":0,
        "tokens":0, "elapsed_seconds":0
    }))?;
    assert_eq!(counts.checkpoint_ms, 0);
    for _ in 0..3 {
        counts.add_embedding(EmbeddingWork {
            tokens: Some(7),
            tokenizer: Duration::from_micros(400),
            inference: Duration::from_micros(700),
        });
    }
    assert_eq!(
        (counts.tokenizer_ms, counts.inference_ms, counts.tokens),
        (1, 2, Some(21))
    );
    Ok(())
}

#[test]
fn remote_embeddings_keep_order_across_requests_and_sum_token_usage() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut db = store::connect(dir.path())?;
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let server = Server::new(move |request| {
        count.fetch_add(1, Ordering::Relaxed);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let texts = body["input"].as_array().unwrap();
        assert!(texts.len() <= 2);
        let vectors: Vec<_> = texts
            .iter()
            .map(|s| vec![s.as_str().unwrap().parse::<f32>().unwrap()])
            .collect();
        Reply::json(json!({"embeddings": vectors,"prompt_eval_count":texts.len()*3}))
    });
    let mut engine = Engine::fixture(&server.url, DEFAULT_MODEL)?;
    if let Runtime::Ollama(_, batches) = &mut engine.runtime {
        *batches = RemoteBatches {
            size: 2,
            concurrent: 2,
        };
    }
    let mut progress = Progress {
        tokens: Some(0),
        ..Progress::default()
    };
    let result = engine.embed(&mut db, &["1", "2", "3", "4"], &mut |work| {
        progress.add_embedding(work)
    })?;
    assert_eq!(result, [[1.0], [2.0], [3.0], [4.0]]);
    assert_eq!(requests.load(Ordering::Relaxed), 2);
    assert_eq!(progress.tokens, Some(12));
    Ok(())
}
