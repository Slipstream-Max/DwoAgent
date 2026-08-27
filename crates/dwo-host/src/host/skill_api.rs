use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dwo_context::{PromptBuildError, SkillSnapshot, SystemPromptBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Host;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillInstallParam {
    name: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    files: Option<Vec<SkillFileParam>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillFileParam {
    path: String,
    content_base64: String,
}

impl Host {
    pub(crate) async fn dispatch_skill(&self, method: &str, params: Value) -> Result<Value> {
        let action = method.strip_prefix("skill.").unwrap_or_default();
        match action {
            "list" => self.skill_list(),
            "enable" | "disable" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("skill name is required"))?
                    .to_string();
                self.skill_set_enabled(name, action == "enable").await
            }
            "install" => {
                let params: SkillInstallParam = serde_json::from_value(params)?;
                self.skill_install(params).await
            }
            "uninstall" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("skill name is required"))?
                    .to_string();
                self.skill_uninstall(name).await
            }
            _ => anyhow::bail!("unknown skill action: {action}"),
        }
    }

    pub(crate) fn skill_list(&self) -> Result<Value> {
        let external = self
            .profile
            .read()
            .expect("profile lock poisoned")
            .config
            .external_skills_dirs
            .clone();
        let active = skill_snapshots(&self.profile_root, &external, &self.profile_root)?;
        let disabled = list_skill_names(&self.profile_root.join("resource/skills.disabled"))?;
        Ok(json!({"skills": active, "disabled": disabled}))
    }

    pub(crate) async fn skill_set_enabled(&self, name: String, enabled: bool) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let root = self.profile_root.join("resource/skills");
        let disabled_root = self.profile_root.join("resource/skills.disabled");
        let (from, to) = if enabled {
            (disabled_root.join(&name), root.join(&name))
        } else {
            (root.join(&name), disabled_root.join(&name))
        };
        anyhow::ensure!(from.is_dir(), "skill not found: {name}");
        self.config_manager
            .move_resource_dir(&from, &to, true)
            .await?;
        self.events
            .publish("skill.changed", json!({"name": name, "enabled": enabled}))
            .await;
        Ok(json!({"name": name, "enabled": enabled}))
    }

    async fn skill_install(&self, params: SkillInstallParam) -> Result<Value> {
        super::validate_resource_name(&params.name)?;
        let name = params.name;
        let files = decode_skill_files(params.content, params.files)?;
        let dir = self.profile_root.join("resource/skills").join(&name);
        let disabled = self
            .profile_root
            .join("resource/skills.disabled")
            .join(&name);
        anyhow::ensure!(
            !dir.exists() && !disabled.exists(),
            "skill already exists: {name}"
        );
        let result = async {
            for (relative, bytes) in &files {
                self.config_manager
                    .write_resource(&dir.join(relative), bytes.clone(), |_| Ok(()))
                    .await?;
            }
            let external = self
                .profile
                .read()
                .expect("profile lock poisoned")
                .config
                .external_skills_dirs
                .clone();
            skill_snapshots(&self.profile_root, &external, &self.profile_root)?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            let _ = self.config_manager.remove_resource_dir(&dir).await;
            return Err(error);
        }
        self.events
            .publish("skill.changed", json!({"name": name, "action": "install"}))
            .await;
        Ok(json!({
            "name": name,
            "installed": true,
            "path": dir.join("SKILL.md"),
            "fileCount": files.len(),
        }))
    }

    pub(crate) async fn skill_uninstall(&self, name: String) -> Result<Value> {
        super::validate_resource_name(&name)?;
        let mut removed = false;
        for dir in [
            self.profile_root.join("resource/skills").join(&name),
            self.profile_root
                .join("resource/skills.disabled")
                .join(&name),
        ] {
            removed |= self.config_manager.remove_resource_dir(&dir).await?;
        }
        if removed {
            self.events
                .publish(
                    "skill.changed",
                    json!({"name": name, "action": "uninstall"}),
                )
                .await;
        }
        Ok(json!({"name": name, "removed": removed}))
    }
}

pub(super) fn skill_snapshots(
    profile_root: &Path,
    external_dirs: &[PathBuf],
    cwd: &Path,
) -> Result<Vec<SkillSnapshot>, PromptBuildError> {
    let external_dirs = external_dirs
        .iter()
        .map(|dir| {
            if dir.is_absolute() {
                dir.clone()
            } else {
                profile_root.join(dir)
            }
        })
        .collect();
    SystemPromptBuilder::new(Some(profile_root.to_path_buf()), cwd.to_path_buf())
        .with_external_skill_dirs(Arc::new(RwLock::new(external_dirs)))
        .scan_skills()
}

fn list_skill_names(root: &std::path::Path) -> Result<Vec<String>> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn decode_skill_files(
    content: Option<String>,
    files: Option<Vec<SkillFileParam>>,
) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let has_content = content.is_some();
    let has_files = files.as_ref().is_some_and(|files| !files.is_empty());
    anyhow::ensure!(
        has_content ^ has_files,
        "skill install requires exactly one of content or files"
    );
    if let Some(content) = content {
        return Ok(vec![(PathBuf::from("SKILL.md"), content.into_bytes())]);
    }

    let mut paths = BTreeSet::new();
    let mut decoded = Vec::new();
    for file in files.unwrap_or_default() {
        let path = parse_skill_relative_path(&file.path)?;
        anyhow::ensure!(
            paths.insert(path.clone()),
            "skill install contains duplicate file: {}",
            file.path
        );
        let bytes = STANDARD
            .decode(file.content_base64.as_bytes())
            .map_err(|error| anyhow::anyhow!("decode skill file {}: {error}", file.path))?;
        decoded.push((path, bytes));
    }
    anyhow::ensure!(
        paths.contains(&PathBuf::from("SKILL.md")),
        "skill directory must contain SKILL.md"
    );
    Ok(decoded)
}

fn parse_skill_relative_path(value: &str) -> Result<PathBuf> {
    anyhow::ensure!(!value.is_empty(), "skill file path must not be empty");
    let mut path = PathBuf::new();
    for component in value.split('/') {
        anyhow::ensure!(
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.contains('\\')
                && !component.contains(':'),
            "skill file path must be a relative slash-separated path: {value}"
        );
        path.push(component);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::tests::write_test_profile;

    #[tokio::test]
    async fn installs_a_skill_file_tree_without_losing_binary_resources() {
        let root = tempfile::tempdir().unwrap();
        let host = Host::build(&write_test_profile(root.path())).await.unwrap();
        let asset = [0_u8, 1, 2, 255];
        let result = host
            .handle_method(
                "skill.install",
                json!({
                    "name": "deploy",
                    "files": [
                        {
                            "path": "SKILL.md",
                            "contentBase64": STANDARD.encode("---\nname: deploy\ndescription: Deploy safely\n---\nDeploy it."),
                        },
                        {
                            "path": "references/checklist.md",
                            "contentBase64": STANDARD.encode("Check the deployment."),
                        },
                        {
                            "path": "assets/icon.bin",
                            "contentBase64": STANDARD.encode(asset),
                        }
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["installed"], true);
        assert_eq!(result["fileCount"], 3);
        let directory = root.path().join("resource/skills/deploy");
        assert_eq!(
            std::fs::read(directory.join("assets/icon.bin")).unwrap(),
            asset
        );
        let skills = host.handle_method("skill.list", json!({})).await.unwrap();
        assert_eq!(skills["skills"][0]["name"], "deploy");
        host.shutdown().await;
    }

    #[test]
    fn rejects_skill_file_paths_that_escape_the_skill_directory() {
        assert!(parse_skill_relative_path("../SKILL.md").is_err());
        assert!(parse_skill_relative_path("references\\escape.md").is_err());
        assert!(parse_skill_relative_path("C:/SKILL.md").is_err());
        assert_eq!(
            parse_skill_relative_path("references/checklist.md").unwrap(),
            PathBuf::from("references").join("checklist.md")
        );
    }
}
