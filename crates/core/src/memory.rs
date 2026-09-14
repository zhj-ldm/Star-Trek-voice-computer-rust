//! 记忆系统：长期记忆条目持久化（JSON），UI 可增删查，Agent 可写入。

use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub memory_type: String, // knowledge / preference / gotcha / narrative / workflow ...
    pub title: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Clone, Default)]
pub struct MemoryStore {
    entries: Arc<RwLock<Vec<MemoryEntry>>>,
    path: PathBuf,
}

use std::sync::Arc;

impl MemoryStore {
    pub fn init(&mut self, path: PathBuf) {
        self.path = path.clone();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(list) = serde_json::from_str::<Vec<MemoryEntry>>(&s) {
                self.entries = Arc::new(RwLock::new(list));
            }
        }
    }

    pub async fn list(&self) -> Vec<MemoryEntry> {
        self.entries.read().await.clone()
    }

    pub async fn add(&self, memory_type: &str, title: &str, content: &str) -> Result<MemoryEntry> {
        let entry = MemoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            memory_type: memory_type.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            created_at: Local::now().to_rfc3339(),
        };
        {
            let mut list = self.entries.write().await;
            list.insert(0, entry.clone());
        }
        self.persist().await?;
        Ok(entry)
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        {
            let mut list = self.entries.write().await;
            list.retain(|e| e.id != id);
        }
        self.persist().await?;
        Ok(())
    }

    pub async fn clear(&self) -> Result<()> {
        {
            let mut list = self.entries.write().await;
            list.clear();
        }
        self.persist().await?;
        Ok(())
    }

    /// 返回用于 Agent 上下文的记忆文本
    pub async fn context_text(&self, limit: usize) -> String {
        let list = self.entries.read().await;
        let mut out = String::new();
        for e in list.iter().take(limit) {
            out.push_str(&format!(
                "【{}】{}\n{}\n",
                e.memory_type, e.title, e.content
            ));
        }
        if out.is_empty() {
            "（暂无长期记忆）".into()
        } else {
            out
        }
    }

    async fn persist(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let list = self.entries.read().await;
        let s = serde_json::to_string_pretty(&*list)?;
        std::fs::write(&self.path, s)?;
        Ok(())
    }
}
