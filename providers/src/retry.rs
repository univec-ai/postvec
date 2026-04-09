//! Exponential backoff for transient provider HTTP errors.

use crate::EmbeddingError;
use rand::Rng;
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::time::sleep;

// --- Constants for Backoff Logic ---

/// The base delay before the first retry. Jitter multiplies it by 1.0–2.0,
/// so the first wait is 1–2 s and the second 2–4 s.
const INITIAL_DELAY_MS: u64 = 1000;
/// The base for the exponential calculation (e.g., 2^retry_num).
const EXPONENTIAL_BASE: f64 = 2.0;
/// The maximum number of retries to attempt before failing.
///
/// Deliberately small: the extension holds one overall gRPC deadline
/// (postvec.embed_timeout_ms, default 30 s) and the queue's own
/// retry_backoff_ms machinery is the real retry loop — this in-client retry
/// exists only to absorb momentary blips.
const MAX_RETRIES: u32 = 2;

/// Retry `operation` on transient errors (network, 429, 5xx, Bedrock 408/424).
/// Permanent 4xx, parse and config errors are not retried.
///
/// Stops when `deadline` cannot fit another attempt, so the last error is
/// surfaced instead of burning time the caller no longer has.
pub async fn retry_with_backoff<F, T, Fut>(
    deadline: Option<Instant>,
    operation: F,
) -> Result<T, EmbeddingError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, EmbeddingError>>,
{
    let mut num_retries = 0;
    let mut delay = Duration::from_millis(INITIAL_DELAY_MS);
    // Do NOT create rng here. `ThreadRng` is not `Send` and cannot live
    // across an `.await` point if the future needs to be `Send`.

    if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(EmbeddingError::Deadline(
                "budget exhausted before the first attempt".to_string(),
            ));
        }
    }

    loop {
        // Execute the operation, bounded by the caller's absolute deadline:
        // the attempt itself is an HTTP round trip otherwise limited only by
        // the client's per-attempt timeout (default 20 s), while a search()
        // caller may hold a 2 s budget — an attempt must never outlive the
        // budget while holding the provider semaphore and a tower slot.
        let outcome = match deadline {
            Some(deadline) => {
                match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation())
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => {
                        return Err(EmbeddingError::Deadline(
                            "budget exhausted mid-attempt".to_string(),
                        ))
                    }
                }
            }
            None => operation().await,
        };
        match outcome {
            Ok(result) => {
                // Success! Return the result.
                return Ok(result);
            }
            Err(e) => {
                let should_retry = match &e {
                    // 429 Too Many Requests (Rate Limit)
                    EmbeddingError::Api { status: 429, .. } => true,
                    // 5xx Server Errors (transient)
                    EmbeddingError::Api {
                        status: 500..=599, ..
                    } => true,
                    // 408 and 424: Bedrock's `ModelTimeoutException` and
                    // `ModelErrorException` on `InvokeModel`. The model failed
                    // to answer; the input was not rejected. Kept in step with
                    // the gateway's mapping, which classifies both
                    // `UpstreamServiceUnavailable`.
                    EmbeddingError::Api {
                        status: 408 | 424, ..
                    } => true,
                    // Network errors (transient)
                    EmbeddingError::Network(_) => true,
                    // All other errors (4xx client errors, auth, config,
                    // deserialization, deadline) are permanent.
                    _ => false,
                };

                if !should_retry {
                    // Not a retriable error, so fail immediately.
                    return Err(e);
                }

                // We should retry. Increment the counter.
                num_retries += 1;

                // Check if we've exceeded the max retries.
                if num_retries > MAX_RETRIES {
                    // We're out of retries; surface the last error as-is so
                    // the caller's error mapping sees the real failure class
                    // (a 503 must not mutate into a configuration error).
                    return Err(e);
                }

                // --- Jitter Calculation ---
                // Jitter the *current* delay and only then grow it, so the
                // first retry waits INITIAL_DELAY_MS..2×, as documented. The
                // previous order multiplied before sleeping and made the
                // first wait 2–4 s — a large slice of a `search()` budget,
                // and not what the constant said.
                let wait = {
                    // Create the thread-local RNG here, in a tight scope, and
                    // drop it before the await: `ThreadRng` is not `Send`.
                    let mut rng = rand::thread_rng();
                    // jitter_multiplier = 1.0 + (random value between 0.0 and 1.0)
                    let jitter_multiplier = 1.0 + rng.gen_range(0.0..=1.0);
                    let wait = Duration::from_millis(
                        (delay.as_millis() as f64 * jitter_multiplier) as u64,
                    );
                    delay =
                        Duration::from_millis((delay.as_millis() as f64 * EXPONENTIAL_BASE) as u64);
                    wait
                }; // <-- `rng` is dropped here, *before* the await.
                let delay = wait;

                // Deadline awareness: if waiting out the backoff would land
                // past the caller's budget, stop now with the real error.
                if let Some(deadline) = deadline {
                    if Instant::now() + delay >= deadline {
                        return Err(e);
                    }
                }

                log::warn!(
                    "provider API error: {e}. Retrying in {:.2}s (attempt {num_retries}/{MAX_RETRIES})...",
                    delay.as_secs_f32(),
                );

                // Sleep for the calculated delay.
                // `rng` no longer exists, so the future is `Send`.
                sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn returns_immediately_on_success() {
        let calls = AtomicU32::new(0);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(None, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(42) }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "must not retry on success");
    }

    #[tokio::test]
    async fn does_not_retry_permanent_4xx() {
        let calls = AtomicU32::new(0);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(None, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(EmbeddingError::Api {
                    status: 400,
                    message: "bad input".into(),
                })
            }
        })
        .await;
        // A 4xx (non-429) is permanent: surfaced as-is, with no retry.
        assert!(matches!(
            result,
            Err(EmbeddingError::Api { status: 400, .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx must not be retried");
    }

    #[tokio::test]
    async fn does_not_retry_configuration_error() {
        let calls = AtomicU32::new(0);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(None, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(EmbeddingError::Configuration("nope".into())) }
        })
        .await;
        assert!(matches!(result, Err(EmbeddingError::Configuration(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_then_succeeds() {
        // `start_paused` makes tokio auto-advance virtual time over the backoff
        // sleeps, so this exercises the real retry path without wall-clock waits.
        let calls = AtomicU32::new(0);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(None, || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    // Two transient failures (one 503, one 429) then success.
                    let status = if n == 0 { 503 } else { 429 };
                    Err(EmbeddingError::Api {
                        status,
                        message: "transient".into(),
                    })
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "two retries then success");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_budget_surfaces_the_last_real_error() {
        // Every attempt fails 503: after MAX_RETRIES the *last error* comes
        // back untouched, so the caller's error mapping still sees a 5xx.
        let calls = AtomicU32::new(0);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(None, || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(EmbeddingError::Api {
                    status: 503,
                    message: "still down".into(),
                })
            }
        })
        .await;
        assert!(matches!(
            result,
            Err(EmbeddingError::Api { status: 503, .. })
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1 + MAX_RETRIES,
            "initial attempt plus the retry budget"
        );
    }

    /// The upstream statuses that mean "the model did not answer", not "this
    /// input is unacceptable". Bedrock's `ModelTimeoutException` (408) and
    /// `ModelErrorException` (424) are the reason these are here; classifying
    /// them with the rest of 4xx dead-lettered healthy rows.
    #[tokio::test(start_paused = true)]
    async fn bedrock_model_failures_are_retried_like_any_other_outage() {
        for status in [408, 424] {
            let calls = AtomicU32::new(0);
            let result: Result<i32, EmbeddingError> = retry_with_backoff(None, || {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Err(EmbeddingError::Api {
                        status,
                        message: "model unavailable".into(),
                    })
                }
            })
            .await;
            assert!(matches!(result, Err(EmbeddingError::Api { .. })));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1 + MAX_RETRIES,
                "status {status} must be retried"
            );
        }
    }

    /// `INITIAL_DELAY_MS` is the *first* wait, not the one before it. The
    /// previous order multiplied before sleeping, so a constant documented as
    /// 1 s produced a 2–4 s first retry — a large slice of a `search()`
    /// budget. A 1.5 s deadline must therefore still fit one retry.
    #[tokio::test]
    async fn the_first_backoff_is_the_documented_one() {
        let calls = AtomicU32::new(0);
        let deadline = Instant::now() + Duration::from_millis(2_500);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(Some(deadline), || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(EmbeddingError::Api {
                        status: 503,
                        message: "transient".into(),
                    })
                } else {
                    Ok(9)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 9, "one retry must fit a 2.5s budget");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deadline_that_cannot_fit_a_retry_stops_early() {
        // The first backoff is ≥ 1s; a 50ms budget cannot fit it, so exactly
        // one attempt runs and the transient error surfaces immediately.
        let calls = AtomicU32::new(0);
        let deadline = Instant::now() + Duration::from_millis(50);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(Some(deadline), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(EmbeddingError::Api {
                    status: 503,
                    message: "transient".into(),
                })
            }
        })
        .await;
        assert!(matches!(
            result,
            Err(EmbeddingError::Api { status: 503, .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no retry fits the budget");
    }

    #[tokio::test(start_paused = true)]
    async fn an_attempt_is_cut_at_the_deadline_not_at_the_http_timeout() {
        // The operation would run for a minute (think: a slow provider and a
        // generous per-attempt HTTP timeout); a 100ms budget must cut it at
        // the deadline and surface Deadline — never sit in the attempt.
        let calls = AtomicU32::new(0);
        let deadline = Instant::now() + Duration::from_millis(100);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(Some(deadline), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                sleep(Duration::from_secs(60)).await;
                Ok(1)
            }
        })
        .await;
        assert!(
            matches!(result, Err(EmbeddingError::Deadline(_))),
            "{result:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausted_deadline_refuses_before_the_first_attempt() {
        let calls = AtomicU32::new(0);
        let deadline = Instant::now() - Duration::from_millis(1);
        let result: Result<i32, EmbeddingError> = retry_with_backoff(Some(deadline), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(1) }
        })
        .await;
        assert!(matches!(result, Err(EmbeddingError::Deadline(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
