//! HawDB immutable objects for exact vectors and restart checkpoints.
use anyhow::{Context, Result, ensure};
use hawdb_integrity::Sha256Digest;
use hawdb_storage::immutable_object::{
    ImmutableObjectError, ImmutableObjectStore, ObjectKind, ObjectReference,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

const KIND: ObjectKind = ObjectKind::CheckpointArtifact;
const FORMAT: u16 = 1;
const MAX_VECTOR_BYTES: u64 = 1024 * 1024;

pub(super) struct Payloads {
    root: PathBuf,
    store: Option<ImmutableObjectStore>,
}

impl Payloads {
    pub(super) fn new(dir: &Path, owner: &str) -> Self {
        Self {
            root: dir.join(format!("vector-payloads-{:x}", Sha256::digest(owner))),
            store: None,
        }
    }

    fn open(&mut self) -> Result<&mut ImmutableObjectStore> {
        if self.store.is_none() {
            self.store = Some(ImmutableObjectStore::open_with_limit(
                &self.root,
                MAX_VECTOR_BYTES,
            )?);
        }
        Ok(self.store.as_mut().expect("HawDB store is open"))
    }

    fn reference(metadata: &[u8]) -> Result<ObjectReference> {
        ensure!(metadata.len() == 40, "invalid HawDB vector reference");
        let (length, digest) = metadata.split_at(8);
        Ok(ObjectReference {
            kind: KIND,
            format_version: FORMAT,
            byte_length: u64::from_le_bytes(length.try_into()?),
            sha256: Sha256Digest::from_bytes(digest.try_into()?),
        })
    }

    pub(super) fn reference_bytes(bytes: &[u8]) -> Vec<u8> {
        let reference = ObjectReference::for_bytes(KIND, FORMAT, bytes);
        [
            reference.byte_length.to_le_bytes().as_slice(),
            reference.sha256.as_bytes(),
        ]
        .concat()
    }

    pub(super) fn read(&mut self, legacy: Vec<u8>, metadata: &[u8]) -> Result<Vec<u8>> {
        if legacy.is_empty() {
            Ok(self.open()?.read(Self::reference(metadata)?)?)
        } else {
            ensure!(Sha256::digest(&legacy)[..] == *metadata, "向量校验失败");
            Ok(legacy)
        }
    }

    pub(super) fn write(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        ensure!(!bytes.is_empty(), "cannot store an empty vector payload");
        let reference = ObjectReference::for_bytes(KIND, FORMAT, bytes);
        let store = self.open()?;
        let result = store.publish(reference, bytes);
        if let Err(ImmutableObjectError::ExistingObjectCorrupt { .. }) = result {
            // The expected digest came from the new vector. Remove only that damaged object.
            fs::remove_file(store.object_path(reference))?;
            store.publish(reference, bytes)?;
        } else {
            result?;
        }
        Ok(Self::reference_bytes(bytes))
    }

    pub(super) fn disk_bytes(&self) -> Result<u64> {
        self.visit(|_, length| Ok(length))
    }

    /// SQLite's committed published and checkpoint references define reachability.
    pub(super) fn collect_orphans(
        &mut self,
        metadata: impl IntoIterator<Item = Vec<u8>>,
    ) -> Result<()> {
        let mut referenced = BTreeSet::new();
        for bytes in metadata {
            // An unreadable catalog entry is no basis for garbage collection.
            referenced.insert(Self::reference(&bytes)?.sha256);
        }
        let mut orphans = Vec::new();
        self.visit(|digest, length| {
            if !referenced.contains(&digest) {
                orphans.push(ObjectReference {
                    kind: KIND,
                    format_version: FORMAT,
                    byte_length: length,
                    sha256: digest,
                });
            }
            Ok(0)
        })?;
        if !orphans.is_empty() {
            self.open()?.reclaim_unreachable(&orphans, &[])?;
        }
        Ok(())
    }

    fn visit(&self, mut visit: impl FnMut(Sha256Digest, u64) -> Result<u64>) -> Result<u64> {
        // Resolve the namespace from HawDB's object layout. Reading status never creates it.
        let objects = self.root.join("objects").join("checkpoint-artifact");
        let entries = match fs::read_dir(objects) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = 0_u64;
        for entry in entries {
            let entry = entry?;
            let Ok(digest) = entry.file_name().to_string_lossy().parse() else {
                continue;
            };
            match fs::symlink_metadata(entry.path()) {
                Ok(file) if file.is_file() => {
                    bytes = bytes
                        .checked_add(visit(digest, file.len())?)
                        .context("vector storage size overflow")?;
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(bytes)
    }
}
