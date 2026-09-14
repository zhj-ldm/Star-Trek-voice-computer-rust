//! voice-serve 客户端：core 通过 HTTP 驱动语音链路。
//! base 端点用 Arc<Mutex> 承载，运行期可随配置热更新。

use anyhow::Result;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct VoiceClient {
    base: Arc<Mutex<String>>,
    client: reqwest::Client,
}

impl Default for VoiceClient {
    fn default() -> Self {
        Self {
            base: Arc::new(Mutex::new("http://127.0.0.1:8420".into())),
            client: reqwest::Client::new(),
        }
    }
}

impl VoiceClient {
    pub fn set_base(&self, base: String) {
        if let Ok(mut b) = self.base.lock() {
            *b = base;
        }
    }

    fn base(&self) -> String {
        self.base.lock().map(|b| b.clone()).unwrap_or_default()
    }

    pub async fn health(&self) -> bool {
        let r = self
            .client
            .get(format!("{}/health", self.base()))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        matches!(r, Ok(resp) if resp.status().is_success())
    }

    pub async fn status(&self) -> Result<serde_json::Value> {
        let r = self
            .client
            .get(format!("{}/status", self.base()))
            .timeout(Duration::from_secs(3))
            .send()
            .await?;
        Ok(r.json().await?)
    }

    /// 合成并播放文本（阻塞至播完或被打断）
    pub async fn speak(&self, text: &str, voice: &str, rate: f32) -> Result<()> {
        let r = self
            .client
            .post(format!("{}/speak", self.base()))
            .json(&json!({"text": text, "voice": voice, "rate": rate}))
            .timeout(Duration::from_secs(120))
            .send()
            .await?;
        let v: serde_json::Value = r.json().await?;
        let t = v["text"].as_str().unwrap_or("");
        if t == "ok" {
            Ok(())
        } else {
            Err(anyhow::anyhow!("{t}"))
        }
    }

    /// 打断当前播放
    pub async fn interrupt(&self) -> Result<()> {
        self.client
            .post(format!("{}/interrupt", self.base()))
            .timeout(Duration::from_secs(3))
            .send()
            .await?;
        Ok(())
    }

    pub async fn is_speaking(&self) -> bool {
        let r = self
            .client
            .get(format!("{}/is_speaking", self.base()))
            .timeout(Duration::from_secs(2))
            .send()
            .await;
        match r {
            Ok(resp) => resp
                .text()
                .await
                .map(|t| t.trim() == "true")
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 播放提示音
    pub async fn beep(&self, file: Option<&str>) -> Result<()> {
        let mut body = json!({});
        if let Some(f) = file {
            body = json!({"file": f});
        }
        self.client
            .post(format!("{}/beep", self.base()))
            .json(&body)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;
        Ok(())
    }

    /// 一次性录音转文字
    pub async fn listen_once(&self, max_secs: f64) -> Result<String> {
        let r = self
            .client
            .post(format!("{}/listen_once", self.base()))
            .json(&json!({"max_secs": max_secs}))
            .timeout(Duration::from_secs((max_secs + 20.0) as u64))
            .send()
            .await?;
        let v: serde_json::Value = r.json().await?;
        let t = v["text"].as_str().unwrap_or("");
        if t.is_empty() || t.starts_with("error") {
            Err(anyhow::anyhow!("{t}"))
        } else {
            Ok(t.to_string())
        }
    }

    /// 设置常驻监听开关
    pub async fn set_listening(&self, enabled: bool) -> Result<()> {
        self.client
            .post(format!("{}/listening", self.base()))
            .json(&json!({"enabled": enabled}))
            .timeout(Duration::from_secs(3))
            .send()
            .await?;
        Ok(())
    }

    /// 直接用外部 goose-tts 二进制播放（tts_backend = "goose-tts"）
    pub fn speak_goose_tts(&self, bin: &str, text: &str, voice: &str, rate: f32) -> Result<()> {
        let rate_pct = ((rate - 1.0) * 100.0).round() as i64;
        let rate_str = format!("{:+}%", rate_pct);
        let status = std::process::Command::new(bin)
            .args(["--text", text, "--voice", voice, "--rate", &rate_str])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("goose-tts exit: {status}"))
        }
    }
}
