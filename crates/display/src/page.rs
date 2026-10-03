//! The "Displays" page: arrangement, per-output settings staged as drafts and
//! applied with a revert countdown, DDC/CI controls and global render options.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{Result, anyhow};
use gtk::glib;
use hyprdeck_core::hypr::ctl;
use hyprdeck_core::hypr::managed::{self, MonitorRule, OptValue};
use hyprdeck_core::hypr::model::{self, Value};
use hyprdeck_core::hypr::schema::{self, OptKind};
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::logic::{self, Mode, Rect, ScaleCheck};
use crate::rules::{self, Restore};
use crate::{arrange, hardware};

const REVERT_SECONDS: u32 = 15;

/// `(value, label)` for monitor `vrr` and global `misc.vrr`.
pub(crate) const VRR_CHOICES: [(i64, &str); 4] = [
    (0, "Off"),
    (1, "On"),
    (2, "Fullscreen only"),
    (3, "Fullscreen video & games"),
];

/// Color-management presets accepted by Hyprland 0.56 (`NCMType::fromString`).
const CM_PRESETS: [(&str, &str); 9] = [
    ("auto", "Auto — sRGB at 8-bit, wide gamut at 10-bit"),
    ("srgb", "sRGB — standard primaries (default)"),
    ("dcip3", "DCI-P3 primaries"),
    ("dp3", "Display P3 (Apple) primaries"),
    ("adobe", "Adobe RGB primaries"),
    ("wide", "Wide gamut — BT.2020 primaries"),
    ("edid", "EDID primaries (often inaccurate)"),
    ("hdr", "HDR — BT.2020 + PQ (experimental)"),
    ("hdredid", "HDR with EDID primaries (experimental)"),
];

/// SDR transfer functions accepted by `NTransferFunction::fromString`.
const SDR_EOTF: [(&str, &str); 5] = [
    ("default", "Default — follow render:cm_sdr_eotf"),
    ("auto", "Auto"),
    ("srgb", "sRGB piecewise"),
    ("gamma22", "Gamma 2.2"),
    ("gamma22force", "Gamma 2.2, forced"),
];

/// `supports_hdr` / `supports_wide_color` (-1..1).
const SUPPORT_CHOICES: [(i64, &str); 3] =
    [(0, "Auto (from EDID)"), (1, "Force on"), (-1, "Force off")];

struct GlobalDef {
    key: &'static str,
    title: &'static str,
    subtitle: &'static str,
    /// Empty = boolean switch.
    choices: &'static [(i64, &'static str)],
}

const GLOBALS: [GlobalDef; 5] = [
    GlobalDef {
        key: "misc.vrr",
        title: "Adaptive sync (VRR) default",
        subtitle: "Used by every display set to “Use global setting”",
        choices: &VRR_CHOICES,
    },
    GlobalDef {
        key: "cursor.no_hardware_cursors",
        title: "Cursor rendering",
        subtitle: "NVIDIA: software cursors avoid flickering or invisible cursors",
        choices: &[
            (2, "Auto"),
            (0, "Hardware cursors"),
            (1, "Software cursors"),
        ],
    },
    GlobalDef {
        key: "render.direct_scanout",
        title: "Direct scanout",
        subtitle: "Send fullscreen windows straight to the display; lower latency, may glitch on NVIDIA",
        choices: &[(0, "Off"), (1, "On"), (2, "Auto (games only)")],
    },
    GlobalDef {
        key: "general.allow_tearing",
        title: "Allow tearing",
        subtitle: "Windows with the immediate rule may tear for the lowest input latency",
        choices: &[],
    },
    GlobalDef {
        key: "debug.vfr",
        title: "Variable frame rate",
        subtitle: "Render only when something changes; Hyprland advises keeping this on",
        choices: &[],
    },
];

/// A hand-written `hl.monitor` rule that applies to an output.
pub(crate) struct ConfigRule {
    pub source: String,
    pub summary: String,
    pub catch_all: bool,
    pub rule: MonitorRule,
}

struct GlobalState {
    def: &'static GlobalDef,
    kind: OptKind,
    live: Option<i64>,
    managed: Option<OptValue>,
    /// `(source, value)` of the last hand-written assignment.
    config: Option<(String, String)>,
}

impl GlobalState {
    fn managed_i64(&self) -> Option<i64> {
        match self.managed.as_ref()? {
            OptValue::Bool(b) => Some(i64::from(*b)),
            OptValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    fn value_label(&self, v: i64) -> String {
        if self.def.choices.is_empty() {
            return if v != 0 { "on".into() } else { "off".into() };
        }
        self.def
            .choices
            .iter()
            .find(|c| c.0 == v)
            .map_or_else(|| v.to_string(), |c| c.1.to_owned())
    }

    fn provenance(&self) -> String {
        match (&self.managed, &self.config) {
            (Some(_), Some((src, v))) => format!("Set by hyprdeck · overrides {src} ({v})"),
            (Some(_), None) => "Set by hyprdeck".into(),
            (None, Some((src, _))) => format!("From {src}"),
            (None, None) => "Hyprland default".into(),
        }
    }
}

struct Snapshot {
    monitors: Vec<ctl::Monitor>,
    managed: managed::Managed,
    config: Vec<Option<ConfigRule>>,
    globals: Vec<GlobalState>,
}

fn load_snapshot() -> Result<Snapshot> {
    let monitors = ctl::monitors()?;
    let managed = managed::load()?;
    let model = model::load();
    let ours = managed::lua_path();
    let config = monitors
        .iter()
        .map(|m| config_rule(&model, &ours, m))
        .collect();
    let globals = GLOBALS
        .iter()
        .filter_map(|def| {
            let kind = schema::kind(def.key)?;
            let config = model
                .options
                .iter()
                .filter(|o| o.key == def.key && o.source.file != ours)
                .max_by_key(|o| o.seq)
                .map(|o| (o.source.display(), o.value.display()));
            Some(GlobalState {
                def,
                kind,
                live: getoption(def.key),
                managed: managed.options.get(def.key).cloned(),
                config,
            })
        })
        .collect();
    Ok(Snapshot {
        monitors,
        managed,
        config,
        globals,
    })
}

fn getoption(key: &str) -> Option<i64> {
    let v: serde_json::Value = ctl::query(&["getoption", key]).ok()?;
    v.get("int")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| v.get("bool")?.as_bool().map(i64::from))
}

fn config_rule(model: &model::ConfigModel, ours: &Path, mon: &ctl::Monitor) -> Option<ConfigRule> {
    let hand = || model.monitors.iter().filter(|s| s.source.file != ours);
    let exact = |o: &str| {
        o == mon.name
            || o.strip_prefix("desc:")
                .is_some_and(|d| !d.trim().is_empty() && mon.description.starts_with(d.trim()))
    };
    let (spec, catch_all) = hand()
        .filter(|s| s.str("output").is_some_and(exact))
        .max_by_key(|s| s.seq)
        .map(|s| (s, false))
        .or_else(|| {
            hand()
                .filter(|s| s.str("output") == Some(""))
                .max_by_key(|s| s.seq)
                .map(|s| (s, true))
        })?;
    let summary = spec
        .fields
        .iter()
        .filter(|(k, _)| k.as_str() != "output")
        .map(|(k, v)| format!("{k} = {}", v.display()))
        .collect::<Vec<_>>()
        .join(", ");
    Some(ConfigRule {
        source: spec.source.display(),
        summary,
        catch_all,
        rule: rule_from_spec(&mon.name, &spec.fields),
    })
}

/// Fields of a hand-written spec as a rule for `output`. `scale = "auto"`
/// and other non-numeric values are left unset.
fn rule_from_spec(output: &str, f: &std::collections::BTreeMap<String, Value>) -> MonitorRule {
    let s = |k: &str| f.get(k).map(Value::display);
    let fl = |k: &str| f.get(k).and_then(Value::as_f64);
    let i = |k: &str| f.get(k).and_then(Value::as_i64);
    MonitorRule {
        output: output.to_owned(),
        disabled: f.get("disabled").and_then(Value::as_bool),
        mode: s("mode"),
        position: s("position"),
        scale: fl("scale"),
        transform: i("transform"),
        vrr: i("vrr"),
        cm: s("cm"),
        bitdepth: i("bitdepth"),
        sdrbrightness: fl("sdrbrightness"),
        sdrsaturation: fl("sdrsaturation"),
        sdr_min_luminance: fl("sdr_min_luminance"),
        sdr_max_luminance: i("sdr_max_luminance"),
        sdr_eotf: s("sdr_eotf"),
        min_luminance: fl("min_luminance"),
        max_luminance: i("max_luminance"),
        max_avg_luminance: i("max_avg_luminance"),
        supports_hdr: i("supports_hdr"),
        supports_wide_color: i("supports_wide_color"),
        icc: s("icc").filter(|p| !p.is_empty()),
        mirror: s("mirror").filter(|m| !m.is_empty()),
    }
}

/// One output: live state plus the staged rule being edited.
pub(crate) struct Out {
    pub live: ctl::Monitor,
    pub modes: Rc<Vec<Mode>>,
    pub managed: Option<MonitorRule>,
    pub config: Option<ConfigRule>,
    /// Rule the page started from (managed rule, or one seeded from config + live state).
    pub baseline: MonitorRule,
    pub draft: MonitorRule,
}

impl Out {
    fn new(live: ctl::Monitor, managed: Option<MonitorRule>, config: Option<ConfigRule>) -> Out {
        let modes: Vec<Mode> = live
            .available_modes
            .iter()
            .filter_map(|s| Mode::parse(s))
            .collect();
        let baseline = managed
            .clone()
            .unwrap_or_else(|| seed(&live, config.as_ref()));
        Out {
            modes: Rc::new(modes),
            draft: baseline.clone(),
            baseline,
            live,
            managed,
            config,
        }
    }

    pub fn name(&self) -> &str {
        &self.live.name
    }

    fn live_mode(&self) -> Mode {
        if self.live.width > 0 && self.live.height > 0 {
            Mode {
                width: self.live.width,
                height: self.live.height,
                refresh: self.live.refresh_rate,
            }
        } else {
            self.modes.first().copied().unwrap_or(Mode {
                width: 1920,
                height: 1080,
                refresh: 60.0,
            })
        }
    }

    pub fn mode(&self) -> Mode {
        self.draft
            .mode
            .as_deref()
            .and_then(Mode::parse)
            .filter(|m| m.refresh > 0.0)
            .unwrap_or_else(|| self.live_mode())
    }

    pub fn scale(&self) -> f64 {
        self.draft
            .scale
            .filter(|s| *s > 0.0)
            .unwrap_or(self.live.scale)
    }

    /// Scale Hyprland will actually use for the staged mode.
    pub fn effective_scale(&self) -> f64 {
        let m = self.mode();
        logic::snap_scale(m.width, m.height, self.scale()).unwrap_or(self.scale())
    }

    pub fn transform(&self) -> i64 {
        self.draft.transform.unwrap_or(self.live.transform)
    }

    pub fn enabled(&self) -> bool {
        !self.draft.disabled.unwrap_or(self.live.disabled)
    }

    pub fn mirror(&self) -> Option<&str> {
        self.draft
            .mirror
            .as_deref()
            .filter(|m| !m.is_empty() && *m != "none")
    }

    pub fn position(&self) -> (i64, i64) {
        self.draft
            .position
            .as_deref()
            .and_then(parse_position)
            .unwrap_or((self.live.x, self.live.y))
    }

    /// Layout rectangle for the arrangement view.
    pub fn rect(&self) -> Rect {
        let m = self.mode();
        let (w, h) =
            logic::logical_size(m.width, m.height, self.effective_scale(), self.transform());
        let (x, y) = self.position();
        Rect {
            x: x as f64,
            y: y as f64,
            w,
            h,
        }
    }

    /// Shown in the arrangement (enabled and not mirroring another output).
    pub fn placed(&self) -> bool {
        self.enabled() && self.mirror().is_none()
    }

    fn dirty(&self) -> bool {
        self.draft != self.baseline
    }

    fn cm(&self) -> String {
        match &self.draft.cm {
            Some(c) => c.clone(),
            None if self.live.color_management_preset.is_empty() => "srgb".into(),
            None => self.live.color_management_preset.clone(),
        }
    }

    fn live_bitdepth(&self) -> i64 {
        if self.live.current_format.contains("2101010") {
            10
        } else {
            8
        }
    }

    /// The rule to write: the draft with the shown mode/position/scale made
    /// explicit and the scale snapped to a value Hyprland accepts unchanged.
    fn rule_to_apply(&self) -> Result<MonitorRule> {
        let mut r = self.draft.clone();
        if self.enabled() {
            let m = self.mode();
            if r.mode.is_none() {
                r.mode = Some(m.to_rule());
            }
            if r.position.is_none() {
                let (x, y) = self.position();
                r.position = Some(format!("{x}x{y}"));
            }
            let s = self.scale();
            let snapped = logic::snap_scale(m.width, m.height, s).ok_or_else(|| {
                anyhow!(
                    "scale {} has no clean divisor for {}×{}",
                    logic::fmt_scale(s),
                    m.width,
                    m.height
                )
            })?;
            r.scale = Some(snapped);
        }
        Ok(r)
    }
}

fn parse_position(s: &str) -> Option<(i64, i64)> {
    let (x, y) = s.split_once('x')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// A rule reproducing the current state: the hand-written rule's settings with
/// the live mode/position/scale/transform (which resolve `preferred`/`auto`).
fn seed(live: &ctl::Monitor, config: Option<&ConfigRule>) -> MonitorRule {
    let mut r = config.map(|c| c.rule.clone()).unwrap_or_default();
    r.output = live.name.clone();
    if live.disabled {
        r.disabled = Some(true);
        return r;
    }
    r.disabled = None;
    r.mode = Some(
        Mode {
            width: live.width,
            height: live.height,
            refresh: live.refresh_rate,
        }
        .to_rule(),
    );
    r.position = Some(format!("{}x{}", live.x, live.y));
    r.scale = Some(live.scale);
    r.transform = (live.transform != 0 || r.transform.is_some()).then_some(live.transform);
    r.mirror =
        (live.mirror_of != "none" && !live.mirror_of.is_empty()).then(|| live.mirror_of.clone());
    r
}

pub(crate) struct Page {
    pub ctx: Ctx,
    pub outs: RefCell<Vec<Out>>,
    globals: RefCell<Vec<GlobalState>>,
    /// Set while widgets are updated programmatically, so their signals don't edit drafts.
    pub syncing: Cell<bool>,
    busy: Cell<bool>,
    pub area: gtk::DrawingArea,
    arrange_hint: gtk::Label,
    spinner: adw::Spinner,
    monitors_box: gtk::Box,
    render_box: gtk::Box,
    vrr_lists: RefCell<Vec<gtk::StringList>>,
    /// DDC/CI groups by output, kept across rebuilds (probing takes seconds).
    pub ddc_groups: RefCell<Vec<(String, adw::PreferencesGroup)>>,
    /// Why monitor hardware controls are missing (i2c module/permissions).
    pub ddc_hint: gtk::Label,
    action_bar: gtk::ActionBar,
    pending: gtk::Label,
    apply_btn: gtk::Button,
    discard_btn: gtk::Button,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = ui::page_scaffold();

    let area = gtk::DrawingArea::builder()
        .content_height(240)
        .hexpand(true)
        .build();
    let frame = gtk::Frame::builder()
        .child(&area)
        .css_classes(["view"])
        .build();
    let arrange_hint = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Reload display state")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let spinner = adw::Spinner::builder()
        .visible(false)
        .valign(gtk::Align::Center)
        .build();
    let suffix = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    suffix.append(&spinner);
    suffix.append(&refresh);
    let arrange_group = adw::PreferencesGroup::builder()
        .title("Arrangement")
        .header_suffix(&suffix)
        .build();
    let arrange_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    arrange_box.append(&frame);
    arrange_box.append(&arrange_hint);
    arrange_group.add(&arrange_box);

    let monitors_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
    let ddc_hint = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .visible(false)
        .css_classes(["dim-label", "caption"])
        .build();
    let render_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&arrange_group);
    content.append(&monitors_box);
    content.append(&ddc_hint);
    content.append(&render_box);

    let pending = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let discard_btn = gtk::Button::builder().label("Discard").build();
    let apply_btn = gtk::Button::builder()
        .label("Apply…")
        .css_classes(["suggested-action"])
        .build();
    let action_bar = gtk::ActionBar::new();
    action_bar.pack_start(&pending);
    action_bar.pack_end(&apply_btn);
    action_bar.pack_end(&discard_btn);
    action_bar.set_revealed(false);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&scroller);
    root.append(&action_bar);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        outs: RefCell::default(),
        globals: RefCell::default(),
        syncing: Cell::new(false),
        busy: Cell::new(false),
        area,
        arrange_hint,
        spinner,
        monitors_box,
        render_box,
        vrr_lists: RefCell::default(),
        ddc_groups: RefCell::default(),
        ddc_hint,
        action_bar,
        pending,
        apply_btn,
        discard_btn,
    });
    // The root widget owns the page state for its lifetime; signal handlers hold weak refs.
    root.connect_destroy({
        let page = page.clone();
        move |_| drop(page.clone())
    });
    arrange::setup(&page);

    refresh.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                if page.dirty() {
                    page.ctx
                        .toast("Apply or discard your changes before reloading");
                } else {
                    page.reload();
                }
            }
        }
    });
    page.apply_btn.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.apply_drafts();
            }
        }
    });
    page.discard_btn.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                for o in page.outs.borrow_mut().iter_mut() {
                    o.draft = o.baseline.clone();
                }
                page.rebuild();
            }
        }
    });
    ui::on_shown(&root, {
        let page = Rc::downgrade(&page);
        move || {
            if let Some(page) = page.upgrade()
                && !page.dirty()
                && !page.busy.get()
            {
                page.reload();
            }
        }
    });
    root.upcast()
}

impl Page {
    fn dirty(&self) -> bool {
        self.outs.borrow().iter().any(Out::dirty)
    }

    /// Edit the draft of output `idx` in response to user input.
    pub fn edit(&self, idx: usize, f: impl FnOnce(&mut MonitorRule)) {
        if self.syncing.get() {
            return;
        }
        if let Some(o) = self.outs.borrow_mut().get_mut(idx) {
            f(&mut o.draft);
        }
        self.changed();
    }

    /// Refresh the action bar and arrangement after drafts changed.
    pub fn changed(&self) {
        let outs = self.outs.borrow();
        let names: Vec<&str> = outs.iter().filter(|o| o.dirty()).map(Out::name).collect();
        self.action_bar.set_revealed(!names.is_empty());
        self.pending
            .set_label(&format!("Unapplied changes: {}", names.join(", ")));
        let placed = outs.iter().filter(|o| o.placed()).count();
        self.arrange_hint.set_label(match placed {
            0 => "No active displays.",
            1 => "One active display; there is nothing to arrange.",
            _ => "Drag displays to arrange them. Edges snap together; overlapping positions are refused.",
        });
        self.area.queue_draw();
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.spinner.set_visible(busy);
        self.apply_btn.set_sensitive(!busy);
        self.discard_btn.set_sensitive(!busy);
        self.monitors_box.set_sensitive(!busy);
        self.render_box.set_sensitive(!busy);
    }

    /// Re-read live state, managed rules and the config model, discarding drafts.
    pub fn reload(self: &Rc<Self>) {
        let page = self.clone();
        self.set_busy(true);
        glib::spawn_future_local(async move {
            let snap = rt::blocking(load_snapshot).await;
            page.set_busy(false);
            match snap {
                Ok(snap) => {
                    let mut config = snap.config.into_iter();
                    *page.outs.borrow_mut() = snap
                        .monitors
                        .into_iter()
                        .map(|m| {
                            let managed = snap
                                .managed
                                .monitors
                                .iter()
                                .find(|r| r.output == m.name)
                                .cloned();
                            Out::new(m, managed, config.next().flatten())
                        })
                        .collect();
                    *page.globals.borrow_mut() = snap.globals;
                    page.rebuild();
                    page.rebuild_globals();
                    hardware::load(&page);
                }
                Err(e) => {
                    page.ctx.error("Reading display state failed", &e);
                    ui::clear(&page.monitors_box);
                    page.monitors_box.append(
                        &adw::StatusPage::builder()
                            .icon_name("video-display-symbolic")
                            .title("Display state unavailable")
                            .description(glib::markup_escape_text(&format!("{e:#}")).as_str())
                            .build(),
                    );
                }
            }
        });
    }

    /// Rebuild the per-output groups from the current drafts.
    fn rebuild(self: &Rc<Self>) {
        ui::clear(&self.monitors_box);
        self.vrr_lists.borrow_mut().clear();
        let n = self.outs.borrow().len();
        for idx in 0..n {
            self.monitors_box.append(&self.monitor_group(idx));
            let slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let name = self.outs.borrow()[idx].name().to_owned();
            if let Some((_, g)) = self.ddc_groups.borrow().iter().find(|(o, _)| *o == name) {
                if let Some(old) = g.parent().and_downcast::<gtk::Box>() {
                    old.remove(g);
                }
                slot.append(g);
            }
            slot.set_widget_name(&name);
            self.monitors_box.append(&slot);
        }
        self.changed();
    }

    /// DDC slots (one per output) in the monitors box.
    pub fn ddc_slots(&self) -> Vec<(String, gtk::Box)> {
        let mut out = Vec::new();
        let mut child = self.monitors_box.first_child();
        while let Some(w) = child {
            child = w.next_sibling();
            if let Ok(b) = w.downcast::<gtk::Box>() {
                out.push((b.widget_name().to_string(), b));
            }
        }
        out
    }

    fn global_vrr_label(&self) -> String {
        let globals = self.globals.borrow();
        match globals
            .iter()
            .find(|g| g.def.key == "misc.vrr")
            .and_then(|g| g.live.map(|v| g.value_label(v)))
        {
            Some(v) => format!("Use global setting ({v})"),
            None => "Use global setting".into(),
        }
    }

    fn monitor_group(self: &Rc<Self>, idx: usize) -> adw::PreferencesGroup {
        let outs = self.outs.borrow();
        let o = &outs[idx];
        let live = &o.live;
        let lm = o.live_mode();
        let others: Vec<String> = outs
            .iter()
            .filter(|x| x.name() != o.name())
            .map(|x| x.live.name.clone())
            .collect();

        let reset = gtk::Button::builder()
            .label("Reset to Config")
            .tooltip_text("Remove hyprdeck's rule for this display and use the hand-written config")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .sensitive(o.managed.is_some())
            .build();
        let now = if live.disabled {
            "disabled".to_owned()
        } else {
            format!(
                "now {}×{} @ {:.2} Hz, scale {}, at {},{}",
                lm.width,
                lm.height,
                lm.refresh,
                logic::fmt_scale(live.scale),
                live.x,
                live.y
            )
        };
        let group = adw::PreferencesGroup::builder()
            .title(
                glib::markup_escape_text(format!("{} {}", live.make, live.model).trim()).as_str(),
            )
            .description(glib::markup_escape_text(&format!("{} · {now}", live.name)).as_str())
            .header_suffix(&reset)
            .build();
        {
            let page = Rc::downgrade(self);
            let output = o.name().to_owned();
            reset.connect_clicked(move |_| {
                let Some(page) = page.upgrade() else { return };
                let output = output.clone();
                glib::spawn_future_local(async move {
                    let body = format!(
                        "hyprdeck's rule for {output} is removed and the hand-written config applies again. You can keep or revert the result."
                    );
                    if page.ctx.confirm("Reset display to config default?", &body, "Reset", false).await {
                        page.apply_changes(vec![(output, None)]).await;
                    }
                });
            });
        }

        // Provenance.
        let source = match (&o.managed, &o.config) {
            (Some(_), Some(c)) => format!(
                "hyprdeck (hyprdeck.lua) — overrides {}: {}",
                c.source, c.summary
            ),
            (Some(_), None) => "hyprdeck (hyprdeck.lua)".to_owned(),
            (None, Some(c)) if c.catch_all => {
                format!("{} (rule for all displays): {}", c.source, c.summary)
            }
            (None, Some(c)) => format!("{}: {}", c.source, c.summary),
            (None, None) => "No rule — Hyprland defaults".to_owned(),
        };
        group.add(
            &adw::ActionRow::builder()
                .title("Configured by")
                .subtitle(glib::markup_escape_text(&source).as_str())
                .subtitle_selectable(true)
                .css_classes(["property"])
                .build(),
        );

        let mut dependent: Vec<gtk::Widget> = Vec::new();

        // Enabled.
        let enabled = adw::SwitchRow::builder()
            .title("Enabled")
            .subtitle("Turn this output on or off")
            .active(o.enabled())
            .build();
        group.add(&enabled);

        // Resolution / refresh.
        let cur = o.mode();
        let modes = o.modes.clone();
        let resolutions = Rc::new(logic::resolutions(&modes));
        let res_labels: Vec<String> = resolutions
            .iter()
            .map(|(w, h)| format!("{w} × {h}"))
            .collect();
        let res_row = adw::ComboRow::builder()
            .title("Resolution")
            .subtitle(format!("{} modes available", modes.len()))
            .model(&string_list(&res_labels))
            .build();
        if let Some(i) = resolutions
            .iter()
            .position(|&r| r == (cur.width, cur.height))
        {
            res_row.set_selected(i as u32);
        }
        let rates = Rc::new(RefCell::new(logic::rates(&modes, cur.width, cur.height)));
        let rate_list = string_list(&rate_labels(&rates.borrow()));
        let rate_row = adw::ComboRow::builder()
            .title("Refresh rate")
            .model(&rate_list)
            .build();
        if let Some(i) = rates
            .borrow()
            .iter()
            .position(|&r| logic::same_rate(r, cur.refresh))
        {
            rate_row.set_selected(i as u32);
        }
        group.add(&res_row);
        group.add(&rate_row);
        dependent.push(res_row.clone().upcast());
        dependent.push(rate_row.clone().upcast());

        // Scale.
        let scale_row = adw::SpinRow::builder()
            .title("Scale")
            .adjustment(&gtk::Adjustment::new(o.scale(), 0.5, 4.0, 0.05, 0.25, 0.0))
            .digits(2)
            .build();
        let sugg_row = adw::ActionRow::builder()
            .title("Suggested scales")
            .subtitle("Values that divide the resolution into whole pixels")
            .build();
        let sugg_box = gtk::Box::builder()
            .css_classes(["linked"])
            .valign(gtk::Align::Center)
            .build();
        sugg_row.add_suffix(&sugg_box);
        group.add(&scale_row);
        group.add(&sugg_row);
        dependent.push(scale_row.clone().upcast());
        dependent.push(sugg_row.clone().upcast());
        let scale_feedback: Rc<dyn Fn()> = {
            let page = Rc::downgrade(self);
            let scale_row = scale_row.downgrade();
            let sugg_box = sugg_box.downgrade();
            Rc::new(move || {
                let (Some(page), Some(scale_row), Some(sugg_box)) =
                    (page.upgrade(), scale_row.upgrade(), sugg_box.upgrade())
                else {
                    return;
                };
                let (m, s, t) = {
                    let outs = page.outs.borrow();
                    let o = &outs[idx];
                    (o.mode(), o.scale(), o.transform())
                };
                let logical = |s: f64| {
                    let (w, h) = logic::logical_size(m.width, m.height, s, t);
                    format!("{w}×{h} logical")
                };
                let (text, warn) = match logic::check_scale(m.width, m.height, s) {
                    ScaleCheck::Exact => (logical(s), false),
                    ScaleCheck::Rounded(v) => (
                        format!("Hyprland uses {} · {}", logic::fmt_scale(v), logical(v)),
                        false,
                    ),
                    ScaleCheck::Adjusted(v) => (
                        format!(
                            "Not a whole-pixel divisor of {}×{}; Apply snaps it to {} ({})",
                            m.width,
                            m.height,
                            logic::fmt_scale(v),
                            logical(v)
                        ),
                        true,
                    ),
                    ScaleCheck::Invalid => (
                        format!(
                            "No valid scale near this value for {}×{}",
                            m.width, m.height
                        ),
                        true,
                    ),
                };
                scale_row.set_subtitle(&text);
                if warn {
                    scale_row.add_css_class("warning");
                } else {
                    scale_row.remove_css_class("warning");
                }
                ui::clear(&sugg_box);
                for v in logic::suggested_scales(m.width, m.height) {
                    let b = gtk::Button::with_label(&logic::fmt_scale(v));
                    b.set_tooltip_text(Some(&logical(v)));
                    let row = scale_row.downgrade();
                    b.connect_clicked(move |_| {
                        if let Some(row) = row.upgrade() {
                            row.set_value(v);
                        }
                    });
                    sugg_box.append(&b);
                }
            })
        };
        scale_feedback();

        // Rotation.
        let transform_row = adw::ComboRow::builder()
            .title("Rotation")
            .model(&gtk::StringList::new(&logic::TRANSFORMS))
            .selected(o.transform().clamp(0, 7) as u32)
            .build();
        group.add(&transform_row);
        dependent.push(transform_row.clone().upcast());

        // VRR.
        let mut vrr_labels = vec![self.global_vrr_label()];
        vrr_labels.extend(VRR_CHOICES.iter().map(|c| c.1.to_owned()));
        let vrr_list = string_list(&vrr_labels);
        self.vrr_lists.borrow_mut().push(vrr_list.clone());
        let vrr_row = adw::ComboRow::builder()
            .title("Adaptive sync (VRR)")
            .subtitle(if live.vrr {
                "Active right now"
            } else {
                "Not active right now"
            })
            .model(&vrr_list)
            .selected(
                o.draft
                    .vrr
                    .filter(|v| (0..=3).contains(v))
                    .map_or(0, |v| v as u32 + 1),
            )
            .build();
        group.add(&vrr_row);
        dependent.push(vrr_row.clone().upcast());

        // Mirror.
        let mirror_row = (!others.is_empty()).then(|| {
            let mut labels = vec!["Don't mirror".to_owned()];
            labels.extend(others.iter().map(|n| format!("Mirror {n}")));
            let row = adw::ComboRow::builder()
                .title("Mirror")
                .subtitle("Show another display's content instead of extending the desktop")
                .model(&string_list(&labels))
                .selected(
                    o.mirror()
                        .and_then(|m| others.iter().position(|n| n == m))
                        .map_or(0, |i| i as u32 + 1),
                )
                .build();
            group.add(&row);
            dependent.push(row.clone().upcast());
            row
        });

        // Color & HDR.
        let color = adw::ExpanderRow::builder()
            .title("Color &amp; HDR")
            .subtitle(
                glib::markup_escape_text(&format!(
                    "Live: preset {} · {} · {}-bit",
                    if live.color_management_preset.is_empty() {
                        "srgb"
                    } else {
                        &live.color_management_preset
                    },
                    live.current_format,
                    o.live_bitdepth()
                ))
                .as_str(),
            )
            .build();
        group.add(&color);
        dependent.push(color.clone().upcast());
        let cm_now = o.cm();
        let cm_row = adw::ComboRow::builder()
            .title("Color preset")
            .use_subtitle(true)
            .model(&gtk::StringList::new(&CM_PRESETS.map(|c| c.1)))
            .selected(CM_PRESETS.iter().position(|c| c.0 == cm_now).unwrap_or(1) as u32)
            .build();
        color.add_row(&cm_row);
        let depth_row = adw::ComboRow::builder()
            .title("Bit depth")
            .subtitle(format!("Live pixel format: {}", live.current_format))
            .model(&gtk::StringList::new(&["8-bit", "10-bit"]))
            .selected(u32::from(
                o.draft.bitdepth.unwrap_or(o.live_bitdepth()) == 10,
            ))
            .build();
        color.add_row(&depth_row);
        let eotf_now = o.draft.sdr_eotf.clone().unwrap_or_else(|| "default".into());
        let eotf_row = adw::ComboRow::builder()
            .title("SDR transfer function")
            .use_subtitle(true)
            .model(&gtk::StringList::new(&SDR_EOTF.map(|c| c.1)))
            .selected(SDR_EOTF.iter().position(|c| c.0 == eotf_now).unwrap_or(0) as u32)
            .build();
        color.add_row(&eotf_row);
        let icc_row = self.icc_row(idx, o.draft.icc.as_deref());
        color.add_row(&icc_row);

        let hdr_rows: Vec<gtk::Widget> = {
            let d = &o.draft;
            let live_f = |v: Option<f64>, def: f64| v.unwrap_or(def);
            let rows: Vec<gtk::Widget> = vec![
                self.spin_row(
                    idx,
                    "SDR brightness in HDR",
                    format!(
                        "Live {:.2} · typical 1.0–2.0",
                        live_f(live.sdr_brightness, 1.0)
                    ),
                    (0.1, 5.0, 0.05, 2),
                    d.sdrbrightness.unwrap_or(live_f(live.sdr_brightness, 1.0)),
                    |r, v| r.sdrbrightness = Some(v),
                )
                .upcast(),
                self.spin_row(
                    idx,
                    "SDR saturation in HDR",
                    format!("Live {:.2}", live_f(live.sdr_saturation, 1.0)),
                    (0.0, 2.0, 0.05, 2),
                    d.sdrsaturation.unwrap_or(live_f(live.sdr_saturation, 1.0)),
                    |r, v| r.sdrsaturation = Some(v),
                )
                .upcast(),
                self.spin_row(
                    idx,
                    "SDR minimum luminance (nits)",
                    format!("Live {:.2}", live_f(live.sdr_min_luminance, 0.2)),
                    (0.0, 10.0, 0.01, 2),
                    d.sdr_min_luminance
                        .unwrap_or(live_f(live.sdr_min_luminance, 0.2)),
                    |r, v| r.sdr_min_luminance = Some(v),
                )
                .upcast(),
                self.spin_row(
                    idx,
                    "SDR maximum luminance (nits)",
                    format!("Live {:.0}", live_f(live.sdr_max_luminance, 80.0)),
                    (1.0, 10000.0, 10.0, 0),
                    d.sdr_max_luminance
                        .map_or(live_f(live.sdr_max_luminance, 80.0), |v| v as f64),
                    |r, v| r.sdr_max_luminance = Some(v.round() as i64),
                )
                .upcast(),
                self.spin_row(
                    idx,
                    "Display minimum luminance (nits)",
                    "-1 = from EDID".to_owned(),
                    (-1.0, 10.0, 0.01, 2),
                    d.min_luminance.unwrap_or(-1.0),
                    |r, v| r.min_luminance = Some(v),
                )
                .upcast(),
                self.spin_row(
                    idx,
                    "Display maximum luminance (nits)",
                    "-1 = from EDID".to_owned(),
                    (-1.0, 10000.0, 10.0, 0),
                    d.max_luminance.unwrap_or(-1) as f64,
                    |r, v| r.max_luminance = Some(v.round() as i64),
                )
                .upcast(),
                self.spin_row(
                    idx,
                    "Display max average luminance (nits)",
                    "-1 = from EDID".to_owned(),
                    (-1.0, 10000.0, 10.0, 0),
                    d.max_avg_luminance.unwrap_or(-1) as f64,
                    |r, v| r.max_avg_luminance = Some(v.round() as i64),
                )
                .upcast(),
                self.choice_row(idx, "HDR support", d.supports_hdr.unwrap_or(0), |r, v| {
                    r.supports_hdr = Some(v)
                })
                .upcast(),
                self.choice_row(
                    idx,
                    "Wide color support",
                    d.supports_wide_color.unwrap_or(0),
                    |r, v| r.supports_wide_color = Some(v),
                )
                .upcast(),
            ];
            let is_hdr = cm_now.starts_with("hdr");
            for r in &rows {
                r.set_visible(is_hdr);
                color.add_row(r);
            }
            rows
        };
        drop(outs);

        // Signal wiring (after initial values are set).
        let set_dependent = {
            let dependent: Vec<glib::WeakRef<gtk::Widget>> =
                dependent.iter().map(|w| w.downgrade()).collect();
            move |on: bool| {
                for w in dependent.iter().filter_map(glib::WeakRef::upgrade) {
                    w.set_sensitive(on);
                }
            }
        };
        set_dependent(enabled.is_active());
        {
            let page = Rc::downgrade(self);
            enabled.connect_active_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                if page.syncing.get() {
                    return;
                }
                let on = row.is_active();
                let others_on = page
                    .outs
                    .borrow()
                    .iter()
                    .enumerate()
                    .any(|(i, o)| i != idx && o.enabled());
                if !on && !others_on {
                    page.syncing.set(true);
                    row.set_active(true);
                    page.syncing.set(false);
                    page.ctx.toast("At least one display must stay enabled");
                    return;
                }
                set_dependent(on);
                page.edit(idx, |d| d.disabled = Some(!on));
            });
        }
        {
            let page = Rc::downgrade(self);
            let rate_row = rate_row.downgrade();
            let rates = rates.clone();
            let resolutions = resolutions.clone();
            let scale_feedback = scale_feedback.clone();
            res_row.connect_selected_notify(move |row| {
                let (Some(page), Some(rate_row)) = (page.upgrade(), rate_row.upgrade()) else {
                    return;
                };
                let Some(&(w, h)) = resolutions.get(row.selected() as usize) else {
                    return;
                };
                if page.syncing.get() {
                    return;
                }
                let prev = page.outs.borrow()[idx].mode().refresh;
                let new_rates = logic::rates(&modes, w, h);
                let pick = logic::closest_rate(&new_rates, prev).unwrap_or(0);
                let refresh = new_rates.get(pick).copied().unwrap_or(prev);
                page.syncing.set(true);
                rate_row.set_model(Some(&string_list(&rate_labels(&new_rates))));
                rate_row.set_selected(pick as u32);
                page.syncing.set(false);
                *rates.borrow_mut() = new_rates;
                page.edit(idx, |d| {
                    d.mode = Some(
                        Mode {
                            width: w,
                            height: h,
                            refresh,
                        }
                        .to_rule(),
                    )
                });
                scale_feedback();
            });
        }
        {
            let page = Rc::downgrade(self);
            rate_row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let Some(&r) = rates.borrow().get(row.selected() as usize) else {
                    return;
                };
                let m = page.outs.borrow()[idx].mode();
                page.edit(idx, |d| d.mode = Some(Mode { refresh: r, ..m }.to_rule()));
            });
        }
        {
            let page = Rc::downgrade(self);
            let scale_feedback = scale_feedback.clone();
            scale_row.connect_value_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let v = row.value();
                page.edit(idx, |d| d.scale = Some(v));
                scale_feedback();
            });
        }
        {
            let page = Rc::downgrade(self);
            transform_row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let t = i64::from(row.selected());
                page.edit(idx, |d| d.transform = Some(t));
                scale_feedback();
            });
        }
        {
            let page = Rc::downgrade(self);
            vrr_row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let sel = row.selected();
                page.edit(idx, |d| d.vrr = (sel > 0).then(|| i64::from(sel) - 1));
            });
        }
        if let Some(row) = mirror_row {
            let page = Rc::downgrade(self);
            row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let target = (row.selected() as usize)
                    .checked_sub(1)
                    .and_then(|i| others.get(i))
                    .cloned();
                page.edit(idx, |d| d.mirror = target);
            });
        }
        {
            let page = Rc::downgrade(self);
            let hdr_rows: Vec<glib::WeakRef<gtk::Widget>> =
                hdr_rows.iter().map(|w| w.downgrade()).collect();
            cm_row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let Some(&(id, _)) = CM_PRESETS.get(row.selected() as usize) else {
                    return;
                };
                for w in hdr_rows.iter().filter_map(glib::WeakRef::upgrade) {
                    w.set_visible(id.starts_with("hdr"));
                }
                page.edit(idx, |d| d.cm = Some(id.to_owned()));
            });
        }
        {
            let page = Rc::downgrade(self);
            depth_row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let bits = if row.selected() == 1 { 10 } else { 8 };
                page.edit(idx, |d| d.bitdepth = Some(bits));
            });
        }
        {
            let page = Rc::downgrade(self);
            eotf_row.connect_selected_notify(move |row| {
                let Some(page) = page.upgrade() else { return };
                let Some(&(id, _)) = SDR_EOTF.get(row.selected() as usize) else {
                    return;
                };
                page.edit(idx, |d| d.sdr_eotf = Some(id.to_owned()));
            });
        }
        group
    }

    fn spin_row(
        self: &Rc<Self>,
        idx: usize,
        title: &str,
        subtitle: String,
        (min, max, step, digits): (f64, f64, f64, u32),
        value: f64,
        set: fn(&mut MonitorRule, f64),
    ) -> adw::SpinRow {
        let row = adw::SpinRow::builder()
            .title(title)
            .subtitle(subtitle)
            .adjustment(&gtk::Adjustment::new(
                value,
                min,
                max,
                step,
                step * 10.0,
                0.0,
            ))
            .digits(digits)
            .build();
        let page = Rc::downgrade(self);
        row.connect_value_notify(move |row| {
            if let Some(page) = page.upgrade() {
                let v = row.value();
                page.edit(idx, |d| set(d, v));
            }
        });
        row
    }

    fn choice_row(
        self: &Rc<Self>,
        idx: usize,
        title: &str,
        value: i64,
        set: fn(&mut MonitorRule, i64),
    ) -> adw::ComboRow {
        let row = adw::ComboRow::builder()
            .title(title)
            .model(&gtk::StringList::new(&SUPPORT_CHOICES.map(|c| c.1)))
            .selected(
                SUPPORT_CHOICES
                    .iter()
                    .position(|c| c.0 == value)
                    .unwrap_or(0) as u32,
            )
            .build();
        let page = Rc::downgrade(self);
        row.connect_selected_notify(move |row| {
            if let (Some(page), Some(&(v, _))) =
                (page.upgrade(), SUPPORT_CHOICES.get(row.selected() as usize))
            {
                page.edit(idx, |d| set(d, v));
            }
        });
        row
    }

    fn icc_row(self: &Rc<Self>, idx: usize, current: Option<&str>) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title("ICC profile")
            .subtitle(
                glib::markup_escape_text(
                    current.unwrap_or("None — overrides the color preset when set"),
                )
                .as_str(),
            )
            .build();
        let choose = gtk::Button::builder()
            .label("Choose…")
            .valign(gtk::Align::Center)
            .build();
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text("Remove the ICC profile")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .visible(current.is_some())
            .build();
        row.add_suffix(&clear);
        row.add_suffix(&choose);
        {
            let page = Rc::downgrade(self);
            let row_w = row.downgrade();
            let clear_w = clear.downgrade();
            choose.connect_clicked(move |_| {
                let (Some(page), Some(row), Some(clear)) =
                    (page.upgrade(), row_w.upgrade(), clear_w.upgrade())
                else {
                    return;
                };
                let filter = gtk::FileFilter::new();
                filter.set_name(Some("ICC profiles"));
                filter.add_suffix("icc");
                filter.add_suffix("icm");
                let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let dialog = gtk::FileDialog::builder()
                    .title("Choose an ICC profile")
                    .modal(true)
                    .filters(&filters)
                    .build();
                glib::spawn_future_local(async move {
                    let Ok(file) = dialog.open_future(Some(&page.ctx.window)).await else {
                        return;
                    };
                    let Some(path) = file.path() else {
                        page.ctx.toast("ICC profiles must be local files");
                        return;
                    };
                    let path = path.to_string_lossy().into_owned();
                    row.set_subtitle(&glib::markup_escape_text(&path));
                    clear.set_visible(true);
                    page.edit(idx, |d| d.icc = Some(path));
                });
            });
        }
        {
            let page = Rc::downgrade(self);
            let row_w = row.downgrade();
            clear.connect_clicked(move |btn| {
                let (Some(page), Some(row)) = (page.upgrade(), row_w.upgrade()) else {
                    return;
                };
                row.set_subtitle("None");
                btn.set_visible(false);
                page.edit(idx, |d| d.icc = None);
            });
        }
        row
    }

    /// Global rendering options (applied immediately through the managed file).
    fn rebuild_globals(self: &Rc<Self>) {
        ui::clear(&self.render_box);
        let group = adw::PreferencesGroup::builder()
            .title("Rendering")
            .description("Global options, applied immediately")
            .build();
        let label = self.global_vrr_label();
        self.syncing.set(true);
        for list in self.vrr_lists.borrow().iter() {
            list.splice(0, 1, &[label.as_str()]);
        }
        self.syncing.set(false);
        for g in self.globals.borrow().iter() {
            let current = g.managed_i64().or(g.live).unwrap_or(0);
            let subtitle =
                glib::markup_escape_text(&format!("{}\n{}", g.def.subtitle, g.provenance()))
                    .to_string();
            let reset = gtk::Button::builder()
                .icon_name("edit-undo-symbolic")
                .tooltip_text("Remove hyprdeck's override")
                .css_classes(["flat"])
                .valign(gtk::Align::Center)
                .visible(g.managed.is_some())
                .build();
            {
                let page = Rc::downgrade(self);
                let key = g.def.key;
                reset.connect_clicked(move |_| {
                    if let Some(page) = page.upgrade() {
                        page.set_global(key, None);
                    }
                });
            }
            let row: adw::PreferencesRow = if g.def.choices.is_empty() {
                let row = adw::SwitchRow::builder()
                    .title(g.def.title)
                    .subtitle(subtitle)
                    .active(current != 0)
                    .build();
                row.add_suffix(&reset);
                let page = Rc::downgrade(self);
                let (key, kind) = (g.def.key, g.kind);
                row.connect_active_notify(move |row| {
                    if let Some(page) = page.upgrade() {
                        let on = row.is_active();
                        page.set_global(
                            key,
                            Some(if kind == OptKind::Bool {
                                OptValue::Bool(on)
                            } else {
                                OptValue::Int(i64::from(on))
                            }),
                        );
                    }
                });
                row.upcast()
            } else {
                let labels: Vec<&str> = g.def.choices.iter().map(|c| c.1).collect();
                let row = adw::ComboRow::builder()
                    .title(g.def.title)
                    .subtitle(subtitle)
                    .model(&gtk::StringList::new(&labels))
                    .selected(
                        g.def
                            .choices
                            .iter()
                            .position(|c| c.0 == current)
                            .unwrap_or(0) as u32,
                    )
                    .build();
                row.add_suffix(&reset);
                let page = Rc::downgrade(self);
                let (key, choices) = (g.def.key, g.def.choices);
                row.connect_selected_notify(move |row| {
                    if let (Some(page), Some(&(v, _))) =
                        (page.upgrade(), choices.get(row.selected() as usize))
                    {
                        page.set_global(key, Some(OptValue::Int(v)));
                    }
                });
                row.upcast()
            };
            group.add(&row);
        }
        self.render_box.append(&group);
    }

    /// Set (`Some`) or remove (`None`) a managed global option and reload Hyprland's config.
    fn set_global(self: &Rc<Self>, key: &'static str, value: Option<OptValue>) {
        if self.busy.get() {
            return;
        }
        let page = self.clone();
        self.set_busy(true);
        glib::spawn_future_local(async move {
            let res = rt::blocking(move || -> Result<(Vec<String>, Vec<GlobalState>)> {
                managed::update(|m| match value {
                    Some(v) => {
                        m.options.insert(key.to_owned(), v);
                    }
                    None => {
                        m.options.remove(key);
                    }
                })?;
                let errors = managed::apply(false)?;
                Ok((errors, load_snapshot()?.globals))
            })
            .await;
            page.set_busy(false);
            match res {
                Ok((errors, globals)) => {
                    *page.globals.borrow_mut() = globals;
                    page.rebuild_globals();
                    match errors.first() {
                        Some(e) => page.ctx.toast(format!("Applied with config errors: {e}")),
                        None => page.ctx.toast("Setting applied"),
                    }
                }
                Err(e) => {
                    page.ctx.error("Changing setting failed", &e);
                    page.rebuild_globals();
                }
            }
        });
    }

    fn apply_drafts(self: &Rc<Self>) {
        let changes: Result<Vec<(String, Option<MonitorRule>)>> = self
            .outs
            .borrow()
            .iter()
            .filter(|o| o.dirty())
            .map(|o| Ok((o.name().to_owned(), Some(o.rule_to_apply()?))))
            .collect();
        match changes {
            Ok(changes) if !changes.is_empty() => {
                let page = self.clone();
                glib::spawn_future_local(async move { page.apply_changes(changes).await });
            }
            Ok(_) => {}
            Err(e) => self.ctx.error("Cannot apply", &e),
        }
    }

    /// Write the given managed monitor rules (`None` removes the rule), apply
    /// them with a modeset and keep them only if the user confirms in time.
    async fn apply_changes(self: &Rc<Self>, changes: Vec<(String, Option<MonitorRule>)>) {
        if self.busy.get() {
            return;
        }
        self.set_busy(true);
        let res = rt::blocking(move || -> Result<(Vec<Restore>, Vec<String>)> {
            let mut restores = Vec::new();
            managed::update(|m| {
                for (output, next) in changes {
                    restores.push(rules::swap(m, &output, next));
                }
            })?;
            match managed::apply(true) {
                Ok(errors) => Ok((restores, errors)),
                Err(e) => {
                    let _ = revert(&restores);
                    Err(e)
                }
            }
        })
        .await;
        let (restores, errors) = match res {
            Ok(r) => r,
            Err(e) => {
                self.ctx.error("Applying display settings failed", &e);
                self.set_busy(false);
                self.reload();
                return;
            }
        };
        if self.confirm_keep(&errors).await {
            self.ctx.toast("Display settings saved");
        } else {
            match rt::blocking(move || revert(&restores)).await {
                Ok(_) => self.ctx.toast("Reverted to the previous display settings"),
                Err(e) => self.ctx.error("Reverting display settings failed", &e),
            }
        }
        // Give the compositor a moment to finish the modeset before re-reading.
        glib::timeout_future(Duration::from_millis(400)).await;
        self.set_busy(false);
        self.reload();
    }

    /// "Keep these display settings?" with a revert countdown. `true` = keep.
    async fn confirm_keep(&self, errors: &[String]) -> bool {
        let extra = if errors.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nHyprland reported config errors:\n{}",
                errors.join("\n")
            )
        };
        let body =
            move |n: u32| format!("Reverting to the previous settings in {n} seconds.{extra}");
        let dialog = adw::AlertDialog::new(
            Some("Keep these display settings?"),
            Some(&body(REVERT_SECONDS)),
        );
        dialog.add_responses(&[("revert", "Revert"), ("keep", "Keep Changes")]);
        dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("revert");
        let left = Rc::new(Cell::new(REVERT_SECONDS));
        let timer = glib::timeout_add_seconds_local(1, {
            let dialog = dialog.downgrade();
            let left = left.clone();
            move || {
                let Some(dialog) = dialog.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let n = left.get().saturating_sub(1);
                left.set(n);
                if n == 0 {
                    dialog.force_close();
                    return glib::ControlFlow::Break;
                }
                dialog.set_body(&body(n));
                glib::ControlFlow::Continue
            }
        });
        let response = dialog.choose_future(Some(&self.ctx.window)).await;
        if left.get() > 0 {
            timer.remove();
        }
        response == "keep"
    }
}

/// Undo [`rules::swap`]s and re-apply.
fn revert(restores: &[Restore]) -> Result<Vec<String>> {
    managed::update(|m| {
        for r in restores.iter().rev() {
            rules::restore(m, r);
        }
    })?;
    managed::apply(true)
}

fn string_list<S: AsRef<str>>(items: &[S]) -> gtk::StringList {
    let list = gtk::StringList::new(&[]);
    for s in items {
        list.append(s.as_ref());
    }
    list
}

fn rate_labels(rates: &[f64]) -> Vec<String> {
    rates.iter().map(|r| format!("{r:.2} Hz")).collect()
}
