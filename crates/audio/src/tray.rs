//! "Audio" submenu in the hyprdeck tray.

use std::sync::{Arc, Mutex};

use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::tray::{self, TrayItem, TrayProvider};

use crate::engine::{self, Status};
use crate::{PAGE_ID, daemon, ui};

const PRESETS: [u32; 5] = [0, 25, 50, 75, 100];

struct AudioTray;

static LAST_MENU: Mutex<String> = Mutex::new(String::new());

pub fn register() {
    tray::register(Arc::new(AudioTray));
}

/// Called after every snapshot; asks the tray to rebuild only when the menu changed.
pub fn changed() {
    let rendered = format!("{:?}", AudioTray.items());
    let mut last = LAST_MENU.lock().unwrap_or_else(|e| e.into_inner());
    if *last != rendered {
        *last = rendered;
        tray::refresh();
    }
}

fn check(label: impl Into<String>, id: impl Into<String>, checked: bool) -> TrayItem {
    TrayItem::Check {
        label: label.into(),
        id: id.into(),
        checked,
        enabled: true,
    }
}

fn info(label: impl Into<String>) -> TrayItem {
    TrayItem::Action {
        label: label.into(),
        id: String::new(),
        enabled: false,
    }
}

fn menu(st: &Status) -> Vec<TrayItem> {
    let mut items = vec![
        info(format!(
            "Simultaneous output: {}",
            if st.active { "On" } else { "Off" }
        )),
        check(
            if st.active {
                "Turn off simultaneous output"
            } else {
                "Turn on simultaneous output"
            },
            "toggle",
            st.active,
        ),
        TrayItem::Separator,
    ];

    let mut group: Vec<TrayItem> = st
        .devices()
        .map(|s| {
            check(
                &s.label,
                format!("member:{}", s.name),
                st.config.selected.contains(&s.name),
            )
        })
        .collect();
    if group.is_empty() {
        group.push(info("No outputs detected"));
    }
    items.push(TrayItem::Submenu {
        label: "Outputs in the group".into(),
        items: group,
    });

    let defaults = st
        .sinks
        .iter()
        .filter(|s| !s.is_virtual() || (s.is_primary() && st.active))
        .map(|s| TrayItem::Radio {
            label: s.label.clone(),
            id: format!("default:{}", s.name),
            selected: s.is_default,
        })
        .collect();
    items.push(TrayItem::Submenu {
        label: "Default output".into(),
        items: defaults,
    });

    let mut volume: Vec<TrayItem> = PRESETS
        .iter()
        .map(|p| TrayItem::action(format!("{p}%"), format!("volall:{p}")))
        .collect();
    volume.extend([
        TrayItem::Separator,
        TrayItem::action("Mute all outputs", "muteall"),
        TrayItem::action("Unmute all outputs", "unmuteall"),
    ]);
    let devices: Vec<TrayItem> = st
        .devices()
        .map(|s| {
            let mut sub: Vec<TrayItem> = PRESETS
                .iter()
                .map(|p| TrayItem::action(format!("{p}%"), format!("vol:{p}:{}", s.name)))
                .collect();
            sub.push(TrayItem::Separator);
            sub.push(check("Muted", format!("mute:{}", s.name), s.muted));
            TrayItem::Submenu {
                label: format!("{} — {}%", s.label, s.volume),
                items: sub,
            }
        })
        .collect();
    if !devices.is_empty() {
        volume.push(TrayItem::Separator);
        volume.extend(devices);
    }
    items.push(TrayItem::Submenu {
        label: "Volume".into(),
        items: volume,
    });

    let mut playing: Vec<TrayItem> = st
        .streams
        .iter()
        .map(|stream| {
            let targets = st
                .sinks
                .iter()
                .map(|s| TrayItem::Radio {
                    label: s.label.clone(),
                    id: format!("move:{}:{}", stream.index, s.name),
                    selected: s.index == stream.sink_index,
                })
                .collect();
            let detail = stream.detail();
            let label = if detail.is_empty() {
                stream.label()
            } else {
                format!("{} — {detail}", stream.label())
            };
            TrayItem::Submenu {
                label,
                items: targets,
            }
        })
        .collect();
    if playing.is_empty() {
        playing.push(info("No applications are playing"));
    }
    items.push(TrayItem::Submenu {
        label: "Playing now".into(),
        items: playing,
    });

    let mut routing: Vec<TrayItem> = st
        .config
        .routes
        .iter()
        .map(|r| {
            check(
                format!("{} → {}", r.display(), st.labels(&r.sinks)),
                format!("route:{}", r.id),
                r.enabled,
            )
        })
        .collect();
    if routing.is_empty() {
        routing.push(info("No routing rules yet"));
    }
    routing.extend([
        TrayItem::Separator,
        TrayItem::action("Manage app routing…", "routing"),
    ]);
    items.push(TrayItem::Submenu {
        label: "App routing".into(),
        items: routing,
    });

    items.extend([
        TrayItem::Separator,
        TrayItem::action("Audio settings…", "page"),
    ]);
    items
}

impl TrayProvider for AudioTray {
    fn items(&self) -> Vec<TrayItem> {
        let items = match daemon::current().as_deref() {
            Some(Ok(st)) => menu(st),
            Some(Err(e)) => vec![
                info(format!("Audio unavailable: {e}")),
                TrayItem::action("Audio settings…", "page"),
            ],
            None => vec![info("Reading audio outputs…")],
        };
        vec![TrayItem::Submenu {
            label: "Audio".into(),
            items,
        }]
    }

    fn activate(&self, id: &str) {
        let (verb, arg) = id.split_once(':').unwrap_or((id, ""));
        let arg = arg.to_owned();
        match verb {
            "toggle" => daemon::tray_action(engine::toggle()),
            "member" => daemon::tray_action(async move { engine::toggle_member(&arg).await }),
            "default" => daemon::tray_action(async move { engine::set_default(&arg).await }),
            "volall" => daemon::tray_action(async move {
                engine::apply_to_all(arg.parse().unwrap_or(0.0)).await
            }),
            "muteall" => daemon::tray_action(engine::mute_all(true)),
            "unmuteall" => daemon::tray_action(engine::mute_all(false)),
            "vol" => daemon::tray_action(async move {
                let (pct, sink) = arg.split_once(':').unwrap_or(("0", ""));
                engine::set_volume(sink, pct.parse().unwrap_or(0.0))
                    .await
                    .map(|()| String::new())
            }),
            "mute" => {
                let muted = match daemon::current().as_deref() {
                    Some(Ok(st)) => st.sink(&arg).is_some_and(|s| s.muted),
                    _ => false,
                };
                daemon::tray_action(async move {
                    engine::set_mute(&arg, !muted).await.map(|()| String::new())
                });
            }
            "move" => daemon::tray_action(async move {
                let (idx, sink) = arg.split_once(':').unwrap_or(("", ""));
                engine::move_stream(idx.parse()?, sink).await
            }),
            "route" => daemon::tray_action(async move { engine::toggle_route(&arg).await }),
            "page" => events::send(AppEvent::ShowPage(PAGE_ID.into())),
            "routing" => ui::show_tab(ui::ROUTING_TAB),
            _ => {}
        }
    }
}
