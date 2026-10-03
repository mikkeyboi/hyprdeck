//! Add / edit keybind dialog: key capture (through an empty Hyprland submap so
//! bound combos reach us), action pickers, flags and conflict preview.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::{self, WeakRef};
use gtk::{gdk, gio};
use hyprdeck_core::hypr::ctl::lua_str;
use hyprdeck_core::hypr::managed::{BindFlags, BindRule};
use hyprdeck_core::hypr::model::Combo;
use hyprdeck_core::ui::Ctx;
use hyprdeck_core::{cmd, rt};

use crate::actions::{self, Args, parse_args};
use crate::binds::{self, BindItem, Original, Snapshot};
use crate::noctalia::{self, NoctCmd};
use crate::{keys, widgets};

/// Installed application, collected off the main thread.
#[derive(Debug, Clone)]
struct App {
    id: String,
    name: String,
    icon: Option<String>,
    description: String,
    /// Executable basename, for naming plain `uwsm app -- prog` commands.
    exe: String,
    /// `Exec` line without field codes, used when no launcher is available.
    command: String,
}

fn apps() -> Vec<App> {
    let mut list: Vec<App> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show())
        .filter_map(|a| {
            let command = a
                .commandline()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some(App {
                id: a.id()?.to_string(),
                name: a.display_name().to_string(),
                icon: a
                    .icon()
                    .and_then(|i| IconExt::to_string(&i))
                    .map(|s| s.to_string()),
                description: a.description().map(|d| d.to_string()).unwrap_or_default(),
                exe: a
                    .executable()
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                command: strip_field_codes(&command),
            })
        })
        .collect();
    list.sort_by_key(|a| a.name.to_lowercase());
    list
}

/// Command line without desktop-entry field codes (`%U`, `%f`, …).
fn strip_field_codes(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|w| !(w.len() == 2 && w.starts_with('%') && *w != "%%"))
        .map(|w| w.replace("%%", "%"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prefix that starts a desktop id from a bind: `uwsm app -- ` in a uwsm
/// session, else `gtk-launch ` when installed; `None` means binds run the
/// app's own command line (blocking).
fn app_launcher() -> Option<&'static str> {
    let prefix = cmd::launch_prefix();
    if !prefix.is_empty() {
        return Some(prefix);
    }
    cmd::which("gtk-launch").map(|_| "gtk-launch ")
}

/// Dispatcher presets offered for windows and workspaces; `{n}` = workspace number.
struct Preset {
    label: &'static str,
    dsp: &'static str,
    args: &'static str,
}

const WINDOW_PRESETS: [Preset; 8] = [
    Preset {
        label: "Close window",
        dsp: "window.close",
        args: "",
    },
    Preset {
        label: "Toggle fullscreen",
        dsp: "window.fullscreen",
        args: "",
    },
    Preset {
        label: "Maximize (keep bar visible)",
        dsp: "window.fullscreen",
        args: "{ mode = 1 }",
    },
    Preset {
        label: "Toggle floating",
        dsp: "window.float",
        args: "{ action = \"toggle\" }",
    },
    Preset {
        label: "Pin to all workspaces",
        dsp: "window.pin",
        args: "",
    },
    Preset {
        label: "Center window",
        dsp: "window.center",
        args: "",
    },
    Preset {
        label: "Move to workspace…",
        dsp: "window.move",
        args: "{ workspace = {n} }",
    },
    Preset {
        label: "Move to special workspace",
        dsp: "window.move",
        args: "{ workspace = \"special\" }",
    },
];

const WORKSPACE_PRESETS: [Preset; 5] = [
    Preset {
        label: "Go to workspace…",
        dsp: "focus",
        args: "{ workspace = {n} }",
    },
    Preset {
        label: "Previous workspace on this monitor",
        dsp: "focus",
        args: "{ workspace = \"m-1\" }",
    },
    Preset {
        label: "Next workspace on this monitor",
        dsp: "focus",
        args: "{ workspace = \"m+1\" }",
    },
    Preset {
        label: "Last used workspace",
        dsp: "focus",
        args: "{ workspace = \"previous\" }",
    },
    Preset {
        label: "Toggle special workspace",
        dsp: "workspace.toggle_special",
        args: "",
    },
];

/// Match a dispatcher call against presets → (index, workspace number).
fn match_preset(presets: &[Preset], dsp: &str, args: &Args) -> Option<(usize, i64)> {
    presets.iter().enumerate().find_map(|(i, p)| {
        if p.dsp != dsp {
            return None;
        }
        if p.args.contains("{n}") {
            let n = args.get("workspace")?.as_int()?;
            (args.fields.len() == 1 && args.positional.is_empty()).then_some((i, n))
        } else {
            (parse_args(p.args) == *args).then_some((i, 1))
        }
    })
}

fn preset_args(p: &Preset, n: i64) -> String {
    p.args.replace("{n}", &n.to_string())
}

const TYPES: [&str; 6] = [
    "Run application",
    "Run command",
    "Noctalia action",
    "Window",
    "Workspace",
    "Advanced",
];
const T_APP: u32 = 0;
const T_CMD: u32 = 1;
const T_NOCT: u32 = 2;
const T_WIN: u32 = 3;
const T_WS: u32 = 4;
const T_ADV: u32 = 5;

/// Action types offered: Noctalia actions only when Noctalia is installed.
fn offered_kinds(noctalia: bool) -> Vec<u32> {
    (0..TYPES.len() as u32)
        .filter(|&k| noctalia || k != T_NOCT)
        .collect()
}

/// Initial action state derived from an existing bind.
#[derive(Default)]
struct Prefill {
    kind: u32,
    app: Option<String>,
    command: String,
    noct: Option<(String, String)>,
    win: Option<(usize, i64)>,
    ws: Option<(usize, i64)>,
    dsp: String,
    args: String,
}

fn prefill(item: Option<&BindItem>, has_noctalia: bool) -> Prefill {
    let Some(item) = item else {
        return Prefill {
            kind: T_APP,
            ..Default::default()
        };
    };
    let dsp = item.dispatcher.clone().unwrap_or_default();
    let a = parse_args(&item.args);
    let base = Prefill {
        kind: T_ADV,
        dsp: dsp.clone(),
        args: item.args.clone(),
        ..Default::default()
    };
    if dsp == "exec_cmd"
        && let Some(cmd) = a.first_str()
        && a.positional.len() == 1
    {
        let cmd = cmd.trim();
        if let Some(id) = actions::LAUNCH_PREFIXES
            .iter()
            .find_map(|p| cmd.strip_prefix(p))
            && id.ends_with(".desktop")
            && !id.contains(char::is_whitespace)
        {
            return Prefill {
                kind: T_APP,
                app: Some(id.to_owned()),
                ..base
            };
        }
        if has_noctalia && let Some(rest) = cmd.strip_prefix(noctalia::PREFIX) {
            let (c, args) = rest.split_once(' ').unwrap_or((rest, ""));
            return Prefill {
                kind: T_NOCT,
                noct: Some((c.to_owned(), args.trim().to_owned())),
                ..base
            };
        }
        return Prefill {
            kind: T_CMD,
            command: cmd.to_owned(),
            ..base
        };
    }
    if let Some(m) = match_preset(&WINDOW_PRESETS, &dsp, &a) {
        return Prefill {
            kind: T_WIN,
            win: Some(m),
            ..base
        };
    }
    if let Some(m) = match_preset(&WORKSPACE_PRESETS, &dsp, &a) {
        return Prefill {
            kind: T_WS,
            ws: Some(m),
            ..base
        };
    }
    base
}

struct Editor {
    ctx: Ctx,
    dialog: WeakRef<adw::Dialog>,
    editing: Option<BindItem>,
    snapshot: Rc<Snapshot>,
    on_saved: Box<dyn Fn()>,
    // shortcut
    keys_entry: adw::EntryRow,
    caps_slot: gtk::Box,
    record: gtk::ToggleButton,
    capture_row: adw::ActionRow,
    conflict_row: adw::ActionRow,
    capturing: Cell<bool>,
    record_guard: widgets::Guard,
    // action
    kind: adw::ComboRow,
    /// Action type (`T_*`) behind each entry of `kind`.
    kinds: Vec<u32>,
    /// Prefix that launches a desktop id; `None` = run the app's command line.
    launcher: Cell<Option<&'static str>>,
    groups: Vec<adw::PreferencesGroup>,
    app_row: adw::ActionRow,
    app_icon: gtk::Image,
    app: RefCell<Option<App>>,
    apps: RefCell<Rc<Vec<App>>>,
    command: adw::EntryRow,
    noct_cmd: adw::ComboRow,
    noct_args: adw::EntryRow,
    noct_list: RefCell<Vec<NoctCmd>>,
    win_preset: adw::ComboRow,
    win_n: adw::SpinRow,
    ws_preset: adw::ComboRow,
    ws_n: adw::SpinRow,
    adv_dsp: adw::EntryRow,
    adv_args: adw::EntryRow,
    preview: adw::ActionRow,
    // options
    flags: Vec<(&'static str, adw::SwitchRow)>,
    description: adw::EntryRow,
    save: gtk::Button,
}

/// Open the editor; `editing` = existing bind, `None` = new one.
pub fn open(
    ctx: &Ctx,
    editing: Option<BindItem>,
    snapshot: Rc<Snapshot>,
    on_saved: impl Fn() + 'static,
) {
    widgets::ensure_css();
    let has_noctalia = noctalia::installed();
    let pre = prefill(editing.as_ref(), has_noctalia);
    let kinds = offered_kinds(has_noctalia);
    let dialog = adw::Dialog::builder()
        .title(if editing.is_some() {
            "Edit Keybind"
        } else {
            "Add Keybind"
        })
        .content_width(620)
        .content_height(760)
        .build();
    let header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .show_start_title_buttons(false)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save);
    let prefs = adw::PreferencesPage::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&prefs));
    dialog.set_child(Some(&view));

    // Shortcut
    let shortcut = adw::PreferencesGroup::builder()
        .title("Shortcut")
        .description("Record it, or type it as SUPER + SHIFT + S (mouse: mouse:272, mouse_down)")
        .build();
    let record = gtk::ToggleButton::builder()
        .label("Record")
        .valign(gtk::Align::Center)
        .build();
    let caps_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    caps_slot.set_valign(gtk::Align::Center);
    let capture_row = adw::ActionRow::builder().use_markup(false).build();
    capture_row.set_title("Keys");
    capture_row.set_subtitle("Not set");
    capture_row.add_suffix(&caps_slot);
    capture_row.add_suffix(&record);
    let keys_entry = adw::EntryRow::builder().title("Typed shortcut").build();
    if let Some(e) = &editing {
        keys_entry.set_text(&e.combo.0);
    }
    let conflict_row = adw::ActionRow::builder()
        .use_markup(false)
        .visible(false)
        .build();
    conflict_row.set_title("Replaces");
    conflict_row.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    conflict_row.add_css_class("warning");
    shortcut.add(&capture_row);
    shortcut.add(&keys_entry);
    shortcut.add(&conflict_row);
    prefs.add(&shortcut);

    // Action type
    let action_group = adw::PreferencesGroup::builder().title("Action").build();
    let kind_labels: Vec<&str> = kinds.iter().map(|&k| TYPES[k as usize]).collect();
    let kind = adw::ComboRow::builder()
        .title("Type")
        .model(&gtk::StringList::new(&kind_labels))
        .build();
    action_group.add(&kind);
    let preview = adw::ActionRow::builder()
        .use_markup(false)
        .subtitle_selectable(true)
        .build();
    preview.set_title("Does");
    preview.set_subtitle("—");
    action_group.add(&preview);
    prefs.add(&action_group);

    let app_group = adw::PreferencesGroup::new();
    let app_icon = gtk::Image::from_icon_name("application-x-executable-symbolic");
    app_icon.set_pixel_size(32);
    let app_row = adw::ActionRow::builder()
        .use_markup(false)
        .activatable(true)
        .build();
    app_row.set_title("Application");
    app_row.set_subtitle("None chosen");
    app_row.add_prefix(&app_icon);
    let choose = gtk::Button::builder()
        .label("Choose…")
        .valign(gtk::Align::Center)
        .build();
    app_row.add_suffix(&choose);
    app_group.add(&app_row);

    let cmd_group = adw::PreferencesGroup::new();
    let command = adw::EntryRow::builder()
        .title("Command line")
        .text(&pre.command)
        .build();
    cmd_group.add(&command);

    let noct_group = adw::PreferencesGroup::new();
    let noct_cmd = adw::ComboRow::builder()
        .use_markup(false)
        .enable_search(true)
        .expression(gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            None::<gtk::Expression>,
            "string",
        ))
        .build();
    noct_cmd.set_title("Command");
    noct_cmd.set_subtitle("Loading noctalia commands…");
    let noct_args = adw::EntryRow::builder().use_markup(false).build();
    noct_args.set_title("Arguments");
    noct_group.add(&noct_cmd);
    noct_group.add(&noct_args);

    let win_group = adw::PreferencesGroup::new();
    let win_labels: Vec<&str> = WINDOW_PRESETS.iter().map(|p| p.label).collect();
    let win_preset = adw::ComboRow::builder()
        .title("Window action")
        .model(&gtk::StringList::new(&win_labels))
        .build();
    let win_n = adw::SpinRow::with_range(1.0, 100.0, 1.0);
    win_n.set_title("Workspace");
    win_group.add(&win_preset);
    win_group.add(&win_n);

    let ws_group = adw::PreferencesGroup::new();
    let ws_labels: Vec<&str> = WORKSPACE_PRESETS.iter().map(|p| p.label).collect();
    let ws_preset = adw::ComboRow::builder()
        .title("Workspace action")
        .model(&gtk::StringList::new(&ws_labels))
        .build();
    let ws_n = adw::SpinRow::with_range(1.0, 100.0, 1.0);
    ws_n.set_title("Workspace");
    ws_group.add(&ws_preset);
    ws_group.add(&ws_n);

    let adv_group = adw::PreferencesGroup::builder()
        .description("Any hl.dsp dispatcher, e.g. window.move with { workspace = \"r+1\" }; checked against Hyprland before saving")
        .build();
    let adv_dsp = adw::EntryRow::builder()
        .title("Dispatcher (after hl.dsp.)")
        .text(&pre.dsp)
        .build();
    let adv_args = adw::EntryRow::builder()
        .title("Arguments (Lua)")
        .text(&pre.args)
        .build();
    adv_group.add(&adv_dsp);
    adv_group.add(&adv_args);

    let groups = vec![
        app_group, cmd_group, noct_group, win_group, ws_group, adv_group,
    ];
    for g in &groups {
        prefs.add(g);
    }

    // Options
    let opt_group = adw::PreferencesGroup::builder().title("Options").build();
    let fl = editing
        .as_ref()
        .map(|e| e.flags.clone())
        .unwrap_or_default();
    let flags: Vec<(&'static str, adw::SwitchRow)> = [
        (
            "repeating",
            "Repeat while held",
            "Keeps firing while the keys stay pressed",
            fl.repeating,
        ),
        (
            "locked",
            "Works on the lock screen",
            "Also active while the session is locked or inhibited",
            fl.locked,
        ),
        (
            "release",
            "Trigger on release",
            "Fire when the keys are let go instead of pressed",
            fl.release,
        ),
        (
            "non_consuming",
            "Also send to the app",
            "The focused app receives the key press too",
            fl.non_consuming,
        ),
        (
            "ignore_mods",
            "Ignore modifiers",
            "Fire regardless of which modifiers are held",
            fl.ignore_mods,
        ),
    ]
    .into_iter()
    .map(|(k, t, s, on)| {
        let r = adw::SwitchRow::builder()
            .title(t)
            .subtitle(s)
            .active(on)
            .build();
        opt_group.add(&r);
        (k, r)
    })
    .collect();
    let description = adw::EntryRow::builder()
        .title("Description (optional)")
        .build();
    if let Some(d) = editing.as_ref().and_then(|e| e.description.as_deref()) {
        description.set_text(d);
    }
    opt_group.add(&description);
    prefs.add(&opt_group);

    let ed = Rc::new(Editor {
        ctx: ctx.clone(),
        dialog: dialog.downgrade(),
        editing,
        snapshot,
        on_saved: Box::new(on_saved),
        keys_entry,
        caps_slot,
        record,
        capture_row,
        conflict_row,
        capturing: Cell::new(false),
        record_guard: widgets::Guard::default(),
        kind,
        kinds,
        launcher: Cell::new(None),
        groups,
        app_row,
        app_icon,
        app: RefCell::new(None),
        apps: RefCell::default(),
        command,
        noct_cmd,
        noct_args,
        noct_list: RefCell::default(),
        win_preset,
        win_n,
        ws_preset,
        ws_n,
        adv_dsp,
        adv_args,
        preview,
        flags,
        description,
        save: save.clone(),
    });

    // Prefill
    ed.set_kind(pre.kind);
    if let Some((i, n)) = pre.win {
        ed.win_preset.set_selected(i as u32);
        ed.win_n.set_value(n as f64);
    }
    if let Some((i, n)) = pre.ws {
        ed.ws_preset.set_selected(i as u32);
        ed.ws_n.set_value(n as f64);
    }
    ed.show_kind();
    ed.keys_changed();
    ed.update_preview();

    // Signals (closures hold weak refs: the editor owns these widgets).
    let w = Rc::downgrade(&ed);
    let weak = move || w.upgrade();
    {
        let wk = weak.clone();
        ed.kind.connect_selected_notify(move |_| {
            if let Some(e) = wk() {
                e.show_kind();
                e.update_preview();
            }
        });
    }
    for row in [&ed.command, &ed.noct_args, &ed.adv_dsp, &ed.adv_args] {
        let wk = weak.clone();
        row.connect_changed(move |_| {
            if let Some(e) = wk() {
                e.update_preview();
            }
        });
    }
    for row in [&ed.noct_cmd, &ed.win_preset, &ed.ws_preset] {
        let wk = weak.clone();
        row.connect_selected_notify(move |_| {
            if let Some(e) = wk() {
                e.show_kind();
                e.update_preview();
            }
        });
    }
    for row in [&ed.win_n, &ed.ws_n] {
        let wk = weak.clone();
        row.connect_value_notify(move |_| {
            if let Some(e) = wk() {
                e.update_preview();
            }
        });
    }
    {
        let wk = weak.clone();
        ed.keys_entry.connect_changed(move |_| {
            if let Some(e) = wk() {
                e.keys_changed();
            }
        });
    }
    {
        let wk = weak.clone();
        ed.record.connect_toggled(move |b| {
            let Some(e) = wk() else { return };
            if e.record_guard.active() {
                return;
            }
            if b.is_active() {
                e.start_capture()
            } else {
                e.stop_capture()
            }
        });
    }
    {
        let wk = weak.clone();
        choose.connect_clicked(move |_| {
            if let Some(e) = wk() {
                e.pick_app();
            }
        });
    }
    {
        let wk = weak.clone();
        ed.app_row.connect_activated(move |_| {
            if let Some(e) = wk() {
                e.pick_app();
            }
        });
    }

    // Key capture: runs in the capture phase so nothing in the dialog sees the keys.
    let keyc = gtk::EventControllerKey::new();
    keyc.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let wk = weak.clone();
        keyc.connect_key_pressed(move |c, keyval, keycode, state| {
            let Some(e) = wk() else {
                return glib::Propagation::Proceed;
            };
            if !e.capturing.get() {
                return glib::Propagation::Proceed;
            }
            e.on_capture_key(c.widget().as_ref(), keyval, keycode, state);
            glib::Propagation::Stop
        });
    }
    dialog.add_controller(keyc);

    // Leaving the window or closing the dialog always ends the capture.
    let active_handler = {
        let wk = weak.clone();
        ctx.window.connect_is_active_notify(move |w| {
            if !w.is_active()
                && let Some(e) = wk()
            {
                e.stop_capture();
            }
        })
    };
    {
        let window = ctx.window.downgrade();
        let handler = RefCell::new(Some(active_handler));
        // The dialog owns the editor until it is closed.
        let keep = RefCell::new(Some(ed.clone()));
        dialog.connect_closed(move |_| {
            if let Some(e) = keep.take() {
                e.stop_capture();
            }
            if let (Some(w), Some(h)) = (window.upgrade(), handler.take()) {
                w.disconnect(h);
            }
        });
    }
    {
        let d = dialog.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(d) = d.upgrade() {
                d.close();
            }
        });
    }
    {
        let wk = weak.clone();
        save.connect_clicked(move |_| {
            if let Some(e) = wk() {
                e.save();
            }
        });
    }

    dialog.present(Some(&ctx.window));

    // Load apps, the app launcher and Noctalia commands off the main thread.
    let wk = weak;
    let app_id = pre.app;
    let noct = pre.noct;
    glib::spawn_future_local(async move {
        let (list, launcher, cmds) = rt::blocking(move || {
            let cmds = has_noctalia.then(noctalia::commands);
            (apps(), app_launcher(), cmds)
        })
        .await;
        let Some(e) = wk() else { return };
        e.launcher.set(launcher);
        let list = Rc::new(list);
        e.apps.replace(list.clone());
        if let Some(id) = &app_id {
            let found = list
                .iter()
                .find(|a| &a.id == id)
                .cloned()
                .unwrap_or_else(|| App {
                    id: id.clone(),
                    name: id.trim_end_matches(".desktop").to_owned(),
                    icon: None,
                    description: String::new(),
                    exe: String::new(),
                    command: String::new(),
                });
            e.set_app(Some(found));
        }
        match cmds {
            None => {}
            Some(Ok(cmds)) => e.set_noctalia(cmds, noct),
            Some(Err(err)) => {
                e.noct_cmd.set_subtitle(&format!("Unavailable: {err:#}"));
                if let Some((c, a)) = noct {
                    // Keep the existing command selectable even without the help listing.
                    let cmd = NoctCmd {
                        name: c.clone(),
                        args: String::new(),
                        desc: String::new(),
                    };
                    e.set_noctalia(vec![cmd], Some((c, a)));
                }
            }
        }
        e.update_preview();
    });
}

impl Drop for Editor {
    /// Last line of defence: never leave Hyprland in the capture submap.
    fn drop(&mut self) {
        if self.capturing.get() {
            rt::spawn(async {
                let _ = rt::blocking(keys::end).await;
            });
        }
    }
}

impl Editor {
    /// Selected action type (`T_*`).
    fn kind(&self) -> u32 {
        self.kinds
            .get(self.kind.selected() as usize)
            .copied()
            .unwrap_or(T_ADV)
    }

    fn set_kind(&self, kind: u32) {
        if let Some(i) = self.kinds.iter().position(|&k| k == kind) {
            self.kind.set_selected(i as u32);
        }
    }

    fn show_kind(&self) {
        let k = self.kind();
        for (i, g) in self.groups.iter().enumerate() {
            g.set_visible(i as u32 == k);
        }
        let wp = &WINDOW_PRESETS[self.win_preset.selected() as usize % WINDOW_PRESETS.len()];
        self.win_n.set_visible(wp.args.contains("{n}"));
        let sp = &WORKSPACE_PRESETS[self.ws_preset.selected() as usize % WORKSPACE_PRESETS.len()];
        self.ws_n.set_visible(sp.args.contains("{n}"));
        let noct = self.noct_list.borrow();
        if let Some(c) = noct.get(self.noct_cmd.selected() as usize) {
            self.noct_cmd.set_subtitle(&c.desc);
            let hint = if c.args.is_empty() {
                "Arguments (none)".to_owned()
            } else {
                format!("Arguments {}", c.args)
            };
            self.noct_args.set_title(&hint);
        }
    }

    fn set_noctalia(&self, cmds: Vec<NoctCmd>, pre: Option<(String, String)>) {
        let labels: Vec<String> = cmds.iter().map(|c| c.name.clone()).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let sel = pre
            .as_ref()
            .and_then(|(c, _)| cmds.iter().position(|x| &x.name == c));
        self.noct_list.replace(cmds);
        self.noct_cmd
            .set_model(Some(&gtk::StringList::new(&labels)));
        if let Some(i) = sel {
            self.noct_cmd.set_selected(i as u32);
        }
        if let Some((_, args)) = pre {
            self.noct_args.set_text(&args);
        }
        self.show_kind();
    }

    fn set_app(&self, app: Option<App>) {
        match &app {
            Some(a) => {
                self.app_row.set_title(&a.name);
                self.app_row.set_subtitle(&a.id);
                match a
                    .icon
                    .as_deref()
                    .and_then(|i| gio::Icon::for_string(i).ok())
                {
                    Some(icon) => self.app_icon.set_from_gicon(&icon),
                    None => self
                        .app_icon
                        .set_icon_name(Some("application-x-executable-symbolic")),
                }
            }
            None => {
                self.app_row.set_title("Application");
                self.app_row.set_subtitle("None chosen");
            }
        }
        self.app.replace(app);
        self.update_preview();
    }

    /// The combo currently entered, canonicalized.
    fn combo(&self) -> Combo {
        Combo::parse(&self.keys_entry.text())
    }

    fn keys_changed(&self) {
        let combo = self.combo();
        hyprdeck_core::ui::clear(&self.caps_slot);
        if combo.0.is_empty() {
            self.capture_row.set_subtitle("Not set");
        } else {
            self.caps_slot.append(&widgets::keycaps(&combo.0));
            self.capture_row.set_subtitle("");
        }
        // Conflicts: other active binds on the same combo (default submap).
        let own = self.editing.as_ref().map(|e| &e.combo);
        let mut lines: Vec<String> = self
            .snapshot
            .binds
            .iter()
            .filter(|b| b.submap.is_empty() && !combo.0.is_empty() && b.combo.matches(&combo))
            .filter(|b| own.is_none_or(|o| !o.matches(&b.combo)))
            .map(|b| format!("{} ({})", b.label, b.source))
            .collect();
        let replaces = !lines.is_empty();
        if let Some(d) = self
            .snapshot
            .disabled
            .iter()
            .find(|d| !combo.0.is_empty() && d.combo.matches(&combo))
        {
            lines.push(format!(
                "{} is currently disabled; saving binds it again",
                d.combo.0
            ));
        }
        if let Some(e) = &self.editing
            && !combo.0.is_empty()
            && !e.combo.matches(&combo)
            && e.has_handwritten()
        {
            lines.push(format!("{} (from your config) will be disabled", e.combo.0));
        }
        self.conflict_row.set_visible(!lines.is_empty());
        self.conflict_row
            .set_title(if replaces { "Replaces" } else { "Note" });
        self.conflict_row.set_subtitle(&lines.join("\n"));
    }

    // ---- capture ----------------------------------------------------------------

    fn start_capture(self: &Rc<Self>) {
        self.record.set_sensitive(false);
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = rt::blocking(keys::begin).await;
            let Some(e) = w.upgrade() else {
                // Dialog gone while we were switching: never leave the submap active.
                let _ = rt::blocking(keys::end).await;
                return;
            };
            e.record.set_sensitive(true);
            match res {
                Ok(()) if e.record.is_active() => {
                    e.capturing.set(true);
                    e.record.set_label("Recording…");
                    e.capture_row
                        .set_subtitle("Press the shortcut now · Esc cancels");
                    e.record.grab_focus();
                }
                Ok(()) => {
                    let _ = rt::blocking(keys::end).await;
                }
                Err(err) => {
                    e.record_guard.hold(|| e.record.set_active(false));
                    e.ctx.error("Could not start recording", &err);
                }
            }
        });
    }

    fn stop_capture(&self) {
        if self.capturing.replace(false) {
            rt::spawn(async {
                if let Err(e) = rt::blocking(keys::end).await {
                    tracing::error!("leaving capture submap: {e:#}");
                }
            });
        }
        self.record_guard.hold(|| self.record.set_active(false));
        self.record.set_label("Record");
        if self.combo().0.is_empty() {
            self.capture_row.set_subtitle("Not set");
        } else {
            self.capture_row.set_subtitle("");
        }
    }

    fn on_capture_key(
        &self,
        widget: Option<&gtk::Widget>,
        keyval: gdk::Key,
        keycode: u32,
        state: gdk::ModifierType,
    ) {
        let Some(display) = widget.map(|w| w.display()) else {
            return;
        };
        let Some(sym) = keys::base_keysym(&display, keycode, keyval) else {
            return;
        };
        let mods = keys::modifier_names(state);
        if keys::is_modifier_sym(&sym) {
            let partial = keys::combo(&mods, "");
            self.capture_row.set_subtitle(&if partial.is_empty() {
                "…".into()
            } else {
                format!("{partial} + …")
            });
            return;
        }
        if sym == "Escape" && mods.is_empty() {
            self.stop_capture();
            return;
        }
        self.keys_entry
            .set_text(&keys::combo(&mods, &keys::hypr_key_name(&sym)));
        self.stop_capture();
    }

    // ---- action -----------------------------------------------------------------

    /// (dispatcher, Lua args) of the current action, or why it is incomplete.
    fn action(&self) -> Result<(String, String), String> {
        let exec = |cmd: String| Ok(("exec_cmd".to_owned(), lua_str(&cmd)));
        match self.kind() {
            T_APP => match self.app.borrow().as_ref() {
                Some(a) => match self.launcher.get() {
                    Some(prefix) => exec(format!("{prefix}{}", a.id)),
                    None if !a.command.is_empty() => exec(a.command.clone()),
                    None => Err(format!("{} has no command line to run", a.name)),
                },
                None => Err("Choose an application".into()),
            },
            T_CMD => {
                let c = self.command.text().trim().to_owned();
                if c.is_empty() {
                    Err("Enter a command".into())
                } else {
                    exec(c)
                }
            }
            T_NOCT => {
                let list = self.noct_list.borrow();
                let Some(c) = list.get(self.noct_cmd.selected() as usize) else {
                    return Err("Choose a Noctalia command".into());
                };
                let args = self.noct_args.text().trim().to_owned();
                if c.needs_args() && args.is_empty() {
                    return Err(format!("“{}” needs arguments: {}", c.name, c.args));
                }
                let noct = noctalia::PREFIX;
                exec(if args.is_empty() {
                    format!("{noct}{}", c.name)
                } else {
                    format!("{noct}{} {args}", c.name)
                })
            }
            T_WIN => {
                let p = &WINDOW_PRESETS[self.win_preset.selected() as usize % WINDOW_PRESETS.len()];
                Ok((p.dsp.to_owned(), preset_args(p, self.win_n.value() as i64)))
            }
            T_WS => {
                let p = &WORKSPACE_PRESETS
                    [self.ws_preset.selected() as usize % WORKSPACE_PRESETS.len()];
                Ok((p.dsp.to_owned(), preset_args(p, self.ws_n.value() as i64)))
            }
            _ => {
                let d = self
                    .adv_dsp
                    .text()
                    .trim()
                    .trim_start_matches("hl.dsp.")
                    .to_owned();
                if d.is_empty() {
                    Err("Enter a dispatcher".into())
                } else {
                    Ok((d, self.adv_args.text().trim().to_owned()))
                }
            }
        }
    }

    fn update_preview(&self) {
        match self.action() {
            Ok((d, args)) => {
                let apps = self.apps.borrow();
                let names = |p: &str| {
                    apps.iter()
                        .find(|a| a.id == p || a.exe == p)
                        .map(|a| a.name.clone())
                };
                let (label, cat) = actions::describe(Some(&d), &args, &names);
                self.preview.set_title(&label);
                self.preview
                    .set_subtitle(&format!("{} · hl.dsp.{d}({args})", cat.title()));
            }
            Err(why) => {
                self.preview.set_title("Does");
                self.preview.set_subtitle(&why);
            }
        }
    }

    fn pick_app(self: &Rc<Self>) {
        let apps = self.apps.borrow().clone();
        let dialog = adw::Dialog::builder()
            .title("Choose Application")
            .content_width(460)
            .content_height(600)
            .build();
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search applications")
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_valign(gtk::Align::Start);
        list.set_margin_start(12);
        list.set_margin_end(12);
        list.set_margin_bottom(12);
        let mut hay = Vec::with_capacity(apps.len());
        for app in apps.iter() {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .activatable(true)
                .build();
            row.set_title(&app.name);
            row.set_subtitle(&app.id);
            let img = match app
                .icon
                .as_deref()
                .and_then(|i| gio::Icon::for_string(i).ok())
            {
                Some(icon) => gtk::Image::from_gicon(&icon),
                None => gtk::Image::from_icon_name("application-x-executable-symbolic"),
            };
            img.set_pixel_size(32);
            row.add_prefix(&img);
            list.append(&row);
            hay.push((
                row.downgrade(),
                format!("{} {} {}", app.name, app.id, app.description).to_lowercase(),
            ));
            let (e, d, a) = (Rc::downgrade(self), dialog.downgrade(), app.clone());
            row.connect_activated(move |_| {
                if let Some(e) = e.upgrade() {
                    e.set_app(Some(a.clone()));
                }
                if let Some(d) = d.upgrade() {
                    d.close();
                }
            });
        }
        if apps.is_empty() {
            list.append(
                &adw::ActionRow::builder()
                    .title("Still loading applications…")
                    .build(),
            );
        }
        search.connect_search_changed(move |s| {
            let q = s.text().to_lowercase();
            for (row, h) in &hay {
                if let Some(r) = row.upgrade() {
                    r.set_visible(q.is_empty() || h.contains(&q));
                }
            }
        });
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&search);
        body.append(&scroller);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&body));
        dialog.set_child(Some(&view));
        dialog.present(Some(&self.ctx.window));
        search.grab_focus();
    }

    fn flags(&self) -> BindFlags {
        let mut f = self
            .editing
            .as_ref()
            .map(|e| e.flags.clone())
            .unwrap_or_default();
        for (k, row) in &self.flags {
            let on = row.is_active();
            match *k {
                "repeating" => f.repeating = on,
                "locked" => f.locked = on,
                "release" => f.release = on,
                "non_consuming" => f.non_consuming = on,
                "ignore_mods" => f.ignore_mods = on,
                _ => {}
            }
        }
        f
    }

    fn save(self: &Rc<Self>) {
        self.stop_capture();
        let combo = self.combo();
        if combo.0.is_empty() {
            self.ctx.toast("Set a shortcut first");
            return;
        }
        let (dispatcher, args) = match self.action() {
            Ok(a) => a,
            Err(why) => {
                self.ctx.toast(why);
                return;
            }
        };
        let desc = self.description.text().trim().to_owned();
        let rule = BindRule {
            keys: combo.0.clone(),
            dispatcher,
            args,
            flags: self.flags(),
            description: (!desc.is_empty()).then_some(desc),
        };
        let original = self.editing.as_ref().map(|e| Original {
            combo: e.combo.clone(),
            has_handwritten: e.has_handwritten(),
        });
        self.save.set_sensitive(false);
        let e = self.clone();
        glib::spawn_future_local(async move {
            let res = rt::blocking(move || {
                binds::validate(&rule.dispatcher, &rule.args)?;
                binds::save(original, rule)
            })
            .await;
            e.save.set_sensitive(true);
            match res {
                Ok(errors) => {
                    widgets::report_errors(&e.ctx, &format!("Saved {}", combo.0), &errors);
                    if let Some(d) = e.dialog.upgrade() {
                        d.close();
                    }
                    (e.on_saved)();
                }
                Err(err) => e.ctx.error("Could not save keybind", &err),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_round_trip() {
        for (i, p) in WINDOW_PRESETS.iter().enumerate() {
            let a = parse_args(&preset_args(p, 4));
            let m = match_preset(&WINDOW_PRESETS, p.dsp, &a).unwrap();
            assert_eq!(m.0, i, "{}", p.label);
            if p.args.contains("{n}") {
                assert_eq!(m.1, 4);
            }
        }
        for (i, p) in WORKSPACE_PRESETS.iter().enumerate() {
            let a = parse_args(&preset_args(p, 7));
            assert_eq!(match_preset(&WORKSPACE_PRESETS, p.dsp, &a).unwrap().0, i);
        }
        assert!(
            match_preset(
                &WINDOW_PRESETS,
                "window.move",
                &parse_args("{ direction = \"l\" }")
            )
            .is_none()
        );
    }
}
