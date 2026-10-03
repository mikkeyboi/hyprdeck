//! Physical grouping of the many input (sub-)devices Hyprland reports.

use std::collections::HashSet;

use hyprdeck_core::hypr::ctl;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DevKind {
    Keyboard,
    Pointer,
    Touchpad,
    Tablet,
    Touch,
}

impl DevKind {
    pub fn label(self) -> &'static str {
        match self {
            DevKind::Keyboard => "keyboard",
            DevKind::Pointer => "pointer",
            DevKind::Touchpad => "touchpad",
            DevKind::Tablet => "tablet",
            DevKind::Touch => "touch screen",
        }
    }

    pub fn is_pointer(self) -> bool {
        matches!(self, DevKind::Pointer | DevKind::Touchpad)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupClass {
    Physical,
    System,
    Virtual,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub kind: DevKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceGroup {
    pub base: String,
    pub label: String,
    pub class: GroupClass,
    pub members: Vec<Member>,
}

impl DeviceGroup {
    pub fn has(&self, f: impl Fn(DevKind) -> bool) -> bool {
        self.members.iter().any(|m| f(m.kind))
    }

    pub fn names_where(&self, f: impl Fn(DevKind) -> bool) -> Vec<String> {
        self.members
            .iter()
            .filter(|m| f(m.kind))
            .map(|m| m.name.clone())
            .collect()
    }

    /// `keyboard, pointer` summary of member kinds.
    pub fn kinds(&self) -> String {
        let mut kinds: Vec<DevKind> = self.members.iter().map(|m| m.kind).collect();
        kinds.sort();
        kinds.dedup();
        kinds
            .iter()
            .map(|k| k.label())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

const SUFFIXES: [&str; 9] = [
    "-keyboard",
    "-mouse",
    "-consumer-control",
    "-system-control",
    "-pointer",
    "-stylus",
    "-pen",
    "-eraser",
    "-touchpad",
];

/// Strip sub-device suffixes: `acme-…-dongle-keyboard-1` → `acme-…-dongle`.
pub fn base_name(name: &str) -> &str {
    let mut s = name;
    loop {
        let before = s.len();
        if let Some((head, tail)) = s.rsplit_once('-')
            && !tail.is_empty()
            && tail.chars().all(|c| c.is_ascii_digit())
            && !head.is_empty()
        {
            s = head;
        }
        for suf in SUFFIXES {
            if let Some(head) = s.strip_suffix(suf)
                && !head.is_empty()
            {
                s = head;
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

fn class_of(base: &str) -> GroupClass {
    if base.contains("virtual") {
        GroupClass::Virtual
    } else if matches!(
        base,
        "video-bus" | "power-button" | "sleep-button" | "lid-switch" | "pc-speaker"
    ) || base.starts_with("intel-hid")
        || base.starts_with("asus-wmi")
        || base.starts_with("thinkpad-extra")
    {
        GroupClass::System
    } else {
        GroupClass::Physical
    }
}

/// `acme-acme-gaming-mouse-dongle` → `Acme Gaming Mouse Dongle`;
/// `hl-virtual-keyboard-shell` → `Shell virtual keyboard`.
pub fn humanize(base: &str) -> String {
    if let Some(owner) = base.strip_prefix("hl-virtual-keyboard-") {
        return format!("{} virtual keyboard", title_words(owner));
    }
    if let Some(owner) = base.strip_suffix("-virtual") {
        return format!("{} virtual devices", title_words(owner));
    }
    title_words(base)
}

fn title_words(s: &str) -> String {
    let mut words: Vec<&str> = s.split('-').filter(|w| !w.is_empty()).collect();
    if words.len() > 1 && words[0] == words[1] {
        words.remove(0);
    }
    words
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Group devices by physical device. `touchpads` holds normalized names of
/// libinput touchpads (see [`touchpad_names`]).
pub fn group(devs: &ctl::Devices, touchpads: &HashSet<String>) -> Vec<DeviceGroup> {
    let mut all: Vec<Member> = Vec::new();
    let mut push = |name: &str, kind: DevKind| {
        if !all.iter().any(|m| m.name == name) {
            all.push(Member {
                name: name.to_owned(),
                kind,
            });
        }
    };
    for k in &devs.keyboards {
        push(&k.name, DevKind::Keyboard);
    }
    for m in &devs.mice {
        let tp = touchpads.contains(&m.name) || touchpads.contains(strip_index(&m.name));
        push(
            &m.name,
            if tp {
                DevKind::Touchpad
            } else {
                DevKind::Pointer
            },
        );
    }
    for t in &devs.tablets {
        push(&t.name, DevKind::Tablet);
    }
    for t in &devs.touch {
        push(&t.name, DevKind::Touch);
    }

    let mut groups: Vec<DeviceGroup> = Vec::new();
    for m in all {
        let base = base_name(&m.name);
        match groups.iter_mut().find(|g| g.base == base) {
            Some(g) => g.members.push(m),
            None => groups.push(DeviceGroup {
                base: base.to_owned(),
                label: String::new(),
                class: class_of(base),
                members: vec![m],
            }),
        }
    }
    // Fold groups whose base extends another's (`example-virtual-absolute`
    // into `example-virtual`).
    let mut i = 0;
    while i < groups.len() {
        let parent = (0..groups.len()).find(|&j| {
            j != i
                && groups[i].base.len() > groups[j].base.len()
                && groups[i].base.starts_with(&groups[j].base)
                && groups[i].base.as_bytes()[groups[j].base.len()] == b'-'
                && groups[j].base.contains('-')
        });
        match parent {
            Some(j) => {
                let g = groups.remove(i);
                let j = if j > i { j - 1 } else { j };
                groups[j].members.extend(g.members);
                i = 0;
            }
            None => i += 1,
        }
    }
    for g in &mut groups {
        g.label = humanize(&g.base);
        g.members.sort_by(|a, b| a.name.cmp(&b.name));
    }
    groups.sort_by(|a, b| a.class.cmp(&b.class).then_with(|| a.label.cmp(&b.label)));
    groups
}

fn strip_index(name: &str) -> &str {
    match name.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => name,
    }
}

/// Hyprland's device name for a kernel input name: lowercase, spaces → `-`.
pub fn normalize(kernel_name: &str) -> String {
    kernel_name.trim().to_lowercase().replace(' ', "-")
}

/// Names (Hyprland-normalized) of devices udev tags as touchpads (blocking).
pub fn touchpad_names() -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/input") else {
        return out;
    };
    for e in dir.flatten() {
        let file_name = e.file_name();
        if !file_name.to_string_lossy().starts_with("event") {
            continue;
        }
        let p = e.path();
        let (Ok(name), Ok(dev)) = (
            std::fs::read_to_string(p.join("device/name")),
            std::fs::read_to_string(p.join("dev")),
        ) else {
            continue;
        };
        let data =
            std::fs::read_to_string(format!("/run/udev/data/c{}", dev.trim())).unwrap_or_default();
        if data.lines().any(|l| l == "E:ID_INPUT_TOUCHPAD=1") {
            out.insert(normalize(&name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb(name: &str) -> ctl::Keyboard {
        ctl::Keyboard {
            name: name.into(),
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
            active_keymap: String::new(),
            caps_lock: false,
            num_lock: false,
            main: false,
        }
    }

    fn mouse(name: &str) -> ctl::Pointer {
        ctl::Pointer {
            name: name.into(),
            default_speed: 0.0,
            scroll_factor: -1.0,
        }
    }

    #[test]
    fn base_names_strip_sub_devices() {
        assert_eq!(
            base_name("acme-acme-gaming-mouse-dongle-keyboard-1"),
            "acme-acme-gaming-mouse-dongle"
        );
        assert_eq!(
            base_name("generic-wireless-receiver-mouse"),
            "generic-wireless-receiver"
        );
        assert_eq!(
            base_name("example-usb-headset-consumer-control"),
            "example-usb-headset"
        );
        assert_eq!(base_name("power-button-1"), "power-button");
        assert_eq!(base_name("example-virtual-mouse-1"), "example-virtual");
        assert_eq!(base_name("mouse"), "mouse");
    }

    #[test]
    fn groups_real_device_list() {
        let devs = ctl::Devices {
            keyboards: vec![
                kb("acme-acme-gaming-mouse-dongle-keyboard"),
                kb("acme-acme-gaming-mouse-dongle-1"),
                kb("hl-virtual-keyboard-shell"),
                kb("example-virtual-keyboard"),
                kb("power-button"),
                kb("power-button-1"),
                kb("video-bus"),
            ],
            mice: vec![
                mouse("acme-acme-gaming-mouse-dongle"),
                mouse("acme-acme-gaming-mouse-dongle-keyboard-1"),
                mouse("example-virtual-mouse-1"),
                mouse("example-virtual-absolute-mouse"),
                mouse("generic-wireless-controller-touchpad"),
            ],
            tablets: vec![],
            touch: vec![],
            switches: vec![],
        };
        let tps = HashSet::from(["generic-wireless-controller-touchpad".to_owned()]);
        let g = group(&devs, &tps);
        let labels: Vec<_> = g
            .iter()
            .map(|g| (g.label.as_str(), g.class, g.members.len()))
            .collect();
        assert_eq!(
            labels,
            [
                ("Acme Gaming Mouse Dongle", GroupClass::Physical, 4),
                ("Generic Wireless Controller", GroupClass::Physical, 1),
                ("Power Button", GroupClass::System, 2),
                ("Video Bus", GroupClass::System, 1),
                ("Example virtual devices", GroupClass::Virtual, 3),
                ("Shell virtual keyboard", GroupClass::Virtual, 1),
            ]
        );
        assert!(g[1].has(|k| k == DevKind::Touchpad));
        assert_eq!(g[0].kinds(), "keyboard, pointer");
    }
}
