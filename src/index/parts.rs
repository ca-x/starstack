//! Durable metadata and body embeddings, reusable across independently changed documents.
use super::*;
use crate::chunking::Policy;

diesel::table! {
    embedding_parts (owner, id, part) {
        owner -> Text,
        id -> BigInt,
        part -> Integer,
        fingerprint -> Binary,
        checksum -> Binary,
    }
}

pub(super) struct Build<'a> {
    pub engine: &'a Engine,
    pub owner: &'a str,
    pub generation: u64,
    pub revision: &'a str,
    pub tier: Tier,
    pub checkpoint_time: &'a mut Duration,
    pub rebuild: bool,
    pub payloads: &'a mut Payloads,
    pub started: std::time::Instant,
    pub observe: &'a dyn Fn(Progress),
}

struct Part {
    owner_index: usize,
    kind: i32,
    hash: Vec<u8>,
    mean: mean::Mean,
    vector: Option<Vec<f32>>,
}

impl Part {
    fn load(&mut self, db: &mut Db, payloads: &mut Payloads, owner: &str, id: i64) -> Result<()> {
        use embedding_parts as t;
        let reference = t::table
            .find((owner, id, self.kind))
            .filter(t::fingerprint.eq(&self.hash))
            .select(t::checksum)
            .first::<Vec<u8>>(db)
            .optional()?;
        self.vector = reference
            .and_then(|reference| payloads.read(Vec::new(), &reference).ok())
            .and_then(|bytes| decode(&bytes).ok());
        Ok(())
    }
}

impl Build<'_> {
    pub fn run(
        &mut self,
        db: &mut Db,
        group: &[&Document],
        counts: &mut Progress,
    ) -> Result<BTreeMap<i64, Vec<f32>>> {
        let policy = &self.engine.selection.policy;
        let Policy::Structured {
            body_cap,
            alpha,
            window,
        } = *policy
        else {
            unreachable!("structured index");
        };
        let key = self.engine.selection.identity();
        let mut work = Vec::new();
        let mut jobs = Vec::new();
        let mut ranges = Vec::new();
        for (owner_index, doc) in group.iter().enumerate() {
            let repo = match store::by_id(db, doc.id) {
                Ok(repo) => repo,
                Err(error) => {
                    ensure!(library_revision(db)? == self.revision, BuildConflict);
                    return Err(error);
                }
            };
            let (meta, body) = (text::metadata(&repo), self.tier.body(&repo, body_cap));
            ensure!(
                structured_hash(&key, &meta, &body) == doc.hash,
                BuildConflict
            );
            let first = work.len();
            for (kind, content) in [(0, meta), (1, body)] {
                if content.trim().is_empty() {
                    continue;
                }
                // Fusion settings cannot invalidate an independently embedded part.
                let identity = format!(
                    "{}|part-v1-{kind}-{window:?}",
                    self.engine.selection.model_identity()
                );
                let mut part = Part {
                    owner_index,
                    kind,
                    hash: fingerprint(&identity, &content),
                    mean: mean::Mean::default(),
                    vector: None,
                };
                part.load(db, self.payloads, self.owner, doc.id)?;
                if part.vector.is_none() {
                    // Ollama's configured character windows also bound unusually long private annotations.
                    let chunks = policy.split(&content);
                    let last = chunks.len() - 1;
                    for (i, chunk) in chunks.into_iter().enumerate() {
                        jobs.push((work.len(), chunk, i == last));
                    }
                }
                work.push(part);
            }
            ranges.push(first..work.len());
        }

        let mut vectors = BTreeMap::new();
        // An empty iteration commits fused vectors if both parts were already cached.
        for batch in jobs
            .chunks(self.engine.group_size())
            .chain((jobs.is_empty()).then_some(&[][..]))
        {
            let mut completed_parts = Vec::new();
            if !batch.is_empty() {
                let texts: Vec<_> = batch.iter().map(|(_, text, _)| text.as_str()).collect();
                let embedded = self.engine.embed(db, &texts, &mut |tokens| {
                    counts.add_embedding(tokens);
                    counts.elapsed_seconds = self.started.elapsed().as_secs();
                    (self.observe)(*counts);
                })?;
                ensure!(
                    embedded.len() == batch.len(),
                    "向量数量与文本分块数量不一致"
                );
                for ((part_id, _, last), vector) in batch.iter().zip(embedded) {
                    let part = &mut work[*part_id];
                    part.mean.push(&vector)?;
                    if *last {
                        part.vector = Some(std::mem::take(&mut part.mean).finish()?);
                        completed_parts.push(*part_id);
                    }
                }
            }
            if !completed_parts.is_empty() || batch.is_empty() {
                let writing = std::time::Instant::now();
                db.immediate_transaction::<_, anyhow::Error, _>(|db| {
                    self.engine.check_current(db)?;
                    ensure!(
                        build_generation(db, self.rebuild)? == self.generation,
                        BuildConflict
                    );
                    use embedding_parts as t;
                    for part_id in &completed_parts {
                        let part = &work[*part_id];
                        let reference = self
                            .payloads
                            .write(&vector_bytes(part.vector.as_ref().context("缺少段向量")?))?;
                        diesel::replace_into(t::table)
                            .values((
                                t::owner.eq(self.owner),
                                t::id.eq(group[part.owner_index].id),
                                t::part.eq(part.kind),
                                t::fingerprint.eq(&part.hash),
                                t::checksum.eq(reference),
                            ))
                            .execute(db)?;
                    }
                    let mut without_body = Vec::new();
                    for (doc, range) in group.iter().zip(&ranges) {
                        if vectors.contains_key(&doc.id)
                            || work[range.clone()].iter().any(|part| part.vector.is_none())
                        {
                            continue;
                        }
                        let parts = &work[range.clone()];
                        let mut fused = mean::Mean::default();
                        let weight = f64::from(alpha) / 100.0;
                        for part in parts {
                            let weight = if parts.len() == 1 {
                                1.0
                            } else if part.kind == 0 {
                                weight
                            } else {
                                1.0 - weight
                            };
                            fused.push_weighted(
                                part.vector.as_ref().context("缺少融合向量")?,
                                weight,
                            )?;
                        }
                        let vector = fused.finish()?;
                        write_checkpoint(db, self.payloads, self.owner, doc, &vector)?;
                        if matches!(self.tier, Tier::Full)
                            && parts.iter().all(|part| part.kind != 1)
                        {
                            without_body.push(doc.id);
                        }
                        vectors.insert(doc.id, vector);
                        counts.repositories += 1;
                    }
                    if !without_body.is_empty() {
                        diesel::delete(
                            t::table
                                .filter(t::owner.eq(self.owner))
                                .filter(t::part.eq(1))
                                .filter(t::id.eq_any(without_body)),
                        )
                        .execute(db)?;
                    }
                    Ok(())
                })?;
                *self.checkpoint_time += writing.elapsed();
                counts.checkpoint_ms = self.checkpoint_time.as_millis() as u64;
            }
            counts.chunks += batch.len();
            counts.elapsed_seconds = self.started.elapsed().as_secs();
            (self.observe)(*counts);
            self.engine.check_profile(db)?;
        }
        Ok(vectors)
    }
}
