//! star-core —— 星际迷航语音助手核心后端库
//! 双 Agent（主/子）+ 语音链路 + skills + 记忆 + 定时任务 + HTTP API

pub mod agents;
pub mod config;
pub mod events;
pub mod paths;
pub mod http;
pub mod memory;
pub mod scheduler;
pub mod search;
pub mod sessions;
pub mod skills;
pub mod state;
pub mod tools;
pub mod voice;
