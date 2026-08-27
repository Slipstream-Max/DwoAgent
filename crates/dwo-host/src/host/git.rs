use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct RepositoryInfo {
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeStatus {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub dirty: bool,
}

pub async fn inspect_repository(path: &Path) -> Result<RepositoryInfo> {
    let root = PathBuf::from(text(path, ["rev-parse", "--show-toplevel"]).await?);
    let common = PathBuf::from(text(path, ["rev-parse", "--git-common-dir"]).await?);
    let common_dir = if common.is_absolute() {
        common
    } else {
        path.join(common)
    };
    let remote_url = text(path, ["remote", "get-url", "origin"]).await.ok();
    Ok(RepositoryInfo {
        root: std::fs::canonicalize(root)?,
        common_dir: std::fs::canonicalize(common_dir)?,
        remote_url,
    })
}

pub async fn worktree_status(path: &Path) -> Result<WorktreeStatus> {
    let output = bytes(path, ["worktree", "list", "--porcelain", "-z"]).await?;
    let canonical = std::fs::canonicalize(path)?;
    let mut current_path = None;
    let mut head = None;
    let mut branch = None;
    let mut selected = None;
    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let (Some(path), Some(head)) = (current_path.take(), head.take())
                && path == canonical
            {
                selected = Some((path, head, branch.take()));
                break;
            }
            branch = None;
            continue;
        }
        let field = String::from_utf8_lossy(field);
        if let Some(value) = field.strip_prefix("worktree ") {
            current_path = std::fs::canonicalize(value).ok();
        } else if let Some(value) = field.strip_prefix("HEAD ") {
            head = Some(value.to_string());
        } else if let Some(value) = field.strip_prefix("branch ") {
            branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(value)
                    .to_string(),
            );
        }
    }
    let (path, head, branch) = selected
        .with_context(|| format!("path is not a registered Git worktree: {}", path.display()))?;
    let dirty = !bytes(&path, ["status", "--porcelain"]).await?.is_empty();
    Ok(WorktreeStatus {
        path,
        branch,
        head,
        dirty,
    })
}

pub async fn clone_repository(
    url: &str,
    path: &Path,
    branch: Option<&str>,
) -> Result<RepositoryInfo> {
    let mut args = vec![OsString::from("clone")];
    if let Some(branch) = branch {
        args.extend([OsString::from("--branch"), OsString::from(branch)]);
    }
    args.extend([OsString::from(url), path.as_os_str().to_owned()]);
    run(None, args).await?;
    inspect_repository(path).await
}

pub async fn create_worktree(
    repository: &Path,
    path: &Path,
    branch: &str,
    start_point: Option<&str>,
) -> Result<WorktreeStatus> {
    let mut args = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("-b"),
        OsString::from(branch),
        path.as_os_str().to_owned(),
    ];
    if let Some(start_point) = start_point {
        args.push(OsString::from(start_point));
    }
    run(Some(repository), args).await?;
    worktree_status(path).await
}

pub async fn remove_worktree(repository: &Path, path: &Path) -> Result<()> {
    run(
        Some(repository),
        vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            path.as_os_str().to_owned(),
        ],
    )
    .await
    .map(|_| ())
}

async fn text<const N: usize>(path: &Path, args: [&str; N]) -> Result<String> {
    let output = bytes(path, args).await?;
    Ok(String::from_utf8(output)?.trim().to_string())
}

async fn bytes<const N: usize>(path: &Path, args: [&str; N]) -> Result<Vec<u8>> {
    run(
        Some(path),
        args.into_iter().map(OsString::from).collect::<Vec<_>>(),
    )
    .await
}

async fn run(cwd: Option<&Path>, args: Vec<OsString>) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    if let Some(cwd) = cwd {
        command.arg("-C").arg(cwd);
    }
    let output = command.args(&args).output().await?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git {} failed: {}",
            args.iter()
                .map(|arg| arg.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            message
        );
    }
    Ok(output.stdout)
}
