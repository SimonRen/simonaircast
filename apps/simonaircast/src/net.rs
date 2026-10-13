//! Finding receivers by name or address.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const AIRPLAY_PORT: u16 = 7000;

pub async fn discover(timeout: Duration) -> Result<Vec<airplay::Device>> {
    Ok(tokio::task::spawn_blocking(move || airplay::discover(timeout)).await??)
}

/// Resolves an IP address or a receiver name (as shown by discovery) to the
/// receiver's control address.
pub async fn resolve(target: &str) -> Result<SocketAddr> {
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, AIRPLAY_PORT));
    }
    let devices = discover(Duration::from_secs(3)).await?;
    let Some(device) = devices.iter().find(|d| d.name.eq_ignore_ascii_case(target)) else {
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        bail!("no receiver named {target:?} (found: {})", names.join(", "));
    };
    let ip = device
        .addresses
        .iter()
        .find(|ip| ip.is_ipv4())
        .or(device.addresses.first())
        .with_context(|| format!("{} advertised no address", device.name))?;
    Ok(SocketAddr::new(*ip, device.port))
}
