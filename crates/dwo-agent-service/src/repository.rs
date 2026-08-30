use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Datelike, Local, TimeZone};
use dwo_context::SessionContext;
use futures::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

use crate::{
    ClientTranscriptEvent, ExecutionPlan, SessionId, SessionInfo, SessionLlmSettings,
    SessionRecord, SessionWorkspace,
};

pub const SESSION_META_FILE: &str = "session.json";
pub const SESSION_MODEL_CONTEXT_FILE: &str = "model_context.json";
pub const SESSION_CLIENT_TRANSCRIPT_FILE: &str = "client_transcript.jsonl";

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn save(&self, record: &SessionRecord) -> Result<()>;
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>>;
    async fn list(&self) -> Result<Vec<SessionRecord>>;
    async fn delete(&self, id: &SessionId) -> Result<bool>;
    async fn append_transcript_event(
        &self,
        id: &SessionId,
        event: &ClientTranscriptEvent,
    ) -> Result<()>;
    async fn load_transcript(&self, id: &SessionId) -> Result<Vec<ClientTranscriptEvent>>;
}

#[derive(Default)]
pub struct MemorySessionRepository {
    records: RwLock<HashMap<SessionId, SessionRecord>>,
    transcripts: RwLock<HashMap<SessionId, Vec<ClientTranscriptEvent>>>,
}

#[async_trait]
impl SessionRepository for MemorySessionRepository {
    async fn save(&self, record: &SessionRecord) -> Result<()> {
        self.records
            .write()
            .await
            .insert(record.info.id.clone(), record.clone());
        self.transcripts
            .write()
            .await
            .entry(record.info.id.clone())
            .or_default();
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
        self.transcripts.write().await.remove(id);
        Ok(self.records.write().await.remove(id).is_some())
    }

    async fn append_transcript_event(
        &self,
        id: &SessionId,
        event: &ClientTranscriptEvent,
    ) -> Result<()> {
        if !self.records.read().await.contains_key(id) {
            bail!("session {id} does not exist");
        }
        self.transcripts
            .write()
            .await
            .entry(id.clone())
            .or_default()
            .push(event.clone());
        Ok(())
    }

    async fn load_transcript(&self, id: &SessionId) -> Result<Vec<ClientTranscriptEvent>> {
        Ok(self
            .transcripts
            .read()
            .await
            .get(id)
            .cloned()
            .unwrap_or_default())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSessionMetadata {
    info: PersistedSessionInfo,
    llm: SessionLlmSettings,
    #[serde(default, skip_serializing_if = "is_false")]
    auto_title_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_plan: Option<ExecutionPlan>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedSessionInfo {
    id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<SessionId>,
    title: String,
    workspace: SessionWorkspace,
    mode: dwo_tools::SessionMode,
    created_at_ms: u64,
    updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    ephemeral: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delete_after_ms: Option<u64>,
}

impl PersistedSessionInfo {
    fn from_runtime(info: &SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            parent_session_id: info.parent_session_id.clone(),
            title: info.title.clone(),
            workspace: info.workspace.clone(),
            mode: info.mode,
            created_at_ms: info.created_at_ms,
            updated_at_ms: info.updated_at_ms,
            ephemeral: info.ephemeral,
            completed: info.completed,
            delete_after_ms: info.delete_after_ms,
        }
    }

    fn into_runtime(self) -> SessionInfo {
        SessionInfo {
            id: self.id,
            parent_session_id: self.parent_session_id,
            title: self.title,
            cwd: PathBuf::new(),
            workspace: self.workspace,
            mode: self.mode,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            ephemeral: self.ephemeral,
            completed: self.completed,
            delete_after_ms: self.delete_after_ms,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PersistedSessionMetadata {
    fn from_record(record: &SessionRecord) -> Self {
        Self {
            info: PersistedSessionInfo::from_runtime(&record.info),
            llm: record.llm.clone(),
            auto_title_pending: record.auto_title_pending(),
            current_plan: record.current_plan.clone(),
        }
    }
}

pub struct FsSessionRepository {
    root: PathBuf,
    paths: RwLock<HashMap<SessionId, PathBuf>>,
    locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
}

impl FsSessionRepository {
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await?;
        let mut paths = HashMap::new();
        for session_dir in Self::scan_session_dirs(&root).await? {
            let metadata: PersistedSessionMetadata =
                Self::read_json(&session_dir.join(SESSION_META_FILE)).await?;
            paths.insert(metadata.info.id, session_dir);
        }
        Ok(Self {
            root,
            paths: RwLock::new(paths),
            locks: Mutex::new(HashMap::new()),
        })
    }

    fn session_dir(&self, record: &SessionRecord) -> Result<PathBuf> {
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
            .join(record.info.id.as_str()))
    }

    async fn session_lock(&self, id: &SessionId) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn scan_session_dirs(root: &Path) -> Result<Vec<PathBuf>> {
        let mut directories = vec![root.to_path_buf()];
        let mut sessions = Vec::new();
        while let Some(directory) = directories.pop() {
            if directory != root && tokio::fs::try_exists(directory.join(SESSION_META_FILE)).await?
            {
                sessions.push(directory);
                continue;
            }
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                if entry.file_type().await?.is_dir() {
                    directories.push(entry.path());
                }
            }
        }
        Ok(sessions)
    }

    async fn session_dirs(&self) -> Vec<PathBuf> {
        self.paths.read().await.values().cloned().collect()
    }

    async fn find_dir(&self, id: &SessionId) -> Option<PathBuf> {
        self.paths.read().await.get(id).cloned()
    }

    async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
    }

    async fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        crate::atomic_file::write(path, bytes).await
    }

    async fn read_record(session_dir: &Path) -> Result<SessionRecord> {
        let metadata: PersistedSessionMetadata =
            Self::read_json(&session_dir.join(SESSION_META_FILE)).await?;
        let context: SessionContext =
            Self::read_json(&session_dir.join(SESSION_MODEL_CONTEXT_FILE)).await?;
        Ok(SessionRecord::from_persisted_parts(
            metadata.info.into_runtime(),
            metadata.llm,
            context,
            metadata.auto_title_pending,
            metadata.current_plan,
        ))
    }

    async fn ensure_transcript(session_dir: &Path) -> Result<()> {
        let path = session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        if !tokio::fs::try_exists(&path).await? {
            tokio::fs::write(path, []).await?;
        }
        Ok(())
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
        let lock = self.session_lock(&record.info.id).await;
        let _write = lock.lock().await;
        let session_dir = self.session_dir(record)?;
        tokio::fs::create_dir_all(&session_dir).await?;
        Self::write_json(
            &session_dir.join(SESSION_MODEL_CONTEXT_FILE),
            &record.context,
        )
        .await?;
        Self::write_json(
            &session_dir.join(SESSION_META_FILE),
            &PersistedSessionMetadata::from_record(record),
        )
        .await?;
        Self::ensure_transcript(&session_dir).await?;
        self.paths
            .write()
            .await
            .insert(record.info.id.clone(), session_dir);
        Ok(())
    }

    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>> {
        let lock = self.session_lock(id).await;
        let _read = lock.lock().await;
        let Some(session_dir) = self.find_dir(id).await else {
            return Ok(None);
        };
        Self::read_record(&session_dir).await.map(Some)
    }

    async fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut records = stream::iter(self.session_dirs().await)
            .map(|session_dir| async move { Self::read_record(&session_dir).await })
            .buffer_unordered(16)
            .try_collect::<Vec<_>>()
            .await?;
        records.sort_by_key(|record| Reverse(record.info.updated_at_ms));
        Ok(records)
    }

    async fn delete(&self, id: &SessionId) -> Result<bool> {
        let lock = self.session_lock(id).await;
        let _write = lock.lock().await;
        let Some(session_dir) = self.find_dir(id).await else {
            return Ok(false);
        };
        tokio::fs::remove_dir_all(&session_dir).await?;
        self.remove_empty_date_directories(&session_dir).await?;
        self.paths.write().await.remove(id);
        Ok(true)
    }

    async fn append_transcript_event(
        &self,
        id: &SessionId,
        event: &ClientTranscriptEvent,
    ) -> Result<()> {
        let lock = self.session_lock(id).await;
        let _write = lock.lock().await;
        let session_dir = self
            .find_dir(id)
            .await
            .with_context(|| format!("session {id} does not exist"))?;
        let path = session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("open transcript {}", path.display()))?;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(())
    }

    async fn load_transcript(&self, id: &SessionId) -> Result<Vec<ClientTranscriptEvent>> {
        let lock = self.session_lock(id).await;
        let _read = lock.lock().await;
        let Some(session_dir) = self.find_dir(id).await else {
            return Ok(Vec::new());
        };
        let path = session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        if !tokio::fs::try_exists(&path).await? {
            return Ok(Vec::new());
        }
        let bytes = tokio::fs::read(&path).await?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let mut events = bytes[..complete_len]
            .split(|byte| *byte == b'\n')
            .map(trim_ascii)
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice(line)
                    .with_context(|| format!("parse transcript event in {}", path.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let tail = trim_ascii(&bytes[complete_len..]);
        if !tail.is_empty() {
            match serde_json::from_slice(tail) {
                Ok(event) => events.push(event),
                Err(error) => {
                    tracing::warn!(
                        event = "session.transcript_tail_truncated",
                        path = %path.display(),
                        error = %error,
                        "truncate incomplete final transcript record"
                    );
                    tokio::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .await?
                        .set_len(complete_len as u64)
                        .await?;
                }
            }
        }
        Ok(events)
    }
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{SessionEventPayload, SessionLlmSettings, TurnId};

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
    async fn filesystem_repository_uses_three_file_session_layout() {
        let root = tempfile::tempdir().unwrap();
        let repository = FsSessionRepository::new(root.path()).await.unwrap();
        let mut record = SessionRecord::new(
            SessionId::new(),
            "dated".to_string(),
            SessionWorkspace::External {
                pwd: root.path().to_path_buf(),
            },
            root.path().to_path_buf(),
            dwo_tools::SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        record.info.created_at_ms = 1_768_521_600_000;
        record.context.usage.current_tokens = 321;
        record.context.usage.last_model = Some("persisted-model".to_string());

        repository.save(&record).await.unwrap();
        let event = ClientTranscriptEvent::new(SessionEventPayload::AssistantDelta {
            message_id: crate::MessageId::new(),
            turn_id: TurnId::parse("turn-test").unwrap(),
            step_id: 1,
            revision: 1,
            delta: "hello".to_string(),
        });
        repository
            .append_transcript_event(&record.info.id, &event)
            .await
            .unwrap();

        let session_dir = root.path().join("2026/01/16").join(record.info.id.as_str());
        assert!(session_dir.join(SESSION_META_FILE).is_file());
        assert!(session_dir.join(SESSION_MODEL_CONTEXT_FILE).is_file());
        assert!(session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE).is_file());
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(session_dir.join(SESSION_META_FILE)).unwrap())
                .unwrap();
        assert!(metadata.get("context").is_none());
        assert!(metadata.get("max_model_steps").is_none());
        assert!(metadata["info"].get("cwd").is_none());
        assert!(metadata["info"].get("workspaceId").is_none());
        assert!(metadata["info"].get("worktreeId").is_none());
        assert_eq!(metadata["info"]["workspace"]["kind"], "external");
        assert_eq!(
            metadata["info"]["workspace"]["pwd"],
            root.path().to_string_lossy().as_ref()
        );
        let model_context: serde_json::Value = serde_json::from_slice(
            &std::fs::read(session_dir.join(SESSION_MODEL_CONTEXT_FILE)).unwrap(),
        )
        .unwrap();
        assert!(model_context.get("transcript").is_none());
        assert_eq!(model_context["usage"]["current_tokens"], 321);
        assert_eq!(model_context["usage"]["last_model"], "persisted-model");
        let transcript_path = session_dir.join(SESSION_CLIENT_TRANSCRIPT_FILE);
        let first_append = std::fs::read_to_string(&transcript_path).unwrap();
        repository
            .append_transcript_event(
                &record.info.id,
                &ClientTranscriptEvent::new(SessionEventPayload::AssistantReasoningDelta {
                    message_id: crate::MessageId::new(),
                    turn_id: TurnId::parse("turn-test").unwrap(),
                    step_id: 1,
                    revision: 2,
                    delta: "reasoning".to_string(),
                }),
            )
            .await
            .unwrap();
        let second_append = std::fs::read_to_string(&transcript_path).unwrap();
        assert!(second_append.starts_with(&first_append));
        assert_eq!(second_append.lines().count(), 2);
        assert_eq!(
            repository
                .load_transcript(&record.info.id)
                .await
                .unwrap()
                .len(),
            2
        );
        drop(repository);
        let repository = FsSessionRepository::new(root.path()).await.unwrap();
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
        assert_eq!(
            repository
                .load(&record.info.id)
                .await
                .unwrap()
                .unwrap()
                .context
                .usage
                .current_tokens,
            321
        );
        assert_eq!(repository.list().await.unwrap().len(), 1);
        assert!(repository.delete(&record.info.id).await.unwrap());
        assert!(repository.load(&record.info.id).await.unwrap().is_none());
        assert!(!root.path().join("2026").exists());
    }

    #[tokio::test]
    async fn filesystem_repository_truncates_only_an_incomplete_transcript_tail() {
        let root = tempfile::tempdir().unwrap();
        let repository = FsSessionRepository::new(root.path()).await.unwrap();
        let record = SessionRecord::new(
            SessionId::new(),
            "tail".to_string(),
            SessionWorkspace::External {
                pwd: root.path().to_path_buf(),
            },
            root.path().to_path_buf(),
            dwo_tools::SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        repository.save(&record).await.unwrap();
        repository
            .append_transcript_event(
                &record.info.id,
                &ClientTranscriptEvent::new(SessionEventPayload::TurnCompleted {
                    turn_id: TurnId::parse("turn-test").unwrap(),
                }),
            )
            .await
            .unwrap();
        let path = repository
            .find_dir(&record.info.id)
            .await
            .unwrap()
            .join(SESSION_CLIENT_TRANSCRIPT_FILE);
        let complete_len = std::fs::metadata(&path).unwrap().len();
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(br#"{"recorded_at_ms":123,"payload"#)
            .await
            .unwrap();
        file.flush().await.unwrap();

        let events = repository.load_transcript(&record.info.id).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(std::fs::metadata(path).unwrap().len(), complete_len);
    }

    #[tokio::test]
    async fn filesystem_repository_rejects_corruption_before_the_transcript_tail() {
        let root = tempfile::tempdir().unwrap();
        let repository = FsSessionRepository::new(root.path()).await.unwrap();
        let record = SessionRecord::new(
            SessionId::new(),
            "middle".to_string(),
            SessionWorkspace::External {
                pwd: root.path().to_path_buf(),
            },
            root.path().to_path_buf(),
            dwo_tools::SessionMode::Confirm,
            SessionLlmSettings::default(),
        );
        repository.save(&record).await.unwrap();
        let path = repository
            .find_dir(&record.info.id)
            .await
            .unwrap()
            .join(SESSION_CLIENT_TRANSCRIPT_FILE);
        std::fs::write(path, b"not-json\n{\"still\":\"a tail\"}").unwrap();

        let error = repository
            .load_transcript(&record.info.id)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("parse transcript event"));
    }
}
