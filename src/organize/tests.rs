use super::*;
use diesel::connection::SimpleConnection;

fn insert(db: &mut Db, id: i64, language: &str) -> Result<()> {
    let repo = serde_json::from_value(json!({
        "id": id, "full_name":format!("example/repo-{id}"), "html_url":format!("https://github.com/example/repo-{id}"),
        "description":"sample", "language":language, "topics":["tool"], "stargazers_count":1,
        "starred_at":"2026-01-01", "archived":false, "readme":"private README fixture"
    }))?;
    store::upsert(db, &repo)?;
    Ok(())
}

fn tag(id: i64, tags: &[&str]) -> ApplyItem {
    ApplyItem {
        id,
        tags: tags.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn previews_filter_and_page_by_stable_id_with_tenant_lists() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut user = store::connect_as(temp.path(), "user")?;
    let mut other = store::connect_as(temp.path(), "other")?;
    for (id, lang) in [(9, "Rust"), (1, "Rust"), (3, "Go")] {
        insert(&mut user, id, lang)?;
    }
    insert(&mut other, 77, "Rust")?;
    insert(&mut other, 1, "Rust")?;
    apply(
        &mut other,
        ApplyInput {
            items: vec![tag(1, &["secret-tag"])],
        },
    )?;
    apply(
        &mut user,
        ApplyInput {
            items: vec![tag(3, &["my-tag"])],
        },
    )?;
    user.batch_execute("INSERT INTO github_lists VALUES ('user','l','User list','','',1),('other','l','Other list','','',1);
        INSERT INTO github_list_members VALUES ('user','l',1),('other','l',1)")?;

    let filter = Filter {
        language: Some("rust".into()),
        ..Default::default()
    };
    let page = |offset| PreviewInput {
        filter: Some(filter.clone()),
        offset: Some(offset),
        limit: Some(1),
    };
    let first = prepare(&mut user, page(0))?;
    assert_eq!(
        (first.repos[0].id, first.total, first.next_offset),
        (1, 2, Some(1))
    );
    assert_eq!(first.repos[0].lists, ["User list"]);
    assert_eq!(first.known_tags, ["my-tag"]);
    assert!(!serde_json::to_string(&first)?.contains("private README"));
    let ids = snapshot_ids(&mut user, &filter)?;
    assert_eq!(ids, [1, 9]);
    let next = prepare(&mut user, page(1))?;
    assert_eq!((next.repos[0].id, next.next_offset), (9, None));
    assert!(prepare(&mut user, page(2))?.repos.is_empty());
    store::deactivate_missing(&mut user, &BTreeSet::from([3, 9]))?;
    let remaining = by_ids(&mut user, &[77, 9, 1])?;
    assert_eq!(
        remaining.repos.iter().map(|r| r.id).collect::<Vec<_>>(),
        [9]
    );
    assert_eq!(remaining.total, 1);
    assert!(by_ids(&mut user, &[1; 21]).is_err());
    assert!(
        prepare(
            &mut user,
            PreviewInput {
                limit: Some(0),
                ..Default::default()
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn appending_retries_preserves_annotations_and_other_accounts() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut a = store::connect_as(temp.path(), "a")?;
    let mut b = store::connect_as(temp.path(), "b")?;
    insert(&mut a, 4, "Rust")?;
    insert(&mut b, 4, "Rust")?;
    store::edit(
        &mut a,
        store::Edit {
            id: 4,
            tags: Some(vec!["Old".into()]),
            note: Some("personal".into()),
            pinned: Some(true),
            summary: None,
        },
    )?;
    let batch = || ApplyInput {
        items: vec![tag(4, &["old", " 新 ", "Next", "NEXT"])],
    };
    assert_eq!(apply(&mut a, batch())?, json!({"updated":1,"added":2}));
    let revision = store::meta(&mut a, "library_revision")?;
    assert_eq!(apply(&mut a, batch())?, json!({"updated":0,"added":0}));
    assert_eq!(store::meta(&mut a, "library_revision")?, revision);
    let repo = store::by_id(&mut a, 4)?;
    assert_eq!(repo.tags, ["Old", "新", "Next"]);
    assert_eq!(repo.note, "personal");
    assert!(repo.pinned);
    assert!(store::by_id(&mut b, 4)?.tags.is_empty());
    Ok(())
}

#[test]
fn invalid_or_inactive_items_roll_back_the_whole_batch() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut a = store::connect_as(temp.path(), "a")?;
    let mut b = store::connect_as(temp.path(), "b")?;
    for id in [1, 2, 3] {
        insert(&mut a, id, "Rust")?;
    }
    insert(&mut b, 4, "Rust")?;
    store::deactivate_missing(&mut a, &BTreeSet::from([1, 2]))?;
    let revision = store::meta(&mut a, "library_revision")?;
    for invalid in [
        tag(3, &["inactive"]),
        tag(4, &["other"]),
        tag(2, &[" "]),
        tag(2, &["a\nb"]),
        tag(2, &["1", "2", "3", "4", "5", "6"]),
        tag(1, &["duplicate"]),
    ] {
        let batch = ApplyInput {
            items: vec![tag(1, &["first"]), invalid],
        };
        assert!(apply(&mut a, batch).is_err());
        assert!(store::by_id(&mut a, 1)?.tags.is_empty());
        assert_eq!(store::meta(&mut a, "library_revision")?, revision);
    }
    assert!(
        apply(
            &mut a,
            ApplyInput {
                items: (1..=21).map(|id| tag(id, &["extra"])).collect()
            }
        )
        .is_err()
    );
    Ok(())
}
