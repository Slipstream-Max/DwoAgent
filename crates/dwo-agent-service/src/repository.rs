use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock};

use crate::{SessionId, SessionRecord};

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn save(&self, record: &SessionRecord) -> Result<()>;
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>>;
    async fn list(&self) -> Result<Vec<SessionRecord>>;
    async fn delete(&self, id: &SessionId) -> Result<bool>;
}

#[derive(Default)]
pub struct MemorySessionRepository {
    records: RwLock<HashMap<SessionId, SessionRecord>>,
}

#[async_trait]
impl SessionRepository for MemorySessionRepository {
    async fn save(&self, record: &SessionRecord) -> Result<()> {
        self.records
            .write()
            .await
            .insert(record.info.id.clone(), record.clone());
        Ok(())
    }

    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>> {
        Ok(self.records.read().await.get(id).cloned())
    }

    async fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut records: Vec<_> = self.records.read().await.values().cloned().collect();
        records.sort_by_key(|record| Reverse(record.info.updated_at_ms));
        Ok(records)
    }

    async fn delete(&self, id: &SessionId) -> Result<bool> {
        Ok(self.records.write().await.remove(id).is_some())
    }
}

pub struct FsSessionRepository {
    root: PathBuf,
    locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
}

impl FsSessionRepository {
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self {
            root,
            locks: Mutex::new(HashMap::new()),
        })
    }

    fn path(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{}.json", id.as_str()))
    }

    async fn read_record(path: &Path) -> Result<SessionRecord> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read session record {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse session record {}", path.display()))
    }

    async fn session_lock(&self, id: &SessionId) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[async_trait]
impl SessionRepository for FsSessionRepository {
    async fn save(&self, record: &SessionRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record)?;
        let lock = self.session_lock(&record.info.id).await;
        let _write = lock.lock().await;
        let path = self.path(&record.info.id);
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        tokio::fs::write(&temporary, bytes).await?;
        if tokio::fs::try_exists(&path).await? {
            tokio::fs::remove_file(&path).await?;
        }
        tokio::fs::rename(&temporary, &path).await?;
        Ok(())
    }

    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>> {
        let lock = self.session_lock(id).await;
        let _read = lock.lock().await;
        let path = self.path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        Self::read_record(&path).await.map(Some)
    }

    async fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                records.push(Self::read_record(&path).await?);
            }
        }
        records.sort_by_key(|record| Reverse(record.info.updated_at_ms));
        Ok(records)
    }

    async fn delete(&self, id: &SessionId) -> Result<bool> {
        let lock = self.session_lock(id).await;
        let _write = lock.lock().await;
        let path = self.path(id);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(false);
        }
        tokio::fs::remove_file(path).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn filesystem_repository_scopes_locks_per_session() {
        let root = tempfile::tempdir().unwrap();
        let repository = FsSessionRepository::new(root.path()).await.unwrap();
        let first = SessionId::new();
        let second = SessionId::new();

        let first_lock = repository.session_lock(&first).await;
        assert!(Arc::ptr_eq(
            &first_lock,
            &repository.session_lock(&first).await
        ));
        assert!(!Arc::ptr_eq(
            &first_lock,
            &repository.session_lock(&second).await
        ));
    }
}
