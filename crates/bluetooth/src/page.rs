//! The Bluetooth page: adapter card, paired devices, nearby devices (scan + pair)
//! and the dialogs of the pairing agent.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use bluer::agent::ReqError;
use bluer::{AdapterProperty, Address};
use futures_util::future::{self, Either};
use gtk::glib;
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};
use tokio::task::AbortHandle;

use crate::agent::{self, Reply, Request};
use crate::bt::{self, Change, Rfkill};
use crate::info::{AdapterInfo, DeviceInfo, Followup, cmp_nearby, cmp_paired, signal_label};

const SCAN_LIMIT: Duration = Duration::from_secs(60);
const RESORT_DELAY: Duration = Duration::from_millis(800);

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let page = Page::new(ctx.clone());
    page.connect();
    page.root.clone().upcast()
}

struct Page {
    ctx: Ctx,
    root: gtk::Stack,
    status: adw::StatusPage,
    rfkill_group: adw::PreferencesGroup,
    rfkill_row: adw::ActionRow,
    unblock: gtk::Button,
    adapter_group: adw::PreferencesGroup,
    adapter_row: adw::ActionRow,
    powered: adw::SwitchRow,
    discoverable: adw::SwitchRow,
    pairable: adw::SwitchRow,
    paired_list: gtk::ListBox,
    scan_row: adw::SwitchRow,
    scan_spinner: adw::Spinner,
    unnamed_row: adw::SwitchRow,
    nearby_list: gtk::ListBox,
    nearby_placeholder: gtk::Label,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    adapter: Option<AdapterInfo>,
    rfkill: Rfkill,
    devices: HashMap<Address, DeviceInfo>,
    rows: HashMap<Address, Row>,
    /// Devices with an operation in flight, with the status text to show.
    busy: HashMap<Address, &'static str>,
    watch: Option<AbortHandle>,
    scan: Option<AbortHandle>,
    scan_gen: u64,
    pairing: HashMap<Address, AbortHandle>,
    resort_pending: bool,
    display: Option<DisplayDialog>,
    display_seq: u64,
}

/// The "type this code" dialog (updated in place as digits are typed).
struct DisplayDialog {
    address: Address,
    dialog: adw::AlertDialog,
    code: gtk::Label,
    seq: u64,
    closed_by_us: Rc<Cell<bool>>,
}

impl Page {
    fn new(ctx: Ctx) -> Rc<Self> {
        let (scroller, content) = ui::page_scaffold();

        let rfkill_row = adw::ActionRow::builder()
            .title("Bluetooth is blocked")
            .build();
        rfkill_row.add_prefix(&gtk::Image::from_icon_name("bluetooth-disabled-symbolic"));
        let unblock = gtk::Button::builder()
            .label("Unblock")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        rfkill_row.add_suffix(&unblock);
        let rfkill_group = adw::PreferencesGroup::builder().visible(false).build();
        rfkill_group.add(&rfkill_row);

        let adapter_row = adw::ActionRow::builder()
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        adapter_row.add_prefix(&gtk::Image::from_icon_name("bluetooth-symbolic"));
        let powered = adw::SwitchRow::builder()
            .title("Powered")
            .subtitle("Turn the Bluetooth radio on or off")
            .build();
        let discoverable = adw::SwitchRow::builder().title("Discoverable").build();
        let pairable = adw::SwitchRow::builder()
            .title("Pairable")
            .subtitle("Accept pairing requests started from other devices")
            .build();
        let adapter_group = adw::PreferencesGroup::builder().title("Adapter").build();
        for row in [
            adapter_row.upcast_ref::<gtk::Widget>(),
            powered.upcast_ref(),
            discoverable.upcast_ref(),
            pairable.upcast_ref(),
        ] {
            adapter_group.add(row);
        }

        let paired_list = boxed_list(&placeholder(
            "No paired devices yet — pair one from Nearby devices below",
        ));
        let paired_group = adw::PreferencesGroup::builder()
            .title("My devices")
            .description("Connected headsets and speakers appear as audio outputs on the Audio page automatically")
            .build();
        paired_group.add(&paired_list);

        let scan_spinner = adw::Spinner::builder()
            .visible(false)
            .valign(gtk::Align::Center)
            .build();
        let scan_row = adw::SwitchRow::builder().title("Scan for devices").build();
        let unnamed_row = adw::SwitchRow::builder()
            .title("Show unnamed devices")
            .subtitle("Include devices that don't advertise a name (mostly beacons and trackers)")
            .build();
        let nearby_placeholder = placeholder("");
        let nearby_list = boxed_list(&nearby_placeholder);
        nearby_list.set_margin_top(12);
        let nearby_group = adw::PreferencesGroup::builder()
            .title("Nearby devices")
            .description("Put the device in pairing mode, then scan")
            .header_suffix(&scan_spinner)
            .build();
        nearby_group.add(&scan_row);
        nearby_group.add(&unnamed_row);
        nearby_group.add(&nearby_list);

        for g in [&rfkill_group, &adapter_group, &paired_group, &nearby_group] {
            content.append(g);
        }

        let status = adw::StatusPage::builder()
            .icon_name("bluetooth-disabled-symbolic")
            .vexpand(true)
            .build();
        let retry = gtk::Button::builder()
            .label("Retry")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        status.set_child(Some(&retry));

        let root = gtk::Stack::new();
        root.add_named(&scroller, Some("main"));
        root.add_named(&status, Some("status"));

        let page = Rc::new(Page {
            ctx,
            root,
            status,
            rfkill_group,
            rfkill_row,
            unblock,
            adapter_group,
            adapter_row,
            powered,
            discoverable,
            pairable,
            paired_list,
            scan_row,
            scan_spinner,
            unnamed_row,
            nearby_list,
            nearby_placeholder,
            state: RefCell::default(),
        });
        let p = page.clone();
        retry.connect_clicked(move |_| p.restart_watch());
        page
    }

    fn connect(self: &Rc<Self>) {
        let p = self.clone();
        self.root.connect_map(move |_| {
            agent::set_ui_visible(true);
            if p.state.borrow().watch.is_none() {
                p.restart_watch();
            }
        });
        let p = self.clone();
        self.root.connect_unmap(move |_| {
            agent::set_ui_visible(false);
            p.stop_scan();
            if let Some(w) = p.state.borrow_mut().watch.take() {
                w.abort();
            }
        });

        let p = self.clone();
        self.unblock.connect_clicked(move |b| {
            b.set_sensitive(false);
            let p = p.clone();
            let b = b.clone();
            glib::spawn_future_local(async move {
                match rt::blocking(bt::rfkill_unblock).await {
                    Ok(()) => p.ctx.toast("Bluetooth unblocked"),
                    Err(e) => p.ctx.error("Couldn't unblock Bluetooth", &e),
                }
                b.set_sensitive(true);
                p.refresh_rfkill().await;
            });
        });

        let p = self.clone();
        self.powered.connect_active_notify(move |row| {
            let on = row.is_active();
            if p.state
                .borrow()
                .adapter
                .as_ref()
                .is_some_and(|a| a.powered != on)
            {
                p.set_powered(on);
            }
        });
        let p = self.clone();
        self.discoverable.connect_active_notify(move |row| {
            let on = row.is_active();
            if p.state
                .borrow()
                .adapter
                .as_ref()
                .is_some_and(|a| a.discoverable != on)
            {
                p.adapter_action(
                    row.upcast_ref(),
                    "Couldn't change visibility",
                    bt::set_discoverable(on),
                );
            }
        });
        let p = self.clone();
        self.pairable.connect_active_notify(move |row| {
            let on = row.is_active();
            if p.state
                .borrow()
                .adapter
                .as_ref()
                .is_some_and(|a| a.pairable != on)
            {
                p.adapter_action(
                    row.upcast_ref(),
                    "Couldn't change pairable mode",
                    bt::set_pairable(on),
                );
            }
        });

        let p = self.clone();
        self.scan_row.connect_active_notify(move |row| {
            let scanning = p.state.borrow().scan.is_some();
            match (row.is_active(), scanning) {
                (true, false) => p.start_scan(),
                (false, true) => p.stop_scan(),
                _ => {}
            }
        });
        let p = self.clone();
        self.unnamed_row
            .connect_active_notify(move |_| p.sync_all());

        let p = self.clone();
        self.paired_list
            .set_sort_func(move |a, b| p.compare_rows(a, b, cmp_paired).into());
        let p = self.clone();
        self.nearby_list
            .set_sort_func(move |a, b| p.compare_rows(a, b, cmp_nearby).into());

        // Pairing-agent requests: drained for the app's lifetime (this page is built once).
        let p = self.clone();
        glib::spawn_future_local(async move {
            let rx = agent::requests();
            while let Ok(req) = rx.recv().await {
                p.handle_request(req);
            }
        });

        self.sync_adapter();
    }

    // ---- change feed ----------------------------------------------------

    fn restart_watch(self: &Rc<Self>) {
        if let Some(w) = self.state.borrow_mut().watch.take() {
            w.abort();
        }
        let (tx, rx) = async_channel::unbounded();
        let handle = rt::spawn(bt::watch(tx));
        self.state.borrow_mut().watch = Some(handle.abort_handle());
        let p = self.clone();
        glib::spawn_future_local(async move {
            // Ends when the watch task is aborted (sender dropped).
            while let Ok(change) = rx.recv().await {
                p.on_change(change).await;
            }
        });
    }

    async fn on_change(self: &Rc<Self>, change: Change) {
        match change {
            Change::Reset => self.reload().await,
            Change::Adapter(prop) => {
                let mut cleared = false;
                if let Some(a) = self.state.borrow_mut().adapter.as_mut() {
                    a.apply(&prop);
                }
                match prop {
                    // BlueZ invalidates RSSI when discovery stops (not signalled as a change).
                    AdapterProperty::Discovering(false) => {
                        for d in self.state.borrow_mut().devices.values_mut() {
                            d.rssi = None;
                        }
                        cleared = true;
                    }
                    AdapterProperty::Powered(_) => self.refresh_rfkill().await,
                    _ => {}
                }
                self.sync_adapter();
                if cleared || matches!(prop, AdapterProperty::Powered(_)) {
                    self.sync_all();
                }
            }
            Change::Added(addr) => self.refetch(addr).await,
            Change::Removed(addr) => {
                self.state.borrow_mut().devices.remove(&addr);
                self.sync_device(addr);
            }
            Change::Device(addr, prop) => {
                let followup = self
                    .state
                    .borrow_mut()
                    .devices
                    .get_mut(&addr)
                    .map(|d| d.apply(&prop));
                match followup {
                    Some(Followup::None) => self.sync_device(addr),
                    Some(Followup::Refetch) => {
                        self.sync_device(addr);
                        self.refetch(addr).await;
                    }
                    None => self.refetch(addr).await,
                }
            }
        }
    }

    async fn reload(self: &Rc<Self>) {
        let snap = rt::run(bt::snapshot()).await;
        let rfkill = rt::blocking(bt::rfkill).await;
        let rows: Vec<Row> = {
            let mut st = self.state.borrow_mut();
            st.rfkill = rfkill;
            st.rows.drain().map(|(_, r)| r).collect()
        };
        for row in rows {
            self.list_for(&row).remove(row.widget());
        }
        match snap {
            Ok(Some(s)) => {
                {
                    let mut st = self.state.borrow_mut();
                    st.adapter = Some(s.adapter);
                    st.devices = s.devices.into_iter().map(|d| (d.address, d)).collect();
                }
                self.root.set_visible_child_name("main");
                self.sync_adapter();
                self.sync_all();
            }
            Ok(None) => {
                self.clear_model();
                self.status.set_title("No Bluetooth adapter");
                self.status.set_description(Some(if rfkill.hard {
                    "The Bluetooth radio is disabled by a hardware switch or firmware setting."
                } else {
                    "BlueZ reports no adapter. Check that the adapter is plugged in and enabled; this page updates when it appears."
                }));
                self.root.set_visible_child_name("status");
            }
            Err(e) if e.is::<bt::NoBluez>() => {
                self.clear_model();
                self.status.set_title("Bluetooth service not running");
                self.status.set_description(Some(
                    "BlueZ (bluetoothd) isn't running. Install BlueZ and start it with “systemctl enable --now bluetooth.service”; this page updates when it appears.",
                ));
                self.root.set_visible_child_name("status");
            }
            Err(e) => {
                self.clear_model();
                self.status.set_title("Can't reach BlueZ");
                self.status
                    .set_description(Some(&glib::markup_escape_text(&format!("{e:#}"))));
                self.root.set_visible_child_name("status");
            }
        }
    }

    fn clear_model(&self) {
        let mut st = self.state.borrow_mut();
        st.adapter = None;
        st.devices.clear();
    }

    async fn refetch(self: &Rc<Self>, addr: Address) {
        if let Ok(d) = rt::run(bt::refetch(addr)).await {
            self.state.borrow_mut().devices.insert(addr, d);
            self.sync_device(addr);
        }
    }

    async fn refresh_rfkill(self: &Rc<Self>) {
        let rf = rt::blocking(bt::rfkill).await;
        self.state.borrow_mut().rfkill = rf;
        self.sync_adapter();
    }

    // ---- adapter --------------------------------------------------------

    fn sync_adapter(&self) {
        let (rf, adapter, scanning) = {
            let st = self.state.borrow();
            (st.rfkill, st.adapter.clone(), st.scan.is_some())
        };
        self.rfkill_group.set_visible(rf.soft || rf.hard);
        self.unblock.set_visible(rf.soft && !rf.hard);
        self.rfkill_row.set_subtitle(if rf.hard {
            "A hardware switch or firmware setting disables the radio; it can't be unblocked from software"
        } else {
            "Switched off by rfkill (e.g. airplane mode). Unblock it here or run “rfkill unblock bluetooth”"
        });

        let Some(a) = adapter else {
            self.adapter_group.set_sensitive(false);
            return;
        };
        self.adapter_group.set_sensitive(true);
        self.adapter_row.set_title(&a.alias);
        self.adapter_row
            .set_subtitle(&format!("{} · {}", a.address, a.name));
        self.powered.set_active(a.powered);
        self.discoverable.set_active(a.discoverable);
        self.discoverable
            .set_subtitle(&match a.discoverable_timeout {
                0 => "Let other devices find this computer".to_owned(),
                s if s % 60 == 0 => format!(
                    "Let other devices find this computer for {} minutes",
                    s / 60
                ),
                s => format!("Let other devices find this computer for {s} seconds"),
            });
        self.pairable.set_active(a.pairable);
        for row in [&self.discoverable, &self.pairable, &self.scan_row] {
            row.set_sensitive(a.powered);
        }
        self.scan_spinner.set_visible(scanning);
        self.scan_row.set_subtitle(if scanning {
            "Searching… stops automatically after 60 seconds"
        } else {
            "Look for devices that are in pairing mode"
        });
        self.nearby_placeholder.set_label(if !a.powered {
            "Turn Bluetooth on to find nearby devices"
        } else if scanning {
            "Searching for devices in pairing mode…"
        } else {
            "Turn on scanning to find devices in pairing mode"
        });
    }

    fn set_powered(self: &Rc<Self>, on: bool) {
        let soft_blocked = self.state.borrow().rfkill.soft;
        let fut = async move {
            if on && soft_blocked {
                tokio::task::spawn_blocking(bt::rfkill_unblock).await??;
                // Let BlueZ notice the unblock before powering on.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            bt::set_powered(on).await
        };
        let what = if on {
            "Couldn't turn Bluetooth on"
        } else {
            "Couldn't turn Bluetooth off"
        };
        self.adapter_action(self.powered.upcast_ref(), what, fut);
    }

    /// Run an adapter change; on failure report it and restore the row from the model.
    fn adapter_action(
        self: &Rc<Self>,
        row: &gtk::Widget,
        what: &'static str,
        fut: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) {
        row.set_sensitive(false);
        let p = self.clone();
        let row = row.clone();
        glib::spawn_future_local(async move {
            let result = rt::run(fut).await;
            row.set_sensitive(true);
            if let Err(e) = result {
                p.ctx.error(what, &e);
            }
            p.refresh_rfkill().await;
        });
    }

    // ---- scanning -------------------------------------------------------

    fn start_scan(self: &Rc<Self>) {
        let handle = rt::spawn(bt::scan(SCAN_LIMIT));
        let seq = {
            let mut st = self.state.borrow_mut();
            st.scan = Some(handle.abort_handle());
            st.scan_gen += 1;
            st.scan_gen
        };
        self.sync_adapter();
        let p = self.clone();
        glib::spawn_future_local(async move {
            let result = handle.await;
            let current = {
                let mut st = p.state.borrow_mut();
                let current = st.scan_gen == seq;
                if current {
                    st.scan = None;
                }
                current
            };
            if !current {
                return;
            }
            p.scan_row.set_active(false);
            p.sync_adapter();
            if let Ok(Err(e)) = result {
                p.ctx.error("Couldn't scan for devices", &e);
            }
        });
    }

    fn stop_scan(&self) {
        let scan = {
            let mut st = self.state.borrow_mut();
            st.scan_gen += 1;
            st.scan.take()
        };
        if let Some(h) = scan {
            h.abort();
            self.scan_row.set_active(false);
            self.sync_adapter();
        }
    }

    // ---- device rows ----------------------------------------------------

    fn list_for(&self, row: &Row) -> &gtk::ListBox {
        match row {
            Row::Paired(_) => &self.paired_list,
            Row::Nearby(_) => &self.nearby_list,
        }
    }

    fn compare_rows(
        &self,
        a: &gtk::ListBoxRow,
        b: &gtk::ListBoxRow,
        cmp: fn(&DeviceInfo, &DeviceInfo) -> Ordering,
    ) -> Ordering {
        let Ok(st) = self.state.try_borrow() else {
            return Ordering::Equal;
        };
        let get = |r: &gtk::ListBoxRow| {
            r.widget_name()
                .parse::<Address>()
                .ok()
                .and_then(|a| st.devices.get(&a))
        };
        match (get(a), get(b)) {
            (Some(x), Some(y)) => cmp(x, y),
            _ => Ordering::Equal,
        }
    }

    fn sync_all(self: &Rc<Self>) {
        let addrs: HashSet<Address> = {
            let st = self.state.borrow();
            st.devices.keys().chain(st.rows.keys()).copied().collect()
        };
        for addr in addrs {
            self.sync_device(addr);
        }
    }

    /// Create, move, update or remove the row of `addr` to match the model.
    fn sync_device(self: &Rc<Self>, addr: Address) {
        let (want, have) = {
            let st = self.state.borrow();
            let show_unnamed = self.unnamed_row.is_active();
            let want = st.devices.get(&addr).and_then(|d| {
                if d.paired {
                    Some(true)
                } else if st.busy.contains_key(&addr)
                    || (d.is_nearby() && (show_unnamed || d.is_named()))
                {
                    Some(false)
                } else {
                    None
                }
            });
            (want, st.rows.get(&addr).map(Row::is_paired))
        };
        // Take the stale row out first: removing it from the list runs the sort func,
        // which borrows the state.
        let stale = if have.is_some() && have != want {
            self.state.borrow_mut().rows.remove(&addr)
        } else {
            None
        };
        if let Some(row) = stale {
            self.list_for(&row).remove(row.widget());
        }
        let Some(paired) = want else {
            self.schedule_resort();
            return;
        };
        if have != want {
            let row = if paired {
                Row::Paired(PairedRow::new(self, addr))
            } else {
                Row::Nearby(NearbyRow::new(self, addr))
            };
            self.update_row(&row, addr);
            let widget = row.widget().clone();
            let list = self.list_for(&row).clone();
            self.state.borrow_mut().rows.insert(addr, row);
            list.append(&widget);
        } else {
            let st = self.state.borrow();
            if let Some(row) = st.rows.get(&addr) {
                self.update_row(row, addr);
            }
        }
        self.schedule_resort();
    }

    fn update_row(&self, row: &Row, addr: Address) {
        let st = self.state.borrow();
        let Some(d) = st.devices.get(&addr) else {
            return;
        };
        let busy = st.busy.get(&addr).copied();
        let powered = st.adapter.as_ref().is_some_and(|a| a.powered);
        match row {
            Row::Paired(r) => r.update(d, busy, powered),
            Row::Nearby(r) => r.update(d, busy, powered),
        }
    }

    fn schedule_resort(self: &Rc<Self>) {
        {
            let mut st = self.state.borrow_mut();
            if st.resort_pending {
                return;
            }
            st.resort_pending = true;
        }
        let p = self.clone();
        glib::timeout_add_local_once(RESORT_DELAY, move || {
            p.state.borrow_mut().resort_pending = false;
            p.paired_list.invalidate_sort();
            p.nearby_list.invalidate_sort();
        });
    }

    fn set_busy(self: &Rc<Self>, addr: Address, status: Option<&'static str>) {
        {
            let mut st = self.state.borrow_mut();
            match status {
                Some(s) => st.busy.insert(addr, s),
                None => st.busy.remove(&addr),
            };
        }
        self.sync_device(addr);
    }

    fn label(&self, addr: Address) -> String {
        self.state
            .borrow()
            .devices
            .get(&addr)
            .map_or_else(|| addr.to_string(), |d| d.label().to_owned())
    }

    /// Run a device operation with busy state and error reporting.
    fn device_action(
        self: &Rc<Self>,
        addr: Address,
        status: &'static str,
        fut: impl Future<Output = anyhow::Result<()>> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, anyhow::Result<()>) + 'static,
    ) {
        self.set_busy(addr, Some(status));
        let p = self.clone();
        glib::spawn_future_local(async move {
            let result = rt::run(fut).await;
            p.set_busy(addr, None);
            done(&p, result);
        });
    }

    fn toggle_connection(self: &Rc<Self>, addr: Address) {
        let connected = self
            .state
            .borrow()
            .devices
            .get(&addr)
            .is_some_and(|d| d.connected);
        let label = self.label(addr);
        if connected {
            self.device_action(
                addr,
                "Disconnecting…",
                bt::disconnect(addr),
                move |p, r| match r {
                    Ok(()) => p.ctx.toast(format!("Disconnected {label}")),
                    Err(e) => p.ctx.error(&format!("Couldn't disconnect {label}"), &e),
                },
            );
        } else {
            self.device_action(
                addr,
                "Connecting…",
                bt::connect(addr),
                move |p, r| match r {
                    Ok(()) => p.ctx.toast(format!("Connected to {label}")),
                    Err(e) => p.ctx.error(&format!("Couldn't connect to {label}"), &e),
                },
            );
        }
    }

    fn set_flag(self: &Rc<Self>, addr: Address, trusted: bool, on: bool) {
        let current = self
            .state
            .borrow()
            .devices
            .get(&addr)
            .map(|d| if trusted { d.trusted } else { d.blocked });
        if current.is_none_or(|c| c == on) {
            return;
        }
        let label = self.label(addr);
        let status = match (trusted, on) {
            (true, _) => "Updating…",
            (false, true) => "Blocking…",
            (false, false) => "Unblocking…",
        };
        let fut = async move {
            if trusted {
                bt::set_trusted(addr, on).await
            } else {
                bt::set_blocked(addr, on).await
            }
        };
        self.device_action(addr, status, fut, move |p, r| match r {
            Ok(()) => p.ctx.toast(match (trusted, on) {
                (true, true) => format!("{label} is now trusted"),
                (true, false) => format!("{label} is no longer trusted"),
                (false, true) => format!("Blocked {label}"),
                (false, false) => format!("Unblocked {label}"),
            }),
            Err(e) => {
                p.ctx.error(&format!("Couldn't update {label}"), &e);
                p.sync_device(addr);
            }
        });
    }

    fn rename(self: &Rc<Self>, addr: Address, alias: String) {
        let alias = alias.trim().to_owned();
        if self
            .state
            .borrow()
            .devices
            .get(&addr)
            .is_none_or(|d| d.alias == alias)
        {
            return;
        }
        let reset = alias.is_empty();
        let label = self.label(addr);
        self.device_action(
            addr,
            "Renaming…",
            bt::set_alias(addr, alias.clone()),
            move |p, r| match r {
                Ok(()) if reset => p
                    .ctx
                    .toast(format!("Restored the original name of {label}")),
                Ok(()) => p.ctx.toast(format!("Renamed {label} to {alias}")),
                Err(e) => p.ctx.error(&format!("Couldn't rename {label}"), &e),
            },
        );
    }

    fn forget(self: &Rc<Self>, addr: Address) {
        let label = self.label(addr);
        let p = self.clone();
        glib::spawn_future_local(async move {
            let body = format!(
                "The pairing keys are deleted. To use {label} again, put it in pairing mode and pair it from Nearby devices."
            );
            if !p
                .ctx
                .confirm(&format!("Forget {label}?"), &body, "Forget", true)
                .await
            {
                return;
            }
            p.device_action(addr, "Removing…", bt::forget(addr), move |p, r| match r {
                Ok(()) => p.ctx.toast(format!("Forgot {label}")),
                Err(e) => p.ctx.error(&format!("Couldn't forget {label}"), &e),
            });
        });
    }

    /// Pair (with our agent registered for the duration), then trust and connect.
    fn pair(self: &Rc<Self>, addr: Address) {
        let label = self.label(addr);
        self.set_busy(addr, Some("Pairing…"));
        // Discovery interferes with BR/EDR paging; BlueZ recommends stopping it first.
        self.stop_scan();
        let handle = rt::spawn(bt::pair(addr, agent::agent()));
        self.state
            .borrow_mut()
            .pairing
            .insert(addr, handle.abort_handle());
        let p = self.clone();
        glib::spawn_future_local(async move {
            let result = handle.await;
            p.state.borrow_mut().pairing.remove(&addr);
            p.close_display(addr);
            p.set_busy(addr, None);
            match result {
                Ok(Ok(outcome)) => match outcome.connect_error {
                    None => p.ctx.toast(format!("Paired and connected to {label}")),
                    Some(e) => p
                        .ctx
                        .error(&format!("Paired with {label}, but couldn't connect"), &e),
                },
                Ok(Err(e)) => p.ctx.error(&format!("Couldn't pair with {label}"), &e),
                Err(_) => p.ctx.toast(format!("Pairing with {label} canceled")),
            }
        });
    }

    fn cancel_pairing(&self, addr: Address) {
        if let Some(h) = self.state.borrow_mut().pairing.remove(&addr) {
            h.abort();
        }
    }

    // ---- agent dialogs --------------------------------------------------

    fn handle_request(self: &Rc<Self>, req: Request) {
        match req {
            Request::Display {
                address,
                device,
                code,
                entered,
                done,
            } => {
                self.show_display(address, &device, &code, entered, done);
            }
            Request::PinCode { device, reply } => {
                let entry = gtk::Entry::builder()
                    .placeholder_text("PIN code")
                    .activates_default(true)
                    .build();
                let dialog = alert(
                    &format!("Pair with {device}"),
                    "Enter the PIN code for the device. Devices without a keypad often use 0000 or 1234 (see its manual).",
                    "Pair",
                );
                dialog.set_extra_child(Some(&entry));
                validate(&dialog, &entry, |t| {
                    (1..=16).contains(&t.len()) && t.chars().all(|c| c.is_ascii_alphanumeric())
                });
                self.ask(dialog, reply, move || Some(entry.text().trim().to_owned()));
            }
            Request::Passkey { device, reply } => {
                let entry = gtk::Entry::builder()
                    .placeholder_text("6-digit passkey")
                    .input_purpose(gtk::InputPurpose::Digits)
                    .max_length(6)
                    .activates_default(true)
                    .build();
                let dialog = alert(
                    &format!("Pair with {device}"),
                    &format!("Enter the passkey shown on {device}."),
                    "Pair",
                );
                dialog.set_extra_child(Some(&entry));
                validate(&dialog, &entry, |t| {
                    !t.is_empty() && t.len() <= 6 && t.chars().all(|c| c.is_ascii_digit())
                });
                self.ask(dialog, reply, move || entry.text().parse().ok());
            }
            Request::Confirm {
                device,
                passkey,
                reply,
            } => {
                let dialog = alert(
                    &format!("Pair with {device}"),
                    &format!("Confirm that {device} shows this passkey:"),
                    "Pair",
                );
                dialog.set_extra_child(Some(&code_label(&format!("{passkey:06}"))));
                self.ask(dialog, reply, || Some(()));
            }
            Request::Authorize { device, reply } => {
                let dialog = alert(
                    "Allow pairing?",
                    &format!("{device} wants to pair with this computer."),
                    "Allow",
                );
                self.ask(dialog, reply, || Some(()));
            }
            Request::AuthorizeService {
                device,
                service,
                reply,
            } => {
                let dialog = alert(
                    "Allow connection?",
                    &format!("{device} wants to use “{service}” on this computer."),
                    "Allow",
                );
                self.ask(dialog, reply, || Some(()));
            }
        }
    }

    /// Present `dialog`; "accept" replies with `value()`, anything else rejects. If
    /// BlueZ cancels the request first (reply receiver dropped), close the dialog.
    fn ask<T: 'static>(
        &self,
        dialog: adw::AlertDialog,
        mut reply: Reply<T>,
        value: impl FnOnce() -> Option<T> + 'static,
    ) {
        let window = self.ctx.window.clone();
        glib::spawn_future_local(async move {
            let choice = dialog.clone().choose_future(Some(&window));
            let response = {
                let closed = std::pin::pin!(reply.closed());
                match future::select(choice, closed).await {
                    Either::Left((response, _)) => Some(response),
                    Either::Right(((), choice)) => {
                        dialog.force_close();
                        choice.await;
                        None
                    }
                }
            };
            if let Some(response) = response {
                let answer = if response == "accept" {
                    value().ok_or(ReqError::Rejected)
                } else {
                    Err(ReqError::Rejected)
                };
                let _ = reply.send(answer);
            }
        });
    }

    fn show_display(
        self: &Rc<Self>,
        address: Address,
        device: &str,
        code: &str,
        entered: Option<u16>,
        done: tokio::sync::oneshot::Receiver<()>,
    ) {
        let body = match entered {
            Some(n) if n > 0 => format!(
                "Type this code on {device}, then press Enter ({n} of {} typed):",
                code.len()
            ),
            _ => format!("Type this code on {device}, then press Enter:"),
        };
        let shown = spaced(code);
        let (seq, updated) = {
            let mut st = self.state.borrow_mut();
            st.display_seq += 1;
            let seq = st.display_seq;
            let updated = st
                .display
                .as_mut()
                .filter(|d| d.address == address)
                .map(|d| {
                    d.dialog.set_body(&body);
                    d.code.set_label(&shown);
                    d.seq = seq;
                });
            (seq, updated.is_some())
        };
        if !updated {
            self.close_display_any();
            let dialog = adw::AlertDialog::new(Some(&format!("Pair with {device}")), Some(&body));
            dialog.add_response("cancel", "Cancel Pairing");
            dialog.set_close_response("cancel");
            let code = code_label(&shown);
            dialog.set_extra_child(Some(&code));
            let closed_by_us = Rc::new(Cell::new(false));
            self.state.borrow_mut().display = Some(DisplayDialog {
                address,
                dialog: dialog.clone(),
                code,
                seq,
                closed_by_us: closed_by_us.clone(),
            });
            let p = self.clone();
            let window = self.ctx.window.clone();
            glib::spawn_future_local(async move {
                dialog.clone().choose_future(Some(&window)).await;
                let mut st = p.state.borrow_mut();
                if st.display.as_ref().is_some_and(|d| d.dialog == dialog) {
                    st.display = None;
                }
                drop(st);
                if !closed_by_us.get() {
                    p.cancel_pairing(address);
                }
            });
        }
        // BlueZ cancels (Ok) or the agent goes away / a newer request supersedes (Err).
        let p = self.clone();
        glib::spawn_future_local(async move {
            let _ = done.await;
            let current = p
                .state
                .borrow()
                .display
                .as_ref()
                .is_some_and(|d| d.seq == seq);
            if current {
                p.close_display_any();
            }
        });
    }

    fn close_display(&self, addr: Address) {
        let matches = self
            .state
            .borrow()
            .display
            .as_ref()
            .is_some_and(|d| d.address == addr);
        if matches {
            self.close_display_any();
        }
    }

    fn close_display_any(&self) {
        let display = self.state.borrow_mut().display.take();
        if let Some(d) = display {
            d.closed_by_us.set(true);
            d.dialog.force_close();
        }
    }
}

fn boxed_list(placeholder: &gtk::Label) -> gtk::ListBox {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.set_placeholder(Some(placeholder));
    list
}

fn placeholder(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["dim-label"])
        .build()
}

fn alert(heading: &str, body: &str, accept: &str) -> adw::AlertDialog {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("reject", "Cancel"), ("accept", accept)]);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("accept"));
    dialog.set_close_response("reject");
    dialog
}

fn validate(dialog: &adw::AlertDialog, entry: &gtk::Entry, ok: fn(&str) -> bool) {
    dialog.set_response_enabled("accept", false);
    let d = dialog.clone();
    entry.connect_changed(move |e| d.set_response_enabled("accept", ok(e.text().trim())));
}

fn code_label(code: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(code)
        .selectable(true)
        .css_classes(["title-1", "numeric"])
        .build()
}

/// "123456" → "123 456" for readability.
fn spaced(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_owned()
    }
}

fn subtitle(d: &DeviceInfo, busy: Option<&str>) -> String {
    let mut parts: Vec<String> = vec![d.kind().label().into()];
    if let Some(b) = busy {
        parts.push(b.into());
    } else if d.paired {
        parts.push(
            if d.connected {
                "Connected"
            } else {
                "Not connected"
            }
            .into(),
        );
    }
    if d.paired {
        if let Some(b) = d.battery {
            parts.push(format!("{b}% battery"));
        }
        if d.blocked {
            parts.push("Blocked".into());
        } else if d.trusted {
            parts.push("Trusted".into());
        }
    }
    if let Some(r) = d.rssi {
        parts.push(format!("signal {} ({r} dBm)", signal_label(r)));
    }
    if !d.paired {
        parts.push(d.address.to_string());
    }
    parts.join(" · ")
}

enum Row {
    Paired(PairedRow),
    Nearby(NearbyRow),
}

impl Row {
    fn widget(&self) -> &gtk::ListBoxRow {
        match self {
            Row::Paired(r) => r.expander.upcast_ref(),
            Row::Nearby(r) => r.row.upcast_ref(),
        }
    }

    fn is_paired(&self) -> bool {
        matches!(self, Row::Paired(_))
    }
}

struct PairedRow {
    expander: adw::ExpanderRow,
    icon: gtk::Image,
    spinner: adw::Spinner,
    button: gtk::Button,
    trusted: adw::SwitchRow,
    blocked: adw::SwitchRow,
    name: adw::EntryRow,
    address: adw::ActionRow,
}

impl PairedRow {
    fn new(page: &Rc<Page>, addr: Address) -> Self {
        let expander = adw::ExpanderRow::builder()
            .use_markup(false)
            .name(addr.to_string())
            .build();
        let icon = gtk::Image::new();
        expander.add_prefix(&icon);
        let spinner = adw::Spinner::builder().visible(false).build();
        let button = gtk::Button::builder().valign(gtk::Align::Center).build();
        expander.add_suffix(&spinner);
        expander.add_suffix(&button);

        let name = adw::EntryRow::builder().title("Name").build();
        let trusted = adw::SwitchRow::builder()
            .title("Trusted")
            .subtitle("Let the device reconnect and use services without asking")
            .build();
        let blocked = adw::SwitchRow::builder()
            .title("Blocked")
            .subtitle("Refuse all connections from this device")
            .build();
        let address = adw::ActionRow::builder()
            .title("Address")
            .subtitle(addr.to_string())
            .subtitle_selectable(true)
            .build();
        address.add_css_class("property");
        let forget = adw::ActionRow::builder()
            .title("Forget device")
            .subtitle("Remove the pairing; the device must be paired again to use it")
            .build();
        let forget_btn = gtk::Button::builder()
            .label("Forget…")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        forget.add_suffix(&forget_btn);
        for row in [
            name.upcast_ref::<gtk::Widget>(),
            trusted.upcast_ref(),
            blocked.upcast_ref(),
            address.upcast_ref(),
            forget.upcast_ref(),
        ] {
            expander.add_row(row);
        }

        let p = page.clone();
        button.connect_clicked(move |_| p.toggle_connection(addr));
        let p = page.clone();
        trusted.connect_active_notify(move |r| p.set_flag(addr, true, r.is_active()));
        let p = page.clone();
        blocked.connect_active_notify(move |r| p.set_flag(addr, false, r.is_active()));
        let p = page.clone();
        name.connect_apply(move |e| p.rename(addr, e.text().to_string()));
        let p = page.clone();
        forget_btn.connect_clicked(move |_| p.forget(addr));

        PairedRow {
            expander,
            icon,
            spinner,
            button,
            trusted,
            blocked,
            name,
            address,
        }
    }

    fn update(&self, d: &DeviceInfo, busy: Option<&str>, powered: bool) {
        self.expander.set_title(d.label());
        self.expander.set_subtitle(&subtitle(d, busy));
        self.icon.set_icon_name(Some(d.kind().icon()));
        self.spinner.set_visible(busy.is_some());
        self.button
            .set_label(if d.connected { "Disconnect" } else { "Connect" });
        self.button
            .set_sensitive(busy.is_none() && powered && !d.blocked);
        self.trusted.set_active(d.trusted);
        self.blocked.set_active(d.blocked);
        // Don't clobber an edit in progress; the apply button only appears after user edits.
        if !self
            .name
            .state_flags()
            .contains(gtk::StateFlags::FOCUS_WITHIN)
            && self.name.text() != d.alias
        {
            self.name.set_show_apply_button(false);
            self.name.set_text(&d.alias);
            self.name.set_show_apply_button(true);
        }
        self.address
            .set_subtitle(&format!("{} · {}", d.address, d.kind().label()));
    }
}

struct NearbyRow {
    row: adw::ActionRow,
    icon: gtk::Image,
    spinner: adw::Spinner,
    button: gtk::Button,
}

impl NearbyRow {
    fn new(page: &Rc<Page>, addr: Address) -> Self {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .name(addr.to_string())
            .build();
        let icon = gtk::Image::new();
        row.add_prefix(&icon);
        let spinner = adw::Spinner::builder().visible(false).build();
        let button = gtk::Button::builder()
            .label("Pair")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        row.add_suffix(&spinner);
        row.add_suffix(&button);
        let p = page.clone();
        button.connect_clicked(move |_| p.pair(addr));
        NearbyRow {
            row,
            icon,
            spinner,
            button,
        }
    }

    fn update(&self, d: &DeviceInfo, busy: Option<&str>, powered: bool) {
        self.row.set_title(d.label());
        self.row.set_subtitle(&subtitle(d, busy));
        self.icon.set_icon_name(Some(d.kind().icon()));
        self.spinner.set_visible(busy.is_some());
        self.button.set_visible(busy.is_none());
        self.button.set_sensitive(powered);
    }
}
