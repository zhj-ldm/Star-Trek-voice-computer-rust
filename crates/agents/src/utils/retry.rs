use crate::api::ApiError;
use std::time::Duration;

/// Retry configuration.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2000,
            max_delay_ms: 30_000,
        }
    }
}

/// Check if an API error is retryable.
pub fn is_retryable(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::RateLimitError
            | ApiError::Timeout
            | ApiError::NetworkError(_)
            | ApiError::HttpError { status: 500..=599, .. }
    )
}

/// Check if an error indicates the prompt is too long.
pub fn is_prompt_too_long(error: &ApiError) -> bool {
    matches!(error, ApiError::PromptTooLong(_))
}

/// Check if an error is an auth error.
pub fn is_auth_error(error: &ApiError) -> bool {
    matches!(error, ApiError::AuthError(_))
}

/// Calculate retry delay with exponential backoff and jitter.
pub fn get_retry_delay(config: &RetryConfig, attempt: u32) -> Duration {
    let base = config.base_delay_ms as f64;
    let delay = base * 2.0_f64.powi(attempt as i32);
    let max = config.max_delay_ms as f64;
    let clamped = delay.min(max);

    // Add jitter (0.5 to 1.5 of calculated delay)
    let jitter = 0.5 + rand_f64() * 1.0;
    let final_delay = (clamped * jitter) as u64;

    Duration::from_millis(final_delay)
}

/// Execute an async function with retry logic.
pub async fn with_retry<F, Fut, T>(
    config: &RetryConfig,
    mut f: F,
) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    let mut last_error = None;

    for attempt in 0..=config.max_retries {
        match f().await {
            Ok(result) => return Ok(result),
            Err(error) => {
                if !is_retryable(&error) || attempt == config.max_retries {
                    return Err(error);
                }

                let delay = get_retry_delay(config, attempt);
                tracing::warn!(
                    "Retrying after error (attempt {}/{}): {}. Waiting {:?}",
                    attempt + 1,
                    config.max_retries,
                    error,
                    delay
                );
                tokio::time::sleep(delay).await;
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or(ApiError::NetworkError(
        "Max retries exceeded".to_string(),
    )))
}

/// Simple pseudo-random f64 in [0, 1) using time-based seed.
fn rand_f64() -> f64 {
    use std::time::SystemTime;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (seed as f64 % 1000.0) / 1000.0
}
