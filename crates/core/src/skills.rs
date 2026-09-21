//! Skills 系统：每个 skill 是一个文件夹，内含运行文件 + 说明文档（README.md / SKILL.md）。
//! 支持从配置目录扫描、手动导入、启停，并提供说明文本供 Agent 参考。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub description: String,
    /// 来自配置目录扫描还是手动导入
    pub source: String, // "config" | "imported"
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillState {
    pub imported: Vec<Skill>,
    pub disabled: Vec<String>,
}

#[derive(Clone)]
pub struct SkillManager {
    skills: Arc<RwLock<Vec<Skill>>>,
    store_path: PathBuf,
}

impl Default for SkillManager {
    fn default() -> Self {
        Self {
            skills: Arc::new(RwLock::new(Vec::new())),
            store_path: PathBuf::new(),
        }
    }
}

impl SkillManager {
    pub fn init(&mut self, store_path: PathBuf) {
        self.store_path = store_path;
    }

    /// 从配置目录 + 持久化导入记录加载 skills
    pub async fn load(&self, skill_dirs: &[PathBuf]) {
        let mut list: Vec<Skill> = Vec::new();
        // 1. 扫描配置目录的每个一级子目录
        for dir in skill_dirs {
            if !dir.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        let name = p
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if name.starts_with('.') {
                            continue;
                        }
                        let desc = read_desc(&p);
                        list.push(Skill {
                            name,
                            path: p.clone(),
                            enabled: true,
                            description: desc,
                            source: "config".into(),
                        });
                    } else if is_skill_file(&p) {
                        // 单文件 skill（如 goose-tts 二进制所在目录本身被指定）
                        let name = dir
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| dir.to_string_lossy().to_string());
                        if !list.iter().any(|s| s.name == name) {
                            let desc = read_desc(dir);
                            list.push(Skill {
                                name,
                                path: dir.clone(),
                                enabled: true,
                                description: desc,
                                source: "config".into(),
                            });
                        }
                    }
                }
            }
        }

        // 2. 加载手动导入记录
        let state = self.load_state().await;
        for sk in state.imported {
            if sk.path.exists() && !list.iter().any(|s| s.name == sk.name) {
                list.push(sk);
            }
        }

        // 3. 应用禁用列表
        for sk in list.iter_mut() {
            if state.disabled.contains(&sk.name) {
                sk.enabled = false;
            }
        }
        *self.skills.write().await = list;
    }

    pub async fn list(&self) -> Vec<Skill> {
        self.skills.read().await.clone()
    }

    /// 手动导入一个 skill 文件夹
    pub async fn import(&self, path: &str) -> Result<Skill> {
        let p = PathBuf::from(path);
        if !p.is_dir() {
            return Err(anyhow::anyhow!("路径不是有效文件夹: {path}"));
        }
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".into());
        let skill = Skill {
            name: name.clone(),
            path: p.clone(),
            enabled: true,
            description: read_desc(&p),
            source: "imported".into(),
        };
        {
            let mut list = self.skills.write().await;
            if let Some(existing) = list.iter_mut().find(|s| s.name == name) {
                *existing = skill.clone();
            } else {
                list.push(skill.clone());
            }
        }
        self.persist_imported().await?;
        Ok(skill)
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        {
            let mut list = self.skills.write().await;
            list.retain(|s| s.name != name);
        }
        let mut state = self.load_state().await;
        state.imported.retain(|s| s.name != name);
        state.disabled.retain(|d| d != name);
        self.save_state(&state).await?;
        Ok(())
    }

    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        {
            let mut list = self.skills.write().await;
            if let Some(s) = list.iter_mut().find(|s| s.name == name) {
                s.enabled = enabled;
            }
        }
        let mut state = self.load_state().await;
        if enabled {
            state.disabled.retain(|d| d != name);
        } else if !state.disabled.iter().any(|d| d == name) {
            state.disabled.push(name.to_string());
        }
        self.save_state(&state).await?;
        Ok(())
    }

    /// 返回已启用 skills 的使用说明文本（注入主/子 Agent 上下文）
    pub async fn usage_text(&self) -> String {
        let list = self.skills.read().await;
        let mut out = String::new();
        for s in list.iter().filter(|s| s.enabled) {
            out.push_str(&format!(
                "- {}: {}（路径: {}）\n",
                s.name,
                s.description,
                s.path.display()
            ));
        }
        if out.is_empty() {
            "（暂无已启用 skills）".into()
        } else {
            out
        }
    }

    async fn load_state(&self) -> SkillState {
        if self.store_path.as_os_str().is_empty() || !self.store_path.exists() {
            return SkillState::default();
        }
        std::fs::read_to_string(&self.store_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    async fn persist_imported(&self) -> Result<()> {
        let list = self.skills.read().await;
        let state = SkillState {
            imported: list.iter().filter(|s| s.source == "imported").cloned().collect(),
            disabled: list
                .iter()
                .filter(|s| !s.enabled)
                .map(|s| s.name.clone())
                .collect(),
        };
        drop(list);
        self.save_state(&state).await
    }

    async fn save_state(&self, state: &SkillState) -> Result<()> {
        if self.store_path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(p) = self.store_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let s = serde_json::to_string_pretty(state)?;
        std::fs::write(&self.store_path, s)?;
        Ok(())
    }
}

fn is_skill_file(p: &Path) -> bool {
    match p.file_name().and_then(|s| s.to_str()) {
        Some(name) => {
            name.ends_with(".md") || name == "README" || name == "SKILL" || name.ends_with(".json")
        }
        None => false,
    }
}

/// 从 README.md / SKILL.md 提取描述（首段）
pub fn read_desc(dir: &Path) -> String {
    for f in ["SKILL.md", "README.md", "README"] {
        let p = dir.join(f);
        if let Ok(content) = std::fs::read_to_string(&p) {
            let first_line = content
                .lines()
                .map(|l| l.trim())
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .unwrap_or("")
                .to_string();
            if !first_line.is_empty() {
                return first_line.chars().take(120).collect();
            }
        }
    }
    "（无说明文档）".to_string()
}

/// 读取某个 skill 的完整说明文档文本
pub fn read_usage(dir: &Path) -> String {
    for f in ["SKILL.md", "README.md", "README"] {
        let p = dir.join(f);
        if let Ok(content) = std::fs::read_to_string(&p) {
            return content;
        }
    }
    String::new()
}
