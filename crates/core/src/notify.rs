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
    send(&session_bus().await?, summary, body, &[])
        .await
        .map(drop)
}

async fn send(conn: &Connection, summary: &str, body: &str, actions: &[&str]) -> Result<u32> {
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let reply = conn
        .call_method(
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
                actions,
                hints,
                // Actionable notifications stay until dismissed; plain ones use the server default.
                if actions.is_empty() { -1i32 } else { 0i32 },
            ),
        )
        .await?;
    Ok(reply.body().deserialize::<u32>()?)
}

/// Show a notification with one action button and wait for the user: `true` when
/// the action (or the notification body) was clicked, `false` when it was
/// dismissed or closed. Must run on the tokio runtime.
pub async fn notify_action(summary: &str, body: &str, action_label: &str) -> Result<bool> {
    use futures_util::StreamExt;

    let conn = session_bus().await?;
    // Subscribe before sending so a fast click can't be missed.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.Notifications")?
        .path("/org/freedesktop/Notifications")?
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, Some(16)).await?;
    let id = send(&conn, summary, body, &["run", action_label]).await?;
    while let Some(msg) = signals.next().await {
        let msg = msg?;
        let header = msg.header();
        match header.member().map(|m| m.as_str()) {
            Some("ActionInvoked") => {
                let (nid, action): (u32, String) = msg.body().deserialize()?;
                if nid == id {
                    return Ok(action == "run" || action == "default");
                }
            }
            Some("NotificationClosed") => {
                let (nid, _reason): (u32, u32) = msg.body().deserialize()?;
                if nid == id {
                    return Ok(false);
                }
            }
            _ => {}
        }
    }
    Ok(false)
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
