//! Tray submenu: paired devices as check items (checked = connected) plus a link to
//! the page. A background watcher keeps a small cache so `items()` stays cheap.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

use bluer::{AdapterProperty, Address, DeviceProperty};
use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::tray::{self, TrayItem, TrayProvider};
use hyprdeck_core::{notify, rt};

use crate::PAGE_ID;
use crate::bt::{self, Change};
use crate::info::DeviceInfo;

#[derive(Default)]
struct Cache {
    available: bool,
    powered: bool,
    devices: BTreeMap<Address, DeviceInfo>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

const SETTINGS_ID: &str = "bluetooth:settings";
const DEVICE_PREFIX: &str = "bluetooth:device:";

struct Provider;

impl TrayProvider for Provider {
    fn items(&self) -> Vec<TrayItem> {
        let c = cache();
        let mut items = Vec::new();
        if !c.available {
            items.push(TrayItem::Action {
                label: "No Bluetooth adapter".into(),
                id: String::new(),
                enabled: false,
            });
        } else if !c.powered {
            items.push(TrayItem::Action {
                label: "Bluetooth is off".into(),
                id: String::new(),
                enabled: false,
            });
        }
        let mut paired: Vec<&DeviceInfo> = c.devices.values().filter(|d| d.paired).collect();
        paired.sort_by(|a, b| crate::info::cmp_paired(a, b));
        for d in paired {
            let label = match d.battery {
                Some(pct) if d.connected => format!("{} ({pct}%)", d.label()),
                _ => d.label().to_owned(),
            };
            items.push(TrayItem::Check {
                label,
                id: format!("{DEVICE_PREFIX}{}", d.address),
                checked: d.connected,
                enabled: c.powered && !d.blocked,
            });
        }
        if items.is_empty() {
            items.push(TrayItem::Action {
                label: "No paired devices".into(),
                id: String::new(),
                enabled: false,
            });
        }
        items.push(TrayItem::Separator);
        items.push(TrayItem::action("Bluetooth settings…", SETTINGS_ID));
        vec![TrayItem::Submenu {
            label: "Bluetooth".into(),
            items,
        }]
    }

    fn activate(&self, id: &str) {
        if id == SETTINGS_ID {
            events::send(AppEvent::ShowPage(PAGE_ID.into()));
            return;
        }
        let Some(addr) = id
            .strip_prefix(DEVICE_PREFIX)
            .and_then(|a| a.parse::<Address>().ok())
        else {
            return;
        };
        let Some((connected, label)) = cache()
            .devices
            .get(&addr)
            .map(|d| (d.connected, d.label().to_owned()))
        else {
            return;
        };
        rt::spawn(async move {
            let result = if connected {
                bt::disconnect(addr).await
            } else {
                bt::connect(addr).await
            };
            if let Err(e) = result {
                let what = if connected {
                    "disconnect from"
                } else {
                    "connect to"
                };
                notify::notify_bg("Bluetooth", format!("Couldn't {what} {label}: {e:#}"));
                // The click toggled the check mark optimistically; re-render real state.
                tray::refresh();
            }
        });
    }
}

/// Register the tray provider and keep its cache in sync with BlueZ.
pub fn start() {
    tray::register(Arc::new(Provider));
    rt::spawn(async {
        let (tx, rx) = async_channel::unbounded();
        rt::spawn(bt::watch(tx));
        while let Ok(change) = rx.recv().await {
            if apply(change).await {
                tray::refresh();
            }
        }
    });
}

/// Apply a change to the cache; returns whether the menu needs a rebuild.
async fn apply(change: Change) -> bool {
    match change {
        Change::Reset => {
            let snap = bt::snapshot().await;
            let mut c = cache();
            match snap {
                Ok(Some(s)) => {
                    c.available = true;
                    c.powered = s.adapter.powered;
                    c.devices = s.devices.into_iter().map(|d| (d.address, d)).collect();
                }
                Ok(None) | Err(_) => {
                    c.available = false;
                    c.devices.clear();
                }
            }
            true
        }
        Change::Adapter(AdapterProperty::Powered(on)) => {
            cache().powered = on;
            true
        }
        Change::Adapter(_) => false,
        Change::Added(addr) => refetch(addr).await,
        Change::Removed(addr) => cache().devices.remove(&addr).is_some_and(|d| d.paired),
        Change::Device(addr, prop) => {
            let relevant = matches!(
                prop,
                DeviceProperty::Connected(_)
                    | DeviceProperty::Paired(_)
                    | DeviceProperty::Alias(_)
                    | DeviceProperty::Blocked(_)
                    | DeviceProperty::BatteryPercentage(_)
            );
            if !relevant {
                return false;
            }
            let refetch_needed = {
                let mut c = cache();
                match c.devices.get_mut(&addr) {
                    Some(d) => d.apply(&prop) == crate::info::Followup::Refetch,
                    None => true,
                }
            };
            if refetch_needed {
                refetch(addr).await;
            }
            true
        }
    }
}

async fn refetch(addr: Address) -> bool {
    match bt::refetch(addr).await {
        Ok(d) => {
            let paired = d.paired;
            let old = cache().devices.insert(addr, d);
            paired || old.is_some_and(|o| o.paired)
        }
        Err(_) => false,
    }
}
