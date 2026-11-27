//! A first searchable generation can use metadata while body embeddings are computed.
use super::*;

#[derive(Clone, Copy)]
pub(super) enum Tier {
    Metadata(u64),
    Full,
}

impl Tier {
    pub fn body(self, repo: &Repo, cap: usize) -> String {
        match self {
            Self::Metadata(_) => String::new(),
            Self::Full => text::body(repo, cap),
        }
    }

    pub fn initial(db: &mut Db, selection: &Selection, rebuild: bool) -> Result<Option<Self>> {
        let crate::chunking::Policy::Structured { body_cap, .. } = selection.policy else {
            return Ok(None);
        };
        db.transaction(|db| {
            let current = build_generation(db, rebuild)?;
            if current > 0 && !(rebuild && selection.mismatches(db)?) {
                return Ok(None);
            }
            use diesel::sql_types::{Bool, Text};
            #[derive(QueryableByName)]
            struct Exists {
                #[diesel(sql_type = Bool)]
                present: bool,
            }
            let owner = store::owner(db)?;
            let sources = diesel::sql_query(
                "SELECT EXISTS (SELECT 1 FROM repositories WHERE owner_id=? AND active=1
                 AND (code_context<>'' OR commit_index<>'' OR (? AND json_extract(metadata,'$.readme')<>''))) AS present",
            ).bind::<Text, _>(owner).bind::<Bool, _>(body_cap > 0).get_result::<Exists>(db)?;
            Ok(sources.present.then_some(Self::Metadata(current)))
        })
    }
}

pub(super) fn prioritize_bodies(db: &mut Db, owner: &str, missing: &mut [&Document]) -> Result<()> {
    use store::repositories as r;
    if missing.len() < 2 {
        return Ok(());
    }
    let mut order = BTreeMap::new();
    for group in missing.chunks(400) {
        let ids: Vec<_> = group.iter().map(|doc| doc.id).collect();
        let rows = r::table
            .filter(r::owner_id.eq(owner).and(r::id.eq_any(ids)))
            .select((
                r::id,
                r::pinned,
                diesel::dsl::sql::<diesel::sql_types::Text>(
                    "json_extract(metadata,'$.starred_at')",
                ),
            ))
            .load::<(i64, bool, String)>(db)?;
        order.extend(
            rows.into_iter()
                .map(|(id, pinned, starred)| (id, (pinned, starred))),
        );
    }
    missing.sort_unstable_by(|a, b| {
        order
            .get(&b.id)
            .cmp(&order.get(&a.id))
            .then(a.id.cmp(&b.id))
    });
    Ok(())
}
