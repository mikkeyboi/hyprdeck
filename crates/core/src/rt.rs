//! Shared tokio runtime and bridges between the GTK main loop and async/blocking work.
//!
//! GTK widgets live on the main thread (glib main context). Anything that touches
//! D-Bus, spawns processes or blocks must run on the tokio runtime; UI code awaits the
//! result with [`run`] / [`blocking`] inside `glib::spawn_future_local`.

use std::sync::LazyLock;

use tokio::runtime::Runtime;

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("hyprdeck-rt")
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
});

/// The process-wide tokio runtime (2 worker threads; work is I/O bound).
pub fn runtime() -> &'static Runtime {
    &RUNTIME
}

/// Spawn a detached task on the runtime.
pub fn spawn<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(fut)
}

/// Run `fut` on the tokio runtime and await its output from any executor
/// (typically the glib main context).
pub async fn run<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    runtime().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.await.expect("runtime task panicked")
}

/// Run a blocking closure on the runtime's blocking pool and await its output.
pub async fn blocking<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    runtime().spawn_blocking(move || {
        let _ = tx.send(f());
    });
    rx.await.expect("blocking task panicked")
}
