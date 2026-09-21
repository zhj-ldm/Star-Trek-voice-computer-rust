//! Speech-to-text using sherpa-onnx Paraformer-zh (FunASR 同源), replacing whisper.

use anyhow::{Context, Result};
use sherpa_onnx::{OfflineParaformerModelConfig, OfflineRecognizer, OfflineRecognizerConfig};

pub struct Stt {
    rec: OfflineRecognizer,
}

impl Stt {
    pub fn new(model_dir: &std::path::Path) -> Result<Self> {
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.paraformer = OfflineParaformerModelConfig {
            model: Some(model_dir.join("model.int8.onnx").to_string_lossy().into_owned()),
        };
        config.model_config.tokens = Some(model_dir.join("tokens.txt").to_string_lossy().into_owned());
        config.model_config.num_threads = 4;
        config.model_config.provider = Some("cpu".into());
        let rec = OfflineRecognizer::create(&config).context("加载 sherpa-onnx 识别器失败")?;
        Ok(Self { rec })
    }

    pub fn transcribe(&self, sample_rate: i32, samples: &[f32]) -> Result<String> {
        let stream = self.rec.create_stream();
        stream.accept_waveform(sample_rate, samples);
        self.rec.decode(&stream);
        Ok(stream.get_result().map(|r| r.text).unwrap_or_default())
    }

    /// Transcribe a wav file (16k mono).
    #[allow(dead_code)]
    pub fn transcribe_file(&self, path: &str) -> Result<String> {
        let wave = sherpa_onnx::Wave::read(path).context("打开 wav 失败")?;
        let stream = self.rec.create_stream();
        stream.accept_waveform(wave.sample_rate(), wave.samples());
        self.rec.decode(&stream);
        Ok(stream.get_result().map(|r| r.text).unwrap_or_default())
    }
}
