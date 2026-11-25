//! Gestión persistente de redes virtuales para Two Five Five (255).
//!
//! Soporta redes virtuales en modos:
//! - `nat`: Stack NAT en modo usuario con DHCP, ARP, ICMP, DNS, TCP saliente y port-forwarding TCP entrante.
//! - `hostonly`: Red aislada sin salida a Internet, pero accesible entre host y guest (DHCP + port-forwarding SSH).
//! - `internal`: Switch interno aislado entre VMs en el host mediante bus UDP en loopback.
//! - `tap`: Enlace directo por interfaz `/dev/net/tun` TAP Linux.

use crate::devices::virtio_net::{NetConfig, NetMode};
use std::fmt;
use std::fs;
use std::path::Path;

pub const NETWORK_CONFIG_FILE: &str = "networks.conf";
pub const DEFAULT_NETWORK_NAME: &str = "nat-default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    Nat,
    HostOnly,
    Internal,
    Tap,
}

impl NetworkMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkMode::Nat => "nat",
            NetworkMode::HostOnly => "hostonly",
            NetworkMode::Internal => "internal",
            NetworkMode::Tap => "tap",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            NetworkMode::Nat => "NAT (Acceso Internet + Port-Fwd)",
            NetworkMode::HostOnly => "Solo Anfitrion (Host-Only)",
            NetworkMode::Internal => "Red Interna (Switch L2)",
            NetworkMode::Tap => "Puente TAP Linux (/dev/net/tun)",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "nat" | "slirp" => Ok(NetworkMode::Nat),
            "hostonly" | "host-only" | "host_only" => Ok(NetworkMode::HostOnly),
            "internal" | "intnet" => Ok(NetworkMode::Internal),
            "tap" | "bridge" => Ok(NetworkMode::Tap),
            other => Err(format!(
                "Modo de red desconocido '{}'. Usa: nat, hostonly, internal o tap",
                other
            )),
        }
    }

    pub fn to_net_mode(self) -> NetMode {
        match self {
            NetworkMode::Nat => NetMode::Nat,
            NetworkMode::HostOnly => NetMode::HostOnly,
            NetworkMode::Internal => NetMode::Internal,
            NetworkMode::Tap => NetMode::Tap,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortForward {
    pub host_port: u16,
    pub guest_port: u16,
}

impl PortForward {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let trimmed = spec.trim();
        let (h, g) = trimmed
            .split_once(':')
            .ok_or_else(|| format!("Formato port-forward inválido '{}' (usa host:guest)", spec))?;
        let host_port: u16 = h
            .trim()
            .parse()
            .map_err(|_| format!("Puerto host inválido '{}'", h))?;
        let guest_port: u16 = g
            .trim()
            .parse()
            .map_err(|_| format!("Puerto guest inválido '{}'", g))?;
        if host_port == 0 || guest_port == 0 {
            return Err("Los puertos de port-forwarding deben ser > 0".to_string());
        }
        Ok(Self {
            host_port,
            guest_port,
        })
    }
}

impl fmt::Display for PortForward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host_port, self.guest_port)
    }
}

#[derive(Debug, Clone)]
pub struct VirtualNetwork {
    pub name: String,
    pub mode: NetworkMode,
    pub subnet: String,
    pub gateway_ip: [u8; 4],
    pub guest_ip: [u8; 4],
    pub dns_ip: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub tap_if: Option<String>,
    pub internal_udp_port: u16,
    pub port_forwards: Vec<PortForward>,
}

impl VirtualNetwork {
    pub fn default_nat() -> Self {
        Self {
            name: DEFAULT_NETWORK_NAME.to_string(),
            mode: NetworkMode::Nat,
            subnet: "10.0.2.0/24".to_string(),
            gateway_ip: [10, 0, 2, 2],
            guest_ip: [10, 0, 2, 15],
            dns_ip: [10, 0, 2, 3],
            subnet_mask: [255, 255, 255, 0],
            tap_if: None,
            internal_udp_port: 0,
            port_forwards: Vec::new(),
        }
    }

    pub fn from_cidr(
        name: &str,
        mode: NetworkMode,
        cidr: &str,
        tap_if: Option<String>,
        port_forwards: Vec<PortForward>,
    ) -> Result<Self, String> {
        let clean_name = name.trim().replace('|', "-");
        if clean_name.is_empty() {
            return Err("El nombre de la red no puede estar vacío".to_string());
        }
        let (gw, dns, guest, mask) = parse_cidr(cidr)?;
        let internal_udp_port = if mode == NetworkMode::Internal {
            derive_internal_port(&clean_name)
        } else {
            0
        };
        Ok(Self {
            name: clean_name,
            mode,
            subnet: cidr.trim().to_string(),
            gateway_ip: gw,
            guest_ip: guest,
            dns_ip: dns,
            subnet_mask: mask,
            tap_if: tap_if.and_then(|s| {
                let t = s.trim().to_string();
                if t.is_empty() { None } else { Some(t) }
            }),
            internal_udp_port,
            port_forwards,
        })
    }

    /// Convierte esta red virtual en una configuración concreta `NetConfig` para VirtIO-Net.
    pub fn to_net_config(
        &self,
        extra_forwards: &[PortForward],
        guest_mac: [u8; 6],
    ) -> NetConfig {
        let mut forwards: Vec<(u16, u16)> = self
            .port_forwards
            .iter()
            .chain(extra_forwards.iter())
            .map(|pf| (pf.host_port, pf.guest_port))
            .collect();
        forwards.sort_unstable();
        forwards.dedup();

        NetConfig {
            gateway_ip: self.gateway_ip,
            dns_ip: self.dns_ip,
            guest_ip: self.guest_ip,
            subnet_mask: self.subnet_mask,
            gateway_mac: [
                0x52,
                0x54,
                0x00,
                self.gateway_ip[1],
                self.gateway_ip[2],
                0x02,
            ],
            tap_env_var: "MI_VMM_TAP",
            mode: self.mode.to_net_mode(),
            tap_name: self.tap_if.clone(),
            guest_mac,
            port_forwards: forwards,
            internal_udp_port: if self.internal_udp_port != 0 {
                self.internal_udp_port
            } else {
                derive_internal_port(&self.name)
            },
        }
    }
}

fn derive_internal_port(name: &str) -> u16 {
    let mut h: u32 = 2166136261;
    for b in name.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    45500 + (h % 4000) as u16
}

/// Parsea una subred IPv4 CIDR (ej. `"10.0.20.0/24"` o `"192.168.56.0/24"`) y devuelve
/// `(gateway_ip, dns_ip, guest_ip, subnet_mask)`.
pub fn parse_cidr(cidr: &str) -> Result<([u8; 4], [u8; 4], [u8; 4], [u8; 4]), String> {
    let trimmed = cidr.trim();
    let (ip_str, prefix_str) = trimmed
        .split_once('/')
        .ok_or_else(|| format!("Subred CIDR inválida '{}' (ejemplo: 10.0.20.0/24)", cidr))?;
    let octets: Vec<u8> = ip_str
        .split('.')
        .map(|part| part.trim().parse::<u8>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("IP inválida en CIDR '{}'", cidr))?;
    if octets.len() != 4 {
        return Err(format!("IPv4 debe tener 4 octetos en '{}'", cidr));
    }
    let prefix: u32 = prefix_str
        .trim()
        .parse()
        .map_err(|_| format!("Prefijo CIDR inválido '{}'", prefix_str))?;
    if !(8..=29).contains(&prefix) {
        return Err(format!(
            "Prefijo CIDR /{} fuera de rango válido (/8../29)",
            prefix
        ));
    }

    let mask_u32 = (!0u32).checked_shl(32 - prefix).unwrap_or(0);
    let ip_u32 = u32::from_be_bytes([octets[0], octets[1], octets[2], octets[3]]);
    let net_u32 = ip_u32 & mask_u32;

    let gw = (net_u32 + 2).to_be_bytes();
    let dns = (net_u32 + 3).to_be_bytes();
    let guest = if prefix <= 27 {
        (net_u32 + 15).to_be_bytes()
    } else {
        (net_u32 + 4).to_be_bytes()
    };
    let mask = mask_u32.to_be_bytes();
    Ok((gw, dns, guest, mask))
}

pub fn format_ipv4(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

/// Genera una dirección MAC unicast administrada localmente con el prefijo de 255 (`02:55:00:...`).
pub fn generate_mac(seed: &str) -> String {
    let mut h: u32 = 0x811C_9DC5;
    for b in seed.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    let b3 = ((h >> 16) & 0xFF) as u8;
    let b4 = ((h >> 8) & 0xFF) as u8;
    let b5 = ((h & 0xFF) as u8).max(1);
    format!("02:55:00:{:02X}:{:02X}:{:02X}", b3, b4, b5)
}

/// Parsea una dirección MAC ASCII `"XX:XX:XX:XX:XX:XX"` o `"XX-XX-..."`.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part.trim(), 16).ok()?;
    }
    Some(mac)
}

pub fn load_networks() -> Vec<VirtualNetwork> {
    load_networks_from(Path::new(NETWORK_CONFIG_FILE))
}

pub fn load_networks_from(path: &Path) -> Vec<VirtualNetwork> {
    let mut list = Vec::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 3 {
                continue;
            }
            let name = parts[0].trim().to_string();
            if name.is_empty() || list.iter().any(|n: &VirtualNetwork| n.name == name) {
                continue;
            }
            let mode = NetworkMode::parse(parts[1]).unwrap_or(NetworkMode::Nat);
            let subnet = parts[2].trim().to_string();
            let parsed = parse_cidr(&subnet).unwrap_or((
                [10, 0, 2, 2],
                [10, 0, 2, 3],
                [10, 0, 2, 15],
                [255, 255, 255, 0],
            ));
            let gateway_ip = parts
                .get(3)
                .and_then(|s| parse_ipv4(s))
                .unwrap_or(parsed.0);
            let guest_ip = parts
                .get(4)
                .and_then(|s| parse_ipv4(s))
                .unwrap_or(parsed.2);
            let dns_ip = parts
                .get(5)
                .and_then(|s| parse_ipv4(s))
                .unwrap_or(parsed.1);
            let tap_if = parts
                .get(6)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let internal_udp_port = parts
                .get(7)
                .and_then(|s| s.trim().parse::<u16>().ok())
                .unwrap_or(0);
            let port_forwards = parts
                .get(8)
                .map(|csv| {
                    csv.split(',')
                        .filter_map(|item| PortForward::parse(item).ok())
                        .collect()
                })
                .unwrap_or_default();

            list.push(VirtualNetwork {
                name,
                mode,
                subnet,
                gateway_ip,
                guest_ip,
                dns_ip,
                subnet_mask: parsed.3,
                tap_if,
                internal_udp_port,
                port_forwards,
            });
        }
    }

    if !list.iter().any(|n| n.name == DEFAULT_NETWORK_NAME) {
        list.insert(0, VirtualNetwork::default_nat());
    }
    list
}

pub fn save_networks(nets: &[VirtualNetwork]) -> std::io::Result<()> {
    save_networks_to(Path::new(NETWORK_CONFIG_FILE), nets)
}

pub fn save_networks_to(path: &Path, nets: &[VirtualNetwork]) -> std::io::Result<()> {
    let mut out = String::from("# Two Five Five (255) - Redes Virtuales\n");
    out.push_str("# Formato: nombre|modo|subred|gateway|guest_ip|dns|tap_if|internal_port|port_forwards\n");
    for n in nets {
        let forwards: Vec<String> = n.port_forwards.iter().map(|pf| pf.to_string()).collect();
        out.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
            n.name.replace('|', "/"),
            n.mode.as_str(),
            n.subnet.replace('|', "/"),
            format_ipv4(n.gateway_ip),
            format_ipv4(n.guest_ip),
            format_ipv4(n.dns_ip),
            n.tap_if.as_deref().unwrap_or(""),
            n.internal_udp_port,
            forwards.join(","),
        ));
    }
    fs::write(path, out)
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let parts: Vec<u8> = s
        .trim()
        .split('.')
        .filter_map(|p| p.trim().parse::<u8>().ok())
        .collect();
    if parts.len() == 4 {
        Some([parts[0], parts[1], parts[2], parts[3]])
    } else {
        None
    }
}

/// Sugiere una subred `10.0.<n>.0/24` no utilizada aún en `networks`.
pub fn next_available_subnet(networks: &[VirtualNetwork]) -> String {
    for third in 10u8..=240 {
        let candidate = format!("10.0.{}.0/24", third);
        if !networks.iter().any(|n| n.subnet == candidate) {
            return candidate;
        }
    }
    "10.0.99.0/24".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cidr_parsing_and_mac_generation() {
        let (gw, dns, guest, mask) = parse_cidr("192.168.56.0/24").expect("valid cidr");
        assert_eq!(gw, [192, 168, 56, 2]);
        assert_eq!(dns, [192, 168, 56, 3]);
        assert_eq!(guest, [192, 168, 56, 15]);
        assert_eq!(mask, [255, 255, 255, 0]);
        assert!(parse_cidr("10.0.0.1/32").is_err());

        let mac_str = generate_mac("kali-lab");
        let mac_bytes = parse_mac(&mac_str).expect("valid mac");
        assert_eq!(mac_bytes[0], 0x02);
        assert_eq!(mac_bytes[1], 0x55);
        assert_eq!(mac_bytes[2], 0x00);
    }

    #[test]
    fn test_networks_persistence_roundtrip() {
        let path = std::env::temp_dir().join(format!("tff_nets_test_{}.conf", std::process::id()));
        let mut nets = load_networks_from(&path);
        assert_eq!(nets[0].name, DEFAULT_NETWORK_NAME);

        let lab = VirtualNetwork::from_cidr(
            "dmz-lab",
            NetworkMode::HostOnly,
            "10.0.25.0/24",
            None,
            vec![PortForward::parse("2225:22").unwrap()],
        )
        .unwrap();
        nets.push(lab);
        save_networks_to(&path, &nets).expect("save nets");

        let reloaded = load_networks_from(&path);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded[1].name, "dmz-lab");
        assert_eq!(reloaded[1].mode, NetworkMode::HostOnly);
        assert_eq!(reloaded[1].guest_ip, [10, 0, 25, 15]);
        assert_eq!(reloaded[1].port_forwards[0].host_port, 2225);
        let _ = fs::remove_file(&path);
    }
}
