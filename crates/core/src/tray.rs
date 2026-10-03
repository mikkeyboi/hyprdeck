//! Tray menu contributions. Feature crates register a [`TrayProvider`]; the app
//! shell renders all providers into one StatusNotifierItem menu and calls
//! [`refresh`] watchers whenever a provider reports a change.

use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub enum TrayItem {
    /// Clickable entry; `id` is passed back to [`TrayProvider::activate`].
    Action {
        label: String,
        id: String,
        enabled: bool,
    },
    /// Checkbox entry.
    Check {
        label: String,
        id: String,
        checked: bool,
        enabled: bool,
    },
    /// Mutually exclusive choice among siblings sharing a submenu.
    Radio {
        label: String,
        id: String,
        selected: bool,
    },
    Separator,
    Submenu {
        label: String,
        items: Vec<TrayItem>,
    },
}

impl TrayItem {
    pub fn action(label: impl Into<String>, id: impl Into<String>) -> Self {
        TrayItem::Action {
            label: label.into(),
            id: id.into(),
            enabled: true,
        }
    }
}

pub trait TrayProvider: Send + Sync {
    /// Current menu items for this provider (called on every menu rebuild; keep cheap).
    fn items(&self) -> Vec<TrayItem>;
    /// Called on the tray thread when an item with `id` is clicked.
    fn activate(&self, id: &str);
}

static PROVIDERS: LazyLock<Mutex<Vec<Arc<dyn TrayProvider>>>> = LazyLock::new(Default::default);
static CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Register a provider; its items appear in registration order.
pub fn register(provider: Arc<dyn TrayProvider>) {
    PROVIDERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(provider);
    refresh();
}

pub fn providers() -> Vec<Arc<dyn TrayProvider>> {
    PROVIDERS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Ask the tray to rebuild its menu.
pub fn refresh() {
    CHANGED.notify_one();
}

/// Resolves on the next [`refresh`] (used by the tray host).
pub async fn changed() {
    CHANGED.notified().await;
}
