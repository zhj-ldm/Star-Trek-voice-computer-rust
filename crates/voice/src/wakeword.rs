//! Wake-word detection using sherpa-onnx KWS (zipformer transducer).
//! Detects the wake word (default "computer") from live 16kHz PCM audio,
//! mirroring the proven goose implementation. Models are bundled locally.

use anyhow::{Context, Result};
use sherpa_onnx::{
    KeywordSpotter, KeywordSpotterConfig, OnlineModelConfig, OnlineStream,
    OnlineTransducerModelConfig,
};
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

const ENCODER: &str = "encoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
const DECODER: &str = "decoder-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
const JOINER: &str = "joiner-epoch-12-avg-2-chunk-16-left-64.int8.onnx";
const TOKENS: &str = "tokens.txt";
const KEYWORDS: &str = "keywords_computer.txt";
const TAIL_PADDING_SECS: f32 = 0.66;

/// Validate keyword tokens exist in tokens.txt up front: sherpa-onnx aborts
/// the whole process (exit 255) on an unknown token, so we must not hand it
/// a misconfigured keyword file.
fn validate_keywords(keywords_file: &Path, tokens_file: &Path) -> Result<()> {
    let keywords_text = std::fs::read_to_string(keywords_file)
        .with_context(|| format!("failed to read keywords file {}", keywords_file.display()))?;
    let tokens_text = std::fs::read_to_string(tokens_file)
        .with_context(|| format!("failed to read tokens file {}", tokens_file.display()))?;
    let known_tokens: HashSet<&str> = tokens_text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();

    for line in keywords_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for token in line.split_whitespace() {
            if !known_tokens.contains(token) {
                anyhow::bail!(
                    "wake word token '{token}' (from keyword line '{line}') is not present in {}",
                    tokens_file.display()
                );
            }
        }
    }
    Ok(())
}

pub struct WakewordDetector {
    kws: KeywordSpotter,
    sample_rate: i32,
}

impl WakewordDetector {
    /// 默认使用 keywords_computer.txt（唤醒词表）
    pub fn new(dir: &Path, threshold: f32) -> Result<Self> {
        Self::new_with_keywords(dir, KEYWORDS, threshold)
    }

    /// 指定关键词文件名（唤醒表 / 打断表共用同一模型，仅关键词不同）
    pub fn new_with_keywords(dir: &Path, keywords_name: &str, threshold: f32) -> Result<Self> {
        let encoder = dir.join(ENCODER).to_string_lossy().into_owned();
        let decoder = dir.join(DECODER).to_string_lossy().into_owned();
        let joiner = dir.join(JOINER).to_string_lossy().into_owned();
        let tokens = dir.join(TOKENS).to_string_lossy().into_owned();
        let keywords = dir.join(keywords_name).to_string_lossy().into_owned();

        validate_keywords(dir.join(keywords_name).as_path(), dir.join(TOKENS).as_path())?;

        let model_config = OnlineModelConfig {
            transducer: OnlineTransducerModelConfig {
                encoder: Some(encoder),
                decoder: Some(decoder),
                joiner: Some(joiner),
            },
            tokens: Some(tokens),
            num_threads: 2,
            ..Default::default()
        };
        let config = KeywordSpotterConfig {
            model_config,
            keywords_file: Some(keywords),
            keywords_score: 1.0,
            keywords_threshold: threshold,
            ..Default::default()
        };

        let kws = KeywordSpotter::create(&config)
            .context("failed to create sherpa-onnx keyword spotter")?;
        Ok(Self {
            kws,
            sample_rate: 16000,
        })
    }

    /// Detect whether `samples` (16kHz mono f32 PCM, normalized to [-1, 1])
    /// contains the configured wake word. Streams in 100ms chunks with a
    /// decode step after each chunk, matching live microphone cadence.
    /// KWS 是触发式检测：命中状态必须及时用 get_result 取走，否则会被
    /// 后续输入覆盖导致返回 None（只在 input_finished 后取一次会漏掉命中）。
    pub fn detect(&self, samples: &[f32]) -> Option<String> {
        let stream = self.kws.create_stream();
        let chunk_len = (self.sample_rate as usize / 10).max(1);
        for chunk in samples.chunks(chunk_len) {
            stream.accept_waveform(self.sample_rate, chunk);
            while self.kws.is_ready(&stream) {
                self.kws.decode(&stream);
            }
            if let Some(r) = self.kws.get_result(&stream) {
                let kw = r.keyword.trim().to_string();
                if !kw.is_empty() {
                    return Some(kw);
                }
            }
        }
        let tail_len = (self.sample_rate as f32 * TAIL_PADDING_SECS) as usize;
        let tail = vec![0.0f32; tail_len];
        stream.accept_waveform(self.sample_rate, &tail);
        while self.kws.is_ready(&stream) {
            self.kws.decode(&stream);
        }
        if let Some(r) = self.kws.get_result(&stream) {
            let kw = r.keyword.trim().to_string();
            if !kw.is_empty() {
                return Some(kw);
            }
        }
        stream.input_finished();
        while self.kws.is_ready(&stream) {
            self.kws.decode(&stream);
        }
        match self.kws.get_result(&stream) {
            Some(r) if !r.keyword.trim().is_empty() => Some(r.keyword.trim().to_string()),
            _ => None,
        }
    }
}

/// 一条唤醒检测诊断记录（前端 /kws_diag 展示用，排查"唤醒不了"根因）
#[derive(Clone, Debug, serde::Serialize)]
pub struct KwsDiag {
    /// 时间戳（epoch 毫秒）
    pub ts: u128,
    /// 本批样本数
    pub samples: usize,
    /// 本批能量 RMS
    pub rms: f32,
    /// 推理耗时（毫秒）
    pub elapsed_ms: f32,
    /// 是否命中
    pub hit: bool,
    /// 命中的唤醒词
    pub keyword: String,
}

/// 流式唤醒检测器：对前端增量推来的音频做滑窗解码，
/// 复用同一 KeywordSpotter 实例（与 WakewordDetector::detect 不冲突，
/// 各自持有独立 stream），并记录检测诊断日志。
pub struct StreamingKws {
    kws: KeywordSpotter,
    sample_rate: i32,
    stream: Option<OnlineStream>,
    last_input: Option<Instant>,
    diag: VecDeque<KwsDiag>,
}

impl StreamingKws {
    pub fn new(dir: &Path, threshold: f32) -> Result<Self> {
        let encoder = dir.join(ENCODER).to_string_lossy().into_owned();
        let decoder = dir.join(DECODER).to_string_lossy().into_owned();
        let joiner = dir.join(JOINER).to_string_lossy().into_owned();
        let tokens = dir.join(TOKENS).to_string_lossy().into_owned();
        let keywords = dir.join(KEYWORDS).to_string_lossy().into_owned();

        validate_keywords(dir.join(KEYWORDS).as_path(), dir.join(TOKENS).as_path())?;

        let model_config = OnlineModelConfig {
            transducer: OnlineTransducerModelConfig {
                encoder: Some(encoder),
                decoder: Some(decoder),
                joiner: Some(joiner),
            },
            tokens: Some(tokens),
            num_threads: 2,
            ..Default::default()
        };
        let config = KeywordSpotterConfig {
            model_config,
            keywords_file: Some(keywords),
            keywords_score: 1.0,
            keywords_threshold: threshold,
            ..Default::default()
        };
        let kws = KeywordSpotter::create(&config)
            .context("failed to create sherpa-onnx keyword spotter")?;
        Ok(Self {
            kws,
            sample_rate: 16000,
            stream: None,
            last_input: None,
            diag: VecDeque::new(),
        })
    }

    /// 清空流式状态（前端开启监听 / 切换会话时调用）
    pub fn reset(&mut self) {
        self.stream = None;
        self.last_input = None;
    }

    /// 最近 N 条诊断记录（新的在前）
    #[allow(dead_code)]
    pub fn diag(&self, n: usize) -> Vec<KwsDiag> {
        self.diag.iter().rev().take(n).cloned().collect()
    }

    /// 喂入增量 PCM 采样（16kHz mono f32）。返回命中唤醒词（若有）。
    /// 流在无输入超过 2 秒或命中后自动重建，避免陈旧状态累积。
    pub fn feed(&mut self, samples: &[f32]) -> Option<String> {
        let now = Instant::now();
        let stale = self
            .last_input
            .map(|t| now.duration_since(t).as_secs_f32() > 2.0)
            .unwrap_or(true);
        // 取回旧流（或新建），feed 期间 self.stream 置空避免借用冲突
        let stream = if stale {
            self.kws.create_stream()
        } else {
            self.stream.take()
                .unwrap_or_else(|| self.kws.create_stream())
        };
        self.last_input = Some(now);
        let chunk_len = (self.sample_rate as usize / 10).max(1);
        let mut hit: Option<String> = None;
        for chunk in samples.chunks(chunk_len) {
            stream.accept_waveform(self.sample_rate, chunk);
            while self.kws.is_ready(&stream) {
                self.kws.decode(&stream);
            }
            // 触发式命中：每块解码后立即取结果，命中即返回，避免状态被后续输入覆盖
            if let Some(r) = self.kws.get_result(&stream) {
                let kw = r.keyword.trim().to_string();
                if !kw.is_empty() {
                    hit = Some(kw);
                    break;
                }
            }
        }
        if hit.is_none() {
            if let Some(r) = self.kws.get_result(&stream) {
                let kw = r.keyword.trim().to_string();
                if !kw.is_empty() {
                    hit = Some(kw);
                }
            }
        }
        // 命中则丢弃旧流（下次 feed 重建，避免重复触发）；否则存回以累积上下文
        if hit.is_some() {
            self.stream = None;
        } else {
            self.stream = Some(stream);
        }
        let elapsed = now.elapsed().as_secs_f32() * 1000.0;
        let rms = samples
            .iter()
            .map(|s| s * s)
            .sum::<f32>()
            / samples.len().max(1) as f32;
        self.diag.push_back(KwsDiag {
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            samples: samples.len(),
            rms: rms.sqrt(),
            elapsed_ms: elapsed,
            hit: hit.is_some(),
            keyword: hit.clone().unwrap_or_default(),
        });
        if self.diag.len() > 300 {
            let over = self.diag.len() - 300;
            for _ in 0..over {
                self.diag.pop_front();
            }
        }
        hit
    }
}

