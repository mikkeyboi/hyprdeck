//! Pure device logic: plain snapshots of BlueZ objects, device-kind/icon mapping,
//! naming, sorting, readable errors and CLI target resolution.

use std::cmp::Ordering;

use bluer::{AdapterProperty, Address, DeviceProperty, ErrorKind};

/// Snapshot of the adapter (org.bluez.Adapter1).
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    /// Kernel name, e.g. `hci0`.
    pub name: String,
    pub alias: String,
    pub address: Address,
    pub powered: bool,
    pub discoverable: bool,
    pub discoverable_timeout: u32,
    pub pairable: bool,
    pub discovering: bool,
}

impl AdapterInfo {
    pub fn apply(&mut self, prop: &AdapterProperty) {
        match prop {
            AdapterProperty::Alias(v) => self.alias.clone_from(v),
            AdapterProperty::Powered(v) => self.powered = *v,
            AdapterProperty::Discoverable(v) => self.discoverable = *v,
            AdapterProperty::DiscoverableTimeout(v) => self.discoverable_timeout = *v,
            AdapterProperty::Pairable(v) => self.pairable = *v,
            AdapterProperty::Discovering(v) => self.discovering = *v,
            _ => {}
        }
    }
}

/// Snapshot of one remote device (org.bluez.Device1 + Battery1).
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub address: Address,
    /// Remote name as advertised (absent for many LE beacons).
    pub name: Option<String>,
    /// User alias; BlueZ falls back to the name, then to the dashed address.
    pub alias: String,
    pub icon: Option<String>,
    pub class: Option<u32>,
    pub appearance: Option<u16>,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub blocked: bool,
    /// Signal strength; only present while a discovery is running and the device is in range.
    pub rssi: Option<i16>,
    pub battery: Option<u8>,
}

/// What a consumer must do after applying a property change.
#[derive(Debug, PartialEq, Eq)]
pub enum Followup {
    None,
    /// Re-read the whole device (interfaces such as Battery1 come and go with the connection).
    Refetch,
}

impl DeviceInfo {
    pub fn apply(&mut self, prop: &DeviceProperty) -> Followup {
        match prop {
            DeviceProperty::Name(v) => self.name = Some(v.clone()),
            DeviceProperty::Alias(v) => self.alias.clone_from(v),
            DeviceProperty::Icon(v) => self.icon = Some(v.clone()),
            DeviceProperty::Class(v) => self.class = Some(*v),
            DeviceProperty::Appearance(v) => self.appearance = Some(*v),
            DeviceProperty::Paired(v) => self.paired = *v,
            DeviceProperty::Connected(v) => {
                self.connected = *v;
                return Followup::Refetch;
            }
            DeviceProperty::ServicesResolved(true) => return Followup::Refetch,
            DeviceProperty::Trusted(v) => self.trusted = *v,
            DeviceProperty::Blocked(v) => self.blocked = *v,
            DeviceProperty::Rssi(v) => self.rssi = Some(*v),
            DeviceProperty::BatteryPercentage(v) => self.battery = Some(*v),
            _ => {}
        }
        Followup::None
    }

    /// Whether the device has a real name (not just BlueZ's dashed-address fallback alias).
    pub fn is_named(&self) -> bool {
        has_name(&self.alias, self.name.as_deref(), self.address)
    }

    /// Name to show in the UI.
    pub fn label(&self) -> &str {
        if self.is_named() {
            &self.alias
        } else {
            "Unknown device"
        }
    }

    pub fn kind(&self) -> DeviceKind {
        DeviceKind::detect(self.icon.as_deref(), self.class, self.appearance)
    }

    /// Unpaired device currently seen by a discovery (RSSI is only valid while discovering).
    pub fn is_nearby(&self) -> bool {
        !self.paired && self.rssi.is_some()
    }
}

/// BlueZ sets `Alias` to the address with dashes when the device has no name.
pub fn has_name(alias: &str, name: Option<&str>, address: Address) -> bool {
    if name.is_some_and(|n| !n.trim().is_empty()) {
        return true;
    }
    let alias = alias.trim();
    !alias.is_empty() && !alias.eq_ignore_ascii_case(&address.to_string().replace(':', "-"))
}

/// Coarse device category, used for the icon and a short type label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Headset,
    Headphones,
    Speaker,
    Microphone,
    Audio,
    Keyboard,
    Mouse,
    Gamepad,
    Tablet,
    Phone,
    Computer,
    Watch,
    Display,
    Camera,
    VideoCamera,
    Printer,
    Scanner,
    MediaPlayer,
    Network,
    Modem,
    Other,
}

impl DeviceKind {
    /// Prefer BlueZ's own `Icon` (derived from class/appearance), then the class of
    /// device (BR/EDR), then the GAP appearance (LE).
    pub fn detect(icon: Option<&str>, class: Option<u32>, appearance: Option<u16>) -> Self {
        icon.and_then(Self::from_icon)
            .or_else(|| class.and_then(Self::from_class))
            .or_else(|| appearance.and_then(Self::from_appearance))
            .unwrap_or(Self::Other)
    }

    /// BlueZ `Icon` property values (freedesktop icon names).
    pub fn from_icon(icon: &str) -> Option<Self> {
        Some(match icon {
            "audio-headset" => Self::Headset,
            "audio-headphones" => Self::Headphones,
            "audio-speakers" => Self::Speaker,
            "audio-input-microphone" => Self::Microphone,
            "audio-card" => Self::Audio,
            "input-keyboard" => Self::Keyboard,
            "input-mouse" => Self::Mouse,
            "input-gaming" => Self::Gamepad,
            "input-tablet" => Self::Tablet,
            "phone" => Self::Phone,
            "computer" => Self::Computer,
            "watch" => Self::Watch,
            "video-display" => Self::Display,
            "camera-photo" => Self::Camera,
            "camera-video" => Self::VideoCamera,
            "printer" => Self::Printer,
            "scanner" => Self::Scanner,
            "multimedia-player" => Self::MediaPlayer,
            "network-wireless" => Self::Network,
            "modem" => Self::Modem,
            _ => return None,
        })
    }

    /// Bluetooth class of device: major class in bits 8–12, minor in bits 2–7.
    pub fn from_class(class: u32) -> Option<Self> {
        let minor = (class >> 2) & 0x3f;
        Some(match (class >> 8) & 0x1f {
            0x01 => Self::Computer,
            0x02 => match minor {
                0x04 | 0x05 => Self::Modem,
                _ => Self::Phone,
            },
            0x03 => Self::Network,
            0x04 => match minor {
                0x01 | 0x02 => Self::Headset,
                0x04 => Self::Microphone,
                0x05 | 0x0a => Self::Speaker,
                0x06 => Self::Headphones,
                0x07 => Self::MediaPlayer,
                0x0b..=0x0d => Self::VideoCamera,
                0x0e | 0x0f => Self::Display,
                _ => Self::Audio,
            },
            0x05 => match ((class >> 6) & 0x03, minor & 0x0f) {
                (_, 0x01 | 0x02) => Self::Gamepad,
                (_, 0x05) => Self::Tablet,
                (0x01 | 0x03, _) => Self::Keyboard,
                (0x02, _) => Self::Mouse,
                _ => return None,
            },
            0x06 => {
                if class & 0x80 != 0 {
                    Self::Printer
                } else if class & 0x40 != 0 {
                    Self::Scanner
                } else if class & 0x20 != 0 {
                    Self::Camera
                } else if class & 0x10 != 0 {
                    Self::Display
                } else {
                    return None;
                }
            }
            0x07 if minor & 0x0f == 0x01 => Self::Watch,
            _ => return None,
        })
    }

    /// GAP appearance: category in bits 6–15, subcategory in bits 0–5.
    pub fn from_appearance(appearance: u16) -> Option<Self> {
        let sub = appearance & 0x3f;
        Some(match appearance >> 6 {
            0x001 => Self::Phone,
            0x002 => Self::Computer,
            0x003 => Self::Watch,
            0x005 => Self::Display,
            0x00a => Self::MediaPlayer,
            0x00b => Self::Scanner,
            0x00f => match sub {
                0x01 => Self::Keyboard,
                0x02 => Self::Mouse,
                0x03 | 0x04 => Self::Gamepad,
                0x05 => Self::Tablet,
                _ => return None,
            },
            0x021 => Self::Speaker,
            0x022 => Self::Microphone,
            0x025 => match sub {
                0x03 => Self::Headphones,
                _ => Self::Headset,
            },
            _ => return None,
        })
    }

    /// Symbolic icon available in the Adwaita theme.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Headset => "audio-headset-symbolic",
            Self::Headphones => "audio-headphones-symbolic",
            Self::Speaker => "audio-speakers-symbolic",
            Self::Microphone => "audio-input-microphone-symbolic",
            Self::Audio => "audio-card-symbolic",
            Self::Keyboard => "input-keyboard-symbolic",
            Self::Mouse => "input-mouse-symbolic",
            Self::Gamepad => "input-gaming-symbolic",
            Self::Tablet => "input-tablet-symbolic",
            Self::Phone => "phone-symbolic",
            Self::Computer => "computer-symbolic",
            Self::Display => "video-display-symbolic",
            Self::Camera => "camera-photo-symbolic",
            Self::VideoCamera => "camera-video-symbolic",
            Self::Printer => "printer-symbolic",
            Self::Scanner => "scanner-symbolic",
            Self::MediaPlayer => "multimedia-player-symbolic",
            Self::Network => "network-wireless-symbolic",
            Self::Modem => "modem-symbolic",
            Self::Watch | Self::Other => "bluetooth-symbolic",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Headset => "Headset",
            Self::Headphones => "Headphones",
            Self::Speaker => "Speaker",
            Self::Microphone => "Microphone",
            Self::Audio => "Audio device",
            Self::Keyboard => "Keyboard",
            Self::Mouse => "Mouse",
            Self::Gamepad => "Game controller",
            Self::Tablet => "Tablet",
            Self::Phone => "Phone",
            Self::Computer => "Computer",
            Self::Watch => "Watch",
            Self::Display => "Display",
            Self::Camera => "Camera",
            Self::VideoCamera => "Video camera",
            Self::Printer => "Printer",
            Self::Scanner => "Scanner",
            Self::MediaPlayer => "Media player",
            Self::Network => "Network access point",
            Self::Modem => "Modem",
            Self::Other => "Device",
        }
    }
}

/// Short signal-quality word for an RSSI in dBm.
pub fn signal_label(rssi: i16) -> &'static str {
    match rssi {
        -60.. => "excellent",
        -70..=-61 => "good",
        -80..=-71 => "fair",
        _ => "weak",
    }
}

/// Nearby list order: strongest signal first (devices without RSSI last), then by name.
pub fn cmp_nearby(a: &DeviceInfo, b: &DeviceInfo) -> Ordering {
    b.rssi
        .is_some()
        .cmp(&a.rssi.is_some())
        .then_with(|| b.rssi.cmp(&a.rssi))
        .then_with(|| b.is_named().cmp(&a.is_named()))
        .then_with(|| cmp_names(a, b))
}

/// Paired list order: connected devices first, then alphabetical.
pub fn cmp_paired(a: &DeviceInfo, b: &DeviceInfo) -> Ordering {
    b.connected.cmp(&a.connected).then_with(|| cmp_names(a, b))
}

fn cmp_names(a: &DeviceInfo, b: &DeviceInfo) -> Ordering {
    let (la, lb) = (a.label(), b.label());
    la.chars()
        .flat_map(char::to_lowercase)
        .cmp(lb.chars().flat_map(char::to_lowercase))
        .then_with(|| a.address.cmp(&b.address))
}

/// Human-readable explanation of a BlueZ error (`kind` + raw BlueZ message).
pub fn explain(kind: &ErrorKind, message: &str) -> String {
    let m = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
    // `br-connection-create-socket` is what BlueZ returns when the profile socket's
    // connect fails with EHOSTDOWN (e.g. a switched-off game controller).
    if has(&[
        "page-timeout",
        "host is down",
        "create-socket",
        "connection timed out",
        "le-connection-abort-by-local",
    ]) {
        return "the device is not reachable — make sure it is switched on, in range and not busy with another host"
            .into();
    }
    if has(&["profile-unavailable", "protocol not available"]) {
        return "the device offers no profile this computer can use (for audio, is PipeWire running?)".into();
    }
    if has(&["connection-refused", "connection refused"]) {
        return "the device refused the connection".into();
    }
    if has(&["not-powered", "not powered", "resource not ready"]) {
        return "the Bluetooth adapter is turned off".into();
    }
    if has(&["rfkill"]) {
        return "Bluetooth is blocked by rfkill (run `rfkill unblock bluetooth`)".into();
    }
    if has(&["connection-canceled", "connection canceled"]) {
        return "the connection attempt was canceled".into();
    }
    if has(&["connection-busy", "busy"]) {
        return "the device is busy with another operation — try again in a moment".into();
    }
    match kind {
        ErrorKind::AuthenticationFailed => {
            "authentication failed — the code was wrong or the device rejected pairing".into()
        }
        ErrorKind::AuthenticationCanceled | ErrorKind::AuthenticationRejected => {
            "pairing was canceled".into()
        }
        ErrorKind::AuthenticationTimeout => "pairing timed out waiting for the device".into(),
        ErrorKind::ConnectionAttemptFailed => {
            "the connection attempt failed — is the device switched on and in pairing mode?".into()
        }
        ErrorKind::AlreadyExists => "the device is already paired".into(),
        ErrorKind::DoesNotExist | ErrorKind::NotFound => {
            "the device is no longer known to BlueZ".into()
        }
        ErrorKind::InProgress => "another operation on this device is still in progress".into(),
        ErrorKind::NotReady => "the Bluetooth adapter is not ready (is it powered on?)".into(),
        ErrorKind::NotAuthorized | ErrorKind::NotPermitted => "not permitted by BlueZ".into(),
        _ if message.is_empty() => kind.to_string(),
        _ => format!("{kind}: {message}"),
    }
}

/// Resolve a CLI target (`AA:BB:…` address or device name) against known devices.
/// Exact address, then exact alias/name (case-insensitive), then a unique substring
/// match, preferring paired devices.
pub fn resolve<'a>(devices: &'a [DeviceInfo], query: &str) -> Result<&'a DeviceInfo, String> {
    if let Ok(addr) = query.parse::<Address>() {
        return devices
            .iter()
            .find(|d| d.address == addr)
            .ok_or_else(|| format!("no known device with address {addr}"));
    }
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Err("empty device name".into());
    }
    let names = |d: &'a DeviceInfo| {
        [Some(d.alias.as_str()), d.name.as_deref()]
            .into_iter()
            .flatten()
    };
    let pick = |matches: Vec<&'a DeviceInfo>| -> Result<Option<&'a DeviceInfo>, String> {
        let paired: Vec<_> = matches.iter().copied().filter(|d| d.paired).collect();
        let pool = if paired.is_empty() { matches } else { paired };
        match pool.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            many => Err(format!(
                "“{query}” is ambiguous: {}",
                many.iter()
                    .map(|d| format!("{} ({})", d.label(), d.address))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    };
    let exact = devices
        .iter()
        .filter(|d| names(d).any(|n| n.to_lowercase() == q))
        .collect();
    if let Some(d) = pick(exact)? {
        return Ok(d);
    }
    let partial = devices
        .iter()
        .filter(|d| names(d).any(|n| n.to_lowercase().contains(&q)))
        .collect();
    pick(partial)?.ok_or_else(|| format!("no known device matches “{query}”"))
}

/// Readable name for the service UUIDs BlueZ asks to authorize.
pub fn service_name(uuid16: Option<u16>) -> Option<&'static str> {
    Some(match uuid16? {
        0x1101 => "Serial port",
        0x1105 | 0x1106 => "File transfer (OBEX)",
        0x1108 | 0x1112 => "Headset audio (HSP)",
        0x110a => "Audio streaming source (A2DP)",
        0x110b => "Audio streaming sink (A2DP)",
        0x110c | 0x110e | 0x110f => "Media remote control (AVRCP)",
        0x1115..=0x1117 => "Network (PAN)",
        0x111e | 0x111f => "Hands-free audio (HFP)",
        0x1124 => "Input device (HID)",
        0x112f => "Phonebook access (PBAP)",
        0x1132..=0x1134 => "Message access (MAP)",
        0x1812 => "Input device (HID over GATT)",
        0x184e | 0x184f | 0x1850 | 0x1853 => "LE Audio",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(addr: [u8; 6], alias: &str, name: Option<&str>, rssi: Option<i16>) -> DeviceInfo {
        DeviceInfo {
            address: Address::new(addr),
            name: name.map(str::to_owned),
            alias: alias.into(),
            icon: None,
            class: None,
            appearance: None,
            paired: false,
            connected: false,
            trusted: false,
            blocked: false,
            rssi,
            battery: None,
        }
    }

    #[test]
    fn class_mapping() {
        // Class of Device values as reported by BlueZ for common device types.
        assert_eq!(
            DeviceKind::from_class(0x0024_0404),
            Some(DeviceKind::Headset)
        ); // wireless headset
        assert_eq!(
            DeviceKind::from_class(0x0000_2508),
            Some(DeviceKind::Gamepad)
        ); // game controller
        assert_eq!(
            DeviceKind::from_class(0x0000_2540),
            Some(DeviceKind::Keyboard)
        );
        assert_eq!(DeviceKind::from_class(0x0000_2580), Some(DeviceKind::Mouse));
        assert_eq!(
            DeviceKind::from_class(0x0000_2594),
            Some(DeviceKind::Tablet)
        );
        assert_eq!(
            DeviceKind::from_class(0x0024_0418),
            Some(DeviceKind::Headphones)
        );
        assert_eq!(
            DeviceKind::from_class(0x0024_0414),
            Some(DeviceKind::Speaker)
        );
        assert_eq!(DeviceKind::from_class(0x005a_020c), Some(DeviceKind::Phone));
        assert_eq!(
            DeviceKind::from_class(0x006c_0104),
            Some(DeviceKind::Computer)
        ); // this adapter
        assert_eq!(
            DeviceKind::from_class(0x0000_0680),
            Some(DeviceKind::Printer)
        );
        assert_eq!(DeviceKind::from_class(0x0000_0704), Some(DeviceKind::Watch));
        assert_eq!(DeviceKind::from_class(0x0000_1f00), None);
    }

    #[test]
    fn appearance_mapping() {
        assert_eq!(
            DeviceKind::from_appearance(0x03c1),
            Some(DeviceKind::Keyboard)
        );
        assert_eq!(DeviceKind::from_appearance(0x03c2), Some(DeviceKind::Mouse));
        assert_eq!(
            DeviceKind::from_appearance(0x03c4),
            Some(DeviceKind::Gamepad)
        );
        assert_eq!(
            DeviceKind::from_appearance(0x0941),
            Some(DeviceKind::Headset)
        ); // earbud
        assert_eq!(
            DeviceKind::from_appearance(0x0943),
            Some(DeviceKind::Headphones)
        );
        assert_eq!(DeviceKind::from_appearance(0x0040), Some(DeviceKind::Phone));
        assert_eq!(DeviceKind::from_appearance(0x00c1), Some(DeviceKind::Watch));
        assert_eq!(DeviceKind::from_appearance(0x0000), None);
    }

    #[test]
    fn detect_prefers_icon_then_class_then_appearance() {
        assert_eq!(
            DeviceKind::detect(Some("input-gaming"), Some(0x240404), None),
            DeviceKind::Gamepad
        );
        assert_eq!(
            DeviceKind::detect(Some("unknown-thing"), Some(0x240404), None),
            DeviceKind::Headset
        );
        assert_eq!(
            DeviceKind::detect(None, None, Some(0x03c2)),
            DeviceKind::Mouse
        );
        assert_eq!(DeviceKind::detect(None, None, None), DeviceKind::Other);
        assert_eq!(DeviceKind::Gamepad.icon(), "input-gaming-symbolic");
        assert_eq!(DeviceKind::Other.icon(), "bluetooth-symbolic");
    }

    #[test]
    fn naming() {
        let addr = Address::new([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01]);
        assert!(!has_name("AA-BB-CC-DD-EE-01", None, addr));
        assert!(!has_name("", None, addr));
        assert!(has_name(
            "Wireless Controller",
            Some("Wireless Controller"),
            addr
        ));
        assert!(
            has_name("My pad", None, addr),
            "user-set alias counts as a name"
        );
        let d = dev(
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01],
            "AA-BB-CC-DD-EE-01",
            None,
            Some(-50),
        );
        assert_eq!(d.label(), "Unknown device");
    }

    #[test]
    fn nearby_sorting() {
        let mut v = [
            dev([1; 6], "Weak", Some("Weak"), Some(-90)),
            dev([2; 6], "02-02-02-02-02-02", None, Some(-40)),
            dev([3; 6], "Strong", Some("Strong"), Some(-40)),
            dev([4; 6], "Gone", Some("Gone"), None),
            dev([5; 6], "Mid", Some("Mid"), Some(-65)),
            dev([6; 6], "alpha", Some("alpha"), Some(-65)),
        ];
        v.sort_by(cmp_nearby);
        let order: Vec<_> = v.iter().map(|d| d.address.0[0]).collect();
        assert_eq!(order, [3, 2, 6, 5, 1, 4]);
    }

    #[test]
    fn paired_sorting() {
        let mut a = dev([1; 6], "Zeta", Some("Zeta"), None);
        a.connected = true;
        let b = dev([2; 6], "alpha", Some("alpha"), None);
        let c = dev([3; 6], "Beta", Some("Beta"), None);
        let mut v = [c, b, a];
        v.sort_by(cmp_paired);
        let order: Vec<_> = v.iter().map(|d| d.alias.as_str()).collect();
        assert_eq!(order, ["Zeta", "alpha", "Beta"]);
    }

    #[test]
    fn readable_errors() {
        let host_down = explain(&ErrorKind::Failed, "br-connection-page-timeout");
        assert!(host_down.contains("not reachable"), "{host_down}");
        assert!(explain(&ErrorKind::Failed, "Host is down").contains("not reachable"));
        assert!(
            explain(&ErrorKind::Failed, "br-connection-create-socket").contains("not reachable")
        );
        assert!(explain(&ErrorKind::AuthenticationFailed, "").contains("authentication failed"));
        assert!(
            explain(&ErrorKind::Failed, "br-connection-profile-unavailable").contains("no profile")
        );
        assert!(explain(&ErrorKind::Failed, "Blocked through rfkill").contains("rfkill"));
        assert_eq!(
            explain(&ErrorKind::Failed, "something odd"),
            "Bluetooth operation failed: something odd"
        );
    }

    #[test]
    fn resolving_targets() {
        let mut headphones = dev(
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01],
            "Studio Wireless Headphones",
            Some("Studio Wireless Headphones"),
            None,
        );
        headphones.paired = true;
        let mut earbuds = dev(
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x02],
            "Acme Wireless Earbuds",
            Some("Acme Wireless Earbuds"),
            None,
        );
        earbuds.paired = true;
        let stranger = dev([9; 6], "Acme Phone", Some("Acme Phone"), Some(-70));
        let devs = [headphones, earbuds, stranger];
        assert_eq!(
            resolve(&devs, "AA:BB:CC:DD:EE:01").unwrap().alias,
            "Studio Wireless Headphones"
        );
        assert_eq!(
            resolve(&devs, "studio wireless headphones").unwrap().alias,
            "Studio Wireless Headphones"
        );
        // "acme" matches a paired and an unpaired device: paired wins.
        assert_eq!(
            resolve(&devs, "acme").unwrap().alias,
            "Acme Wireless Earbuds"
        );
        assert_eq!(resolve(&devs, "acme phone").unwrap().alias, "Acme Phone");
        assert!(
            resolve(&devs, "w").is_err(),
            "ambiguous among paired devices"
        );
        assert!(resolve(&devs, "00:00:00:00:00:01").is_err());
        assert!(resolve(&devs, "nothing").is_err());
    }

    #[test]
    fn signal_words() {
        assert_eq!(signal_label(-45), "excellent");
        assert_eq!(signal_label(-65), "good");
        assert_eq!(signal_label(-75), "fair");
        assert_eq!(signal_label(-95), "weak");
    }
}
