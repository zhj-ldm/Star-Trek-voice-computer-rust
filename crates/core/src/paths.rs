//! 可移植路径解析：项目根定位 + resources 相对解析。
//!
//! 原则：所有运行时资源（searchpin-ai、goose-tts、音效等）统一基于
//! 「项目根/resources」相对解析，不硬编码任何用户绝对路径，克隆到任意
//! 电脑 / 任意位置均可运行。
//!
//! 项目根定位顺序：
//! 1. 环境变量 `STAR_TREK_ROOT`（Electron / 打包场景由外层进程注入，最可靠）；
//! 2. 当前可执行文件向上定位（开发场景：`<root>/target/<profile>/<bin>`）；
//! 3. 当前工作目录（开发时 cd 项目根运行）。

use std::path::PathBuf;

/// 定位项目根目录
pub fn project_root() -> PathBuf {
    // 1. 环境变量 STAR_TREK_ROOT（Electron/打包场景注入）
    if let Ok(p) = std::env::var("STAR_TREK_ROOT") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // 2. 当前可执行文件向上定位：<root>/target/<profile>/<bin>
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            // bin_dir = .../target/debug|release
            if matches!(
                bin_dir.file_name().and_then(|n| n.to_str()),
                Some("debug") | Some("release")
            ) {
                if let Some(target_dir) = bin_dir.parent() {
                    if target_dir.file_name().and_then(|n| n.to_str()) == Some("target") {
                        if let Some(root) = target_dir.parent() {
                            if root.join("Cargo.toml").exists() {
                                return root.to_path_buf();
                            }
                        }
                    }
                }
            }
        }
    }
    // 3. 当前工作目录
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    PathBuf::from(".")
}

/// resources 目录：{project_root}/resources
pub fn resources_dir() -> PathBuf {
    project_root().join("resources")
}

/// 单个运行时资源：{project_root}/resources/{name}
pub fn resource(name: &str) -> PathBuf {
    resources_dir().join(name)
}

/// HOME 目录（兜底改通用值：取不到时回退项目根，不再硬编码用户名）
pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| project_root())
}
