//! 全局配置：双 Agent API、语音阈值、skills、TTS 后端等。
//! 持久化到 data/config.json，UI 可读写。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_CORE_PORT: u16 = 8410;
pub const DEFAULT_VOICE_PORT: u16 = 8420;

/// serde 默认值：中途询问开关默认开启
pub fn default_true() -> bool {
    true
}

/// 单条 API 上游配置（多 API key / 多 base_url 分摊 RPM 压力）。
/// 每个上游持有独立 RPM 限流器，各自遵守自己的每分钟配额。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// 显示名（如 "本地 Ollama" / "远端 API"）
    #[serde(default)]
    pub name: String,
    /// 上游地址（本地如 http://127.0.0.1:11434，**不要带 /v1**，SDK 会按协议自动拼接）
    pub base_url: String,
    /// API Key（本地 Ollama 可为空）
    #[serde(default)]
    pub api_key: String,
    /// 模型名。每套模型配置独立使用自己的模型名（不同 URL / 不同模型可并存），
    /// 切换当前模型 = 切换 active_model 指向的配置。
    pub model: String,
    /// 该上游独立的每分钟请求数上限；缺省时用全局 rpm_limit
    #[serde(default)]
    pub rpm_limit: Option<u32>,
    /// 同服务商同模型的多个 API Key：每个 Key 展开为一个独立轮询通道（同 URL、同模型）
    #[serde(default)]
    pub api_keys: Vec<String>,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            rpm_limit: None,
            api_keys: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 数据目录（对话/记忆/定时任务/skills 配置）
    pub data_dir: PathBuf,

    // ---- 主 Agent ----
    pub main_base_url: String,
    pub main_api_key: String,
    pub main_model: String,
    pub main_system_prompt: String,

    // ---- 语音 ----
    pub voice_port: u16,
    /// 唤醒词
    pub wakeword: String,
    /// 唤醒提示音
    pub beep_file: String,
    /// TTS 音色
    pub voice: String,
    /// 语速（1.0 为正常）
    pub rate: f32,
    /// TTS 后端: "internal"(edge-tts-rust) / "goose-tts"(外部二进制)
    pub tts_backend: String,
    /// 外部 TTS 二进制路径（goose-tts-copy）
    pub goose_tts_path: String,
    /// 打断关键词（识别到即打断当前任务）
    pub interrupt_keywords: Vec<String>,
    /// 录音最长秒数（用户说完自动截断）
    pub max_record_secs: f64,
    /// 唤醒词检测阈值（keywords_threshold，越低越灵敏）
    pub kws_threshold: f32,
    /// 语音交互开关
    pub voice_enabled: bool,

    // ---- Skills ----
    /// skills 根目录列表（每个 skill 一个子文件夹）
    pub skill_dirs: Vec<PathBuf>,

    // ---- 会话 ----
    /// 主 agent 最大轮数
    pub max_turns: u32,
    /// 每分钟 API 请求数上限（默认 20，超限客户端侧等待；上游拒绝时 SDK 自动重试）
    pub rpm_limit: u32,
    /// 模型思考开关（默认 false=关闭思考；本地模型自动加 think:false 参数/用 -nothink 后缀模型）
    pub enable_thinking: bool,
    /// 允许 AI 中途主动询问用户（语音问答）：AI 可调用 AskUserQuestion 播报问题并聆听语音回复。
    #[serde(default = "default_true")]
    pub ask_user_enabled: bool,

    // ---- 多 API 上游（单 Agent 轮询分摊 RPM）----
    /// 多个上游（多 key/多 base_url/多模型）。非空时优先使用本列表构建 Agent，
    /// 请求按 round-robin 轮询分摊到各上游，每个上游独立遵守自己的 RPM 限速。
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    /// 当前使用的模型配置名（providers 中 name 匹配；空/找不到时回退第一个）
    #[serde(default)]
    pub active_model: String,
}

impl Default for Config {
    fn default() -> Self {
        let root = crate::paths::project_root();
        Self {
            data_dir: root.join("data"),
            main_base_url: "https://api.openai.com/v1".into(),
            main_api_key: String::new(),
            main_model: "gpt-4o".into(),
            main_system_prompt: "你是星舰进取号的舰载电脑，代号 Computer，用户平时就喊你 computer。\
语气冷静、简洁、专业，像精炼的星舰通讯；说中文，用户让你做什么就做什么，不客套、不解释废话。"
                .into(),
            voice_port: DEFAULT_VOICE_PORT,
            wakeword: "computer".into(),
            beep_file: crate::paths::resource("wake_sound.wav")
                .to_string_lossy()
                .into_owned(),
            voice: "zh-CN-XiaoxiaoNeural".into(),
            rate: 1.05,
            tts_backend: "goose-tts".into(),
            goose_tts_path: crate::paths::resource("goose-tts")
                .to_string_lossy()
                .into_owned(),
            interrupt_keywords: vec!["stop".into(), "停止".into(), "停".into(), "够了".into(), "取消".into()],
            max_record_secs: 120.0,
            kws_threshold: 0.25,
            voice_enabled: true,
            skill_dirs: Vec::new(),
            max_turns: 1000,
            rpm_limit: 20,
            enable_thinking: false,
            ask_user_enabled: true,
            providers: Vec::new(),
            active_model: String::new(),
        }
    }
}

impl Config {
    pub fn voice_base(&self) -> String {
        // voice-serve 固定绑定 127.0.0.1（本地回环），无需可配置 host
        format!("http://127.0.0.1:{}", self.voice_port)
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("解析 config.json 失败，使用默认配置: {e}");
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let s = serde_json::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        Ok(())
    }

    /// 旧项目根前缀（历史版本硬编码 /Users/zhj/Projects/star-trek-assistant）
    fn old_project_root() -> PathBuf {
        PathBuf::from(
            std::env::var("HOME")
                .unwrap_or_default()
                .trim_end_matches('/'),
        )
        .join("Projects/star-trek-assistant")
    }

    /// 把历史配置中指向旧绝对位置（旧项目根前缀，或换机后已不存在的绝对路径）
    /// 的路径字段迁移为「相对项目根」的可移植形式（存储层相对、运行层解析）。
    /// 返回是否有变更，调用方决定是否回写 config.json。
    pub fn migrate_absolute_paths(&mut self) -> bool {
        let mut changed = false;
        let old_root = Self::old_project_root();

        // beep_file：旧前缀或不存在 -> resources/<文件名>
        if is_old_abs(&self.beep_file, &old_root) {
            if let Some(name) = Path::new(&self.beep_file).file_name() {
                self.beep_file = format!("resources/{}", name.to_string_lossy());
                changed = true;
            }
        }
        // goose_tts_path：旧前缀或不存在 -> resources/<文件名>
        if is_old_abs(&self.goose_tts_path, &old_root) {
            if let Some(name) = Path::new(&self.goose_tts_path).file_name() {
                self.goose_tts_path = format!("resources/{}", name.to_string_lossy());
                changed = true;
            }
        }
        // data_dir：旧前缀 + /data -> data
        let old_data = old_root.join("data");
        if self.data_dir == old_data || self.data_dir.starts_with(&old_data) {
            self.data_dir = PathBuf::from("data");
            changed = true;
        }
        // skill_dirs：移除位于旧项目根内的条目（换机后必然失效，且非用户自定义外部目录）
        let before = self.skill_dirs.len();
        self.skill_dirs.retain(|d| !d.starts_with(&old_root));
        if self.skill_dirs.len() != before {
            changed = true;
        }

        changed
    }

    /// 将存储层相对路径解析为运行时绝对路径（相对基于项目根），
    /// 供加载 config.json 后调用；后续所有消费方拿到的都是可用绝对路径。
    pub fn resolve_paths(&mut self) {
        let root = crate::paths::project_root();
        if !Path::new(&self.beep_file).is_absolute() {
            self.beep_file = root
                .join(&self.beep_file)
                .to_string_lossy()
                .into_owned();
        }
        if !Path::new(&self.goose_tts_path).is_absolute() {
            self.goose_tts_path = root
                .join(&self.goose_tts_path)
                .to_string_lossy()
                .into_owned();
        }
        if !self.data_dir.is_absolute() {
            self.data_dir = root.join(&self.data_dir);
        }
        for d in self.skill_dirs.iter_mut() {
            if !d.is_absolute() {
                *d = root.join(&d);
            }
        }
    }
}

/// 判断存储路径是否为「旧绝对路径」：以旧项目根前缀开头，或绝对且当前不存在。
fn is_old_abs(p: &str, old_root: &Path) -> bool {
    let path = Path::new(p);
    if !path.is_absolute() {
        return false;
    }
    if path.starts_with(old_root) {
        return true;
    }
    !path.exists()
}
