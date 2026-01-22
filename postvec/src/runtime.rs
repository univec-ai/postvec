//! One lazily-initialized current-thread tokio runtime per process (backend
//! or background worker). Network futures run inside `block_on` on the
//! calling thread, never on a pool thread, so nothing but the main thread
//! touches Postgres state. Do not run SPI inside the futures passed here.

use crate::client::PvError;
use once_cell::sync::Lazy;
use std::future::Future;
use std::time::Duration;

static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("postvec: failed to build current-thread tokio runtime")
});

/// Run `fut` to completion on the process-local current-thread runtime,
/// bounded by `timeout_ms`.
pub fn block_on_with_timeout<T, F>(timeout_ms: u64, fut: F) -> Result<T, PvError>
where
    F: Future<Output = Result<T, PvError>>,
{
    RUNTIME.block_on(async {
        match tokio::time::timeout(Duration::from_millis(timeout_ms), fut).await {
            Ok(res) => res,
            Err(_) => Err(PvError::Deadline { ms: timeout_ms }),
        }
    })
}

/// Run `fut` with no postvec-side deadline. The future is expected to bound
/// itself, for example with per-RPC timeouts.
pub fn block_on<T, F>(fut: F) -> T
where
    F: Future<Output = T>,
{
    RUNTIME.block_on(fut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_fires() {
        let res: Result<(), PvError> = block_on_with_timeout(20, async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        });
        assert!(matches!(res, Err(PvError::Deadline { ms: 20 })));
    }

    #[test]
    fn value_passes_through() {
        let res = block_on_with_timeout(1000, async { Ok::<_, PvError>(42) });
        assert_eq!(res.unwrap(), 42);
    }
}
