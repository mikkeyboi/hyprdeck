//! `hyprdeck bluetooth list | connect <dev> | disconnect <dev>`.

use anyhow::{Result, anyhow, bail};
use hyprdeck_core::rt;

use crate::bt;
use crate::info::{self, DeviceInfo};

const USAGE: &str =
    "usage: hyprdeck bluetooth list | connect <address|name> | disconnect <address|name>";

pub fn run(args: &[String]) -> Result<()> {
    rt::runtime().block_on(async {
        match args {
            [cmd] if cmd == "list" => list().await,
            [cmd, target @ ..]
                if (cmd == "connect" || cmd == "disconnect") && !target.is_empty() =>
            {
                toggle(cmd == "connect", &target.join(" ")).await
            }
            _ => bail!(USAGE),
        }
    })
}

async fn list() -> Result<()> {
    let snap = bt::snapshot()
        .await?
        .ok_or_else(|| anyhow!("no Bluetooth adapter found"))?;
    let a = &snap.adapter;
    let rf = tokio::task::spawn_blocking(bt::rfkill).await?;
    let mut state = vec![if a.powered { "powered" } else { "off" }];
    if a.discoverable {
        state.push("discoverable");
    }
    if a.pairable {
        state.push("pairable");
    }
    if a.discovering {
        state.push("discovering");
    }
    if rf.hard {
        state.push("hard-blocked (rfkill)");
    } else if rf.soft {
        state.push("blocked (rfkill)");
    }
    println!(
        "Adapter {} {} \"{}\": {}",
        a.name,
        a.address,
        a.alias,
        state.join(", ")
    );

    let mut devices = snap.devices;
    devices.sort_by(|x, y| y.paired.cmp(&x.paired).then_with(|| info::cmp_paired(x, y)));
    if devices.is_empty() {
        println!("No known devices");
    }
    for d in &devices {
        println!("{}  {:<28} {}", d.address, d.label(), flags(d));
    }
    Ok(())
}

fn flags(d: &DeviceInfo) -> String {
    let mut f = vec![d.kind().label().to_owned()];
    f.push(if d.connected {
        "connected".into()
    } else {
        "disconnected".into()
    });
    if d.paired {
        f.push("paired".into());
    }
    if d.trusted {
        f.push("trusted".into());
    }
    if d.blocked {
        f.push("blocked".into());
    }
    if let Some(b) = d.battery {
        f.push(format!("battery {b}%"));
    }
    if let Some(r) = d.rssi {
        f.push(format!("{r} dBm"));
    }
    f.join(", ")
}

async fn toggle(connect: bool, target: &str) -> Result<()> {
    let a = bt::adapter().await?;
    let devices = bt::devices(&a).await?;
    let d = info::resolve(&devices, target).map_err(|e| anyhow!(e))?;
    let label = d.label();
    if connect {
        println!("Connecting to {label} ({})…", d.address);
        bt::connect(d.address)
            .await
            .map_err(|e| anyhow!("couldn't connect to {label}: {e:#}"))?;
        println!("Connected to {label}");
    } else {
        bt::disconnect(d.address)
            .await
            .map_err(|e| anyhow!("couldn't disconnect {label}: {e:#}"))?;
        println!("Disconnected {label}");
    }
    Ok(())
}
