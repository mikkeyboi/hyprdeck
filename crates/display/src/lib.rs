//! hyprdeck "Displays": monitor arrangement, modes, scale, VRR, color/HDR,
//! DDC/CI hardware controls and global rendering options.

mod arrange;
mod ddc;
mod hardware;
mod logic;
mod page;
mod rules;

use anyhow::{Context, Result, bail};
use hyprdeck_core::hypr::ctl;
use hyprdeck_core::ui::PageInfo;

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: "display",
        title: "Displays",
        icon: "video-display-symbolic",
        build: page::build,
    }]
}

/// Nothing runs in the background for displays.
pub fn start_background() {}

const USAGE: &str = "usage: hyprdeck display <list | rescue | brightness <0-100> [output]>";

/// `hyprdeck display …`
pub fn cli(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("display") {
        return None;
    }
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    Some(match rest.as_slice() {
        ["list"] => list(),
        ["rescue"] => ctl::rescue_displays()
            .map(|()| println!("Displays re-lit and monitor rules re-applied.")),
        ["brightness", value] => brightness(value, None),
        ["brightness", value, output] => brightness(value, Some(output)),
        _ => Err(anyhow::anyhow!(USAGE)),
    })
}

fn list() -> Result<()> {
    for m in ctl::monitors()? {
        println!("{}  {} {} ({})", m.name, m.make, m.model, m.serial);
        if m.disabled {
            println!("  disabled");
            continue;
        }
        println!(
            "  mode {}x{}@{:.2}Hz  position {}x{}  scale {}  transform {}",
            m.width,
            m.height,
            m.refresh_rate,
            m.x,
            m.y,
            logic::fmt_scale(m.scale),
            logic::TRANSFORMS[m.transform.clamp(0, 7) as usize]
        );
        let cm = if m.color_management_preset.is_empty() {
            "srgb"
        } else {
            &m.color_management_preset
        };
        let mirror = if m.mirror_of == "none" {
            String::new()
        } else {
            format!("  mirrors {}", m.mirror_of)
        };
        println!(
            "  vrr {}  cm {}  format {}  dpms {}{mirror}",
            if m.vrr { "active" } else { "off" },
            cm,
            m.current_format,
            if m.dpms_status { "on" } else { "off" }
        );
    }
    Ok(())
}

fn brightness(value: &str, output: Option<&str>) -> Result<()> {
    let percent: u8 = value
        .parse()
        .ok()
        .filter(|v| *v <= 100)
        .with_context(|| format!("brightness must be 0-100, got {value:?}"))?;
    match ddc::access() {
        ddc::Access::Ready => {}
        ddc::Access::NoDdcutil => bail!("brightness control needs ddcutil, which is not installed"),
        other => bail!("{}", other.hint().unwrap_or_default()),
    }
    let displays = ddc::detect()?;
    let targets: Vec<&ddc::DdcDisplay> = match output {
        Some(o) => vec![
            ddc::for_output(&displays, o)
                .with_context(|| format!("no DDC/CI display on output {o}"))?,
        ],
        None => displays.iter().collect(),
    };
    if targets.is_empty() {
        bail!("no DDC/CI capable display found");
    }
    for display in targets {
        let raw = ddc::set_brightness_percent(display.bus, percent)?;
        let name = display.connector.as_deref().unwrap_or(&display.monitor);
        println!(
            "{name}: brightness set to {percent}% (VCP 0x10 = {raw}, bus {})",
            display.bus
        );
    }
    Ok(())
}
