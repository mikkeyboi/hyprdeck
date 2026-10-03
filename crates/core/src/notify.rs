//! Desktop notifications via `org.freedesktop.Notifications` (Noctalia serves it).

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::Result;
use tokio::sync::OnceCell;
use zbus::Connection;
use zbus::zvariant::Value;

static SESSION: LazyLock<OnceCell<Connection>> = LazyLock::new(OnceCell::new);

/// Shared session-bus connection (lazily opened on the tokio runtime).
pub async fn session_bus() -> Result<Connection> {
    let conn = SESSION
        .get_or_try_init(|| async { Connection::session().await })
        .await?;
    Ok(conn.clone())
}

/// Show a notification. Must run on the tokio runtime (use `rt::spawn`).
pub async fn notify(summary: &str, body: &str) -> Result<()> {
    let conn = session_bus().await?;
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "Hyprdeck",
            0u32,
            crate::APP_ICON,
            summary,
            body,
            Vec::<&str>::new(),
            hints,
            -1i32,
        ),
    )
    .await?;
    Ok(())
}

/// Fire-and-forget notification from any thread.
pub fn notify_bg(summary: impl Into<String>, body: impl Into<String>) {
    let (s, b) = (summary.into(), body.into());
    crate::rt::spawn(async move {
        if let Err(e) = notify(&s, &b).await {
            tracing::warn!("notification failed: {e:#}");
        }
    });
}
