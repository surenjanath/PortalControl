//! Network settings of the manager: who can reach the dashboard.
//!
//! The dashboard always answers on this computer (`http://127.0.0.1:<port>`).
//! That local door is what the tray icon and `portalcontrol stop` use, so
//! they keep working whatever is configured here. On top of that it can be
//! opened to other computers on one address of this machine (a LAN or
//! Tailscale address), optionally over HTTPS with a certificate, and it can
//! answer to host names as well as IP addresses.
//!
//! The settings are stored as one JSON value in the `settings` table and are
//! read when the manager starts; a change takes effect at the next start.
//! `--bind` and `--port` on the command line win over what is stored.

use crate::db::Db;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

const SETTING_KEY: &str = "network";
const MAX_HOSTS: usize = 10;

/// What the user configured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetSettings {
    /// Address of this machine other computers connect to. Empty: this
    /// computer only.
    #[serde(default)]
    pub address: String,
    /// The user confirmed that `address`, although outside the private
    /// ranges, is on their internal network. Some networks use public
    /// address space internally.
    #[serde(default)]
    pub internal_confirmed: bool,
    /// Let computers from other networks in too. Off by default: only
    /// computers on the same network as `address` are answered.
    #[serde(default)]
    pub allow_other_networks: bool,
    /// Unix time after which other computers are refused. `None`: no limit.
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Input only: open for this many minutes from now (sets `expires_at`).
    #[serde(default, skip_serializing)]
    pub open_for_minutes: Option<u32>,
    /// Port of the dashboard. `None`: the default.
    #[serde(default)]
    pub port: Option<u16>,
    /// Host names the dashboard answers to besides IP addresses, for
    /// example `portal.example.lan`.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Serve the network address over HTTPS.
    #[serde(default)]
    pub https: bool,
    /// PEM file with the certificate (and its chain).
    #[serde(default)]
    pub cert_file: String,
    /// PEM file with the private key.
    #[serde(default)]
    pub key_file: String,
}

impl NetSettings {
    pub async fn load(db: &Db) -> Self {
        match db.get_setting(SETTING_KEY).await {
            Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_default(),
            _ => Self::default(),
        }
    }

    pub async fn save(&self, db: &Db) -> Result<(), String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        db.set_setting(SETTING_KEY, Some(json)).await
    }

    /// Tidy the input and refuse what could never work or would be unsafe.
    pub fn validate(mut self) -> Result<Self, String> {
        self.address = self.address.trim().to_string();
        if !self.address.is_empty() {
            let ip: IpAddr = self
                .address
                .parse()
                .map_err(|_| format!("\"{}\" is not an IP address", self.address))?;
            if ip.is_loopback() || ip.is_unspecified() {
                return Err("choose an address of this computer on your network; 127.0.0.1 is always on, and 0.0.0.0 (every network) is not allowed".into());
            }
            if crate::net::is_private_ip(&ip) {
                self.internal_confirmed = false;
            } else {
                // Outside the private ranges. Allowed only for an address this
                // computer really has, and only when the user says the network
                // is internal: PortalControl must never face the internet.
                if !local_addresses().iter().any(|a| a.ip == ip.to_string()) {
                    return Err(format!(
                        "{ip} is not in a private range (10.x, 172.16-31.x, 192.168.x, Tailscale 100.64-127.x) and is not an address of this computer right now, so it cannot be used"
                    ));
                }
                if !self.internal_confirmed {
                    return Err(format!(
                        "{ip} is outside the private ranges, so it could be reachable from the internet. If your network uses this range internally, tick the box that says so and save again"
                    ));
                }
            }
            self.address = ip.to_string();
        } else {
            self.internal_confirmed = false;
        }
        if let Some(minutes) = self.open_for_minutes.take() {
            if minutes == 0 || minutes > 7 * 24 * 60 {
                return Err("the time limit must be between 1 minute and 7 days".into());
            }
            self.expires_at = Some(chrono::Utc::now().timestamp() + i64::from(minutes) * 60);
        }
        if self.address.is_empty() {
            self.expires_at = None;
        }
        if self.port == Some(0) {
            return Err("the port must be between 1 and 65535".into());
        }

        let mut hosts: Vec<String> = Vec::new();
        for raw in std::mem::take(&mut self.hosts) {
            let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
            if host.is_empty() || hosts.contains(&host) {
                continue;
            }
            let label_ok = |l: &str| {
                !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            };
            if host.len() > 253 || !host.split('.').all(label_ok) || !host.bytes().any(|b| b.is_ascii_alphabetic()) {
                return Err(format!("\"{raw}\" is not a host name (use letters, digits, dots and dashes, without http:// or a port)"));
            }
            hosts.push(host);
        }
        if hosts.len() > MAX_HOSTS {
            return Err(format!("at most {MAX_HOSTS} host names are supported"));
        }
        self.hosts = hosts;

        self.cert_file = self.cert_file.trim().trim_matches('"').to_string();
        self.key_file = self.key_file.trim().trim_matches('"').to_string();
        if self.https {
            if self.cert_file.is_empty() || self.key_file.is_empty() {
                return Err("HTTPS needs both a certificate file and a private key file".into());
            }
            load_tls(&self.cert_file, &self.key_file)?;
        }
        Ok(self)
    }
}

/// The settings after the command line had its say: what a start will do.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Effective {
    pub port: u16,
    /// Address opened to other computers, if any.
    pub network: Option<IpAddr>,
    pub https: bool,
    pub cert_file: String,
    pub key_file: String,
    pub hosts: Vec<String>,
    /// `--bind` / `--port` were given and override the stored values.
    pub bind_from_cli: bool,
    pub port_from_cli: bool,
}

pub fn effective(saved: &NetSettings, cli_bind: Option<IpAddr>, cli_port: Option<u16>, default_port: u16) -> Effective {
    let network = match cli_bind {
        Some(ip) => (!ip.is_loopback()).then_some(ip),
        None => saved.address.parse().ok(),
    };
    Effective {
        port: cli_port.or(saved.port).unwrap_or(default_port),
        network,
        // HTTPS belongs to the network address; the local door stays plain.
        https: saved.https && network.is_some(),
        cert_file: saved.cert_file.clone(),
        key_file: saved.key_file.clone(),
        hosts: saved.hosts.clone(),
        bind_from_cli: cli_bind.is_some(),
        port_from_cli: cli_port.is_some(),
    }
}

/// The limits on other computers that apply at once, without a restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Limits {
    pub allow_other_networks: bool,
    pub expires_at: Option<i64>,
}

impl From<&NetSettings> for Limits {
    fn from(s: &NetSettings) -> Self {
        Self { allow_other_networks: s.allow_other_networks, expires_at: s.expires_at }
    }
}

pub const REFUSED_NO_PASSWORD: &str =
    "This PortalControl is not open to other computers: no dashboard password is set. Set one on the computer it runs on.";
pub const REFUSED_EXPIRED: &str =
    "Network access to this PortalControl has ended. Open it again on the computer it runs on (Settings, Network).";
pub const REFUSED_OTHER_NETWORK: &str = "Only computers on the same network as this PortalControl may connect.";

/// One computer that used the network address since the manager started.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ClientSeen {
    pub last_seen: i64,
    pub requests: u64,
    /// Why it was turned away, if it was.
    pub refused: Option<&'static str>,
}

/// Whether two IPv4 addresses are in the same network of `prefix` bits.
pub fn same_network(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

/// What the running manager opened at start.
#[derive(Debug, Clone, Serialize)]
pub struct Active {
    pub effective: Effective,
    pub local: SocketAddr,
    /// The network address that is really being served.
    pub network: Option<SocketAddr>,
    /// Why the network address could not be opened, if it could not.
    pub error: Option<String>,
    /// The network of the network address: the address and its prefix length.
    pub subnet: Option<(Ipv4Addr, u8)>,
}

impl Active {
    pub fn local_only(port: u16) -> Self {
        let none = NetSettings::default();
        Self {
            effective: effective(&none, None, Some(port), port),
            local: local_addr(port),
            network: None,
            error: None,
            subnet: None,
        }
    }

    /// Why the computer at `remote` may not use the network address right
    /// now, or `None` when it may. Checked on every request from the network.
    pub fn refuse(&self, limits: Limits, remote: IpAddr, now: i64, password_set: bool) -> Option<&'static str> {
        if !password_set {
            return Some(REFUSED_NO_PASSWORD);
        }
        if limits.expires_at.is_some_and(|end| now >= end) {
            return Some(REFUSED_EXPIRED);
        }
        if !limits.allow_other_networks {
            let same = match (remote, self.subnet) {
                (IpAddr::V4(r), Some((ip, prefix))) => same_network(r, ip, prefix),
                // Network unknown: only this computer's own address is certain.
                _ => self.network.is_some_and(|n| n.ip() == remote),
            };
            if !same {
                return Some(REFUSED_OTHER_NETWORK);
            }
        }
        None
    }

    /// The network of the network address, written as `192.168.1.0/24`.
    pub fn subnet_text(&self) -> Option<String> {
        self.subnet.map(|(ip, prefix)| {
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
            format!("{}/{prefix}", Ipv4Addr::from(u32::from(ip) & mask))
        })
    }

    pub fn local_url(&self) -> String {
        format!("http://{}/", self.local)
    }

    pub fn network_url(&self) -> Option<String> {
        let scheme = if self.effective.https { "https" } else { "http" };
        self.network.map(|a| format!("{scheme}://{a}/"))
    }

    /// Whether a request's `Host` names one of the configured host names, or
    /// the network address itself (which matters when that address is outside
    /// the private ranges the general rule accepts).
    pub fn host_allowed(&self, authority: &str) -> bool {
        let host = crate::net::host_without_port(authority);
        self.effective.hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
            || self.network.is_some_and(|n| host.parse::<IpAddr>().is_ok_and(|ip| ip == n.ip()))
    }
}

pub fn local_addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Read a certificate and key into a TLS configuration, with errors that say
/// which file is wrong and how.
pub fn load_tls(cert_file: &str, key_file: &str) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_file)
        .map_err(|e| format!("certificate file {cert_file}: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificate file {cert_file}: {e}"))?;
    if certs.is_empty() {
        return Err(format!(
            "certificate file {cert_file}: no certificate found (expected a PEM file with a BEGIN CERTIFICATE block)"
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_file)
        .map_err(|e| format!("private key file {key_file}: {e} (expected a PEM file with a BEGIN PRIVATE KEY block)"))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the certificate and the private key do not belong together: {e}"))?;
    // HTTP/1.1 only: the live log stream is a WebSocket.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// An address of this machine that other computers could use.
#[derive(Debug, Clone, Serialize)]
pub struct LocalAddress {
    pub ip: String,
    pub interface: String,
    /// `lan`, `tailscale`, or `other` for an address outside the private ranges.
    pub kind: &'static str,
    /// Length of the network prefix, for example 24.
    pub prefix: u8,
}

/// This machine's IPv4 addresses on its networks: LAN first, then addresses
/// outside the private ranges, then Tailscale.
pub fn local_addresses() -> Vec<LocalAddress> {
    let networks = sysinfo::Networks::new_with_refreshed_list();
    let mut out: Vec<LocalAddress> = Vec::new();
    for (name, data) in networks.iter() {
        for net in data.ip_networks() {
            let IpAddr::V4(v4) = net.addr else { continue };
            if v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
                continue;
            }
            let kind = if v4.is_private() {
                "lan"
            } else if crate::net::is_private_ip(&net.addr) {
                "tailscale"
            } else {
                "other"
            };
            if !out.iter().any(|a| a.ip == v4.to_string()) {
                out.push(LocalAddress { ip: v4.to_string(), interface: name.clone(), kind, prefix: net.prefix });
            }
        }
    }
    out.sort_by(|a, b| a.kind.cmp(b.kind).then(a.ip.cmp(&b.ip)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(address: &str) -> NetSettings {
        NetSettings { address: address.into(), ..Default::default() }
    }

    #[test]
    fn only_private_addresses_of_a_network_are_accepted() {
        assert_eq!(settings("").validate().unwrap().address, "");
        assert_eq!(settings(" 192.168.1.20 ").validate().unwrap().address, "192.168.1.20");
        assert!(settings("100.101.102.103").validate().is_ok());
        for bad in ["0.0.0.0", "127.0.0.1", "8.8.8.8", "localhost", "192.168.1"] {
            assert!(settings(bad).validate().is_err(), "{bad} must be refused");
        }
        // Outside the private ranges: never an address this computer lacks,
        // even when "it is internal" is ticked.
        let foreign = NetSettings { address: "8.8.8.8".into(), internal_confirmed: true, ..Default::default() };
        assert!(foreign.validate().unwrap_err().contains("not an address of this computer"));
        // The tick is dropped where it means nothing.
        let private = NetSettings { address: "10.0.0.5".into(), internal_confirmed: true, ..Default::default() };
        assert!(!private.validate().unwrap().internal_confirmed);
        assert!(NetSettings { port: Some(0), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn host_names_are_tidied_and_checked() {
        let s = NetSettings { hosts: vec![" Portal.Example.LAN. ".into(), "portal.example.lan".into(), "".into(), "pc-01".into()], ..Default::default() };
        assert_eq!(s.validate().unwrap().hosts, vec!["portal.example.lan", "pc-01"]);
        for bad in ["http://portal.lan", "portal.lan:7878", "bad host", "-x.lan", "192.168.1.5", "a..b"] {
            let s = NetSettings { hosts: vec![bad.into()], ..Default::default() };
            assert!(s.validate().is_err(), "{bad} must be refused");
        }
        let many = NetSettings { hosts: (0..11).map(|i| format!("h{i}.lan")).collect(), ..Default::default() };
        assert!(many.validate().is_err());
    }

    #[test]
    fn https_needs_files_that_can_be_read() {
        let s = NetSettings { https: true, ..Default::default() };
        assert!(s.validate().unwrap_err().contains("both a certificate file and a private key file"));

        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let missing = load_tls(&cert.to_string_lossy(), &key.to_string_lossy()).unwrap_err();
        assert!(missing.starts_with("certificate file"), "{missing}");

        // A file with no certificate in it is named as the problem.
        std::fs::write(&cert, "not a certificate").unwrap();
        std::fs::write(&key, "not a key").unwrap();
        let empty = load_tls(&cert.to_string_lossy(), &key.to_string_lossy()).unwrap_err();
        assert!(empty.contains("no certificate found"), "{empty}");

        // HTTPS switched off: the files are not looked at.
        let off = NetSettings { cert_file: cert.to_string_lossy().into_owned(), ..Default::default() };
        assert!(off.validate().is_ok());
    }

    #[test]
    fn the_command_line_wins_over_stored_settings() {
        let saved = NetSettings { address: "192.168.1.20".into(), port: Some(9000), https: true, ..Default::default() };
        let e = effective(&saved, None, None, 7878);
        assert_eq!((e.port, e.network, e.https), (9000, Some("192.168.1.20".parse().unwrap()), true));
        assert!(!e.bind_from_cli && !e.port_from_cli);

        let e = effective(&saved, Some("100.64.0.9".parse().unwrap()), Some(7000), 7878);
        assert_eq!((e.port, e.network), (7000, Some("100.64.0.9".parse().unwrap())));
        assert!(e.bind_from_cli && e.port_from_cli);

        // `--bind 127.0.0.1` means this computer only, and HTTPS has nothing to protect.
        let e = effective(&saved, Some("127.0.0.1".parse().unwrap()), None, 7878);
        assert_eq!((e.network, e.https), (None, false));

        let e = effective(&NetSettings::default(), None, None, 7878);
        assert_eq!((e.port, e.network, e.https), (7878, None, false));
    }

    #[test]
    fn other_computers_are_refused_by_the_rules() {
        let mut active = Active::local_only(7878);
        active.network = Some("192.0.1.90:7878".parse().unwrap());
        active.subnet = Some(("192.0.1.90".parse().unwrap(), 24));
        assert_eq!(active.subnet_text().unwrap(), "192.0.1.0/24");
        let near: IpAddr = "192.0.1.17".parse().unwrap();
        let far: IpAddr = "192.0.2.17".parse().unwrap();
        let open = Limits::default();

        // Never without a password, whoever asks.
        assert_eq!(active.refuse(open, near, 100, false), Some(REFUSED_NO_PASSWORD));
        // Same network only, unless other networks are allowed.
        assert_eq!(active.refuse(open, near, 100, true), None);
        assert_eq!(active.refuse(open, far, 100, true), Some(REFUSED_OTHER_NETWORK));
        let any = Limits { allow_other_networks: true, expires_at: None };
        assert_eq!(active.refuse(any, far, 100, true), None);
        // A time limit closes it for everyone once it has passed.
        let timed = Limits { allow_other_networks: false, expires_at: Some(200) };
        assert_eq!(active.refuse(timed, near, 199, true), None);
        assert_eq!(active.refuse(timed, near, 200, true), Some(REFUSED_EXPIRED));

        assert!(same_network("10.1.2.3".parse().unwrap(), "10.1.9.9".parse().unwrap(), 16));
        assert!(!same_network("10.1.2.3".parse().unwrap(), "10.2.2.3".parse().unwrap(), 16));

        // "Open for 30 minutes" becomes an end time; switching access off clears it.
        let s = NetSettings { address: "10.0.0.5".into(), open_for_minutes: Some(30), ..Default::default() }.validate().unwrap();
        let left = s.expires_at.unwrap() - chrono::Utc::now().timestamp();
        assert!((1790..=1800).contains(&left), "{left}");
        let off = NetSettings { expires_at: Some(5), ..Default::default() }.validate().unwrap();
        assert_eq!(off.expires_at, None);
        assert!(NetSettings { address: "10.0.0.5".into(), open_for_minutes: Some(0), ..Default::default() }.validate().is_err());
    }

    #[test]
    fn configured_host_names_are_answered() {
        let mut active = Active::local_only(7878);
        assert_eq!(active.local_url(), "http://127.0.0.1:7878/");
        assert_eq!(active.network_url(), None);
        assert!(!active.host_allowed("portal.example.lan:7878"));
        active.effective.hosts = vec!["portal.example.lan".into()];
        assert!(active.host_allowed("Portal.Example.lan:7878"));
        assert!(!active.host_allowed("evil.example.com"));
        // The network address itself is answered, private range or not.
        assert!(!active.host_allowed("192.0.1.90:7878"));
        active.network = Some("192.0.1.90:7878".parse().unwrap());
        assert!(active.host_allowed("192.0.1.90:7878") && !active.host_allowed("192.0.1.91:7878"));
        active.network = Some("192.168.1.20:7878".parse().unwrap());
        active.effective.https = true;
        assert_eq!(active.network_url().unwrap(), "https://192.168.1.20:7878/");
    }
}
