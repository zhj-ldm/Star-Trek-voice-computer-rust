//! VAD（Voice Activity Detection）：区分人声与环境噪音，驱动录音结束判定。
//!
//! 用 sherpa-onnx 内置的 Silero VAD（v5，629 KB onnx），纯 ONNX Runtime 推理，
//! 无 Python 运行时依赖。
//!
//! 与唤醒链路里 RMS 门控的分工：RMS 负责电平归一化（参与增益计算），VAD 只输出
//! 「此刻有没有人在说话」这一个布尔量，不改音频。录音何时结束由 VAD 判定。

use anyhow::{Context, Result};
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};
use std::path::Path;

/// 说完话后需要持续多少秒没有人声才结束录音。
/// 2.5s 的取值：短于 2s 会在思考换气、句中停顿处把话截断；长于 3s 体感明显迟钝。
pub const SILENCE_LIMIT_SECS: f32 = 2.5;

/// 短于此时长的语音视为误触发（清嗓、敲键盘、开关门），不送去识别。
pub const MIN_SPEECH_SECS: f32 = 0.4;

/// Silero VAD 固定要求 16 kHz（与上游 Paraformer 一致，采集侧已重采样到 16k）。
const SAMPLE_RATE: i32 = 16000;

/// 实例内部是 sherpa 的线程安全单对象，用Arc 共享：模型加载一次常驻，
/// 每次录音前调reset() 清掉上一轮的 LSTM 状态与待结算段落。
pub struct Vad {
    det: VoiceActivityDetector,
}

impl Vad {
    /// 从 onnx 模型文件加载。模型缺失时返回 Err，由调用方降级回 RMS 判定。
    pub fn new(model_path: &Path) -> Result<Self> {
        let mut config = VadModelConfig::default();
        config.silero_vad = SileroVadModelConfig {
            model: Some(model_path.to_string_lossy().into_owned()),
            // 0.5 是 Silero 官方推荐工作点：低于 0.3 会把键盘声/风扇当人声，
            // 高于 0.7 在安静环境里会漏掉轻声说话。
            threshold: 0.5,
            // 这两项只决定上游 detected() 翻转的快慢；真正的结束时机由
            // SILENCE_LIMIT_SECS 在外部计时，所以这里取小值让 detected() 及时转 false。
            min_speech_duration: 0.1,
            min_silence_duration: 0.2,
            // Silero v5 强制要求 512；v4 也用 512，保持一致。
            window_size: 512,
            // 必须 > 0：为 0 时上游 buffer 永远超限，会恒定切到 threshold=0.90 的
            // 激进分支（min_silence=0.1s），结果是人声几乎检测不到。
            max_speech_duration: 60.0,
        };
        config.sample_rate = SAMPLE_RATE;
        config.num_threads = 1;
        config.provider = Some("cpu".into());
        let det = VoiceActivityDetector::create(&config, 60.0)
            .context("创建 sherpa-onnx VAD 失败")?;
        Ok(Self { det })
    }

    /// 清空上一轮的状态：LSTM 隐状态、待结算语音段落。录音开始前必须调用。
    pub fn reset(&self) {
        self.det.reset();
    }

    /// 推入一块 16 kHz 单声道音频，返回推入结束时是否检测到人声。
    pub fn push(&self, samples: &[f32]) -> bool {
        self.det.accept_waveform(samples);
        // 上游在检测到一段静音后会把该语音段落入队，这里只用布尔判定、不取分段结果，
        // 所以立即丢弃，避免长时间录音时队列无限增长。
        while !self.det.is_empty() {
            self.det.pop();
        }
        self.det.detected()
    }

    /// 把最后一段还没结算的语音冲出来（录音到硬超时、说话说满 max_secs 时用）。
    pub fn flush(&self) {
        self.det.flush();
    }
}

/// 结束判定的结果与依据，写进诊断便于排查「为什么提前/迟迟不结束」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EndReason {
    /// VAD 判定连续 SILENCE_LIMIT_SECS 无人声
    VadSilence,
    /// 无 VAD 模型，降级用 RMS 能量判定
    RmsSilence,
    /// 到达 max_secs 硬超时
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_reason_is_comparable() {
        // 诊断日志按此值聚类，两种静音路径必须能区分开
        assert_ne!(EndReason::VadSilence, EndReason::RmsSilence);
        assert_eq!(EndReason::VadSilence, EndReason::VadSilence);
    }

    #[test]
    fn silence_limit_is_long_enough_for_thinking_pause() {
        // 回归保护：曾经的 1.2s 会在句中换气处截断，下限锁到 2s 以上
        assert!(SILENCE_LIMIT_SECS >= 2.0, "静默阈值过短，会截断句子");
    }

    #[test]
    fn missing_model_is_error_not_panic() {
        let p = Path::new("/nonexistent/silero_vad.onnx");
        assert!(Vad::new(p).is_err());
    }
}