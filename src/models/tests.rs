use super::*;
use crate::test_http::{Reply, Server};
use reqwest::{Url, blocking::Client};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::registry::{HF_REVISION, MS_REVISION};
fn fetch(server: &Server, loc: &Location) -> Result<()> {
    download::install_files(
        &Client::builder().no_proxy().build()?,
        &|f| Ok(Url::parse(&server.url)?.join(&f.name)?),
        loc,
        &[Artifact::new(
            "weights.bin",
            11,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        )],
        &|_| {},
    )
}

#[test]
fn resumed_install_is_atomic_and_idempotent() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    fs::create_dir(&loc.stage)?;
    fs::write(loc.stage.join("weights.bin"), b"hello ")?;
    let ready = loc.ready.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let srv = Server::new(move |req| {
        assert!(!ready.exists());
        requests.fetch_add(1, Ordering::Relaxed);
        assert_eq!(req.target, "/weights.bin");
        assert!(req.headers.contains("range: bytes=6-"));
        Reply {
            status: 206,
            headers: "Content-Range: bytes 6-10/11\r\n",
            body: "world".into(),
        }
    });
    fetch(&srv, &loc)?;
    assert!(!loc.stage.exists());
    assert_eq!(fs::read(loc.ready.join("weights.bin"))?, b"hello world");
    fetch(&srv, &loc)?;
    assert_eq!(count.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn ignored_range_replaces_partial_contents() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    fs::create_dir(&loc.stage)?;
    fs::write(loc.stage.join("weights.bin"), b"wrong")?;
    let srv = Server::new(|req| {
        assert!(req.headers.contains("range: bytes=5-"));
        Reply {
            status: 200,
            headers: "",
            body: "hello world".into(),
        }
    });
    fetch(&srv, &loc)?;
    assert_eq!(fs::read(loc.ready.join("weights.bin"))?, b"hello world");
    Ok(())
}

#[test]
fn short_response_retries_from_last_byte() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let srv = Server::new(move |req| match requests.fetch_add(1, Ordering::Relaxed) {
        0 => Reply {
            status: 200,
            headers: "",
            body: "hello ".into(),
        },
        1 => {
            assert!(req.headers.contains("range: bytes=6-"));
            Reply {
                status: 206,
                headers: "Content-Range: bytes 6-10/11\r\n",
                body: "world".into(),
            }
        }
        _ => panic!("unexpected request"),
    });
    fetch(&srv, &loc)?;
    assert_eq!(count.load(Ordering::Relaxed), 2);
    assert_eq!(fs::read(loc.ready.join("weights.bin"))?, b"hello world");
    Ok(())
}

#[test]
fn checksum_mismatch_never_publishes() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    let srv = Server::new(|_| Reply {
        status: 200,
        headers: "",
        body: "HELLO WORLD".into(),
    });
    assert!(format!("{:#}", fetch(&srv, &loc).unwrap_err()).contains("SHA-256"));
    assert!(!loc.ready.exists());
    assert!(!loc.stage.join("weights.bin").exists());
    Ok(())
}

#[test]
fn corrupt_partial_file_is_replaced_on_retry() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    fs::create_dir(&loc.stage)?;
    fs::write(loc.stage.join("weights.bin"), b"WRONG ")?;
    let srv = Server::new(|req| {
        if req.headers.contains("range: bytes=6-") {
            Reply {
                status: 206,
                headers: "Content-Range: bytes 6-10/11\r\n",
                body: "world".into(),
            }
        } else {
            Reply {
                status: 200,
                headers: "",
                body: "hello world".into(),
            }
        }
    });
    fetch(&srv, &loc)?;
    assert_eq!(fs::read(loc.ready.join("weights.bin"))?, b"hello world");
    Ok(())
}

#[test]
fn status_reports_bytes_and_active_operations() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut db = store::connect(tmp.path())?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    assert_eq!(list(tmp.path())?[0].state, "missing");
    let lock = loc.lock_exclusive()?;
    assert!(remove(&mut db, DEFAULT_ID).is_err());
    assert_eq!(list(tmp.path())?[0].state, "downloading");
    fs::create_dir(&loc.stage)?;
    fs::write(loc.stage.join("config.json"), b"abc")?;
    drop(lock);
    let partial = list(tmp.path())?.remove(0);
    assert_eq!(partial.state, "partial");
    assert_eq!(partial.downloaded_bytes, 3);
    loc.record("modelscope", Some("fixture error".to_owned()))?;
    let failed = list(tmp.path())?.remove(0);
    assert_eq!(failed.state, "error");
    assert_eq!(failed.source.as_deref(), Some("modelscope"));
    assert_eq!(failed.error.as_deref(), Some("fixture error"));
    assert_eq!(remove(&mut db, DEFAULT_ID)?.state, "missing");
    assert!(!loc.stage.exists());
    Ok(())
}

#[test]
fn retains_active_models_and_uses_catalog_paths() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut db = store::connect(tmp.path())?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    fs::create_dir(&loc.ready)?;
    for artifact in registry::builtin(DEFAULT_ID).unwrap().files {
        File::create(loc.ready.join(artifact.name))?.set_len(artifact.bytes)?;
    }
    let lease = open_installed(tmp.path(), DEFAULT_ID)?;
    assert_eq!(list(tmp.path())?[0].state, "ready");
    assert!(remove(&mut db, DEFAULT_ID).is_err());
    drop(lease);
    store::set_meta(&mut db, "index_backend", "builtin")?;
    assert!(remove(&mut db, DEFAULT_ID).is_err());
    assert!(loc.ready.is_dir());
    store::set_meta(&mut db, "index_backend", "ollama")?;
    assert_eq!(remove(&mut db, DEFAULT_ID)?.state, "missing");
    assert!(remove(&mut db, "../other").is_err());
    for (source, rev) in [("modelscope", MS_REVISION), ("huggingface", HF_REVISION)] {
        assert!(
            registry::file_url(
                &registry::builtin(DEFAULT_ID).unwrap(),
                source,
                &Artifact::new("", 0, "")
            )?
            .path()
            .ends_with(&format!("/resolve/{rev}/"))
        );
    }
    assert!(download::endpoint("http://127.0.0.1/private").is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn deletion_rejects_symlinked_snapshot() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut db = store::connect(tmp.path())?;
    let loc = Location::new(tmp.path(), DEFAULT_ID, true)?;
    let external = tempfile::tempdir()?;
    fs::write(external.path().join("keep"), b"original")?;
    std::os::unix::fs::symlink(external.path(), &loc.ready)?;
    assert!(remove(&mut db, DEFAULT_ID).is_err());
    assert_eq!(fs::read(external.path().join("keep"))?, b"original");
    Ok(())
}

#[test]
fn two_pinned_models_use_independent_locks_and_sources() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mini = Location::new(home.path(), DEFAULT_ID, true)?;
    let qwen = Location::new(home.path(), QWEN_ID, true)?;
    let _a = mini.lock_exclusive()?;
    let _b = qwen.lock_exclusive()?;
    let list = list(home.path())?;
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|item| item.state == "downloading"));
    assert_eq!(list[1].expected_bytes, 406128537);
    let pkg = registry::builtin(QWEN_ID).unwrap();
    for source in ["huggingface", "modelscope"] {
        let weights = registry::file_url(&pkg, source, &pkg.files[1])?;
        let tokenizer = registry::file_url(&pkg, source, &pkg.files[0])?;
        assert!(weights.path().ends_with(registry::QWEN_FILENAME));
        assert!(weights.path().contains("/Mungert/"));
        assert!(
            tokenizer
                .path()
                .contains("/Qwen/Qwen3-Embedding-0.6B/resolve/")
        );
    }
    assert_ne!(
        descriptor(home.path(), DEFAULT_ID)?.fingerprint,
        descriptor(home.path(), QWEN_ID)?.fingerprint
    );
    Ok(())
}

#[test]
fn imports_a_new_content_addressed_bert_after_cpu_validation() -> Result<()> {
    use candle_core::{DType, Device};
    use candle_nn::{VarBuilder, VarMap};
    use candle_transformers::models::bert::{BertModel, Config};
    use serde_json::json;
    let src = tempfile::tempdir()?;
    let home = tempfile::tempdir()?;
    let mut db = store::connect(home.path())?;
    let cfg = json!({"model_type":"bert","hidden_size":384,"num_hidden_layers":1,"num_attention_heads":6,
        "vocab_size":2,"intermediate_size":16,"hidden_act":"gelu","hidden_dropout_prob":0.0,"max_position_embeddings":128,
        "type_vocab_size":2,"initializer_range":0.02,"layer_norm_eps":1e-12,"pad_token_id":0});
    fs::write(src.path().join("config.json"), serde_json::to_vec(&cfg)?)?;
    fs::write(src.path().join("tokenizer.json"),br#"{"version":"1.0","model":{"type":"WordLevel","unk_token":"[UNK]","vocab":{"[PAD]":0,"[UNK]":1}}}"#)?;
    fs::create_dir(src.path().join("1_Pooling"))?;
    fs::write(
        src.path().join("1_Pooling/config.json"),
        br#"{"word_embedding_dimension":384,"pooling_mode_mean_tokens":true}"#,
    )?;
    let cfg: Config = serde_json::from_value(cfg)?;
    let vm = VarMap::new();
    BertModel::load(VarBuilder::from_varmap(&vm, DType::F32, &Device::Cpu), &cfg)?;
    vm.save(src.path().join("model.safetensors"))?;
    let status = import_custom(&mut db, src.path(), &|_| {})?;
    assert_eq!(status.state, "ready");
    assert!(registry::custom_id(&status.id));
    assert_eq!(descriptor(home.path(), &status.id)?.family, "minilm");
    assert_eq!(import_custom(&mut db, src.path(), &|_| {})?.id, status.id);
    assert_eq!(list(home.path())?.len(), 3);
    fs::write(
        src.path().join("1_Pooling/config.json"),
        br#"{"word_embedding_dimension":384,"pooling_mode_mean_tokens":false}"#,
    )?;
    assert!(import_custom(&mut db, src.path(), &|_| {}).is_err());
    assert!(import_files(&mut db, DEFAULT_ID, src.path(), &|_| {}).is_err());
    remove(&mut db, &status.id)?;
    assert_eq!(list(home.path())?.len(), 2);
    Ok(())
}
