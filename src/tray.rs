//! StatusNotifierItem host: renders the core tray registry into one menu.

use std::sync::Arc;

use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::tray::{self, TrayItem, TrayProvider};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{MenuItem, TrayMethods};

struct HyprdeckTray;

impl ksni::Tray for HyprdeckTray {
    const MENU_ON_ACTIVATE: bool = false;

    fn id(&self) -> String {
        "hyprdeck".into()
    }

    fn title(&self) -> String {
        "Hyprdeck".into()
    }

    fn icon_name(&self) -> String {
        hyprdeck_core::APP_ICON.into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Hyprdeck".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        events::send(AppEvent::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = vec![
            StandardItem {
                label: "Open Hyprdeck".into(),
                icon_name: "go-home-symbolic".into(),
                activate: Box::new(|_| events::send(AppEvent::ShowWindow)),
                ..Default::default()
            }
            .into(),
        ];
        for provider in tray::providers() {
            let items = provider.items();
            if items.is_empty() {
                continue;
            }
            menu.push(MenuItem::Separator);
            menu.extend(convert(&provider, items));
        }
        menu.push(MenuItem::Separator);
        menu.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|_| events::send(AppEvent::Quit)),
                ..Default::default()
            }
            .into(),
        );
        menu
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::warn!("tray watcher offline: {reason:?}; waiting for it to return");
        true
    }
}

/// Convert provider items; consecutive `Radio` items become one radio group.
fn convert(provider: &Arc<dyn TrayProvider>, items: Vec<TrayItem>) -> Vec<MenuItem<HyprdeckTray>> {
    let mut out = Vec::with_capacity(items.len());
    let mut radios: Vec<(String, String, bool)> = Vec::new();
    let flush = |radios: &mut Vec<(String, String, bool)>,
                 out: &mut Vec<MenuItem<HyprdeckTray>>| {
        if radios.is_empty() {
            return;
        }
        let group = std::mem::take(radios);
        let selected = group.iter().position(|r| r.2).unwrap_or(usize::MAX);
        let ids: Vec<String> = group.iter().map(|r| r.1.clone()).collect();
        let p = provider.clone();
        out.push(
            RadioGroup {
                selected,
                select: Box::new(move |_, i| {
                    if let Some(id) = ids.get(i) {
                        p.activate(id);
                    }
                }),
                options: group
                    .into_iter()
                    .map(|r| RadioItem {
                        label: r.0,
                        ..Default::default()
                    })
                    .collect(),
            }
            .into(),
        );
    };
    for item in items {
        match item {
            TrayItem::Radio {
                label,
                id,
                selected,
            } => {
                radios.push((label, id, selected));
                continue;
            }
            _ => flush(&mut radios, &mut out),
        }
        out.push(match item {
            TrayItem::Action { label, id, enabled } => {
                let p = provider.clone();
                StandardItem {
                    label,
                    enabled,
                    activate: Box::new(move |_| p.activate(&id)),
                    ..Default::default()
                }
                .into()
            }
            TrayItem::Check {
                label,
                id,
                checked,
                enabled,
            } => {
                let p = provider.clone();
                CheckmarkItem {
                    label,
                    checked,
                    enabled,
                    activate: Box::new(move |_| p.activate(&id)),
                    ..Default::default()
                }
                .into()
            }
            TrayItem::Separator => MenuItem::Separator,
            TrayItem::Submenu { label, items } => SubMenu {
                label,
                submenu: convert(provider, items),
                ..Default::default()
            }
            .into(),
            TrayItem::Radio { .. } => unreachable!("handled above"),
        });
    }
    flush(&mut radios, &mut out);
    out
}

/// Register the tray and keep its menu in sync with provider refreshes.
/// Resolves to `false` when no StatusNotifierWatcher could be reached.
pub async fn run() -> bool {
    let handle = match HyprdeckTray.spawn().await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("tray unavailable: {e}");
            return false;
        }
    };
    hyprdeck_core::rt::spawn(async move {
        loop {
            tray::changed().await;
            if handle.update(|_| {}).await.is_none() {
                break;
            }
        }
    });
    true
}
