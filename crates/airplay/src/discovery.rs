//! Receiver discovery over mDNS.
//!
//! AirPlay 2 receivers advertise `_airplay._tcp`; its TXT record carries the
//! device id, model, OS version, feature bits and the receiver's public key.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

use crate::Result;

const SERVICE_TYPE: &str = "_airplay._tcp.local.";

/// Feature bits from the `features` TXT key (bit numbers per community docs).
pub mod features {
    pub const BUFFERED_AUDIO: u32 = 40;
    pub const PTP_CLOCK: u32 = 41;
    pub const HOMEKIT_PAIRING: u32 = 46;
    pub const TRANSIENT_PAIRING: u32 = 48;
}

#[derive(Debug, Clone)]
pub struct Device {
    /// Display name, e.g. "书房".
    pub name: String,
    /// MAC-style device id from the `deviceid` TXT key.
    pub id: String,
    /// Model identifier, e.g. "AudioAccessory1,1" (original HomePod).
    pub model: String,
    /// Receiver OS version from `osvers`, e.g. "27.0".
    pub os_version: String,
    pub host: String,
    pub port: u16,
    /// IPv4 addresses first, then IPv6.
    pub addresses: Vec<IpAddr>,
    /// 64-bit feature mask decoded from `features` ("0xLO,0xHI").
    pub features: u64,
    /// Receiver Ed25519 public key (hex) from `pk`.
    pub public_key: Option<String>,
}

impl Device {
    pub fn has_feature(&self, bit: u32) -> bool {
        self.features & (1 << bit) != 0
    }
}

/// Browse for receivers for `timeout` and return them sorted by name.
pub fn discover(timeout: Duration) -> Result<Vec<Device>> {
    let mdns = ServiceDaemon::new()?;
    let events = mdns.browse(SERVICE_TYPE)?;
    let deadline = Instant::now() + timeout;
    let mut found = BTreeMap::new();

    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(service)) => {
                let txt = |key: &str| {
                    service
                        .get_property_val_str(key)
                        .unwrap_or_default()
                        .to_string()
                };
                let mut addresses: Vec<IpAddr> = service
                    .get_addresses()
                    .iter()
                    .map(|ip| ip.to_ip_addr())
                    .collect();
                addresses.sort_by_key(|ip| (ip.is_ipv6(), *ip));

                let device = Device {
                    name: instance_name(&service.fullname),
                    id: txt("deviceid"),
                    model: txt("model"),
                    os_version: txt("osvers"),
                    host: service.host.clone(),
                    port: service.port,
                    addresses,
                    features: parse_features(&txt("features")),
                    public_key: service.get_property_val_str("pk").map(str::to_string),
                };
                tracing::debug!(?device, "resolved");
                found.insert(service.fullname.clone(), device);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    // Shutdown failure only means the daemon thread is already gone.
    let _ = mdns.shutdown();
    let mut devices: Vec<Device> = found.into_values().collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(devices)
}

/// "书房._airplay._tcp.local." -> "书房"
fn instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .map(|s| s.trim_end_matches('.'))
        .unwrap_or(fullname)
        .to_string()
}

/// Parses "0x4A7FCA00,0x3C356BD0" (low word, high word) into a 64-bit mask.
fn parse_features(raw: &str) -> u64 {
    let word = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap_or(0);
    match raw.split_once(',') {
        Some((lo, hi)) => word(lo) | (word(hi) << 32),
        None => word(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_word_features() {
        // Values advertised by the HomePod 书房 (OS 27.0).
        let f = parse_features("0x4A7FCA00,0x3C356BD0");
        assert_eq!(f, 0x3C35_6BD0_4A7F_CA00);
    }

    #[test]
    fn parses_single_word_features() {
        assert_eq!(parse_features("0x5A7FFFF7"), 0x5A7F_FFF7);
    }

    #[test]
    fn strips_service_suffix() {
        assert_eq!(instance_name("书房._airplay._tcp.local."), "书房");
    }
}
