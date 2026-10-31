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

    /// 热更新唤醒词检测阈值（voice-serve 侧会重建检测器）
    pub async fn set_kws_threshold(&self, threshold: f32) -> Result<()> {
        let r = self
            .client
            .post(format!("{}/kws_config", self.base()))
            .json(&json!({"threshold": threshold}))
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let v: serde_json::Value = r.json().await?;
        let t = v["text"].as_str().unwrap_or("");
        if t == "kws config updated" {
            Ok(())
        } else {
            Err(anyhow::anyhow!("{t}"))
        }
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

    /// 一次性录音转文字（可指定"说完后静默多久"判一句话结束）。
    /// 连续对话模式用更短的静默阈值（1.5s），让多轮对话更跟手。
    pub async fn listen_once_silence(&self, max_secs: f64, silence_secs: f64) -> Result<String> {
        let r = self
            .client
            .post(format!("{}/listen_once", self.base()))
            .json(&json!({"max_secs": max_secs, "silence_secs": silence_secs}))
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

    /// 一次性录音转文字（可指定"说完后静默多久"判一句话结束，并忽略本机 AI 播报期间的音频）。
    /// 连续对话模式用：忽略播放期音频 = 软件侧回音门，代替整机静音，
    /// AI 的回答仍可正常听到，而麦克风收进来的自己的声音不会被当成用户输入。
    pub async fn listen_once_gated(&self, max_secs: f64, silence_secs: f64) -> Result<String> {
        let r = self
            .client
            .post(format!("{}/listen_once", self.base()))
            .json(&json!({
                "max_secs": max_secs,
                "silence_secs": silence_secs,
                "ignore_playback": true
            }))
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

    /// 等待「人声起点」：VAD 判定有人开口即返回 true，超时无人说话返回 false。
    /// 连续对话模式 AI 工作期间用它做打断判定。
    pub async fn wait_speech(&self, max_secs: f64) -> bool {
        let r = self
            .client
            .post(format!("{}/wait_speech", self.base()))
            .json(&json!({"max_secs": max_secs}))
            .timeout(Duration::from_secs((max_secs + 10.0) as u64))
            .send()
            .await;
        match r {
            Ok(resp) => resp
                .json::<serde_json::Value>()
                .await
                .map(|v| v["text"].as_str().unwrap_or("") == "ok")
                .unwrap_or(false),
            Err(_) => false,
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
}
