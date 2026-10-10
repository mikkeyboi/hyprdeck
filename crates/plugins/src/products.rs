//! Product-level presentation; interface groups only appear in their assigned section.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::controller::ControllerView;
use crate::illustration;
use crate::protocol::{Control, Group, Product, State};

type ControlRenderer = Rc<dyn Fn(&adw::ActionRow, Control)>;

pub(crate) struct Products {
    pub widget: gtk::Box,
    gallery: gtk::FlowBox,
    navigation: gtk::Stack,
    plugin_id: String,
    views: RefCell<HashMap<String, ProductView>>,
    render_control: ControlRenderer,
}

struct ProductView {
    card: gtk::FlowBoxChild,
    card_name: gtk::Label,
    card_connection: gtk::Label,
    card_battery: gtk::Label,
    name: gtk::Label,
    description: gtk::Label,
    connection: gtk::Label,
    battery: gtk::Label,
    body: gtk::Box,
    sections: gtk::Stack,
    tabs: gtk::StackSwitcher,
    groups: HashMap<String, HashMap<String, GroupView>>,
    section_boxes: HashMap<String, gtk::Box>,
    empty_sections: HashMap<String, gtk::Label>,
    navigation_id: String,
}
struct GroupView {
    widget: gtk::Box,
    group: adw::PreferencesGroup,
    list: gtk::ListBox,
    rows: Vec<adw::ActionRow>,
    controller: Option<ControllerView>,
    state: Group,
}

fn text(value: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(value)
        .wrap(true)
        .xalign(0.0)
        .build();
    label.add_css_class(class);
    label
}
fn battery(product: &Product) -> String {
    match &product.battery {
        Some(battery) => match battery.percentage {
            Some(value) => format!(
                "Battery {:.0}%{}",
                value,
                match battery.charging {
                    Some(true) => " · Charging",
                    Some(false) => "",
                    None => " · Charging status unavailable",
                }
            ),
            None => match battery.charging {
                Some(true) => "Charging · Battery level unavailable".into(),
                _ => "Battery unavailable".into(),
            },
        },
        None => "Battery unavailable".into(),
    }
}
fn connection(product: &Product) -> String {
    [product.connection.as_str(), product.status.as_str()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn control_target(control: &Control) -> (&str, &serde_json::Map<String, serde_json::Value>) {
    match control {
        Control::Button { action, args, .. }
        | Control::Switch { action, args, .. }
        | Control::Number { action, args, .. }
        | Control::Choice { action, args, .. }
        | Control::Text { action, args, .. }
        | Control::Color { action, args, .. }
        | Control::Form { action, args, .. } => (action, args),
    }
}

pub(crate) fn compatible_control(old: &Control, new: &Control) -> bool {
    if std::mem::discriminant(old) != std::mem::discriminant(new)
        || control_target(old) != control_target(new)
    {
        return false;
    }
    match (old, new) {
        (
            Control::Number {
                min: a,
                max: b,
                step: c,
                ..
            },
            Control::Number {
                min: x,
                max: y,
                step: z,
                ..
            },
        ) => (a, b, c) == (x, y, z),
        (Control::Choice { options: a, .. }, Control::Choice { options: b, .. }) => a == b,
        (Control::Form { fields: a, .. }, Control::Form { fields: b, .. }) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(a, b)| {
                    a.id == b.id
                        && a.kind == b.kind
                        && a.min == b.min
                        && a.max == b.max
                        && a.step == b.step
                        && a.options == b.options
                })
        }
        _ => true,
    }
}
impl Products {
    pub(crate) fn new(
        plugin_id: &str,
        navigation: &gtk::Stack,
        render_control: impl Fn(&adw::ActionRow, Control) + 'static,
    ) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let gallery = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .min_children_per_line(1)
            .max_children_per_line(3)
            .column_spacing(18)
            .row_spacing(18)
            .homogeneous(true)
            .build();
        widget.append(&gallery);
        Self {
            widget,
            gallery,
            navigation: navigation.clone(),
            plugin_id: plugin_id.into(),
            views: RefCell::new(HashMap::new()),
            render_control: Rc::new(render_control),
        }
    }

    pub(crate) fn update(&self, state: &State, preserve_edits: bool, focus: Option<&gtk::Widget>) {
        let mut views = self.views.borrow_mut();
        let removed: Vec<_> = views
            .keys()
            .filter(|id| !state.products.iter().any(|p| &p.id == *id))
            .cloned()
            .collect();
        for id in removed {
            if let Some(view) = views.remove(&id) {
                // A disappeared device must never retain working action controls.
                self.gallery.remove(&view.card);
                for (_, section) in view.section_boxes {
                    view.sections.remove(&section);
                }
                view.connection.set_text("Disconnected");
                view.battery.set_visible(false);
                view.tabs.set_visible(false);
                view.body.append(&text("This product is no longer available. Return to Products to choose a connected device.", "dim-label"));
                if let Some(child) = self.navigation.child_by_name(&view.navigation_id) {
                    child.add_css_class("hd-disconnected-product");
                }
                // Retain only the currently visible disconnected page until Back.
                if self.navigation.visible_child_name().as_deref() != Some(&view.navigation_id)
                    && let Some(child) = self.navigation.child_by_name(&view.navigation_id)
                {
                    self.navigation.remove(&child);
                }
            }
        }
        for product in &state.products {
            if !views.contains_key(&product.id) {
                let view = ProductView::new(product, &self.plugin_id, &self.navigation);
                self.gallery.insert(&view.card, -1);
                views.insert(product.id.clone(), view);
            }
            if let Some(view) = views.get_mut(&product.id) {
                view.update(product, state, &self.render_control, preserve_edits, focus);
            }
        }
    }

    pub(crate) fn disconnect(&self) {
        for view in self.views.borrow().values() {
            view.connection
                .set_text("Backend unavailable · Controls paused");
            view.sections.set_sensitive(false);
        }
    }

    pub(crate) fn set_sensitive(&self, sensitive: bool) {
        for view in self.views.borrow().values() {
            view.sections.set_sensitive(sensitive);
        }
    }
}

impl ProductView {
    fn new(product: &Product, plugin_id: &str, navigation: &gtk::Stack) -> Self {
        let navigation_id = format!("product-{plugin_id}-{}", product.id);
        let reconnecting = navigation.visible_child_name().as_deref() == Some(&navigation_id);
        if let Some(previous) = navigation.child_by_name(&navigation_id) {
            navigation.remove(&previous);
        }
        let card_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card_content.set_margin_top(16);
        card_content.set_margin_bottom(16);
        card_content.set_margin_start(16);
        card_content.set_margin_end(16);
        card_content.append(&illustration::product(&product.kind, false));
        let card_name = text(&product.name, "title-3");
        let card_connection = text(&connection(product), "dim-label");
        let card_battery = text(&battery(product), "caption");
        card_content.append(&card_name);
        card_content.append(&card_connection);
        card_content.append(&card_battery);
        let open = gtk::Button::builder()
            .child(&card_content)
            .hexpand(true)
            .build();
        open.add_css_class("card");
        open.set_tooltip_text(Some(&format!("Open {}", product.name)));
        open.update_property(&[gtk::accessible::Property::Label(&format!(
            "Open {}",
            product.name
        ))]);
        let card = gtk::FlowBoxChild::new();
        card.set_child(Some(&open));
        let page = gtk::Box::new(gtk::Orientation::Vertical, 18);
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let back = gtk::Button::with_label("‹ Products");
        back.set_tooltip_text(Some("Back to products"));
        toolbar.append(&back);
        page.append(&toolbar);
        let name = text(&product.name, "title-1");
        let description = text(&product.description, "dim-label");
        let connection = text(&connection(product), "body");
        let battery = text(&battery(product), "caption");
        page.append(&name);
        page.append(&description);
        let metadata = gtk::Box::new(gtk::Orientation::Vertical, 4);
        metadata.append(&connection);
        metadata.append(&battery);
        page.append(&metadata);
        let sections = gtk::Stack::builder()
            .hhomogeneous(false)
            .vhomogeneous(false)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let tabs = gtk::StackSwitcher::builder()
            .stack(&sections)
            .halign(gtk::Align::Start)
            .build();
        let tab_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .child(&tabs)
            .build();
        page.append(&tab_scroller);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
        body.append(&sections);
        page.append(&body);
        navigation.add_named(&page, Some(&navigation_id));
        if reconnecting {
            navigation.set_visible_child_name(&navigation_id);
        }
        let nav = navigation.downgrade();
        let target = navigation_id.clone();
        let focus_back = back.downgrade();
        open.connect_clicked(move |_| {
            if let Some(nav) = nav.upgrade() {
                nav.set_visible_child_name(&target);
            }
            if let Some(back) = focus_back.upgrade() {
                back.grab_focus();
            }
        });
        let nav = navigation.downgrade();
        let focus_open = open.downgrade();
        let page_weak = page.downgrade();
        back.connect_clicked(move |_| {
            if let Some(nav) = nav.upgrade() {
                nav.set_visible_child_name("products");
                if let Some(page) = page_weak
                    .upgrade()
                    .filter(|page| page.has_css_class("hd-disconnected-product"))
                {
                    nav.remove(&page);
                }
            }
            if let Some(open) = focus_open.upgrade() {
                open.grab_focus();
            }
        });
        let nav = navigation.downgrade();
        let focus_open = open.downgrade();
        let page_weak = page.downgrade();
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            if key == gtk::gdk::Key::Escape
                || (key == gtk::gdk::Key::Left
                    && modifiers.contains(gtk::gdk::ModifierType::ALT_MASK))
            {
                if let Some(nav) = nav.upgrade() {
                    nav.set_visible_child_name("products");
                    if let Some(page) = page_weak
                        .upgrade()
                        .filter(|page| page.has_css_class("hd-disconnected-product"))
                    {
                        nav.remove(&page);
                    }
                }
                if let Some(open) = focus_open.upgrade() {
                    open.grab_focus();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        page.add_controller(keys);
        Self {
            card,
            card_name,
            card_connection,
            card_battery,
            name,
            description,
            connection,
            battery,
            body,
            sections,
            tabs,
            groups: HashMap::new(),
            section_boxes: HashMap::new(),
            empty_sections: HashMap::new(),
            navigation_id,
        }
    }

    fn update(
        &mut self,
        product: &Product,
        state: &State,
        render: &ControlRenderer,
        preserve: bool,
        focus: Option<&gtk::Widget>,
    ) {
        if self.card_name.text() != product.name {
            self.card_name.set_text(&product.name);
        }
        let connection = connection(product);
        if self.card_connection.text() != connection {
            self.card_connection.set_text(&connection);
        }
        if self.connection.text() != connection {
            self.connection.set_text(&connection);
        }
        let battery = battery(product);
        if self.card_battery.text() != battery {
            self.card_battery.set_text(&battery);
        }
        if self.battery.text() != battery {
            self.battery.set_text(&battery);
        }
        if self.name.text() != product.name {
            self.name.set_text(&product.name);
        }
        if self.description.text() != product.description {
            self.description.set_text(&product.description);
        }
        self.battery.set_tooltip_text(
            product
                .battery
                .as_ref()
                .filter(|b| !b.source.is_empty())
                .map(|b| b.source.as_str()),
        );
        self.sections.set_sensitive(true);
        let section_ids: Vec<_> = self
            .section_boxes
            .keys()
            .filter(|id| !product.sections.iter().any(|section| &section.id == *id))
            .cloned()
            .collect();
        for id in section_ids {
            if let Some(widget) = self.section_boxes.remove(&id) {
                self.sections.remove(&widget);
            }
            self.empty_sections.remove(&id);
        }
        // Nested identity maps avoid rebuilding composite strings on every live sample.
        let removed_sections: Vec<_> = self
            .groups
            .keys()
            .filter(|id| !product.sections.iter().any(|section| &section.id == *id))
            .cloned()
            .collect();
        for id in removed_sections {
            self.groups.remove(&id);
        }
        for section in &product.sections {
            if !self.section_boxes.contains_key(&section.id) {
                let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
                if section.id == "overview" && product.kind != "controller" {
                    content.append(&illustration::product(&product.kind, true));
                }
                self.sections
                    .add_titled(&content, Some(&section.id), &section.title);
                self.section_boxes.insert(section.id.clone(), content);
            }
            let section_box = self
                .section_boxes
                .get(&section.id)
                .expect("section inserted");
            self.sections.page(section_box).set_title(&section.title);
            if !self.empty_sections.contains_key(&section.id) {
                let message = text(
                    "No settings are available for this section from the connected backend.",
                    "dim-label",
                );
                section_box.append(&message);
                self.empty_sections.insert(section.id.clone(), message);
            }
            let empty = self
                .empty_sections
                .get(&section.id)
                .expect("section message inserted");
            empty.set_visible(!section.groups.iter().any(|id| {
                state.groups.iter().any(|group| {
                    &group.id == id
                        && (!group.rows.is_empty()
                            || !group.description.is_empty()
                            || group.visualization.is_some())
                })
            }));
            if !self.groups.contains_key(&section.id) {
                self.groups.insert(section.id.clone(), HashMap::new());
            }
            let groups = self.groups.get_mut(&section.id).expect("section inserted");
            let stale: Vec<_> = groups
                .keys()
                .filter(|id| !section.groups.contains(id))
                .cloned()
                .collect();
            for id in stale {
                if let Some(group) = groups.remove(&id) {
                    section_box.remove(&group.widget);
                }
            }
            for id in &section.groups {
                let Some(group) = state.groups.iter().find(|group| &group.id == id) else {
                    continue;
                };
                if !groups.contains_key(id) {
                    let rendered = GroupView::new(group, render);
                    section_box.append(&rendered.widget);
                    groups.insert(id.clone(), rendered);
                }
                let rendered = groups.get_mut(id).expect("group inserted");
                rendered.update(group, render, preserve, focus);
            }
        }
    }
}

impl GroupView {
    fn new(state: &Group, render: &ControlRenderer) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let controller = state.visualization.as_ref().map(|visualization| {
            let render = render.clone();
            let view = ControllerView::new(visualization, move |row, control| render(row, control));
            widget.append(&view.widget);
            view
        });
        let group = adw::PreferencesGroup::builder()
            .title(glib::markup_escape_text(&state.title))
            .description(glib::markup_escape_text(&state.description))
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        group.add(&list);
        group.set_visible(
            !state.rows.is_empty()
                || (state.visualization.is_none() && !state.description.is_empty()),
        );
        widget.append(&group);
        let rows = state
            .rows
            .iter()
            .map(|item| {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&item.title);
                row.set_subtitle(&item.subtitle);
                if let Some(control) = &item.control {
                    render(&row, control.clone());
                }
                list.append(&row);
                row
            })
            .collect();
        Self {
            widget,
            group,
            list,
            rows,
            controller,
            state: state.clone(),
        }
    }
    fn update(
        &mut self,
        state: &Group,
        render: &ControlRenderer,
        preserve: bool,
        focus: Option<&gtk::Widget>,
    ) {
        if self.state == *state {
            return;
        }
        self.group
            .set_title(&glib::markup_escape_text(&state.title));
        self.group
            .set_description(Some(&glib::markup_escape_text(&state.description)));
        self.group.set_visible(
            !state.rows.is_empty()
                || (state.visualization.is_none() && !state.description.is_empty()),
        );
        match (&self.controller, &state.visualization) {
            (Some(controller), Some(visualization)) => controller.update(visualization),
            (Some(controller), None) => {
                self.widget.remove(&controller.widget);
                self.controller = None;
            }
            (None, Some(visualization)) => {
                let render = render.clone();
                let controller =
                    ControllerView::new(visualization, move |row, control| render(row, control));
                self.widget.prepend(&controller.widget);
                self.controller = Some(controller);
            }
            (None, None) => {}
        }
        let same_rows = self.state.rows.len() == state.rows.len()
            && self
                .state
                .rows
                .iter()
                .zip(&state.rows)
                .all(|(a, b)| a.id == b.id);
        if !same_rows {
            // Reconcile by row identity: capability removal revokes only removed actions,
            // while edits in unrelated fields and the controller inspector survive.
            let mut old: HashMap<_, _> = self
                .rows
                .drain(..)
                .zip(self.state.rows.drain(..))
                .map(|(row, state)| (state.id.clone(), (row, state)))
                .collect();
            for (index, item) in state.rows.iter().enumerate() {
                let (row, snapshot) = old.remove(&item.id).unwrap_or_else(|| {
                    let row = adw::ActionRow::builder().use_markup(false).build();
                    row.set_title(&item.title);
                    row.set_subtitle(&item.subtitle);
                    if let Some(control) = &item.control {
                        render(&row, control.clone());
                    }
                    (row, item.clone())
                });
                if self
                    .list
                    .row_at_index(index as i32)
                    .as_ref()
                    .is_none_or(|current| current != row.upcast_ref::<gtk::ListBoxRow>())
                {
                    if row.parent().is_some() {
                        self.list.remove(&row);
                    }
                    self.list.insert(&row, index as i32);
                }
                self.rows.push(row);
                self.state.rows.push(snapshot);
            }
            for (_, (row, _)) in old {
                self.list.remove(&row);
            }
        }
        for (index, item) in state.rows.iter().enumerate() {
            let row = &self.rows[index];
            if row.title() != item.title {
                row.set_title(&item.title);
            }
            if row.subtitle().as_deref().unwrap_or("") != item.subtitle {
                row.set_subtitle(&item.subtitle);
            }
            let editing = preserve
                || row.has_css_class("hd-pending-edit")
                || focus.is_some_and(|focus| {
                    focus == row.upcast_ref::<gtk::Widget>() || focus.is_ancestor(row)
                });
            let same_action = match (&self.state.rows[index].control, &item.control) {
                (Some(old), Some(new)) => compatible_control(old, new),
                (None, None) => true,
                _ => false,
            };
            if self.state.rows[index].control != item.control && (!editing || !same_action) {
                let replacement = adw::ActionRow::builder().use_markup(false).build();
                replacement.set_title(&item.title);
                replacement.set_subtitle(&item.subtitle);
                if let Some(control) = &item.control {
                    render(&replacement, control.clone());
                }
                self.list.remove(row);
                self.list.insert(&replacement, index as i32);
                self.rows[index] = replacement;
                self.state.rows[index].control.clone_from(&item.control);
            }
            self.state.rows[index].title.clone_from(&item.title);
            self.state.rows[index].subtitle.clone_from(&item.subtitle);
        }
        self.state.title.clone_from(&state.title);
        self.state.description.clone_from(&state.description);
        self.state.visualization.clone_from(&state.visualization);
    }
}
