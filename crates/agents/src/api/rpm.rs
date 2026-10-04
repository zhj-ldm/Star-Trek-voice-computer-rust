//! RPM（每分钟请求数）令牌桶限流器。
//!
//! 每个 `ApiClient` 持有独立限流器（主 Agent 与子 Agent 互不影响），
//! 默认 20 次/分钟，可通过 `RPM_LIMIT` 环境变量或 `AgentOptions::rpm_limit` 覆盖。
//! 超过限速后请求会在此等待令牌，避免上游 API 因并发请求被拒绝（429 / connection refused）。

use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;

/// 默认限速：每分钟 20 次请求。
pub const DEFAULT_RPM: u32 = 20;

#[derive(Debug)]
struct LimiterState {
    /// 当前可用令牌（最多满到容量）
    tokens: f64,
    /// 上次补充令牌的时刻
    last_refill: Instant,
}

/// 令牌桶：容量 = rpm，补充速率 = rpm / 60 每秒。
#[derive(Debug)]
pub struct RpmLimiter {
    rpm: u32,
    state: Mutex<LimiterState>,
}

impl RpmLimiter {
    pub fn new(rpm: u32) -> Self {
        let rpm = rpm.max(1);
        Self {
            rpm,
            state: Mutex::new(LimiterState {
                tokens: rpm as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// 当前 rpm 配置。
    pub fn rpm(&self) -> u32 {
        self.rpm
    }

    /// 获取一个请求令牌；桶空时异步等待直到令牌补充到位。
    pub async fn acquire(&self) {
        let rpm = self.rpm as f64;
        loop {
            let mut st = self.state.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(st.last_refill).as_secs_f64();
            st.tokens = (st.tokens + elapsed * rpm / 60.0).min(rpm);
            st.last_refill = now;

            if st.tokens >= 1.0 {
                st.tokens -= 1.0;
                return;
            }

            // 距离下一个令牌的等待秒数（至少 50ms，避免空转）
            let wait = ((1.0 - st.tokens) * 60.0 / rpm).max(0.05);
            drop(st);
            sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn burst_within_capacity_passes_instantly() {
        let l = RpmLimiter::new(20);
        let start = std::time::Instant::now();
        for _ in 0..20 {
            l.acquire().await;
        }
        // 20 个令牌应瞬间取完（< 200ms）
        assert!(start.elapsed().as_millis() < 200, "took {:?}", start.elapsed());
    }

    #[tokio::test]
    async fn over_capacity_waits_for_refill() {
        let l = RpmLimiter::new(60); // 每秒 1 个令牌
        for _ in 0..60 {
            l.acquire().await;
        }
        let start = std::time::Instant::now();
        l.acquire().await; // 第 61 个：需要等 ~1s 补 1 个
        let waited = start.elapsed().as_secs_f64();
        assert!((0.8..=2.0).contains(&waited), "waited {waited}s");
    }
}
