//! DrDrOS Wi-Fi manager — the userland that turns "there's a radio" into
//! "I'm online".
//!
//! The actual 802.11 association (the WPA2/WPA3 4-way handshake, SAE, the
//! AES/SHA crypto) is done by `wpa_supplicant`, exactly as the kernel does
//! the driving below us — that is security-critical code you do **not**
//! hand-roll. What's *ours* is everything the user touches: scanning,
//! presenting the network list, taking a password, writing the config and
//! bringing the link up. We talk to wpa_supplicant over its `wpa_cli`
//! control channel and pull an address with BusyBox `udhcpc`.
//!
//! Every function fails *soft* — a missing `wpa_supplicant` binary, no
//! radio, a denied operation — none of it panics; it returns an error
//! string the Network panel shows. The pure parsers ([`parse_scan_results`],
//! [`parse_status`]) are unit-tested; the process orchestration is thin.

use std::process::Command;
use std::thread;

/// One scanned access point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// Signal level in dBm (closer to 0 = stronger).
    pub signal: i32,
    /// "open", "WEP", "WPA", "WPA2", "WPA3".
    pub security: String,
    pub bssid: String,
}

impl Network {
    pub fn is_open(&self) -> bool {
        self.security == "open"
    }
    /// A 0..=4 bar strength from the dBm level.
    pub fn bars(&self) -> u8 {
        match self.signal {
            s if s >= -50 => 4,
            s if s >= -60 => 3,
            s if s >= -70 => 2,
            s if s >= -80 => 1,
            _ => 0,
        }
    }
}

/// The live connection state, from `wpa_cli status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiStatus {
    pub state: String,
    pub ssid: String,
    pub ip: String,
}

impl WifiStatus {
    pub fn connected(&self) -> bool {
        self.state == "COMPLETED"
    }
}

/// Wireless interfaces (`/sys/class/net/*/phy80211` exists).
pub fn wireless_ifaces() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/net") {
        for e in rd.flatten() {
            let p = e.path();
            if p.join("phy80211").exists() || p.join("wireless").exists() {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    v.sort();
    v
}

/// Run `wpa_cli -i <iface> <args...>` and return stdout.
fn wpa_cli(iface: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("wpa_cli")
        .arg("-i")
        .arg(iface)
        .args(args)
        .output()
        .map_err(|e| format!("wpa_cli: {e} (is wpa_supplicant installed?)"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Make sure a `wpa_supplicant` is running on `iface`; start one if not.
pub fn ensure_supplicant(iface: &str) -> Result<(), String> {
    // Already up? `wpa_cli ping` answers PONG.
    if let Ok(p) = wpa_cli(iface, &["ping"]) {
        if p.contains("PONG") {
            return Ok(());
        }
    }
    let conf = format!("/tmp/wpa_{iface}.conf");
    let _ = std::fs::write(
        &conf,
        "ctrl_interface=/var/run/wpa_supplicant\nupdate_config=1\n",
    );
    // Bring the link up, then daemonize wpa_supplicant on nl80211.
    let _ = Command::new("ip").args(["link", "set", iface, "up"]).status();
    let status = Command::new("wpa_supplicant")
        .args(["-B", "-i", iface, "-Dnl80211", "-c", &conf])
        .status()
        .map_err(|e| format!("wpa_supplicant: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("wpa_supplicant failed to start (check the radio + firmware)".into())
    }
}

/// Ask the supplicant to start a scan (results arrive asynchronously).
pub fn trigger_scan(iface: &str) -> Result<(), String> {
    ensure_supplicant(iface)?;
    wpa_cli(iface, &["scan"]).map(|_| ())
}

/// Read the most recent scan results.
pub fn scan_results(iface: &str) -> Vec<Network> {
    wpa_cli(iface, &["scan_results"])
        .map(|t| parse_scan_results(&t))
        .unwrap_or_default()
}

/// Connect to `ssid` with `psk` (empty psk = open network). Configures a
/// network in the running supplicant, selects it, and kicks off DHCP in
/// the background so the UI never blocks.
pub fn connect(iface: &str, ssid: &str, psk: &str) -> Result<(), String> {
    ensure_supplicant(iface)?;
    let id = wpa_cli(iface, &["add_network"])?.trim().to_string();
    if id.is_empty() || id.contains("FAIL") {
        return Err("could not allocate a network slot".into());
    }
    let q_ssid = format!("\"{ssid}\"");
    wpa_cli(iface, &["set_network", &id, "ssid", &q_ssid])?;
    if psk.is_empty() {
        wpa_cli(iface, &["set_network", &id, "key_mgmt", "NONE"])?;
    } else {
        let q_psk = format!("\"{psk}\"");
        wpa_cli(iface, &["set_network", &id, "psk", &q_psk])?;
    }
    wpa_cli(iface, &["enable_network", &id])?;
    wpa_cli(iface, &["select_network", &id])?;
    let _ = wpa_cli(iface, &["save_config"]);
    run_dhcp(iface);
    Ok(())
}

/// Pull an address via DHCP in a detached thread (udhcpc can take a few
/// seconds; the desktop must stay responsive).
pub fn run_dhcp(iface: &str) {
    let iface = iface.to_string();
    thread::spawn(move || {
        let _ = Command::new("udhcpc")
            .args(["-i", &iface, "-n", "-q", "-t", "5"])
            .status();
    });
}

/// Current connection status.
pub fn status(iface: &str) -> WifiStatus {
    let mut st = wpa_cli(iface, &["status"])
        .map(|t| parse_status(&t))
        .unwrap_or_default();
    if st.ip.is_empty() {
        st.ip = ipv4_of(iface);
    }
    st
}

/// Best-effort IPv4 of `iface` from `ip -4 addr show`.
fn ipv4_of(iface: &str) -> String {
    let out = Command::new("ip")
        .args(["-4", "addr", "show", "dev", iface])
        .output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout);
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("inet ") {
                if let Some(addr) = rest.split('/').next() {
                    return addr.to_string();
                }
            }
        }
    }
    String::new()
}

// ─── Pure parsers (unit-tested) ──────────────────────────────────────

/// Parse `wpa_cli scan_results` output (tab-separated, header line first):
/// `bssid  frequency  signal  flags  ssid`.
pub fn parse_scan_results(text: &str) -> Vec<Network> {
    let mut nets: Vec<Network> = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 5 {
            continue;
        }
        let ssid = cols[4].trim();
        if ssid.is_empty() {
            continue; // hidden network
        }
        let signal = cols[2].trim().parse::<i32>().unwrap_or(-100);
        let flags = cols[3];
        let security = security_from_flags(flags);
        // De-duplicate by SSID, keeping the strongest signal.
        if let Some(existing) = nets.iter_mut().find(|n| n.ssid == ssid) {
            if signal > existing.signal {
                existing.signal = signal;
                existing.bssid = cols[0].to_string();
            }
            continue;
        }
        nets.push(Network {
            ssid: ssid.to_string(),
            signal,
            security,
            bssid: cols[0].to_string(),
        });
    }
    nets.sort_by(|a, b| b.signal.cmp(&a.signal));
    nets
}

fn security_from_flags(flags: &str) -> String {
    if flags.contains("WPA3") || flags.contains("SAE") {
        "WPA3".into()
    } else if flags.contains("WPA2") || flags.contains("RSN") {
        "WPA2".into()
    } else if flags.contains("WPA") {
        "WPA".into()
    } else if flags.contains("WEP") {
        "WEP".into()
    } else {
        "open".into()
    }
}

/// Parse `wpa_cli status` (`key=value` lines).
pub fn parse_status(text: &str) -> WifiStatus {
    let mut st = WifiStatus::default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "wpa_state" => st.state = v.to_string(),
                "ssid" => st.ssid = v.to_string(),
                "ip_address" => st.ip = v.to_string(),
                _ => {}
            }
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN: &str = "bssid / frequency / signal level / flags / ssid\n\
        00:11:22:33:44:55\t2412\t-42\t[WPA2-PSK-CCMP][ESS]\tHomeNet\n\
        66:77:88:99:aa:bb\t5180\t-67\t[WPA3-SAE][ESS]\tOffice5G\n\
        cc:dd:ee:ff:00:11\t2437\t-80\t[ESS]\tCoffeeShop\n\
        00:11:22:33:44:56\t2412\t-55\t[WPA2-PSK-CCMP][ESS]\tHomeNet\n";

    #[test]
    fn parses_scan_results_and_dedups() {
        let nets = parse_scan_results(SCAN);
        // HomeNet appears twice → deduped to one, strongest signal kept.
        assert_eq!(nets.len(), 3);
        let home = nets.iter().find(|n| n.ssid == "HomeNet").unwrap();
        assert_eq!(home.signal, -42);
        assert_eq!(home.security, "WPA2");
        let office = nets.iter().find(|n| n.ssid == "Office5G").unwrap();
        assert_eq!(office.security, "WPA3");
        let coffee = nets.iter().find(|n| n.ssid == "CoffeeShop").unwrap();
        assert!(coffee.is_open());
        // Sorted strongest-first.
        assert_eq!(nets[0].ssid, "HomeNet");
    }

    #[test]
    fn signal_maps_to_bars() {
        let n = |s| Network { ssid: "x".into(), signal: s, security: "open".into(), bssid: String::new() };
        assert_eq!(n(-40).bars(), 4);
        assert_eq!(n(-65).bars(), 2);
        assert_eq!(n(-95).bars(), 0);
    }

    #[test]
    fn parses_status() {
        let st = parse_status("bssid=00:11:22:33:44:55\nssid=HomeNet\nwpa_state=COMPLETED\nip_address=192.168.1.50\n");
        assert!(st.connected());
        assert_eq!(st.ssid, "HomeNet");
        assert_eq!(st.ip, "192.168.1.50");
    }
}
