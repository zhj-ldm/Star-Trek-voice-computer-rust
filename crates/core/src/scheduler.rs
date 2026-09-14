//! 定时任务：cron 表达式调度，JSON 持久化，到点触发回调。

use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: String,
    pub title: String,
    /// 触发后交给主 Agent 执行的指令
    pub prompt: String,
    /// cron 表达式 "分 时 日 月 周"
    pub cron_expr: String,
    pub enabled: bool,
    pub last_run: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Default)]
pub struct Scheduler {
    tasks: Arc<RwLock<Vec<ScheduleTask>>>,
    path: PathBuf,
    on_trigger: Option<Arc<dyn Fn(String, String) + Send + Sync>>,
}

impl Scheduler {
    pub fn init(&mut self, path: PathBuf) {
        self.path = path.clone();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(list) = serde_json::from_str::<Vec<ScheduleTask>>(&s) {
                self.tasks = Arc::new(RwLock::new(list));
            }
        }
    }

    pub fn set_on_trigger(&mut self, f: Arc<dyn Fn(String, String) + Send + Sync>) {
        self.on_trigger = Some(f);
    }

    pub async fn list(&self) -> Vec<ScheduleTask> {
        self.tasks.read().await.clone()
    }

    pub async fn add(&self, title: &str, prompt: &str, cron_expr: &str) -> Result<ScheduleTask> {
        validate_cron(cron_expr)?;
        let task = ScheduleTask {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.to_string(),
            prompt: prompt.to_string(),
            cron_expr: cron_expr.to_string(),
            enabled: true,
            last_run: None,
            created_at: Local::now().to_rfc3339(),
        };
        {
            let mut list = self.tasks.write().await;
            list.push(task.clone());
        }
        self.persist().await?;
        Ok(task)
    }

    pub async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        prompt: Option<&str>,
        cron_expr: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<bool> {
        if let Some(c) = cron_expr {
            validate_cron(c)?;
        }
        let mut found = false;
        {
            let mut list = self.tasks.write().await;
            if let Some(t) = list.iter_mut().find(|t| t.id == id) {
                if let Some(x) = title {
                    t.title = x.to_string();
                }
                if let Some(x) = prompt {
                    t.prompt = x.to_string();
                }
                if let Some(x) = cron_expr {
                    t.cron_expr = x.to_string();
                }
                if let Some(x) = enabled {
                    t.enabled = x;
                }
                found = true;
            }
        }
        if found {
            self.persist().await?;
        }
        Ok(found)
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        {
            let mut list = self.tasks.write().await;
            list.retain(|t| t.id != id);
        }
        self.persist().await?;
        Ok(())
    }

    /// 每秒检查一次，命中 cron 的触发
    pub async fn run_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.tick().await; // 立即 tick
        loop {
            interval.tick().await;
            let now = Local::now();
            let mut to_run: Vec<(String, String)> = Vec::new();
            {
                let mut list = self.tasks.write().await;
                for t in list.iter_mut() {
                    if !t.enabled {
                        continue;
                    }
                    let matched = match cron::Schedule::from_str(&normalize_cron(&t.cron_expr)) {
                        Ok(sched) => {
                            // cron 库以 UTC 处理，这里手动按当前分钟匹配
                            sched
                                .after(&(now - chrono::Duration::minutes(1)))
                                .take(2)
                                .any(|dt| {
                                    let dt = dt.with_timezone(&Local);
                                    dt.format("%Y-%m-%d %H:%M").to_string()
                                        == now.format("%Y-%m-%d %H:%M").to_string()
                                })
                        }
                        Err(_) => false,
                    };
                    if matched {
                        let last = t.last_run.as_deref().unwrap_or("");
                        if last != now.format("%Y-%m-%d %H:%M").to_string() {
                            t.last_run = Some(now.format("%Y-%m-%d %H:%M").to_string());
                            to_run.push((t.id.clone(), t.prompt.clone()));
                        }
                    }
                }
            }
            if !to_run.is_empty() {
                let _ = self.persist().await;
            }
            for (id, prompt) in to_run {
                if let Some(cb) = &self.on_trigger {
                    cb(id, prompt);
                }
            }
        }
    }

    async fn persist(&self) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let list = self.tasks.read().await;
        let s = serde_json::to_string_pretty(&*list)?;
        std::fs::write(&self.path, s)?;
        Ok(())
    }
}

fn validate_cron(expr: &str) -> Result<()> {
    cron::Schedule::from_str(&normalize_cron(expr))
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("cron 表达式无效: {e}"))
}

/// cron 0.12 需要 6 字段（含秒）。用户侧约定 5 字段「分 时 日 月 周」，
/// 这里自动补秒前缀，5/6 字段均可。
fn normalize_cron(expr: &str) -> String {
    let t = expr.trim();
    let fields: Vec<&str> = t.split_whitespace().collect();
    if fields.len() == 5 {
        format!("0 {t}")
    } else {
        t.to_string()
    }
}
