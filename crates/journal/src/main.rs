//! 个人日志系统 skill CLI。
//!
//! 依托本机 voice-serve（默认 http://127.0.0.1:8420）的内置语音能力：
//!   - 控制个人日志系统开关（开启后持续录音，分段写 wav + 后台转写 md）
//!   - 查看/设置日志存储根目录（写入本目录下的 config.json，可被 AI 或用户直改）
//!   - 按需把 Markdown 文本输出到当日 text 目录
//!   - 开放内置 STT（语音转文字）/ KWS（唤醒词）供 skill 使用
//!
//! 注意：搜索能力不在此开放（搜索仍只给主 Agent）。

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Config {
    /// 日志存储根目录（当日文件夹在其下：<root>/<YYYY-MM-DD>/{audio,text}）
    #[serde(default)]
    root: String,
    /// voice-serve 端口（默认 8420）
    #[serde(default)]
    voice_port: Option<u16>,
    /// 直接指定 voice-serve base（优先级高于端口）
    #[serde(default)]
    base: Option<String>,
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("JOURNAL_CONFIG") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    exe_dir().join("config.json")
}

fn load_config() -> Config {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) -> Result<()> {
    let p = config_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).ok();
    }
    std::fs::write(&p, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

fn default_root() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/Documents/个人日志")
}

fn root_of(cfg: &Config) -> String {
    if cfg.root.trim().is_empty() {
        default_root()
    } else {
        cfg.root.trim().to_string()
    }
}

fn base_url(cfg: &Config) -> String {
    if let Ok(b) = std::env::var("JOURNAL_BASE") {
        if !b.trim().is_empty() {
            return b.trim().trim_end_matches('/').to_string();
        }
    }
    if let Some(b) = &cfg.base {
        if !b.trim().is_empty() {
            return b.trim().trim_end_matches('/').to_string();
        }
    }
    let port = std::env::var("VOICE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .or(cfg.voice_port)
        .unwrap_or(8420);
    format!("http://127.0.0.1:{port}")
}

fn post(client: &reqwest::blocking::Client, base: &str, path: &str, body: Value) -> Result<String> {
    let r = client
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .with_context(|| format!("请求 {path} 失败（voice-serve 是否在运行？）"))?;
    let v: Value = r.json().context("解析响应失败")?;
    Ok(v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())
}

fn get_json(client: &reqwest::blocking::Client, base: &str, path: &str) -> Result<Value> {
    let r = client
        .get(format!("{base}{path}"))
        .send()
        .with_context(|| format!("请求 {path} 失败（voice-serve 是否在运行？）"))?;
    Ok(r.json().context("解析响应失败")?)
}

fn post_kws(client: &reqwest::blocking::Client, base: &str, path: &str) -> Result<String> {
    let r = client
        .post(format!("{base}/journal/kws_file"))
        .json(&json!({"path": path}))
        .send()
        .context("请求 /journal/kws_file 失败（voice-serve 是否在运行？）")?;
    let v: Value = r.json().context("解析响应失败")?;
    Ok(match v.get("keyword").and_then(|k| k.as_str()) {
        Some(k) => k.to_string(),
        None => "(未命中)".to_string(),
    })
}

fn print_help() {
    println!(
        r#"journal —— 个人日志系统 skill 命令行工具

用法:
  journal start                     开启个人日志系统（持续录音 + 后台自动转写）
  journal stop                      结束个人日志系统
  journal status                    查看运行状态、今日目录与统计
  journal dir                       查看当前日志存储根目录
  journal dir <路径>                 设置日志存储根目录（写回 config.json）
  journal write <文本> [--title <标题>]  把 Markdown 文本写入当日 text 目录
  journal stt <wav路径>             对已有 wav 做语音转文字（内置 ASR）
  journal stt-live [秒数]           录一段实时语音并转文字（默认上限 30s）
  journal kws <wav路径>             对已有 wav 做唤醒词检测（内置 KWS）
  journal --help                    显示本帮助

存储结构: <根目录>/<YYYY-MM-DD>/audio/HH-MM-SS.wav
          <根目录>/<YYYY-MM-DD>/text/HH-MM-SS.md
默认根目录: ~/Documents/个人日志
"#
    );
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = load_config();
    let base = base_url(&cfg);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    match args.first().map(|s| s.as_str()).unwrap_or("") {
        "" | "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "start" => {
            let root = root_of(&cfg);
            std::fs::create_dir_all(&root).ok();
            post(&client, &base, "/journal/dir", json!({ "root": root }))?;
            let out = post(&client, &base, "/journal/start", json!({}))?;
            println!("{out}");
            Ok(())
        }
        "stop" => {
            println!("{}", post(&client, &base, "/journal/stop", json!({}))?);
            Ok(())
        }
        "status" => {
            let v = get_json(&client, &base, "/journal/status")?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        "dir" => match args.get(1) {
            None => {
                println!("{}", root_of(&cfg));
                Ok(())
            }
            Some(new_root) => {
                let new_root = new_root.trim().to_string();
                if new_root.is_empty() {
                    anyhow::bail!("目录不能为空");
                }
                std::fs::create_dir_all(&new_root).ok();
                let mut c = load_config();
                c.root = new_root.clone();
                save_config(&c)?;
                println!(
                    "{}",
                    post(&client, &base, "/journal/dir", json!({ "root": new_root }))?
                );
                Ok(())
            }
        },
        "write" => {
            let mut text = String::new();
            let mut title: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--title" | "-t" => {
                        i += 1;
                        title = args.get(i).cloned();
                    }
                    other => {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(other);
                    }
                }
                i += 1;
            }
            if text.trim().is_empty() {
                anyhow::bail!("缺少要写入的文本");
            }
            println!(
                "{}",
                post(
                    &client,
                    &base,
                    "/journal/write",
                    json!({ "text": text, "title": title })
                )?
            );
            Ok(())
        }
        "stt" => {
            let path = args.get(1).context("用法: journal stt <wav路径>")?.clone();
            println!(
                "{}",
                post(&client, &base, "/journal/stt_file", json!({ "path": path }))?
            );
            Ok(())
        }
        "stt-live" => {
            let secs: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30.0);
            println!(
                "{}",
                post(&client, &base, "/listen_once", json!({ "max_secs": secs }))?
            );
            Ok(())
        }
        "kws" => {
            let path = args.get(1).context("用法: journal kws <wav路径>")?.clone();
            println!("{}", post_kws(&client, &base, &path)?);
            Ok(())
        }
        other => anyhow::bail!("未知命令: {other}（journal --help 查看用法）"),
    }
}
