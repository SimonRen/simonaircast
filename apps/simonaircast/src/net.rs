//! Finding receivers by name or address.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail};

pub const AIRPLAY_PORT: u16 = 7000;
/// mDNS discovery is relatively expensive (and can take several seconds on a
/// busy Wi-Fi network). Keep a short-lived address cache for reconnects while
/// still allowing DHCP changes to converge quickly.
const RESOLVE_CACHE_TTL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
struct CachedAddress {
    address: SocketAddr,
    expires: Instant,
}

fn resolve_cache() -> &'static Mutex<HashMap<String, CachedAddress>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedAddress>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(target: &str) -> String {
    target.trim().to_ascii_lowercase()
}

/// Drops a cached name after a connection-level failure. The next retry will
/// perform mDNS discovery, which lets a receiver that got a new DHCP address
/// recover without waiting for the cache TTL.
pub fn invalidate(target: &str) {
    if let Ok(mut cache) = resolve_cache().lock() {
        cache.remove(&cache_key(target));
    }
}

pub async fn discover(timeout: Duration) -> Result<Vec<airplay::Device>> {
    Ok(tokio::task::spawn_blocking(move || airplay::discover(timeout)).await??)
}

/// Resolves an IP address or a receiver name (as shown by discovery) to the
/// receiver's control address.
pub async fn resolve(target: &str) -> Result<SocketAddr> {
    let target = target.trim();
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, AIRPLAY_PORT));
    }

    let key = cache_key(target);
    if let Ok(mut cache) = resolve_cache().lock() {
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires > now);
        if let Some(entry) = cache.get(&key) {
            tracing::debug!(receiver = target, address = %entry.address, "using cached receiver address");
            return Ok(entry.address);
        }
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
    let address = SocketAddr::new(*ip, device.port);
    if let Ok(mut cache) = resolve_cache().lock() {
        cache.insert(
            key,
            CachedAddress {
                address,
                expires: Instant::now() + RESOLVE_CACHE_TTL,
            },
        );
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_ignore_case_and_surrounding_whitespace() {
        assert_eq!(cache_key("  Study Room "), "study room");
        assert_eq!(cache_key("STUDY ROOM"), cache_key("study room"));
    }

    #[test]
    fn invalidating_a_name_removes_its_cached_address() {
        resolve_cache().lock().unwrap().insert(
            cache_key("Study Room"),
            CachedAddress {
                address: "192.0.2.10:7000".parse().unwrap(),
                expires: Instant::now() + RESOLVE_CACHE_TTL,
            },
        );
        invalidate(" study room ");
        assert!(!resolve_cache().lock().unwrap().contains_key("study room"));
    }

    #[tokio::test]
    async fn literal_addresses_skip_discovery() {
        assert_eq!(
            resolve(" 192.0.2.10 ").await.unwrap(),
            "192.0.2.10:7000".parse().unwrap()
        );
    }
}
