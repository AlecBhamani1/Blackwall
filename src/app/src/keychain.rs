//! Bound caller waits without pretending a blocking macOS security call can be cancelled.
use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::sync::Semaphore;

static READS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WAITING: &str = "Keychain access is still waiting. Respond to any macOS Keychain prompt, then try again. Your saved keys have not been changed.";

pub(crate) async fn read<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    read_with(READS.clone(), READ_TIMEOUT, operation).await
}

async fn read_with<T: Send + 'static>(
    gate: Arc<Semaphore>,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::time::timeout(timeout, async move {
        let permit = gate.acquire_owned().await.map_err(|_| WAITING.to_owned())?;
        tokio::task::spawn_blocking(move || {
            // Keep the permit inside the OS call even after timeout/caller cancellation.
            // Retries may wait, but cannot launch more simultaneous security prompts.
            let _permit = permit;
            operation()
        })
        .await
        .map_err(|_| "Keychain access could not finish. Try again.".to_owned())?
    })
    .await
    .map_err(|_| WAITING.to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn timeout_keeps_inflight_read_exclusive_and_discards_its_late_result() {
        let gate = Arc::new(Semaphore::new(1));
        let (release, wait) = std::sync::mpsc::channel();
        let first = read_with(gate.clone(), Duration::from_millis(30), move || {
            let _ = wait.recv();
            Ok("late result")
        })
        .await;
        let attempted = Arc::new(AtomicBool::new(false));
        let observed = attempted.clone();
        let second = read_with(gate.clone(), Duration::from_millis(10), move || {
            observed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        // Always release the worker before asserting, including on test failure.
        let _ = release.send(());
        assert!(first.is_err_and(|error| error.contains("Keychain")));
        assert!(second.is_err());
        assert!(!attempted.load(Ordering::SeqCst));
        assert_eq!(
            read_with(gate, Duration::from_secs(1), || Ok(7)).await,
            Ok(7)
        );
    }

    #[tokio::test]
    async fn cancelling_caller_does_not_release_an_active_os_read() {
        let gate = Arc::new(Semaphore::new(1));
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker_gate = gate.clone();
        let caller = tokio::spawn(async move {
            read_with(worker_gate, Duration::from_secs(1), move || {
                let _ = started.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
        });
        let entered = entered.await;
        caller.abort();
        let cancelled = caller.await;
        let held = gate.available_permits() == 0;
        let _ = release.send(());
        assert!(entered.is_ok());
        assert!(cancelled.is_err());
        assert!(held);
        assert_eq!(
            read_with(gate, Duration::from_secs(1), || Ok(9)).await,
            Ok(9)
        );
    }

    #[tokio::test]
    async fn denied_read_preserves_error_and_allows_an_explicit_retry() {
        let gate = Arc::new(Semaphore::new(1));
        let denied = read_with::<()>(gate.clone(), Duration::from_secs(1), || {
            Err("Keychain permission was denied.".into())
        })
        .await;
        assert_eq!(denied, Err("Keychain permission was denied.".into()));
        assert_eq!(
            read_with(gate, Duration::from_secs(1), || Ok(())).await,
            Ok(())
        );
    }
}
