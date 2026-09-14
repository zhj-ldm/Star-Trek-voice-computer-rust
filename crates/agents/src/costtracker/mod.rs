use crate::types::Usage;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-model pricing (per million tokens).
#[derive(Debug, Clone)]
struct ModelPricing {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

fn get_pricing(model: &str) -> ModelPricing {
    match model {
        m if m.contains("opus") => ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_read: 1.5,
            cache_write: 18.75,
        },
        m if m.contains("sonnet") => ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        m if m.contains("haiku") => ModelPricing {
            input: 0.8,
            output: 4.0,
            cache_read: 0.08,
            cache_write: 1.0,
        },
        _ => ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
    }
}

/// Per-model usage tracking.
#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// Tracks token usage, costs, and other metrics across the agent session.
#[derive(Clone)]
pub struct CostTracker {
    inner: Arc<RwLock<CostTrackerInner>>,
}

#[derive(Debug, Default)]
struct CostTrackerInner {
    model_usage: HashMap<String, ModelUsage>,
    api_duration_ms: u64,
    tool_duration_ms: u64,
    lines_added: u64,
    lines_removed: u64,
    web_searches: u64,
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(CostTrackerInner::default())),
        }
    }

    /// Add token usage for a specific model.
    pub async fn add_usage(&self, model: &str, usage: &Usage) {
        let mut inner = self.inner.write().await;
        let entry = inner
            .model_usage
            .entry(model.to_string())
            .or_default();
        entry.input_tokens += usage.input_tokens;
        entry.output_tokens += usage.output_tokens;
        entry.cache_read_tokens += usage.cache_read_input_tokens;
        entry.cache_write_tokens += usage.cache_creation_input_tokens;
    }

    /// Add API call duration.
    pub async fn add_api_duration(&self, duration_ms: u64) {
        let mut inner = self.inner.write().await;
        inner.api_duration_ms += duration_ms;
    }

    /// Add tool execution duration.
    pub async fn add_tool_duration(&self, duration_ms: u64) {
        let mut inner = self.inner.write().await;
        inner.tool_duration_ms += duration_ms;
    }

    /// Add code change metrics.
    pub async fn add_code_changes(&self, added: u64, removed: u64) {
        let mut inner = self.inner.write().await;
        inner.lines_added += added;
        inner.lines_removed += removed;
    }

    /// Increment web search count.
    pub async fn add_web_search(&self) {
        let mut inner = self.inner.write().await;
        inner.web_searches += 1;
    }

    /// Calculate total cost in USD.
    pub async fn total_cost(&self) -> f64 {
        let inner = self.inner.read().await;
        let mut total = 0.0;

        for (model, usage) in &inner.model_usage {
            let pricing = get_pricing(model);
            total += (usage.input_tokens as f64 / 1_000_000.0) * pricing.input;
            total += (usage.output_tokens as f64 / 1_000_000.0) * pricing.output;
            total += (usage.cache_read_tokens as f64 / 1_000_000.0) * pricing.cache_read;
            total += (usage.cache_write_tokens as f64 / 1_000_000.0) * pricing.cache_write;
        }

        total
    }

    /// Get total tokens across all models.
    pub async fn total_tokens(&self) -> u64 {
        let inner = self.inner.read().await;
        inner
            .model_usage
            .values()
            .map(|u| u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens)
            .sum()
    }

    /// Get usage for a specific model.
    pub async fn get_model_usage(&self, model: &str) -> Option<ModelUsage> {
        let inner = self.inner.read().await;
        inner.model_usage.get(model).cloned()
    }

    /// Get all model usage.
    pub async fn get_all_usage(&self) -> HashMap<String, ModelUsage> {
        let inner = self.inner.read().await;
        inner.model_usage.clone()
    }

    /// Get summary metrics.
    pub async fn summary(&self) -> CostSummary {
        let inner = self.inner.read().await;
        let total_cost = {
            let mut total = 0.0;
            for (model, usage) in &inner.model_usage {
                let pricing = get_pricing(model);
                total += (usage.input_tokens as f64 / 1_000_000.0) * pricing.input;
                total += (usage.output_tokens as f64 / 1_000_000.0) * pricing.output;
                total += (usage.cache_read_tokens as f64 / 1_000_000.0) * pricing.cache_read;
                total += (usage.cache_write_tokens as f64 / 1_000_000.0) * pricing.cache_write;
            }
            total
        };

        CostSummary {
            total_cost_usd: total_cost,
            total_input_tokens: inner.model_usage.values().map(|u| u.input_tokens).sum(),
            total_output_tokens: inner.model_usage.values().map(|u| u.output_tokens).sum(),
            api_duration_ms: inner.api_duration_ms,
            tool_duration_ms: inner.tool_duration_ms,
            lines_added: inner.lines_added,
            lines_removed: inner.lines_removed,
            web_searches: inner.web_searches,
        }
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct CostSummary {
    pub total_cost_usd: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub api_duration_ms: u64,
    pub tool_duration_ms: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
    pub web_searches: u64,
}
