//! 本地 embedding 重排（对齐 SearchPIN 的 `_embedding_rerank`）
//!
//! 默认模型：EmbeddingGemma-300M（ONNX int8，约 309MB）。
//! 采用三态策略，避免开源用户被动下载大模型：
//! - `auto`（默认）：本地有完整模型才启用向量重排，没有就直接用词法，**绝不联网**；
//! - `on`：期望使用向量重排，模型缺失时仅报错提示，不自动下载；
//! - `off`：强制词法重排。
//! 模型只能用 `aggrsearch models download` 显式安装。
//! 文档表示与 SearchPIN 一致：`title + " " + snippet`，query 与文档一起 embed，
//! 用余弦相似度作为 `_rerank_score`。模型不可用时由调用方回退到词法重排。

use fastembed::{AutoDevicePolicy, EmbeddingModel, TextEmbedding, TextInitOptions};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// EmbeddingGemma 官方检索提示模板（只对 Gemma 系模型加，避免污染其他模型）
const GEMMA_QUERY_PREFIX: &str = "task: search result | query: ";
const GEMMA_DOC_PREFIX: &str = "title: ";
const GEMMA_DOC_SEP: &str = " | text: ";

/// 模型仓库里的通用 tokenizer / config 文件
const TOKENIZER_FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
];

struct ModelSpec {
    model: EmbeddingModel,
    name: &'static str,
    prompt: bool,
    max_length: usize,
    repo: &'static str,
    model_files: &'static [&'static str],
}

struct Loaded {
    model: Mutex<TextEmbedding>,
    name: &'static str,
    prompt: bool,
}

static LOADED: OnceLock<Result<Loaded, String>> = OnceLock::new();

/// embedding 工作模式（`AGGRSEARCH_EMBED` 控制）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 强制关闭，只用词法重排
    Off,
    /// 期望使用向量重排；模型缺失时报告错误但不下载
    On,
    /// 本地有模型才用，否则词法；默认值，绝不联网
    Auto,
}

/// 读取当前工作模式；未配置时为 `auto`
pub fn mode() -> Mode {
    match std::env::var("AGGRSEARCH_EMBED")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" | "0" | "false" | "none" | "no" => Mode::Off,
        "on" | "1" | "true" | "yes" => Mode::On,
        _ => Mode::Auto,
    }
}

/// 当前模式名（用于日志 / 健康检查）
pub fn mode_name() -> &'static str {
    match mode() {
        Mode::Off => "off",
        Mode::On => "on",
        Mode::Auto => "auto",
    }
}

/// 本次调用是否应尝试向量重排。
/// `auto` 只检查本地模型是否完整，不会触发下载；`on` 允许尝试加载（仍不下载）。
pub fn enabled() -> bool {
    match mode() {
        Mode::Off => false,
        Mode::On => true,
        Mode::Auto => local_ready(),
    }
}

fn model_spec() -> Result<ModelSpec, String> {
    let raw = std::env::var("AGGRSEARCH_EMBED_MODEL").unwrap_or_default();
    model_spec_from(&raw)
}

fn model_spec_from(raw: &str) -> Result<ModelSpec, String> {
    let key = raw.trim().to_ascii_lowercase();
    match key.as_str() {
        "" | "gemma" | "gemma-int8" | "embeddinggemma-300m" | "embeddinggemma300m" => {
            Ok(ModelSpec {
                model: EmbeddingModel::EmbeddingGemma300MQ,
                name: "embeddinggemma-300m-int8",
                prompt: true,
                max_length: 512,
                repo: "onnx-community/embeddinggemma-300m-ONNX",
                model_files: &[
                    "onnx/model_quantized.onnx",
                    "onnx/model_quantized.onnx_data",
                ],
            })
        }
        "gemma-q4" | "embeddinggemma-300m-q4" | "embeddinggemma300mq4" => Ok(ModelSpec {
            model: EmbeddingModel::EmbeddingGemma300MQ4,
            name: "embeddinggemma-300m-q4",
            prompt: true,
            max_length: 512,
            repo: "onnx-community/embeddinggemma-300m-ONNX",
            model_files: &["onnx/model_q4.onnx", "onnx/model_q4.onnx_data"],
        }),
        // 仅用于测试/CI：体积很小，验证 embedding 链路
        "minilm" | "all-minilm-l6-v2" | "allminilml6v2" => Ok(ModelSpec {
            model: EmbeddingModel::AllMiniLML6V2Q,
            name: "all-minilm-l6-v2-q",
            prompt: false,
            max_length: 512,
            repo: "Xenova/all-MiniLM-L6-v2",
            model_files: &["onnx/model_quantized.onnx"],
        }),
        other => Err(format!(
            "未知 AGGRSEARCH_EMBED_MODEL: {other}（可选: gemma、gemma-q4、minilm）"
        )),
    }
}

fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("AGGRSEARCH_EMBED_CACHE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }

    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local).join("aggrsearch").join("models");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Caches")
                .join("aggrsearch")
                .join("models");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            return PathBuf::from(xdg).join("aggrsearch").join("models");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".cache")
                .join("aggrsearch")
                .join("models");
        }
    }
    PathBuf::from(".aggrsearch-models")
}

fn intra_threads() -> Option<usize> {
    std::env::var("AGGRSEARCH_EMBED_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
}

fn hf_endpoint() -> String {
    std::env::var("HF_ENDPOINT")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_string())
}

fn model_files(spec: &ModelSpec) -> Vec<&'static str> {
    spec.model_files
        .iter()
        .chain(TOKENIZER_FILES.iter())
        .copied()
        .collect()
}

fn repo_cache_dir(root: &std::path::Path, repo: &str) -> PathBuf {
    root.join(format!("models--{}", repo.replace('/', "--")))
}

/// 大模型权重分段文件常有几百 MB，几 MB 的通常是下载被截断的残片
fn file_ok(name: &str, len: u64) -> bool {
    len > 0 && (!name.ends_with(".onnx_data") || len > 50_000_000)
}

fn files_ready(repo_dir: &std::path::Path, revision: &str, spec: &ModelSpec) -> bool {
    model_files(spec).iter().all(|file| {
        repo_dir
            .join("snapshots")
            .join(revision)
            .join(file)
            .metadata()
            .map(|m| file_ok(file, m.len()))
            .unwrap_or(false)
    })
}

/// 本地模型是否完整可用；只读文件系统，绝不联网。
pub fn local_ready() -> bool {
    let Ok(spec) = model_spec() else {
        return false;
    };
    let repo_dir = repo_cache_dir(&cache_dir(), spec.repo);
    std::fs::read_to_string(repo_dir.join("refs").join("main"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|rev| files_ready(&repo_dir, &rev, &spec))
        .unwrap_or(false)
}

fn spec_for(model: Option<&str>) -> Result<ModelSpec, String> {
    match model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(key) => model_spec_from(key),
        None => model_spec(),
    }
}

/// 显式下载模型（唯一的联网入口）。`model` 为空时使用 `AGGRSEARCH_EMBED_MODEL` 或默认值。
pub fn download(model: Option<&str>) -> Result<PathBuf, String> {
    let spec = spec_for(model)?;
    ensure_cache(&spec, true)
}

/// 删除本地模型缓存，回到词法重排。
pub fn remove(model: Option<&str>) -> Result<PathBuf, String> {
    let spec = spec_for(model)?;
    let dir = repo_cache_dir(&cache_dir(), spec.repo);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("删除模型目录失败: {e}"))?;
    }
    Ok(dir)
}

/// 已下载模型占用的字节数（未安装返回 0）
fn installed_size() -> u64 {
    let Ok(spec) = model_spec() else {
        return 0;
    };
    let dir = repo_cache_dir(&cache_dir(), spec.repo);
    let Ok(rev) = std::fs::read_to_string(dir.join("refs").join("main")) else {
        return 0;
    };
    let snapshot = dir.join("snapshots").join(rev.trim());
    model_files(&spec)
        .iter()
        .filter_map(|f| snapshot.join(f).metadata().ok().map(|m| m.len()))
        .sum()
}

fn http_client() -> Result<reqwest::blocking::Client, String> {
    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

    let mut headers = HeaderMap::new();
    if let Ok(token) = std::env::var("HF_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| format!("HF_TOKEN 非法: {e}"))?;
            headers.insert(AUTHORIZATION, value);
        }
    }
    reqwest::blocking::Client::builder()
        .user_agent(concat!("aggrsearch/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(1800))
        .default_headers(headers)
        .build()
        .map_err(|e| format!("创建下载客户端失败: {e}"))
}

/// 检查/补齐本地缓存目录。
/// `allow_download = false` 时只接受完整可用的本地模型，缺失直接报错（绝不联网）；
/// 只有显式的 `models download` 命令才允许传入 true。
/// 下载用普通 GET（不带 Range），兼容不支持 Range 的国内镜像（hf-mirror）。
fn ensure_cache(spec: &ModelSpec, allow_download: bool) -> Result<PathBuf, String> {
    let root = cache_dir();
    let repo_dir = repo_cache_dir(&root, spec.repo);
    let ref_file = repo_dir.join("refs").join("main");
    let files = model_files(spec);

    let mut revision = std::fs::read_to_string(&ref_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(rev) = &revision {
        if files_ready(&repo_dir, rev, spec) {
            return Ok(root);
        }
    }

    if !allow_download {
        return Err(format!(
            "本地未找到可用的 embedding 模型（{}）。运行 `aggrsearch models download` 安装，或继续使用词法重排",
            repo_dir.display()
        ));
    }

    let client = http_client()?;
    let endpoint = hf_endpoint();

    if revision
        .as_deref()
        .map(|rev| !files_ready(&repo_dir, rev, spec))
        .unwrap_or(true)
    {
        let url = format!("{endpoint}/api/models/{}", spec.repo);
        let resolved = client
            .get(&url)
            .send()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| r.json::<serde_json::Value>().ok())
            .and_then(|v| v.get("sha").and_then(|s| s.as_str()).map(str::to_string));
        revision = Some(resolved.unwrap_or_else(|| "main".to_string()));
    }
    let revision = revision.unwrap_or_else(|| "main".to_string());
    let snapshot = repo_dir.join("snapshots").join(&revision);

    std::fs::create_dir_all(&snapshot)
        .and_then(|_| std::fs::create_dir_all(repo_dir.join("refs")))
        .map_err(|e| format!("创建模型缓存目录失败: {e}"))?;

    for file in &files {
        let target = snapshot.join(file);
        if target.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            continue;
        }
        download_file(&client, &endpoint, spec.repo, &revision, file, &target)?;
    }

    std::fs::write(&ref_file, format!("{revision}\n"))
        .map_err(|e| format!("写入模型版本标记失败: {e}"))?;
    Ok(root)
}

fn download_file(
    client: &reqwest::blocking::Client,
    endpoint: &str,
    repo: &str,
    revision: &str,
    file: &str,
    target: &std::path::Path,
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let url = format!("{endpoint}/{repo}/resolve/{revision}/{file}");
    eprintln!("[embedding] 下载 {file} ...");

    let mut response = client
        .get(&url)
        .send()
        .map_err(|e| format!("下载 {file} 失败: {e}"))?
        .error_for_status()
        .map_err(|e| format!("下载 {file} 失败: {e}"))?;

    let tmp = target.with_extension("part");
    let mut out = std::fs::File::create(&tmp)
        .map_err(|e| format!("创建临时文件 {} 失败: {e}", tmp.display()))?;
    std::io::copy(&mut response, &mut out).map_err(|e| format!("写入 {file} 失败: {e}"))?;
    out.sync_all().ok();
    drop(out);
    std::fs::rename(&tmp, target).map_err(|e| format!("重命名模型文件失败: {e}"))?;

    let size = target.metadata().map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "[embedding] 已缓存 {file} ({:.1} MB)",
        size as f64 / 1_000_000.0
    );
    Ok(())
}

fn load() -> Result<&'static Loaded, String> {
    let result = LOADED.get_or_init(|| {
        let spec = model_spec()?;
        // 正常推理路径只读本地缓存；下载只允许由 `models download` 显式触发
        let cache = ensure_cache(&spec, false)?;
        let mut options = TextInitOptions::new(spec.model)
            .with_cache_dir(cache)
            .with_show_download_progress(true)
            .with_max_length(spec.max_length)
            // macOS 默认会注册 CoreML，长驻进程里多轮推理内存会持续膨胀；
            // 强制 CPU 推理，内存稳定，短文本检索性能足够
            .with_auto_device(AutoDevicePolicy::PreferCPU);
        if let Some(threads) = intra_threads() {
            options = options.with_intra_threads(threads);
        }
        let model = TextEmbedding::try_new(options)
            .map_err(|e| format!("加载 embedding 模型 {} 失败: {e}", spec.name))?;
        Ok(Loaded {
            model: Mutex::new(model),
            name: spec.name,
            prompt: spec.prompt,
        })
    });
    result.as_ref().map_err(|e| e.clone())
}

/// 启动预热：本地已有模型才加载；不会联网下载，失败只告警不退出
pub fn warmup() {
    if mode() == Mode::Off {
        eprintln!("[embedding] 已通过 AGGRSEARCH_EMBED=off 关闭，使用词法重排");
        return;
    }
    if mode() == Mode::Auto && !local_ready() {
        eprintln!("[embedding] 未安装本地模型，使用词法重排（需要向量重排请运行 `aggrsearch models download`）");
        return;
    }
    let started = std::time::Instant::now();
    match load() {
        Ok(loaded) => eprintln!(
            "[embedding] 模型就绪: {} ({:.1}s)",
            loaded.name,
            started.elapsed().as_secs_f64()
        ),
        Err(e) => eprintln!("[embedding] {e}；本次将回退词法重排"),
    }
}

fn query_text(prompt: bool, query: &str) -> String {
    if prompt {
        format!("{GEMMA_QUERY_PREFIX}{}", query.trim())
    } else {
        query.to_string()
    }
}

fn doc_text(prompt: bool, title: &str, snippet: &str) -> String {
    let title = title.trim();
    let snippet = snippet.trim();
    if prompt {
        let title = if title.is_empty() { "none" } else { title };
        let snippet = if snippet.is_empty() { "none" } else { snippet };
        format!("{GEMMA_DOC_PREFIX}{title}{GEMMA_DOC_SEP}{snippet}")
    } else {
        format!("{title} {snippet}").trim().to_string()
    }
}

/// 计算 query 与每条文档的余弦相似度（与 SearchPIN 相同：直接作为 `_rerank_score`）
pub fn score(query: &str, docs: &[(String, String)]) -> Result<Vec<f64>, String> {
    let loaded = load()?;
    if docs.is_empty() {
        return Ok(Vec::new());
    }

    let mut texts = Vec::with_capacity(docs.len() + 1);
    texts.push(query_text(loaded.prompt, query));
    for (title, snippet) in docs {
        texts.push(doc_text(loaded.prompt, title, snippet));
    }

    let mut model = loaded
        .model
        .lock()
        .map_err(|_| "embedding 模型锁已中毒".to_string())?;
    let embeddings = model
        .embed(texts, None)
        .map_err(|e| format!("embedding 推理失败: {e}"))?;
    let query_vec = embeddings
        .first()
        .ok_or_else(|| "embedding 输出为空".to_string())?;

    Ok(embeddings[1..]
        .iter()
        .map(|doc_vec| cosine(query_vec, doc_vec))
        .collect())
}

/// 余弦相似度；零向量返回 0，避免 NaN 污染排序
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// 健康检查里展示的 embedding 状态（不触发模型加载）
pub fn status() -> serde_json::Value {
    let model = model_spec()
        .map(|s| s.name)
        .unwrap_or("unknown")
        .to_string();
    let loaded = matches!(LOADED.get(), Some(Ok(_)));
    serde_json::json!({
        "mode": mode_name(),
        "enabled": enabled(),
        "model": model,
        // installed：磁盘上模型完整；loaded：已载入内存并可用于推理
        "installed": local_ready(),
        "loaded": loaded,
        "size_bytes": installed_size(),
        "cache_dir": cache_dir().display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_handles_basic_and_zero_vectors() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn gemma_prompts_follow_model_card() {
        assert_eq!(
            query_text(true, " rust async "),
            "task: search result | query: rust async"
        );
        assert_eq!(
            doc_text(true, "Rust", "async runtime"),
            "title: Rust | text: async runtime"
        );
        assert_eq!(doc_text(true, "", ""), "title: none | text: none");
    }

    #[test]
    fn plain_mode_matches_searchpin_doc_text() {
        assert_eq!(doc_text(false, "Rust", "async"), "Rust async");
        assert_eq!(query_text(false, "rust"), "rust");
    }

    #[test]
    #[ignore = "需要下载 embedding 模型"]
    fn embedding_smoke() {
        let docs = vec![
            (
                "Rust async 编程".to_string(),
                "Tokio 运行时教程".to_string(),
            ),
            ("今日天气".to_string(), "明日多云转晴".to_string()),
        ];
        let scores = score("Rust 异步编程", &docs).expect("embedding 计算失败");
        assert_eq!(scores.len(), 2);
        assert!(scores.iter().all(|s| (-1.0..=1.0).contains(s)));
        assert!(scores[0] > scores[1], "相关文档应排前: {scores:?}");
    }
}
