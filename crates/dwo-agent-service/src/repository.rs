use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Local, TimeZone};
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

    fn path(&self, record: &SessionRecord) -> Result<PathBuf> {
        let timestamp = i64::try_from(record.info.created_at_ms)
            .context("session created_at_ms exceeds i64")?;
        let created = Local
            .timestamp_millis_opt(timestamp)
            .single()
            .context("session created_at_ms is out of range")?;
        Ok(self
            .root
            .join(format!("{:04}", created.year()))
            .join(format!("{:02}", created.month()))
            .join(format!("{:02}", created.day()))
            .join(format!("{}.json", record.info.id.as_str())))
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

    async fn json_paths(&self) -> Result<Vec<PathBuf>> {
        let mut directories = vec![self.root.clone()];
        let mut paths = Vec::new();
        while let Some(directory) = directories.pop() {
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    directories.push(entry.path());
                } else if file_type.is_file()
                    && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
                {
                    paths.push(entry.path());
                }
            }
        }
        Ok(paths)
    }

    async fn find_path(&self, id: &SessionId) -> Result<Option<PathBuf>> {
        let filename = format!("{}.json", id.as_str());
        Ok(self.json_paths().await?.into_iter().find(|path| {
            path.file_name()
                .is_some_and(|name| name == filename.as_str())
        }))
    }

    async fn remove_empty_date_directories(&self, path: &Path) -> Result<()> {
        let mut directory = path.parent().map(Path::to_path_buf);
        while let Some(current) = directory {
            if current == self.root {
                break;
            }
            let mut entries = tokio::fs::read_dir(&current).await?;
            if entries.next_entry().await?.is_some() {
                break;
            }
            directory = current.parent().map(Path::to_path_buf);
            tokio::fs::remove_dir(current).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl SessionRepository for FsSessionRepository {
    async fn save(&self, record: &SessionRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record)?;
        let lock = self.session_lock(&record.info.id).await;
        let _write = lock.lock().await;
        let path = self.path(record)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
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
        let Some(path) = self.find_path(id).await? else {
            return Ok(None);
        };
        Self::read_record(&path).await.map(Some)
    }

    async fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut records = Vec::new();
        for path in self.json_paths().await? {
            records.push(Self::read_record(&path).await?);
        }
        records.sort_by_key(|record| Reverse(record.info.updated_at_ms));
        Ok(records)
    }

    async fn delete(&self, id: &SessionId) -> Result<bool> {
        let lock = self.session_lock(id).await;
        let _write = lock.lock().await;
        let Some(path) = self.find_path(id).await? else {
            return Ok(false);
        };
        tokio::fs::remove_file(&path).await?;
        self.remove_empty_date_directories(&path).await?;
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

    #[tokio::test]
    async fn filesystem_repository_partitions_sessions_by_creation_date() {
        let root = tempfile::tempdir().unwrap();
        let repository = FsSessionRepository::new(root.path()).await.unwrap();
        let mut record = SessionRecord::new(
            SessionId::new(),
            "dated".to_string(),
            root.path().to_path_buf(),
            dwo_tools::SessionMode::Confirm,
            crate::SessionLlmSettings::default(),
        );
        record.info.created_at_ms = 1_768_521_600_000;

        repository.save(&record).await.unwrap();

        let expected = root
            .path()
            .join("2026/01/16")
            .join(format!("{}.json", record.info.id));
        assert!(expected.exists(), "missing {}", expected.display());
        assert_eq!(
            repository
                .load(&record.info.id)
                .await
                .unwrap()
                .unwrap()
                .info
                .id,
            record.info.id
        );
        assert_eq!(repository.list().await.unwrap().len(), 1);
        assert!(repository.delete(&record.info.id).await.unwrap());
        assert!(!root.path().join("2026").exists());
    }
}
