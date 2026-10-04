use crate::types::{ApiToolParam, Message, SystemBlock, ThinkingConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use super::client::{ApiClient, ModelConfig};
use super::provider::{ApiType, ProviderResponse};

/// 多 API 客户端轮询路由。
///
/// 支持配置多个上游（多 base_url / 多 API key / 多模型），每次请求按
/// round-robin 顺序选取下一个客户端，把请求压力分摊到多个上游上；
/// 每个客户端持有**独立**的 RPM 限流器，各自遵守自己的每分钟配额，
/// 互不影响（某一路被限速等待不会拖累其他路）。
#[derive(Clone)]
pub struct ApiRouter {
    clients: Arc<RwLock<Vec<ApiClient>>>,
    next: Arc<AtomicUsize>,
}

impl ApiRouter {
    /// 从一组客户端构造路由器。clients 不能为空。
    pub fn new(clients: Vec<ApiClient>) -> Self {
        assert!(
            !clients.is_empty(),
            "ApiRouter requires at least one API client"
        );
        Self {
            clients: Arc::new(RwLock::new(clients)),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 上游数量。
    pub fn len(&self) -> usize {
        self.clients.read().unwrap().len()
    }

    /// 是否为空（恒为 false，构造时已断言非空）。
    pub fn is_empty(&self) -> bool {
        self.clients.read().unwrap().is_empty()
    }

    /// round-robin 取出下一个客户端（clone 返回，每次调用推进计数器）。
    pub fn next(&self) -> ApiClient {
        let guard = self.clients.read().unwrap();
        let n = guard.len();
        let i = self.next.fetch_add(1, Ordering::Relaxed) % n;
        guard[i].clone()
    }

    /// 第一个客户端（作为"主"客户端，用于展示/系统消息等非请求场景）。
    pub fn primary(&self) -> ApiClient {
        self.clients.read().unwrap()[0].clone()
    }

    /// 模型名（取主客户端的模型）。
    pub fn model(&self) -> String {
        self.clients.read().unwrap()[0].model().to_string()
    }

    /// 设置模型：同步应用到全部客户端。
    pub fn set_model(&mut self, model: String) {
        let mut guard = self.clients.write().unwrap();
        for c in guard.iter_mut() {
            c.set_model(model.clone());
        }
    }

    /// 主客户端的模型配置（上下文窗口/输出上限）。
    pub fn model_config(&self) -> ModelConfig {
        self.clients.read().unwrap()[0].model_config()
    }

    /// 发送一次流式请求：轮询选取下一个客户端并调用其 create_message。
    /// 每个客户端的 RPM 限流在该客户端内部独立生效。
    pub async fn create_message(
        &self,
        messages: &[Message],
        system: Option<Vec<SystemBlock>>,
        tools: Option<Vec<ApiToolParam>>,
        max_tokens: Option<u64>,
        thinking: Option<ThinkingConfig>,
    ) -> Result<ProviderResponse, super::client::ApiError> {
        self.next()
            .create_message(messages, system, tools, max_tokens, thinking)
            .await
    }

    /// 当前路由的 API 类型（主客户端）。
    pub fn api_type(&self) -> ApiType {
        self.clients.read().unwrap()[0].api_type().clone()
    }
}
