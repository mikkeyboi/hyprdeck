//! "Input Devices" page: global keyboard/mouse/touchpad/cursor options and
//! per-device overrides, all written to hyprdeck's managed config.
//!
//! The page is rebuilt whenever it is shown, so closures stored on widgets only
//! hold weak references back to widgets to avoid reference cycles.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::{self, WeakRef};
use hyprdeck_core::hypr::managed::OptValue;
use hyprdeck_core::hypr::model::Value;
use hyprdeck_core::hypr::schema::{self, OptKind};
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::devices::{DevKind, DeviceGroup, GroupClass};
use crate::inputs::{self, DeviceSettings, InputState, Origin, parse_bool};
use crate::widgets::{self, Debounce, Guard};
use crate::xkb::{self, CATALOG};

pub fn build(ctx: &Ctx) -> gtk::Widget {
    widgets::ensure_css();
    let (scroller, content) = ui::page_scaffold();
    let page = Page {
        ctx: ctx.clone(),
        content,
        loading: Rc::default(),
    };
    ui::on_shown(&scroller, move || page.reload());
    scroller.upcast()
}

#[derive(Clone)]
struct Page {
    ctx: Ctx,
    content: gtk::Box,
    loading: Rc<Cell<bool>>,
}

/// Live values keyed by option name.
type Values = HashMap<String, String>;

/// Hook that re-applies widget state after a reset.
type Apply<T> = RefCell<Option<Box<dyn Fn(&T)>>>;

fn value_of<'a>(vals: &'a Values, key: &str) -> &'a str {
    vals.get(key).map_or("", String::as_str)
}

fn add(g: &adw::PreferencesGroup, row: Option<impl IsA<gtk::Widget>>) {
    if let Some(r) = row {
        g.add(&r);
    }
}

fn set_accent(label: &gtk::Label, accent: bool) {
    if accent {
        label.remove_css_class("dim-label");
        label.add_css_class("accent");
    } else {
        label.add_css_class("dim-label");
        label.remove_css_class("accent");
    }
}

/// One (or a few coupled) global options bound to a row.
struct Opt {
    keys: Vec<&'static str>,
    origin: gtk::Label,
    reset: WeakRef<gtk::Button>,
    guard: Guard,
    debounce: Debounce,
    /// Re-apply widget state from live values (after a reset).
    apply: Apply<Values>,
}

impl Opt {
    fn show_origin(&self, o: &Origin) {
        self.origin.set_label(o.label());
        self.origin.set_tooltip_text(Some(&o.tooltip()));
        let managed = matches!(o, Origin::Hyprdeck { .. });
        set_accent(&self.origin, managed);
        if let Some(r) = self.reset.upgrade() {
            r.set_visible(managed);
        }
    }

    fn on_apply(&self, f: impl Fn(&Values) + 'static) {
        self.apply.replace(Some(Box::new(f)));
    }
}

impl Page {
    fn reload(&self) {
        if self.loading.replace(true) {
            return;
        }
        if self.content.first_child().is_none() {
            self.content.append(&widgets::loading());
        }
        let p = self.clone();
        glib::spawn_future_local(async move {
            let res = rt::blocking(inputs::load).await;
            p.loading.set(false);
            ui::clear(&p.content);
            match res {
                Ok(st) => p.render(&st),
                Err(e) => {
                    let p2 = p.clone();
                    p.content.append(&widgets::error_page(
                        "Could not read input settings",
                        &e,
                        move || p2.reload(),
                    ));
                }
            }
        });
    }

    fn render(&self, st: &InputState) {
        self.content.append(&self.keyboard_group(st));
        self.content.append(&self.mouse_group(st));
        self.content.append(&self.focus_group(st));
        if st.has_touchpad {
            self.content.append(&self.touchpad_group(st));
        }
        self.content.append(&self.cursor_group(st));
        self.device_groups(st);
    }

    // ---- global option plumbing -------------------------------------------------

    fn opt(
        &self,
        row: &impl IsA<adw::PreferencesRow>,
        keys: Vec<&'static str>,
        st: &InputState,
    ) -> Rc<Opt> {
        let origin = widgets::origin_label();
        let reset = widgets::icon_button(
            "edit-undo-symbolic",
            "Remove hyprdeck's value (use config or default)",
        );
        if let Some(r) = row.dynamic_cast_ref::<adw::ActionRow>() {
            r.add_suffix(&origin);
            r.add_suffix(&reset);
        } else if let Some(r) = row.dynamic_cast_ref::<adw::ExpanderRow>() {
            r.add_suffix(&origin);
            r.add_suffix(&reset);
        }
        let opt = Rc::new(Opt {
            keys,
            origin,
            reset: reset.downgrade(),
            guard: Guard::default(),
            debounce: Debounce::default(),
            apply: RefCell::new(None),
        });
        opt.show_origin(&st.origin(opt.keys[0]));
        let p = self.clone();
        let o = opt.clone();
        reset.connect_clicked(move |_| p.write(&o, None, 0));
        opt
    }

    /// Persist one value per key of `opt` (`None` = remove hyprdeck's values)
    /// and reload Hyprland; refreshes the origin label afterwards.
    fn write(&self, opt: &Rc<Opt>, values: Option<Vec<OptValue>>, delay_ms: u64) {
        let p = self.clone();
        let o = opt.clone();
        opt.debounce.run(delay_ms, move || {
            let keys = o.keys.clone();
            let resetting = values.is_none();
            glib::spawn_future_local(async move {
                let res = rt::blocking(move || -> anyhow::Result<(Vec<String>, Values, Origin)> {
                    let changes: Vec<(&str, Option<OptValue>)> = match values {
                        Some(vs) => keys.iter().copied().zip(vs.into_iter().map(Some)).collect(),
                        None => keys.iter().map(|k| (*k, None)).collect(),
                    };
                    let errors = inputs::set_options(&changes)?;
                    let vals = inputs::live_values(&keys)?;
                    let (_, origin) = inputs::refresh_key(keys[0])?;
                    Ok((errors, vals, origin))
                })
                .await;
                match res {
                    Ok((errors, vals, origin)) => {
                        o.show_origin(&origin);
                        if resetting {
                            if let Some(apply) = o.apply.borrow().as_ref() {
                                o.guard.hold(|| apply(&vals));
                            }
                            widgets::report_errors(
                                &p.ctx,
                                &format!("Now using the {} value", origin.label()),
                                &errors,
                            );
                        } else if !errors.is_empty() {
                            widgets::report_errors(&p.ctx, "", &errors);
                        }
                    }
                    Err(e) => p.ctx.error("Could not apply input setting", &e),
                }
            });
        });
    }

    fn switch(
        &self,
        st: &InputState,
        key: &'static str,
        title: &str,
        subtitle: &str,
    ) -> Option<adw::SwitchRow> {
        schema::kind(key)?;
        let row = adw::SwitchRow::builder().use_markup(false).build();
        row.set_title(title);
        row.set_subtitle(subtitle);
        row.set_active(parse_bool(st.value(key)));
        let opt = self.opt(&row, vec![key], st);
        let r = row.downgrade();
        opt.on_apply(move |v| {
            if let Some(r) = r.upgrade() {
                r.set_active(parse_bool(value_of(v, key)));
            }
        });
        let p = self.clone();
        row.connect_active_notify(move |r| {
            if !opt.guard.active() {
                p.write(&opt, Some(vec![OptValue::Bool(r.is_active())]), 0);
            }
        });
        Some(row)
    }

    #[allow(clippy::too_many_arguments)]
    fn spin(
        &self,
        st: &InputState,
        key: &'static str,
        title: &str,
        subtitle: &str,
        range: (f64, f64),
        step: f64,
        digits: u32,
    ) -> Option<adw::SpinRow> {
        let kind = schema::kind(key)?;
        let row = adw::SpinRow::with_range(range.0, range.1, step);
        row.set_use_markup(false);
        row.set_title(title);
        row.set_subtitle(subtitle);
        row.set_digits(digits);
        row.set_value(st.value(key).parse().unwrap_or(0.0));
        let opt = self.opt(&row, vec![key], st);
        let r = row.downgrade();
        opt.on_apply(move |v| {
            if let Some(r) = r.upgrade() {
                r.set_value(value_of(v, key).parse().unwrap_or(0.0));
            }
        });
        let p = self.clone();
        row.connect_value_notify(move |r| {
            if opt.guard.active() {
                return;
            }
            let v = match kind {
                OptKind::Float => OptValue::Float(r.value()),
                _ => OptValue::Int(r.value().round() as i64),
            };
            p.write(&opt, Some(vec![v]), 600);
        });
        Some(row)
    }

    /// Combo row over fixed values. A label may carry a detail after `|`
    /// (`"Click to focus|Moving the cursor never changes focus"`), shown under
    /// the subtitle while that item is selected.
    fn choice(
        &self,
        st: &InputState,
        key: &'static str,
        title: &str,
        subtitle: &str,
        choices: &[(&str, &str)],
    ) -> Option<adw::ComboRow> {
        let kind = schema::kind(key)?;
        // (value, label, detail)
        let mut items: Vec<(String, String, String)> = choices
            .iter()
            .map(|(v, l)| {
                let (label, detail) = l.split_once('|').unwrap_or((l, ""));
                ((*v).to_owned(), label.to_owned(), detail.to_owned())
            })
            .collect();
        let current = st.value(key).to_owned();
        if !items.iter().any(|(v, ..)| *v == current) {
            items.push((current.clone(), format!("Custom: {current}"), String::new()));
        }
        let labels: Vec<&str> = items.iter().map(|(_, l, _)| l.as_str()).collect();
        let row = adw::ComboRow::builder()
            .use_markup(false)
            .model(&gtk::StringList::new(&labels))
            .build();
        row.set_title(title);
        fn index(v: &str, items: &[(String, String, String)]) -> u32 {
            items.iter().position(|(x, ..)| x == v).unwrap_or(0) as u32
        }
        let base = subtitle.to_owned();
        let describe = move |r: &adw::ComboRow, items: &[(String, String, String)]| match items
            .get(r.selected() as usize)
            .map(|(_, _, d)| d.as_str())
            .filter(|d| !d.is_empty())
        {
            Some(d) => r.set_subtitle(&format!("{base}\n{d}")),
            None => r.set_subtitle(&base),
        };
        row.set_selected(index(&current, &items));
        describe(&row, &items);
        let items = Rc::new(items);
        let opt = self.opt(&row, vec![key], st);
        let (r, it) = (row.downgrade(), items.clone());
        opt.on_apply(move |v| {
            if let Some(r) = r.upgrade() {
                r.set_selected(index(value_of(v, key), &it));
            }
        });
        let p = self.clone();
        row.connect_selected_notify(move |r| {
            describe(r, &items);
            if opt.guard.active() {
                return;
            }
            let Some((v, ..)) = items.get(r.selected() as usize) else {
                return;
            };
            let v = match (kind, v.parse::<i64>()) {
                (OptKind::Int | OptKind::IntOrStr, Ok(i)) => OptValue::Int(i),
                _ => OptValue::Str(v.clone()),
            };
            p.write(&opt, Some(vec![v]), 0);
        });
        Some(row)
    }

    // ---- groups -----------------------------------------------------------------

    fn keyboard_group(&self, st: &InputState) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder()
            .title("Keyboard")
            .description("Applies to every keyboard unless a device below overrides it")
            .build();
        g.add(&self.layouts_row(st));
        g.add(&self.xkb_options_row(st));
        add(
            &g,
            self.spin(
                st,
                "input.repeat_rate",
                "Repeat rate",
                "Characters per second while a key is held",
                (1.0, 100.0),
                1.0,
                0,
            ),
        );
        add(
            &g,
            self.spin(
                st,
                "input.repeat_delay",
                "Repeat delay",
                "Milliseconds before a held key starts repeating",
                (100.0, 2000.0),
                25.0,
                0,
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.numlock_by_default",
                "Num Lock on at login",
                "Turn Num Lock on when a keyboard appears",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.resolve_binds_by_sym",
                "Resolve keybinds by symbol",
                "Match binds by the character your layout types rather than the physical key",
            ),
        );
        g
    }

    fn layouts_row(&self, st: &InputState) -> adw::ExpanderRow {
        let exp = adw::ExpanderRow::builder().use_markup(false).build();
        exp.set_title("Layouts");
        let opt = self.opt(&exp, vec!["input.kb_layout", "input.kb_variant"], st);
        let ui = Rc::new(LayoutsUi {
            page: self.clone(),
            exp: exp.downgrade(),
            opt: Rc::downgrade(&opt),
            state: RefCell::new(layout_pairs(
                st.value("input.kb_layout"),
                st.value("input.kb_variant"),
            )),
            rows: RefCell::default(),
        });
        ui.render();
        opt.on_apply(move |v| {
            ui.state.replace(layout_pairs(
                value_of(v, "input.kb_layout"),
                value_of(v, "input.kb_variant"),
            ));
            ui.render();
        });
        exp
    }

    fn xkb_options_row(&self, st: &InputState) -> adw::ExpanderRow {
        let current = xkb::split_list(st.value("input.kb_options"));
        let exp = adw::ExpanderRow::builder().use_markup(false).build();
        exp.set_title("Key behaviour options");
        exp.set_subtitle(&options_summary(&current));
        let opt = self.opt(&exp, vec!["input.kb_options"], st);
        let mut switches: Vec<(&'static str, WeakRef<adw::SwitchRow>)> = Vec::new();
        let mut rows = Vec::new();
        for o in xkb::COMMON_OPTIONS {
            let Some(desc) = CATALOG.option_desc(o) else {
                continue;
            };
            let group = CATALOG.group_desc(o).unwrap_or("");
            let row = adw::SwitchRow::builder().use_markup(false).build();
            row.set_title(desc);
            row.set_subtitle(&format!("{group} · {o}"));
            row.set_active(current.iter().any(|c| c == o));
            exp.add_row(&row);
            switches.push((o, row.downgrade()));
            rows.push(row);
        }
        let other = adw::EntryRow::builder()
            .title("Other options (comma-separated)")
            .show_apply_button(true)
            .build();
        other.set_text(&uncommon(&current));
        exp.add_row(&other);
        let ui = Rc::new(OptionsUi {
            switches,
            other: other.downgrade(),
            exp: exp.downgrade(),
        });

        for row in &rows {
            let (p, o, u) = (self.clone(), opt.clone(), ui.clone());
            row.connect_active_notify(move |_| {
                if !o.guard.active() {
                    u.commit(&p, &o);
                }
            });
        }
        let (p, o, u) = (self.clone(), opt.clone(), ui.clone());
        other.connect_apply(move |_| u.commit(&p, &o));
        opt.on_apply(move |v| ui.show(&xkb::split_list(value_of(v, "input.kb_options"))));
        exp
    }

    fn mouse_group(&self, st: &InputState) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder()
            .title("Mouse &amp; Pointer")
            .description("Applies to every pointer unless a device below overrides it")
            .build();
        add(
            &g,
            self.spin(
                st,
                "input.sensitivity",
                "Pointer speed",
                "−1 slowest … 1 fastest; 0 keeps the device's native speed",
                (-1.0, 1.0),
                0.05,
                2,
            ),
        );
        add(
            &g,
            self.choice(
                st,
                "input.accel_profile",
                "Acceleration",
                "How pointer speed reacts to how fast you move",
                &[
                    ("", "Device default|libinput's choice for each device"),
                    ("adaptive", "Adaptive|Faster movements travel further"),
                    (
                        "flat",
                        "Flat|Constant speed regardless of how fast you move",
                    ),
                ],
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.natural_scroll",
                "Natural scrolling",
                "Content follows the wheel direction like a touchscreen",
            ),
        );
        add(
            &g,
            self.spin(
                st,
                "input.scroll_factor",
                "Scroll speed",
                "Multiplier for wheel scroll distance",
                (0.1, 10.0),
                0.1,
                1,
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.left_handed",
                "Left-handed",
                "Swap the left and right mouse buttons",
            ),
        );
        g
    }

    fn focus_group(&self, st: &InputState) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder().title("Focus").build();
        add(
            &g,
            self.choice(
                st,
                "input.follow_mouse",
                "Focus follows mouse",
                "Which window receives your typing as the cursor moves",
                &[
                    ("0", "Click to focus|Moving the cursor never changes focus"),
                    ("1", "Follow the cursor|Hovering a window focuses it for mouse and keyboard"),
                    ("2", "Click to type|Scrolling and hover go to the window under the cursor; typing needs a click"),
                    ("3", "Fully separate|Hover moves mouse focus only; clicking never moves keyboard focus"),
                ],
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.mouse_refocus",
                "Refocus on mouse move",
                "When following the cursor, moving inside a window re-focuses it after focus moved away",
            ),
        );
        add(
            &g,
            self.choice(
                st,
                "input.float_switch_override_focus",
                "Focus when crossing floating windows",
                "Whether moving the cursor between tiled and floating windows changes focus",
                &[
                    ("0", "Never"),
                    (
                        "1",
                        "Tiled ↔ floating|Moving between tiled and floating windows changes focus",
                    ),
                    (
                        "2",
                        "All windows|Also when moving between two floating windows",
                    ),
                ],
            ),
        );
        g
    }

    fn touchpad_group(&self, st: &InputState) -> adw::PreferencesGroup {
        let names: Vec<&str> = st
            .groups
            .iter()
            .filter(|g| g.has(|k| k == DevKind::Touchpad))
            .map(|g| g.label.as_str())
            .collect();
        let g = adw::PreferencesGroup::builder()
            .title("Touchpad")
            .description(glib::markup_escape_text(&format!(
                "Detected: {}",
                names.join(", ")
            )))
            .build();
        add(
            &g,
            self.switch(
                st,
                "input.touchpad.tap_to_click",
                "Tap to click",
                "A light tap counts as a click",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.touchpad.natural_scroll",
                "Natural scrolling",
                "Content moves with your fingers",
            ),
        );
        add(
            &g,
            self.spin(
                st,
                "input.touchpad.scroll_factor",
                "Scroll speed",
                "Multiplier for two-finger scroll distance",
                (0.1, 10.0),
                0.1,
                1,
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.touchpad.disable_while_typing",
                "Disable while typing",
                "Ignore the touchpad while keys are pressed",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.touchpad.clickfinger_behavior",
                "Click with multiple fingers",
                "Two-finger click = right click, three = middle click",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.touchpad.middle_button_emulation",
                "Middle-click emulation",
                "Pressing left and right together acts as a middle click",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "input.touchpad.tap_and_drag",
                "Tap and drag",
                "Tap then hold to drag",
            ),
        );
        g
    }

    fn cursor_group(&self, st: &InputState) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder().title("Cursor").build();
        add(
            &g,
            self.spin(
                st,
                "cursor.inactive_timeout",
                "Hide after inactivity",
                "Seconds without movement before the cursor hides; 0 = never",
                (0.0, 120.0),
                1.0,
                0,
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "cursor.hide_on_key_press",
                "Hide while typing",
                "Hide the cursor on a key press until the mouse moves",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "cursor.hide_on_touch",
                "Hide on touch input",
                "Hide the cursor when the screen is touched",
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "cursor.no_warps",
                "Never move the cursor automatically",
                "Stop Hyprland from warping the cursor to focused windows",
            ),
        );
        add(
            &g,
            self.choice(
                st,
                "cursor.warp_on_change_workspace",
                "Move cursor on workspace change",
                "Warp to the last focused window after switching workspaces",
                &[
                    ("0", "Off"),
                    ("1", "On"),
                    ("2", "Always (even with warps disabled)"),
                ],
            ),
        );
        add(
            &g,
            self.choice(
                st,
                "cursor.no_hardware_cursors",
                "Cursor rendering",
                "Software cursors avoid glitches on some GPUs at a small cost",
                &[
                    ("2", "Automatic"),
                    ("0", "Hardware cursor"),
                    ("1", "Software cursor"),
                ],
            ),
        );
        add(
            &g,
            self.switch(
                st,
                "cursor.enable_hyprcursor",
                "Use hyprcursor themes",
                "Prefer hyprcursor themes over XCursor",
            ),
        );
        g
    }

    // ---- devices ----------------------------------------------------------------

    fn device_groups(&self, st: &InputState) {
        let physical = adw::PreferencesGroup::builder()
            .title("Devices")
            .description("Per-device overrides. Sub-devices of the same hardware are grouped and changed together.")
            .build();
        let other = adw::PreferencesGroup::builder()
            .title("System &amp; Virtual Devices")
            .description("Power buttons and input devices created by software, such as shells and key remappers")
            .build();
        for g in &st.groups {
            let row = self.device_row(st, g);
            if g.class == GroupClass::Physical {
                physical.add(&row)
            } else {
                other.add(&row)
            }
        }
        other.set_visible(st.groups.iter().any(|g| g.class != GroupClass::Physical));
        self.content.append(&physical);
        self.content.append(&other);
    }

    fn device_row(&self, st: &InputState, g: &DeviceGroup) -> adw::ExpanderRow {
        let all: Rc<Vec<String>> = Rc::new(g.members.iter().map(|m| m.name.clone()).collect());
        let n = g.members.len();
        let exp = adw::ExpanderRow::builder().use_markup(false).build();
        exp.set_title(&g.label);
        exp.set_subtitle(&format!(
            "{} · {n} sub-device{}",
            g.kinds(),
            if n == 1 { "" } else { "s" }
        ));
        let icon = if g.has(|k| k == DevKind::Touchpad) {
            "input-touchpad-symbolic"
        } else if g.has(|k| k == DevKind::Pointer) {
            "input-mouse-symbolic"
        } else if g.has(|k| k == DevKind::Tablet) {
            "input-tablet-symbolic"
        } else {
            "input-keyboard-symbolic"
        };
        exp.add_prefix(&gtk::Image::from_icon_name(icon));
        let badge = widgets::badge("hyprdeck", Some("accent"));
        exp.add_suffix(&badge);
        if g.class == GroupClass::Virtual {
            exp.add_suffix(&widgets::badge("virtual", None));
        }

        let members = adw::ActionRow::builder()
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        members.set_title("Sub-devices");
        members.set_subtitle(
            &g.members
                .iter()
                .map(|m| format!("{} ({})", m.name, m.kind.label()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        exp.add_row(&members);
        let config_rules: Vec<String> = all
            .iter()
            .filter_map(|n| st.device_settings.get(n)?.config.as_ref())
            .map(|(fields, src)| {
                let f: Vec<String> = fields
                    .iter()
                    .filter(|(k, _)| *k != "name")
                    .map(|(k, v)| format!("{k} = {}", v.display()))
                    .collect();
                format!("{src}: {}", f.join(", "))
            })
            .collect();
        if !config_rules.is_empty() {
            let rules = adw::ActionRow::builder().use_markup(false).build();
            rules.set_title("Rules in your config");
            rules.set_subtitle(&config_rules.join("\n"));
            exp.add_row(&rules);
        }

        let reset = gtk::Button::with_label("Reset");
        reset.add_css_class("destructive-action");
        reset.set_valign(gtk::Align::Center);
        let dev = Rc::new(DevCtx {
            page: self.clone(),
            label: g.label.clone(),
            names: all.clone(),
            badge: badge.downgrade(),
            reset: reset.downgrade(),
            settings: all
                .iter()
                .map(|n| {
                    (
                        n.clone(),
                        st.device_settings.get(n).cloned().unwrap_or_default(),
                    )
                })
                .collect(),
            fields: RefCell::default(),
        });

        dev.enabled_row(&exp, all.clone());
        let pointers = Rc::new(g.names_where(DevKind::is_pointer));
        if !pointers.is_empty() {
            dev.spin(
                &exp,
                st,
                &pointers,
                "sensitivity",
                "input.sensitivity",
                "Pointer speed",
                (-1.0, 1.0),
                0.05,
                2,
                true,
            );
            dev.choice(
                &exp,
                st,
                &pointers,
                "accel_profile",
                "input.accel_profile",
                "Acceleration",
                &[("flat", "Flat"), ("adaptive", "Adaptive")],
            );
            dev.tri(
                &exp,
                st,
                &pointers,
                "natural_scroll",
                "input.natural_scroll",
                "Natural scrolling",
            );
            dev.tri(
                &exp,
                st,
                &pointers,
                "left_handed",
                "input.left_handed",
                "Left-handed",
            );
            dev.spin(
                &exp,
                st,
                &pointers,
                "scroll_factor",
                "input.scroll_factor",
                "Scroll speed",
                (0.1, 10.0),
                0.1,
                1,
                true,
            );
        }
        let touchpads = Rc::new(g.names_where(|k| k == DevKind::Touchpad));
        if !touchpads.is_empty() {
            dev.tri(
                &exp,
                st,
                &touchpads,
                "tap_to_click",
                "input.touchpad.tap_to_click",
                "Tap to click",
            );
            dev.tri(
                &exp,
                st,
                &touchpads,
                "disable_while_typing",
                "input.touchpad.disable_while_typing",
                "Disable while typing",
            );
        }
        let keyboards = Rc::new(g.names_where(|k| k == DevKind::Keyboard));
        if !keyboards.is_empty() {
            dev.entry(
                &exp,
                st,
                &keyboards,
                "kb_layout",
                "input.kb_layout",
                "Layouts (e.g. us,de)",
            );
            dev.entry(
                &exp,
                st,
                &keyboards,
                "kb_variant",
                "input.kb_variant",
                "Layout variants",
            );
            dev.entry(
                &exp,
                st,
                &keyboards,
                "kb_options",
                "input.kb_options",
                "XKB options",
            );
            dev.spin(
                &exp,
                st,
                &keyboards,
                "repeat_rate",
                "input.repeat_rate",
                "Repeat rate",
                (1.0, 100.0),
                1.0,
                0,
                false,
            );
            dev.spin(
                &exp,
                st,
                &keyboards,
                "repeat_delay",
                "input.repeat_delay",
                "Repeat delay (ms)",
                (100.0, 2000.0),
                25.0,
                0,
                false,
            );
        }

        let reset_row = adw::ActionRow::builder()
            .title("Reset device")
            .subtitle("Remove every hyprdeck override for this device")
            .build();
        reset_row.add_suffix(&reset);
        exp.add_row(&reset_row);
        let d = dev.clone();
        reset.connect_clicked(move |_| d.reset_all());
        dev.sync();
        exp
    }
}

fn layout_pairs(layouts: &str, variants: &str) -> Vec<(String, String)> {
    let vs = xkb::split_list(variants);
    xkb::split_list(layouts)
        .into_iter()
        .enumerate()
        .map(|(i, l)| (l, vs.get(i).cloned().unwrap_or_default()))
        .collect()
}

fn options_summary(list: &[String]) -> String {
    if list.is_empty() {
        "None".into()
    } else {
        list.join(", ")
    }
}

fn uncommon(list: &[String]) -> String {
    list.iter()
        .filter(|c| !xkb::COMMON_OPTIONS.contains(&c.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join(",")
}

fn layout_name(layout: &str, variant: &str) -> String {
    if !variant.is_empty()
        && let Some((_, d)) = CATALOG.variants_of(layout).find(|(v, _)| *v == variant)
    {
        return d.to_owned();
    }
    CATALOG.layout_desc(layout).unwrap_or(layout).to_owned()
}

struct OptionsUi {
    switches: Vec<(&'static str, WeakRef<adw::SwitchRow>)>,
    other: WeakRef<adw::EntryRow>,
    exp: WeakRef<adw::ExpanderRow>,
}

impl OptionsUi {
    fn collect(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .switches
            .iter()
            .filter(|(_, r)| r.upgrade().is_some_and(|r| r.is_active()))
            .map(|(o, _)| (*o).to_owned())
            .collect();
        if let Some(other) = self.other.upgrade() {
            v.extend(
                other
                    .text()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
        v
    }

    fn commit(&self, page: &Page, opt: &Rc<Opt>) {
        let list = self.collect();
        if let Some(e) = self.exp.upgrade() {
            e.set_subtitle(&options_summary(&list));
        }
        page.write(opt, Some(vec![OptValue::Str(list.join(","))]), 0);
    }

    fn show(&self, list: &[String]) {
        for (o, row) in &self.switches {
            if let Some(row) = row.upgrade() {
                row.set_active(list.iter().any(|c| c == o));
            }
        }
        if let Some(other) = self.other.upgrade() {
            other.set_text(&uncommon(list));
        }
        if let Some(e) = self.exp.upgrade() {
            e.set_subtitle(&options_summary(list));
        }
    }
}

struct LayoutsUi {
    page: Page,
    exp: WeakRef<adw::ExpanderRow>,
    /// Weak: the option's apply hook owns this struct.
    opt: std::rc::Weak<Opt>,
    state: RefCell<Vec<(String, String)>>,
    rows: RefCell<Vec<WeakRef<gtk::Widget>>>,
}

impl LayoutsUi {
    fn render(self: &Rc<Self>) {
        let Some(exp) = self.exp.upgrade() else {
            return;
        };
        for r in self.rows.take() {
            if let Some(r) = r.upgrade() {
                exp.remove(&r);
            }
        }
        let pairs = self.state.borrow().clone();
        let names: Vec<String> = pairs.iter().map(|(l, v)| layout_name(l, v)).collect();
        exp.set_subtitle(&if names.is_empty() {
            "Hyprland default".into()
        } else {
            names.join(", ")
        });
        let mut rows = Vec::new();
        for (i, (layout, variant)) in pairs.iter().enumerate() {
            let variants: Vec<(&str, &str)> = CATALOG.variants_of(layout).collect();
            let mut labels = vec!["Standard"];
            labels.extend(variants.iter().map(|(_, d)| *d));
            let row = adw::ComboRow::builder()
                .use_markup(false)
                .model(&gtk::StringList::new(&labels))
                .enable_search(true)
                .expression(gtk::PropertyExpression::new(
                    gtk::StringObject::static_type(),
                    None::<gtk::Expression>,
                    "string",
                ))
                .build();
            row.set_title(CATALOG.layout_desc(layout).unwrap_or(layout));
            row.set_subtitle(&if i == 0 {
                format!("{layout} · default layout")
            } else {
                layout.clone()
            });
            let sel = variants
                .iter()
                .position(|(v, _)| v == variant)
                .map_or(0, |p| p + 1);
            row.set_selected(sel as u32);
            let variant_codes: Vec<String> =
                variants.iter().map(|(v, _)| (*v).to_owned()).collect();
            let me = Rc::downgrade(self);
            row.connect_selected_notify(move |r| {
                let Some(me) = me.upgrade() else { return };
                let v = match r.selected() {
                    0 => String::new(),
                    n => variant_codes
                        .get(n as usize - 1)
                        .cloned()
                        .unwrap_or_default(),
                };
                if let Some(p) = me.state.borrow_mut().get_mut(i) {
                    p.1 = v;
                }
                me.commit();
            });
            if i > 0 {
                let up = widgets::icon_button(
                    "go-up-symbolic",
                    "Move up (the first layout is the default)",
                );
                let me = Rc::downgrade(self);
                up.connect_clicked(move |_| {
                    let Some(me) = me.upgrade() else { return };
                    me.state.borrow_mut().swap(i, i - 1);
                    me.render();
                    me.commit();
                });
                row.add_suffix(&up);
            }
            if pairs.len() > 1 {
                let rm = widgets::icon_button("list-remove-symbolic", "Remove layout");
                let me = Rc::downgrade(self);
                rm.connect_clicked(move |_| {
                    let Some(me) = me.upgrade() else { return };
                    me.state.borrow_mut().remove(i);
                    me.render();
                    me.commit();
                });
                row.add_suffix(&rm);
            }
            exp.add_row(&row);
            rows.push(row.upcast::<gtk::Widget>().downgrade());
        }
        let add = adw::ButtonRow::builder()
            .title("Add Layout…")
            .start_icon_name("list-add-symbolic")
            .build();
        let me = Rc::downgrade(self);
        add.connect_activated(move |_| {
            if let Some(me) = me.upgrade() {
                me.pick();
            }
        });
        exp.add_row(&add);
        rows.push(add.upcast::<gtk::Widget>().downgrade());
        self.rows.replace(rows);
    }

    fn commit(&self) {
        let pairs = self.state.borrow();
        let layouts: Vec<&str> = pairs.iter().map(|(l, _)| l.as_str()).collect();
        let variants: Vec<&str> = pairs.iter().map(|(_, v)| v.as_str()).collect();
        let variants = if variants.iter().all(|v| v.is_empty()) {
            String::new()
        } else {
            variants.join(",")
        };
        if let Some(opt) = self.opt.upgrade() {
            self.page.write(
                &opt,
                Some(vec![
                    OptValue::Str(layouts.join(",")),
                    OptValue::Str(variants),
                ]),
                0,
            );
        }
    }

    fn pick(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder()
            .title("Add Layout")
            .content_width(420)
            .content_height(560)
            .build();
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search layouts")
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_margin_start(12);
        list.set_margin_end(12);
        list.set_margin_bottom(12);
        list.set_valign(gtk::Align::Start);
        let mut hay = Vec::new();
        for (code, desc) in &CATALOG.layouts {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .activatable(true)
                .build();
            row.set_title(desc);
            row.set_subtitle(code);
            list.append(&row);
            hay.push((
                row.downgrade(),
                format!("{} {}", code.to_lowercase(), desc.to_lowercase()),
            ));
            let (me, d, code) = (Rc::downgrade(self), dialog.downgrade(), code.clone());
            row.connect_activated(move |_| {
                if let Some(me) = me.upgrade()
                    && !me.state.borrow().iter().any(|(l, _)| *l == code)
                {
                    me.state.borrow_mut().push((code.clone(), String::new()));
                    me.render();
                    me.commit();
                }
                if let Some(d) = d.upgrade() {
                    d.close();
                }
            });
        }
        search.connect_search_changed(move |s| {
            let q = s.text().to_lowercase();
            for (row, h) in &hay {
                if let Some(row) = row.upgrade() {
                    row.set_visible(q.is_empty() || h.contains(&q));
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
        dialog.present(Some(&self.page.ctx.window));
        search.grab_focus();
    }
}

// ---- per-device fields ------------------------------------------------------------

/// Shared state of one device group's expander.
struct DevCtx {
    page: Page,
    label: String,
    names: Rc<Vec<String>>,
    badge: WeakRef<gtk::Label>,
    reset: WeakRef<gtk::Button>,
    /// Managed/config settings per member at load time.
    settings: HashMap<String, DeviceSettings>,
    fields: RefCell<Vec<Rc<DevField>>>,
}

/// A single per-device setting row.
struct DevField {
    names: Rc<Vec<String>>,
    key: &'static str,
    origin: gtk::Label,
    reset: Option<WeakRef<gtk::Button>>,
    guard: Guard,
    debounce: Debounce,
    /// Value in effect without an override (config rule or global), as text.
    inherited: String,
    inherited_origin: String,
    overridden: Cell<bool>,
    apply: Apply<str>,
}

impl DevField {
    fn show(&self, overridden: bool) {
        self.overridden.set(overridden);
        self.origin.set_label(if overridden {
            "hyprdeck"
        } else {
            &self.inherited_origin
        });
        set_accent(&self.origin, overridden);
        if let Some(r) = self.reset.as_ref().and_then(WeakRef::upgrade) {
            r.set_visible(overridden);
        }
    }

    /// Back to the inherited value after the override was removed.
    fn reverted(&self) {
        self.show(false);
        if let Some(apply) = self.apply.borrow().as_ref() {
            self.guard.hold(|| apply(&self.inherited));
        }
    }
}

fn opt_text(v: &OptValue) -> String {
    match v {
        OptValue::Bool(b) => b.to_string(),
        OptValue::Int(i) => i.to_string(),
        OptValue::Float(f) => f.to_string(),
        OptValue::Str(s) => s.clone(),
        OptValue::Lua { lua } => lua.clone(),
    }
}

impl DevCtx {
    /// Badge + reset button follow whether any override is left.
    fn sync(&self) {
        let any = self.fields.borrow().iter().any(|f| f.overridden.get());
        if let Some(b) = self.badge.upgrade() {
            b.set_visible(any);
        }
        if let Some(r) = self.reset.upgrade() {
            r.set_sensitive(any);
        }
    }

    /// (managed value, inherited value, inherited origin) for `key` across `names`.
    fn current(
        &self,
        st: &InputState,
        names: &[String],
        key: &str,
        global: &str,
    ) -> (Option<String>, String, String) {
        let managed = names
            .iter()
            .find_map(|n| self.settings.get(n)?.managed.get(key).map(opt_text));
        let config = names.iter().find_map(|n| {
            let (fields, src) = self.settings.get(n)?.config.as_ref()?;
            fields.get(key).map(|v| (v.display(), src.clone()))
        });
        let (inherited, origin) =
            config.unwrap_or_else(|| (st.value(global).to_owned(), "global".to_owned()));
        (managed, inherited, origin)
    }

    #[allow(clippy::too_many_arguments)]
    fn field(
        self: &Rc<Self>,
        row: &impl IsA<adw::ActionRow>,
        names: &Rc<Vec<String>>,
        key: &'static str,
        inherited: String,
        inherited_origin: String,
        overridden: bool,
        with_reset: bool,
    ) -> Rc<DevField> {
        let origin = widgets::origin_label();
        row.add_suffix(&origin);
        let reset = with_reset.then(|| {
            let b = widgets::icon_button("edit-undo-symbolic", "Remove override (inherit)");
            row.add_suffix(&b);
            b
        });
        let f = Rc::new(DevField {
            names: names.clone(),
            key,
            origin,
            reset: reset.as_ref().map(|b| b.downgrade()),
            guard: Guard::default(),
            debounce: Debounce::default(),
            inherited,
            inherited_origin,
            overridden: Cell::new(false),
            apply: RefCell::new(None),
        });
        f.show(overridden);
        if let Some(b) = reset {
            let (d, f2) = (Rc::downgrade(self), Rc::downgrade(&f));
            b.connect_clicked(move |_| {
                if let (Some(d), Some(f)) = (d.upgrade(), f2.upgrade()) {
                    d.write(&f, None, 0);
                }
            });
        }
        self.fields.borrow_mut().push(f.clone());
        f
    }

    fn write(self: &Rc<Self>, f: &Rc<DevField>, value: Option<OptValue>, delay: u64) {
        let (d, f) = (self.clone(), f.clone());
        f.clone().debounce.run(delay, move || {
            let names = f.names.to_vec();
            let key = f.key;
            let set = value.is_some();
            glib::spawn_future_local(async move {
                match rt::blocking(move || inputs::set_device(&names, key, value)).await {
                    Ok(errors) => {
                        if set {
                            f.show(true)
                        } else {
                            f.reverted()
                        }
                        d.sync();
                        if !errors.is_empty() {
                            widgets::report_errors(&d.page.ctx, "", &errors);
                        } else if !set {
                            d.page.ctx.toast("Override removed");
                        }
                    }
                    Err(e) => d.page.ctx.error("Could not apply device setting", &e),
                }
            });
        });
    }

    fn reset_all(self: &Rc<Self>) {
        let d = self.clone();
        if let Some(r) = self.reset.upgrade() {
            r.set_sensitive(false);
        }
        glib::spawn_future_local(async move {
            let names = d.names.to_vec();
            match rt::blocking(move || inputs::reset_devices(&names)).await {
                Ok(errors) => {
                    for f in d.fields.borrow().iter() {
                        f.reverted();
                    }
                    widgets::report_errors(
                        &d.page.ctx,
                        &format!("Removed overrides for {}", d.label),
                        &errors,
                    );
                }
                Err(e) => d.page.ctx.error("Could not reset device", &e),
            }
            d.sync();
        });
    }

    fn enabled_row(self: &Rc<Self>, exp: &adw::ExpanderRow, names: Rc<Vec<String>>) {
        let managed = names
            .iter()
            .find_map(|n| self.settings.get(n)?.managed.get("enabled").cloned());
        let config = names.iter().find_map(|n| {
            self.settings
                .get(n)?
                .config
                .as_ref()?
                .0
                .get("enabled")
                .and_then(Value::as_bool)
        });
        let inherited = config.unwrap_or(true);
        let on = match &managed {
            Some(OptValue::Bool(b)) => *b,
            _ => inherited,
        };
        let row = adw::SwitchRow::builder()
            .title("Enabled")
            .subtitle("Turn this device off entirely")
            .active(on)
            .build();
        let origin = if config.is_some() {
            "config"
        } else {
            "default"
        };
        let f = self.field(
            &row,
            &names,
            "enabled",
            inherited.to_string(),
            origin.into(),
            managed.is_some(),
            true,
        );
        let r = row.downgrade();
        f.apply.replace(Some(Box::new(move |v| {
            if let Some(r) = r.upgrade() {
                r.set_active(parse_bool(v));
            }
        })));
        let (d, fw) = (Rc::downgrade(self), Rc::downgrade(&f));
        row.connect_active_notify(move |r| {
            let (Some(d), Some(f)) = (d.upgrade(), fw.upgrade()) else { return };
            if f.guard.active() {
                return;
            }
            if r.is_active() {
                d.write(&f, if inherited { None } else { Some(OptValue::Bool(true)) }, 0);
                return;
            }
            let r = r.clone();
            glib::spawn_future_local(async move {
                let ok = d
                    .page
                    .ctx
                    .confirm(
                        &format!("Disable {}?", d.label),
                        "The device stops sending input until you turn it back on here. Keep the keyboard and mouse you need to undo this enabled.",
                        "Disable",
                        true,
                    )
                    .await;
                if ok {
                    d.write(&f, Some(OptValue::Bool(false)), 0);
                } else {
                    f.guard.hold(|| r.set_active(true));
                }
            });
        });
        exp.add_row(&row);
    }

    #[allow(clippy::too_many_arguments)]
    fn spin(
        self: &Rc<Self>,
        exp: &adw::ExpanderRow,
        st: &InputState,
        names: &Rc<Vec<String>>,
        key: &'static str,
        global: &str,
        title: &str,
        range: (f64, f64),
        step: f64,
        digits: u32,
        float: bool,
    ) {
        let (managed, inherited, origin) = self.current(st, names, key, global);
        let row = adw::SpinRow::with_range(range.0, range.1, step);
        row.set_use_markup(false);
        row.set_title(title);
        row.set_digits(digits);
        row.set_value(
            managed
                .as_deref()
                .unwrap_or(&inherited)
                .parse()
                .unwrap_or(0.0),
        );
        let f = self.field(&row, names, key, inherited, origin, managed.is_some(), true);
        let r = row.downgrade();
        f.apply.replace(Some(Box::new(move |v| {
            if let Some(r) = r.upgrade() {
                r.set_value(v.parse().unwrap_or(0.0));
            }
        })));
        let (d, fw) = (Rc::downgrade(self), Rc::downgrade(&f));
        row.connect_value_notify(move |r| {
            let (Some(d), Some(f)) = (d.upgrade(), fw.upgrade()) else {
                return;
            };
            if f.guard.active() {
                return;
            }
            let v = if float {
                OptValue::Float(r.value())
            } else {
                OptValue::Int(r.value().round() as i64)
            };
            d.write(&f, Some(v), 600);
        });
        exp.add_row(&row);
    }

    /// Inherit / On / Off.
    fn tri(
        self: &Rc<Self>,
        exp: &adw::ExpanderRow,
        st: &InputState,
        names: &Rc<Vec<String>>,
        key: &'static str,
        global: &str,
        title: &str,
    ) {
        let (managed, inherited, origin) = self.current(st, names, key, global);
        let inherit = format!(
            "Inherit ({})",
            if parse_bool(&inherited) { "on" } else { "off" }
        );
        let row = adw::ComboRow::builder()
            .use_markup(false)
            .model(&gtk::StringList::new(&[inherit.as_str(), "On", "Off"]))
            .build();
        row.set_title(title);
        row.set_selected(match managed.as_deref() {
            None => 0,
            Some(v) if parse_bool(v) => 1,
            Some(_) => 2,
        });
        self.combo_field(
            exp,
            row,
            names,
            key,
            inherited,
            origin,
            managed.is_some(),
            |i| match i {
                1 => Some(OptValue::Bool(true)),
                2 => Some(OptValue::Bool(false)),
                _ => None,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn choice(
        self: &Rc<Self>,
        exp: &adw::ExpanderRow,
        st: &InputState,
        names: &Rc<Vec<String>>,
        key: &'static str,
        global: &str,
        title: &str,
        choices: &'static [(&'static str, &'static str)],
    ) {
        let (managed, inherited, origin) = self.current(st, names, key, global);
        let inherit = format!(
            "Inherit ({})",
            if inherited.is_empty() {
                "device default"
            } else {
                inherited.as_str()
            }
        );
        let mut labels = vec![inherit.as_str()];
        labels.extend(choices.iter().map(|(_, l)| *l));
        let row = adw::ComboRow::builder()
            .use_markup(false)
            .model(&gtk::StringList::new(&labels))
            .build();
        row.set_title(title);
        let sel = managed
            .as_deref()
            .and_then(|m| choices.iter().position(|(v, _)| *v == m))
            .map_or(0, |p| p + 1);
        row.set_selected(sel as u32);
        self.combo_field(
            exp,
            row,
            names,
            key,
            inherited,
            origin,
            managed.is_some(),
            move |i| {
                i.checked_sub(1)
                    .and_then(|i| choices.get(i as usize))
                    .map(|(v, _)| OptValue::Str((*v).to_owned()))
            },
        );
    }

    /// Combo rows where index 0 means "inherit" (no override).
    #[allow(clippy::too_many_arguments)]
    fn combo_field(
        self: &Rc<Self>,
        exp: &adw::ExpanderRow,
        row: adw::ComboRow,
        names: &Rc<Vec<String>>,
        key: &'static str,
        inherited: String,
        origin: String,
        overridden: bool,
        value_at: impl Fn(u32) -> Option<OptValue> + 'static,
    ) {
        let f = self.field(&row, names, key, inherited, origin, overridden, false);
        let r = row.downgrade();
        f.apply.replace(Some(Box::new(move |_| {
            if let Some(r) = r.upgrade() {
                r.set_selected(0);
            }
        })));
        let (d, fw) = (Rc::downgrade(self), Rc::downgrade(&f));
        row.connect_selected_notify(move |r| {
            let (Some(d), Some(f)) = (d.upgrade(), fw.upgrade()) else {
                return;
            };
            if !f.guard.active() {
                d.write(&f, value_at(r.selected()), 0);
            }
        });
        exp.add_row(&row);
    }

    fn entry(
        self: &Rc<Self>,
        exp: &adw::ExpanderRow,
        st: &InputState,
        names: &Rc<Vec<String>>,
        key: &'static str,
        global: &str,
        title: &str,
    ) {
        let (managed, inherited, origin) = self.current(st, names, key, global);
        let row = adw::EntryRow::builder()
            .use_markup(false)
            .show_apply_button(true)
            .build();
        row.set_title(title);
        row.set_text(managed.as_deref().unwrap_or(&inherited));
        let origin_label = widgets::origin_label();
        let reset = widgets::icon_button("edit-undo-symbolic", "Remove override (inherit)");
        row.add_suffix(&origin_label);
        row.add_suffix(&reset);
        let f = Rc::new(DevField {
            names: names.clone(),
            key,
            origin: origin_label,
            reset: Some(reset.downgrade()),
            guard: Guard::default(),
            debounce: Debounce::default(),
            inherited,
            inherited_origin: origin,
            overridden: Cell::new(false),
            apply: RefCell::new(None),
        });
        f.show(managed.is_some());
        self.fields.borrow_mut().push(f.clone());
        let (d, fw) = (Rc::downgrade(self), Rc::downgrade(&f));
        reset.connect_clicked(move |_| {
            if let (Some(d), Some(f)) = (d.upgrade(), fw.upgrade()) {
                d.write(&f, None, 0);
            }
        });
        let r = row.downgrade();
        f.apply.replace(Some(Box::new(move |v| {
            if let Some(r) = r.upgrade() {
                r.set_text(v);
            }
        })));
        let (d, fw) = (Rc::downgrade(self), Rc::downgrade(&f));
        row.connect_apply(move |r| {
            if let (Some(d), Some(f)) = (d.upgrade(), fw.upgrade()) {
                d.write(&f, Some(OptValue::Str(r.text().trim().to_owned())), 0);
            }
        });
        exp.add_row(&row);
    }
}
