//! Same-user administration for a host without a desktop session.
use ox_host_service::Paths;
use ox_host_web::{
    access::{self, Operation},
    admin::{self, DeviceClass, Request},
};
use std::{
    io::{self, IsTerminal},
    process::Command,
};

pub fn pair(paths: &Paths, terminal: bool) -> Result<(), String> {
    let report = access::run(paths, Operation::Enable)?;
    if let Some(help) = report.denied {
        if !io::stdin().is_terminal() {
            return Err(format!(
                "{} Run ox host pair in an interactive terminal to authorize Tailscale Serve.",
                help.error
            ));
        }
        eprintln!(
            "Tailscale requires permission to enable private HTTPS. Running {}",
            help.suggested()
        );
        let lease = help.prepare()?;
        let status = Command::new("sudo")
            .args(help.args())
            .status()
            .map_err(|e| e.to_string())?;
        if let Some(error) = help.finish(status.code(), Some(lease))?.notice {
            return Err(error);
        }
    }
    let reply = access::run(paths, Operation::Pair { terminal })?.host;
    let pairing = reply
        .pairing
        .ok_or("The host did not issue an invitation")?;
    if terminal {
        println!(
            "This invitation grants terminal access on this host to the device that redeems it."
        );
    }
    let qr = qrcode::QrCode::new(&pairing.url).map_err(|e| e.to_string())?;
    println!(
        "{}",
        qr.render::<qrcode::render::unicode::Dense1x2>()
            .module_dimensions(1, 1)
            .build()
    );
    println!(
        "Scan on your phone or paste this invitation into Add host on your desktop. It expires in five minutes.\n{}",
        pairing.url
    );
    if let (Some(origin), Some(code)) = (reply.origin, pairing.short_code) {
        println!("Host  {origin}\nCode  {code}");
    }
    Ok(())
}

pub fn devices(paths: &Paths) -> Result<(), String> {
    let reply = access::status(paths)?;
    if reply.devices.is_empty() {
        println!("No paired devices. Run ox host pair to add one.");
    }
    for device in reply.devices {
        let class = match device.class {
            DeviceClass::Phone => "phone",
            DeviceClass::Desktop => "desktop",
        };
        let grant = if device.terminal {
            "terminal enabled"
        } else {
            "terminal off"
        };
        println!(
            "{}  {}  {class}  {}  {grant}",
            device.id, device.label, device.login
        );
    }
    Ok(())
}
fn match_device<'a>(rows: &'a [admin::Device], id: &str) -> Result<&'a admin::Device, String> {
    let mut matches = rows
        .iter()
        .filter(|d| d.id.starts_with(id) || d.label.eq_ignore_ascii_case(id));
    let device = matches
        .next()
        .filter(|_| !id.is_empty())
        .ok_or("Unknown device. Run ox host devices to list names and IDs.")?;
    if matches.next().is_some() {
        return Err("Several devices match. Use a longer ID prefix.".into());
    }
    Ok(device)
}
pub fn terminal(paths: &Paths, id: &str, enabled: bool) -> Result<(), String> {
    let devices = access::status(paths)?.devices;
    let device = match_device(&devices, id)?;
    admin::request(
        &paths.state_dir,
        &Request::TerminalGrant {
            id: device.id.clone(),
            enabled,
        },
    )
    .map_err(|e| e.to_string())?;
    println!(
        "Terminal access {} for {}.",
        if enabled { "enabled" } else { "disabled" },
        device.label
    );
    Ok(())
}
pub fn revoke(paths: &Paths, id: &str) -> Result<(), String> {
    let devices = access::status(paths)?.devices;
    let device = match_device(&devices, id)?;
    admin::request(
        &paths.state_dir,
        &Request::Revoke {
            id: device.id.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    println!("Revoked {}.", device.label);
    Ok(())
}
