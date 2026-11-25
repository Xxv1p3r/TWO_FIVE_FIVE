//! Emulación de dispositivo PCI VirtIO-Net (Red Ethernet transitional legacy).
//!
//! Traducido y optimizado a partir de los patrones de VirtualBox (`DevVirtioNet.cpp`).
//! Es compatible con el controlador de red `virtio_net` estándar del kernel Linux
//! (Linux Mint, Ubuntu, Debian, Fedora, Arch, etc.) y Windows.
//!
//! Características implementadas:
//! 1. Colas VirtQueue Split Legacy (Queue 0: RX, Queue 1: TX) con tamaño 64.
//! 2. Cabecera estándar legacy de 10 bytes (`virtio_net_hdr` sin MRG_RXBUF).
//! 3. Stack de red en modo usuario totalmente integrado y autónomo:
//!    - Servidor DHCP (asigna IP 10.0.2.15, gateway 10.0.2.2, DNS 10.0.2.3, máscara 255.255.255.0).
//!    - Respondedor ARP para resolución instantánea de 10.0.2.2 y 10.0.2.3.
//!    - Respondedor ICMP Echo (Ping hacia 10.0.2.2 y 10.0.2.3).
//!    - Proxy / Forwarder DNS transparente UDP hacia servidores upstream (8.8.8.8).
//! 4. Conectividad TAP nativa en Linux si se define la variable `MI_VMM_TAP` (ej. `MI_VMM_TAP=tap0`).

use super::IoDevice;
use crate::guest_mem::GuestMemory;
use std::collections::{HashMap, VecDeque};
use std::sync::{mpsc, Arc, Mutex};

// ─── Constantes VirtIO Legacy PCI ──────────────────────────────────
pub const VIRTIO_PCI_HOST_FEATURES: u16 = 0x00;
pub const VIRTIO_PCI_GUEST_FEATURES: u16 = 0x04;
pub const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;
pub const VIRTIO_PCI_QUEUE_NUM: u16 = 0x0C;
pub const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;
pub const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;
pub const VIRTIO_PCI_STATUS: u16 = 0x12;
pub const VIRTIO_PCI_ISR: u16 = 0x13;

pub const VIRTIO_NET_F_MAC: u32 = 1 << 5;
pub const VIRTIO_NET_F_STATUS: u32 = 1 << 16;

#[allow(dead_code)]
pub const VIRTIO_CONFIG_S_ACKNOWLEDGE: u8 = 1;
#[allow(dead_code)]
pub const VIRTIO_CONFIG_S_DRIVER: u8 = 2;
pub const VIRTIO_CONFIG_S_DRIVER_OK: u8 = 4;
#[allow(dead_code)]
pub const VIRTIO_CONFIG_S_FAILED: u8 = 128;

pub const VIRTIO_NET_S_LINK_UP: u16 = 1;

pub const QUEUE_SIZE: u16 = 64;

// Banderas de descriptor VirtIO
const VRING_DESC_F_NEXT: u16 = 0x0001;
const VRING_DESC_F_WRITE: u16 = 0x0002;

// ─── Direcciones de Red por Defecto (Slirp / Modo Usuario) ─────────
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
pub const SUBNET_MASK: [u8; 4] = [255, 255, 255, 0];
#[allow(dead_code)]
pub const BROADCAST_IP: [u8; 4] = [10, 0, 2, 255];

pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x02];
pub const DEFAULT_GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
pub const BROADCAST_MAC: [u8; 6] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// Modo de operación de la interfaz de red virtual.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetMode {
    Nat,
    HostOnly,
    Internal,
    Tap,
}

/// Configuración de red por adaptador para evitar colisiones entre VirtIO-Net y E1000.
#[derive(Clone, Debug)]
pub struct NetConfig {
    pub gateway_ip: [u8; 4],
    pub dns_ip: [u8; 4],
    pub guest_ip: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub gateway_mac: [u8; 6],
    pub tap_env_var: &'static str,
    pub mode: NetMode,
    pub tap_name: Option<String>,
    pub guest_mac: [u8; 6],
    pub port_forwards: Vec<(u16, u16)>,
    pub internal_udp_port: u16,
}

impl NetConfig {
    pub fn default_slirp() -> Self {
        Self {
            gateway_ip: GATEWAY_IP,
            dns_ip: DNS_IP,
            guest_ip: GUEST_IP,
            subnet_mask: SUBNET_MASK,
            gateway_mac: GATEWAY_MAC,
            tap_env_var: "MI_VMM_TAP",
            mode: NetMode::Nat,
            tap_name: None,
            guest_mac: DEFAULT_GUEST_MAC,
            port_forwards: Vec::new(),
            internal_udp_port: 0,
        }
    }

    pub fn e1000() -> Self {
        Self {
            gateway_ip: [10, 0, 3, 2],
            dns_ip: [10, 0, 3, 3],
            guest_ip: [10, 0, 3, 15],
            subnet_mask: [255, 255, 255, 0],
            gateway_mac: [0x52, 0x54, 0x00, 0x12, 0x35, 0x02],
            tap_env_var: "TWO_FIVE_FIVE_E1000_TAP",
            mode: NetMode::Nat,
            tap_name: None,
            guest_mac: [0x08, 0x00, 0x27, 0x12, 0x34, 0x56],
            port_forwards: Vec::new(),
            internal_udp_port: 0,
        }
    }
}

/// Lee el primer servidor DNS IPv4 no-loopback de `/etc/resolv.conf` con fallback `8.8.8.8:53`.
fn detect_host_dns_server() -> String {
    if let Ok(contents) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in contents.lines() {
            let trimmed = line.split('#').next().unwrap_or("").trim();
            if let Some(rest) = trimmed.strip_prefix("nameserver") {
                let ip = rest.trim();
                if let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() {
                    if !addr.is_loopback() && !addr.is_unspecified() {
                        return format!("{}:53", addr);
                    }
                }
            }
        }
    }
    "8.8.8.8:53".to_string()
}

/// Cabecera VirtIO-Net Legacy de 10 bytes (VirtIO 1.0, 5.1.6).
#[allow(dead_code)]
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
}

/// Cola VirtQueue (Split Virtqueue Legacy).
#[derive(Debug, Clone, Copy)]
pub struct VirtQueue {
    pub size: u16,
    pub pfn: u32,
    pub last_avail_idx: u16,
    pub last_used_idx: u16,
}

impl Default for VirtQueue {
    fn default() -> Self {
        Self {
            size: QUEUE_SIZE,
            pfn: 0,
            last_avail_idx: 0,
            last_used_idx: 0,
        }
    }
}

impl VirtQueue {
    #[inline]
    pub fn is_ready(&self) -> bool {
        self.pfn != 0
    }

    #[inline]
    pub fn desc_addr(&self) -> usize {
        (self.pfn as usize) << 12
    }

    #[inline]
    pub fn avail_addr(&self) -> usize {
        self.desc_addr() + 16 * (self.size as usize)
    }

    #[inline]
    pub fn used_addr(&self) -> usize {
        let avail_end = self.avail_addr() + 4 + 2 * (self.size as usize);
        (avail_end + 4095) & !4095
    }

    /// Escribe un paquete en una cadena de descriptores disponible (Host -> Guest, RX).
    /// Devuelve true si se escribió el buffer con éxito.
    pub fn write_buffer(&mut self, mem: &GuestMemory, data: &[u8]) -> bool {
        if !self.is_ready() || data.is_empty() {
            return false;
        }

        let avail_addr = self.avail_addr();
        let avail_idx = mem.read_u16(avail_addr + 2);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        if avail_idx == self.last_avail_idx {
            return false;
        }

        let desc_table = self.desc_addr();
        let head_slot = (self.last_avail_idx % self.size) as usize;
        let head_idx = mem.read_u16(avail_addr + 4 + 2 * head_slot);

        let mut cur_idx = head_idx;
        let mut bytes_written = 0;
        let mut remaining = data;
        let mut loop_guard = self.size as usize;

        while loop_guard > 0 {
            loop_guard -= 1;
            if (cur_idx as usize) >= (self.size as usize) {
                break;
            }
            let desc_entry = desc_table + (cur_idx as usize) * 16;
            let buf_low = mem.read_u32(desc_entry) as usize;
            let buf_high = mem.read_u32(desc_entry + 4) as usize;
            let buf_gpa = buf_low | (buf_high << 32);
            let buf_len = mem.read_u32(desc_entry + 8) as usize;
            let flags = mem.read_u16(desc_entry + 12);
            let next = mem.read_u16(desc_entry + 14);

            if (flags & VRING_DESC_F_WRITE) != 0 && !remaining.is_empty() {
                let to_write = remaining.len().min(buf_len);
                mem.copy_to(buf_gpa, &remaining[..to_write]);
                remaining = &remaining[to_write..];
                bytes_written += to_write;
            }

            if (flags & VRING_DESC_F_NEXT) != 0 {
                if (next as usize) >= (self.size as usize) {
                    break;
                }
                cur_idx = next;
            } else {
                break;
            }
        }

        // Registrar elemento en el Used Ring con índice local rastreado y barrera Release
        let used_addr = self.used_addr();
        let used_idx = self.last_used_idx;
        let used_slot = (used_idx % self.size) as usize;
        let elem_addr = used_addr + 4 + 8 * used_slot;

        mem.write_u32(elem_addr, head_idx as u32);
        mem.write_u32(elem_addr + 4, bytes_written as u32);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        mem.write_u16(used_addr + 2, used_idx.wrapping_add(1));
        self.last_used_idx = used_idx.wrapping_add(1);
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        true
    }

    /// Lee un paquete de una cadena de descriptores disponible (Guest -> Host, TX).
    pub fn read_buffer(&mut self, mem: &GuestMemory) -> Option<Vec<u8>> {
        if !self.is_ready() {
            return None;
        }

        let avail_addr = self.avail_addr();
        let avail_idx = mem.read_u16(avail_addr + 2);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        if avail_idx == self.last_avail_idx {
            return None;
        }

        let desc_table = self.desc_addr();
        let head_slot = (self.last_avail_idx % self.size) as usize;
        let head_idx = mem.read_u16(avail_addr + 4 + 2 * head_slot);

        let mut cur_idx = head_idx;
        let mut data = Vec::new();
        let mut loop_guard = self.size as usize;

        while loop_guard > 0 {
            loop_guard -= 1;
            if (cur_idx as usize) >= (self.size as usize) {
                break;
            }
            let desc_entry = desc_table + (cur_idx as usize) * 16;
            let buf_low = mem.read_u32(desc_entry) as usize;
            let buf_high = mem.read_u32(desc_entry + 4) as usize;
            let buf_gpa = buf_low | (buf_high << 32);
            let buf_len = mem.read_u32(desc_entry + 8) as usize;
            let flags = mem.read_u16(desc_entry + 12);
            let next = mem.read_u16(desc_entry + 14);

            if (flags & VRING_DESC_F_WRITE) == 0 && buf_len > 0 && data.len() < 65536 {
                let capped = buf_len.min(65536 - data.len());
                let mut chunk = vec![0u8; capped];
                let copied = mem.copy_from(buf_gpa, &mut chunk);
                chunk.truncate(copied);
                data.extend_from_slice(&chunk);
            }

            if (flags & VRING_DESC_F_NEXT) != 0 {
                if (next as usize) >= (self.size as usize) {
                    break;
                }
                cur_idx = next;
            } else {
                break;
            }
        }

        // Registrar elemento en el Used Ring con len = 0 (especificación VirtIO para buffers de salida)
        let used_addr = self.used_addr();
        let used_idx = self.last_used_idx;
        let used_slot = (used_idx % self.size) as usize;
        let elem_addr = used_addr + 4 + 8 * used_slot;

        mem.write_u32(elem_addr, head_idx as u32);
        mem.write_u32(elem_addr + 4, 0);
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        mem.write_u16(used_addr + 2, used_idx.wrapping_add(1));
        self.last_used_idx = used_idx.wrapping_add(1);
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        Some(data)
    }
}

// ─── Helpers de Protocolos de Red (Checksum, ARP, ICMP, DHCP, UDP) ──

/// Calcula la suma de verificación estándar de 16 bits en complemento a uno (RFC 1071).
pub fn ip_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for chunk in data.chunks(2) {
        let val = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]]) as u32
        } else {
            (chunk[0] as u32) << 8
        };
        sum = sum.wrapping_add(val);
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Construye un paquete UDP completo dentro de una trama Ethernet + IPv4.
pub fn build_udp_packet(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let udp_len = (8 + payload.len()) as u16;
    let ip_total_len = (20 + udp_len) as u16;
    let frame_len = 14 + ip_total_len as usize;
    let mut frame = vec![0u8; frame_len];

    // Cabecera Ethernet (14 bytes)
    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4

    // Cabecera IPv4 (20 bytes)
    frame[14] = 0x45; // Version 4, IHL 5 (20 bytes)
    frame[15] = 0x00; // DSCP / ECN
    frame[16..18].copy_from_slice(&ip_total_len.to_be_bytes());
    frame[18..20].copy_from_slice(&0x1234u16.to_be_bytes());
    frame[20..22].copy_from_slice(&0x0000u16.to_be_bytes());
    frame[22] = 64;   // TTL
    frame[23] = 17;   // Protocolo UDP
    frame[24..26].copy_from_slice(&[0, 0]); // Checksum placeholder
    frame[26..30].copy_from_slice(&src_ip);
    frame[30..34].copy_from_slice(&dst_ip);

    let ip_csum = ip_checksum(&frame[14..34]);
    frame[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    // Cabecera UDP (8 bytes)
    frame[34..36].copy_from_slice(&src_port.to_be_bytes());
    frame[36..38].copy_from_slice(&dst_port.to_be_bytes());
    frame[38..40].copy_from_slice(&udp_len.to_be_bytes());
    frame[40..42].copy_from_slice(&[0, 0]); // Checksum placeholder

    // Carga útil UDP
    frame[42..42 + payload.len()].copy_from_slice(payload);

    // Calcular Checksum UDP con Pseudo-cabecera
    let mut pseudo = Vec::with_capacity(12 + 8 + payload.len());
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(17);
    pseudo.extend_from_slice(&udp_len.to_be_bytes());
    pseudo.extend_from_slice(&frame[34..42 + payload.len()]);
    let udp_csum = ip_checksum(&pseudo);
    let final_csum = if udp_csum == 0 { 0xFFFF } else { udp_csum };
    frame[40..42].copy_from_slice(&final_csum.to_be_bytes());

    if frame.len() < 60 {
        frame.resize(60, 0);
    }

    frame
}

/// Construye un paquete TCP completo dentro de una trama Ethernet + IPv4.
pub fn build_tcp_packet(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> Vec<u8> {
    let tcp_hdr_len: u16 = 20;
    let tcp_total_len = tcp_hdr_len + payload.len() as u16;
    let ip_total_len = 20 + tcp_total_len;
    let frame_len = 14 + ip_total_len as usize;
    let mut frame = vec![0u8; frame_len];

    // Ethernet Header
    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4

    // IPv4 Header
    frame[14] = 0x45;
    frame[15] = 0x00;
    frame[16..18].copy_from_slice(&ip_total_len.to_be_bytes());
    frame[18..20].copy_from_slice(&0x4321u16.to_be_bytes());
    frame[20..22].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
    frame[22] = 64; // TTL
    frame[23] = 6;  // TCP
    frame[26..30].copy_from_slice(&src_ip);
    frame[30..34].copy_from_slice(&dst_ip);
    let ip_csum = ip_checksum(&frame[14..34]);
    frame[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    // TCP Header
    let tcp_off = 34;
    frame[tcp_off..tcp_off + 2].copy_from_slice(&src_port.to_be_bytes());
    frame[tcp_off + 2..tcp_off + 4].copy_from_slice(&dst_port.to_be_bytes());
    frame[tcp_off + 4..tcp_off + 8].copy_from_slice(&seq.to_be_bytes());
    frame[tcp_off + 8..tcp_off + 12].copy_from_slice(&ack.to_be_bytes());
    frame[tcp_off + 12] = 5 << 4; // 20 bytes header
    frame[tcp_off + 13] = flags;
    frame[tcp_off + 14..tcp_off + 16].copy_from_slice(&window.to_be_bytes());

    // TCP Payload
    if !payload.is_empty() {
        frame[tcp_off + 20..tcp_off + 20 + payload.len()].copy_from_slice(payload);
    }

    // Pseudo-header Checksum
    let mut pseudo = Vec::with_capacity(12 + tcp_total_len as usize);
    pseudo.extend_from_slice(&src_ip);
    pseudo.extend_from_slice(&dst_ip);
    pseudo.push(0);
    pseudo.push(6);
    pseudo.extend_from_slice(&tcp_total_len.to_be_bytes());
    pseudo.extend_from_slice(&frame[tcp_off..tcp_off + tcp_total_len as usize]);
    let tcp_csum = ip_checksum(&pseudo);
    let final_csum = if tcp_csum == 0 { 0xFFFF } else { tcp_csum };
    frame[tcp_off + 16..tcp_off + 18].copy_from_slice(&final_csum.to_be_bytes());

    if frame.len() < 60 {
        frame.resize(60, 0);
    }

    frame
}

/// Construye una trama de respuesta ARP (Opcode 2).
pub fn build_arp_reply(
    sender_mac: [u8; 6],
    sender_ip: [u8; 4],
    target_mac: [u8; 6],
    target_ip: [u8; 4],
) -> Vec<u8> {
    let mut frame = vec![0u8; 42];
    // Ethernet Header
    frame[0..6].copy_from_slice(&target_mac);
    frame[6..12].copy_from_slice(&sender_mac);
    frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes()); // ARP

    // ARP Payload
    frame[14..16].copy_from_slice(&1u16.to_be_bytes()); // Hardware type Ethernet
    frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes()); // Protocol IPv4
    frame[18] = 6; // Hardware addr len
    frame[19] = 4; // Protocol addr len
    frame[20..22].copy_from_slice(&2u16.to_be_bytes()); // Opcode 2 (Reply)
    frame[22..28].copy_from_slice(&sender_mac);
    frame[28..32].copy_from_slice(&sender_ip);
    frame[32..38].copy_from_slice(&target_mac);
    frame[38..42].copy_from_slice(&target_ip);

    frame
}

/// Construye una respuesta ICMP Echo Reply a partir de una solicitud ICMP Echo Request entrante.
pub fn build_icmp_echo_reply(request_frame: &[u8]) -> Option<Vec<u8>> {
    if request_frame.len() < 34 {
        return None;
    }
    let ihl = ((request_frame[14] & 0x0F) * 4) as usize;
    let icmp_offset = 14 + ihl;
    if request_frame.len() < icmp_offset + 8 {
        return None;
    }
    // Comprobar si es Echo Request (tipo 8, código 0)
    if request_frame[icmp_offset] != 8 {
        return None;
    }

    let mut reply = request_frame.to_vec();
    // Intercambiar MAC origen y destino
    let dst_mac: [u8; 6] = request_frame[0..6].try_into().unwrap();
    let src_mac: [u8; 6] = request_frame[6..12].try_into().unwrap();
    reply[0..6].copy_from_slice(&src_mac);
    reply[6..12].copy_from_slice(&dst_mac);

    // Intercambiar IP origen y destino
    let src_ip: [u8; 4] = request_frame[26..30].try_into().unwrap();
    let dst_ip: [u8; 4] = request_frame[30..34].try_into().unwrap();
    reply[26..30].copy_from_slice(&dst_ip);
    reply[30..34].copy_from_slice(&src_ip);

    // Recalcular Checksum de cabecera IPv4
    reply[24..26].copy_from_slice(&[0, 0]);
    let ip_csum = ip_checksum(&reply[14..14 + ihl]);
    reply[24..26].copy_from_slice(&ip_csum.to_be_bytes());

    // Cambiar tipo ICMP a 0 (Echo Reply)
    reply[icmp_offset] = 0;
    // Recalcular Checksum ICMP
    reply[icmp_offset + 2..icmp_offset + 4].copy_from_slice(&[0, 0]);
    let icmp_csum = ip_checksum(&reply[icmp_offset..]);
    reply[icmp_offset + 2..icmp_offset + 4].copy_from_slice(&icmp_csum.to_be_bytes());

    Some(reply)
}

// ─── Soporte de Interfaz TAP (Linux) ───────────────────────────────
#[cfg(target_os = "linux")]
fn open_tap_device(ifname: &str) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .ok()?;

    #[repr(C)]
    struct IfReq {
        ifr_name: [u8; 16],
        ifr_flags: i16,
        _pad: [u8; 22],
    }
    const IFF_TAP: i16 = 0x0002;
    const IFF_NO_PI: i16 = 0x1000;
    const TUNSETIFF: libc::c_ulong = 0x400454ca;

    let mut ifr = IfReq {
        ifr_name: [0u8; 16],
        ifr_flags: IFF_TAP | IFF_NO_PI,
        _pad: [0u8; 22],
    };
    let bytes = ifname.as_bytes();
    let len = bytes.len().min(15);
    ifr.ifr_name[..len].copy_from_slice(&bytes[..len]);

    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    let ret = unsafe { libc::ioctl(fd, TUNSETIFF, &ifr) };
    if ret < 0 {
        return None;
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK); };
    }
    Some(file)
}

#[cfg(target_os = "linux")]
fn open_internal_udp_switch(port: u16) -> Option<std::net::UdpSocket> {
    if port == 0 {
        return None;
    }
    let sock = std::net::UdpSocket::bind(("127.0.0.1", port))
        .or_else(|_| std::net::UdpSocket::bind(("0.0.0.0", port)))
        .ok()?;
    let _ = sock.set_nonblocking(true);
    Some(sock)
}

// ─── Worker de Red Asíncrono en Segundo Plano ──────────────────────
#[allow(dead_code)]
pub fn spawn_net_worker(
    tx_receiver: mpsc::Receiver<Vec<u8>>,
    rx_sender: mpsc::Sender<Vec<u8>>,
) {
    spawn_net_worker_with_config(tx_receiver, rx_sender, NetConfig::default_slirp());
}

pub fn spawn_net_worker_with_config(
    tx_receiver: mpsc::Receiver<Vec<u8>>,
    rx_sender: mpsc::Sender<Vec<u8>>,
    cfg: NetConfig,
) {
    let _ = std::thread::Builder::new()
        .name(format!("net-worker-{}", cfg.tap_env_var))
        .spawn(move || {
            #[cfg(target_os = "linux")]
            let tap_name_opt = cfg.tap_name.clone().or_else(|| {
                std::env::var(cfg.tap_env_var)
                    .ok()
                    .or_else(|| {
                        if cfg.tap_env_var == "MI_VMM_TAP" {
                            std::env::var("TWO_FIVE_FIVE_TAP")
                                .ok()
                                .or_else(|| std::env::var("TFF_TAP").ok())
                        } else {
                            None
                        }
                    })
            });
            #[cfg(target_os = "linux")]
            let mut tap_file = if let Some(ref tap_name) = tap_name_opt {
                eprintln!("[NET] Intentando conectar a interfaz TAP '{}'...", tap_name);
                open_tap_device(tap_name)
            } else {
                None
            };
            #[cfg(not(target_os = "linux"))]
            let mut tap_file: Option<std::fs::File> = None;

            #[cfg(target_os = "linux")]
            let internal_sock = if cfg.mode == NetMode::Internal && tap_file.is_none() {
                open_internal_udp_switch(cfg.internal_udp_port)
            } else {
                None
            };
            #[cfg(not(target_os = "linux"))]
            let internal_sock: Option<std::net::UdpSocket> = None;

            let mut inbound_listeners: Vec<(u16, std::net::TcpListener)> = Vec::new();
            if matches!(cfg.mode, NetMode::Nat | NetMode::HostOnly) && tap_file.is_none() {
                for &(host_port, guest_port) in &cfg.port_forwards {
                    if host_port == 0 || guest_port == 0 {
                        continue;
                    }
                    match std::net::TcpListener::bind(("127.0.0.1", host_port)) {
                        Ok(l) => {
                            let _ = l.set_nonblocking(true);
                            eprintln!(
                                "[NET] Port-forward activo: 127.0.0.1:{} -> {}.{}.{}.{}:{}",
                                host_port,
                                cfg.guest_ip[0],
                                cfg.guest_ip[1],
                                cfg.guest_ip[2],
                                cfg.guest_ip[3],
                                guest_port
                            );
                            inbound_listeners.push((guest_port, l));
                        }
                        Err(e) => {
                            eprintln!(
                                "[NET] No se pudo enlazar port-forward {}:{} -> {}",
                                host_port, guest_port, e
                            );
                        }
                    }
                }
            }

            let dns_upstream = detect_host_dns_server();
            if tap_file.is_some() {
                eprintln!("[NET] Interfaz TAP inicializada y conectada.");
            } else {
                eprintln!(
                    "[NET] Stack {:?} activo (Guest {}.{}.{}.{}, GW {}.{}.{}.{}, DNS {}).",
                    cfg.mode,
                    cfg.guest_ip[0], cfg.guest_ip[1], cfg.guest_ip[2], cfg.guest_ip[3],
                    cfg.gateway_ip[0], cfg.gateway_ip[1], cfg.gateway_ip[2], cfg.gateway_ip[3],
                    dns_upstream
                );
            }

            let dns_socket = if cfg.mode == NetMode::Nat {
                std::net::UdpSocket::bind("0.0.0.0:0").ok().inspect(|s| {
                    let _ = s.set_nonblocking(true);
                })
            } else {
                None
            };

            let mut pending_dns: HashMap<u16, ([u8; 6], [u8; 4], u16, [u8; 4], std::time::Instant)> =
                HashMap::new();
            let mut tap_buf = [0u8; 2048];
            let mut dns_buf = [0u8; 2048];
            let mut tcp_buf = [0u8; 1460]; // MTU-safe MSS para no exceder frames Ethernet de 1500B
            let mut observed_guest_mac = cfg.guest_mac;
            let mut next_ephemeral: u16 = 41000;

            struct TcpNatSession {
                stream: Option<std::net::TcpStream>,
                connecting_rx: Option<mpsc::Receiver<Option<std::net::TcpStream>>>,
                guest_mac: [u8; 6],
                guest_ip: [u8; 4],
                guest_port: u16,
                remote_ip: [u8; 4],
                remote_port: u16,
                guest_seq: u32,
                host_seq: u32,
                tx_pending: Vec<u8>,
                fin_pending: bool,
                closed: bool,
            }

            struct TcpInboundSession {
                stream: std::net::TcpStream,
                gateway_port: u16,
                guest_port: u16,
                guest_mac: [u8; 6],
                guest_ip: [u8; 4],
                host_seq: u32,
                guest_seq: u32,
                established: bool,
                tx_to_host: Vec<u8>,
                fin_from_guest: bool,
                closed: bool,
            }

            let mut tcp_conns: HashMap<(u16, [u8; 4], u16), TcpNatSession> = HashMap::new();
            let mut inbound_conns: HashMap<(u16, u16), TcpInboundSession> = HashMap::new();

            loop {
                let mut idle = true;
                let mut disconnected = false;

                // 1. Tramas salientes enviadas por el guest
                loop {
                    match tx_receiver.try_recv() {
                        Ok(frame) => {
                            idle = false;
                            if frame.len() >= 14 && (frame[6] & 0x01) == 0 {
                                if let Ok(mac) = frame[6..12].try_into() {
                                    observed_guest_mac = mac;
                                }
                            }
                            #[cfg(target_os = "linux")]
                            if let Some(ref mut tap) = tap_file {
                                use std::io::Write;
                                let _ = tap.write_all(&frame);
                                continue;
                            }
                            let _ = &internal_sock;

                            if cfg.mode == NetMode::Internal {
                                continue;
                            }

                            // En modo sin TAP: validar longitud total IPv4 para descartar padding Ethernet (60B)
                            if frame.len() >= 34 && u16::from_be_bytes([frame[12], frame[13]]) == 0x0800 {
                                let ihl = ((frame[14] & 0x0F) * 4) as usize;
                                let ip_total_len = u16::from_be_bytes([frame[16], frame[17]]) as usize;
                                if ihl < 20 || ip_total_len < ihl || 14 + ip_total_len > frame.len() {
                                    continue;
                                }
                                let ip_pkt = &frame[14..14 + ip_total_len];
                                let protocol = ip_pkt[9];
                                let src_mac: [u8; 6] = frame[6..12].try_into().unwrap_or(observed_guest_mac);
                                let src_ip: [u8; 4] = ip_pkt[12..16].try_into().unwrap_or(cfg.guest_ip);
                                let dst_ip: [u8; 4] = ip_pkt[16..20].try_into().unwrap_or(cfg.gateway_ip);

                                // 1a. Tráfico UDP (DNS upstream solo en modo NAT)
                                if protocol == 17 && ip_pkt.len() >= ihl + 8 && cfg.mode == NetMode::Nat {
                                    let udp = &ip_pkt[ihl..];
                                    let src_port = u16::from_be_bytes([udp[0], udp[1]]);
                                    let dst_port = u16::from_be_bytes([udp[2], udp[3]]);
                                    if dst_port == 53 && udp.len() >= 10 {
                                        let dns_payload = &udp[8..];
                                        let tx_id = u16::from_be_bytes([dns_payload[0], dns_payload[1]]);
                                        let now = std::time::Instant::now();
                                        pending_dns.retain(|_, v| now.duration_since(v.4).as_secs() < 10);
                                        if pending_dns.len() < 256 {
                                            pending_dns.insert(
                                                tx_id,
                                                (src_mac, src_ip, src_port, dst_ip, now),
                                            );
                                        }
                                        if let Some(ref s) = dns_socket {
                                            let _ = s.send_to(dns_payload, &dns_upstream);
                                        }
                                    }
                                }

                                // 1b. Tráfico TCP (Port-forwarding entrante y NAT saliente)
                                if protocol == 6 && ip_pkt.len() >= ihl + 20 {
                                    let tcp = &ip_pkt[ihl..];
                                    let src_port = u16::from_be_bytes([tcp[0], tcp[1]]);
                                    let dst_port = u16::from_be_bytes([tcp[2], tcp[3]]);
                                    let seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
                                    let data_offset = ((tcp[12] >> 4) * 4) as usize;
                                    if data_offset < 20 || data_offset > tcp.len() {
                                        continue;
                                    }
                                    let flags = tcp[13];
                                    let payload = &tcp[data_offset..];

                                    // ¿Pertenece a una sesión de port-forwarding entrante (127.0.0.1 -> guest)?
                                    if dst_ip == cfg.gateway_ip {
                                        if let Some(in_sess) = inbound_conns.get_mut(&(dst_port, src_port)) {
                                            in_sess.guest_mac = src_mac;
                                            in_sess.guest_ip = src_ip;
                                            if (flags & 0x04) != 0 {
                                                in_sess.closed = true;
                                                continue;
                                            }
                                            if !in_sess.established && (flags & 0x12) == 0x12 {
                                                in_sess.guest_seq = seq.wrapping_add(1);
                                                in_sess.established = true;
                                                let ack_pkt = build_tcp_packet(
                                                    cfg.gateway_mac,
                                                    in_sess.guest_mac,
                                                    cfg.gateway_ip,
                                                    in_sess.guest_ip,
                                                    in_sess.gateway_port,
                                                    in_sess.guest_port,
                                                    in_sess.host_seq,
                                                    in_sess.guest_seq,
                                                    0x10,
                                                    65535,
                                                    &[],
                                                );
                                                let _ = rx_sender.send(ack_pkt);
                                                continue;
                                            }
                                            if in_sess.established {
                                                if seq != in_sess.guest_seq {
                                                    if !payload.is_empty() || (flags & 0x01) != 0 {
                                                        let dup = build_tcp_packet(
                                                            cfg.gateway_mac,
                                                            in_sess.guest_mac,
                                                            cfg.gateway_ip,
                                                            in_sess.guest_ip,
                                                            in_sess.gateway_port,
                                                            in_sess.guest_port,
                                                            in_sess.host_seq,
                                                            in_sess.guest_seq,
                                                            0x10,
                                                            65535,
                                                            &[],
                                                        );
                                                        let _ = rx_sender.send(dup);
                                                    }
                                                    continue;
                                                }
                                                if !payload.is_empty() {
                                                    in_sess.tx_to_host.extend_from_slice(payload);
                                                    in_sess.guest_seq =
                                                        in_sess.guest_seq.wrapping_add(payload.len() as u32);
                                                    let ack_pkt = build_tcp_packet(
                                                        cfg.gateway_mac,
                                                        in_sess.guest_mac,
                                                        cfg.gateway_ip,
                                                        in_sess.guest_ip,
                                                        in_sess.gateway_port,
                                                        in_sess.guest_port,
                                                        in_sess.host_seq,
                                                        in_sess.guest_seq,
                                                        0x10,
                                                        65535,
                                                        &[],
                                                    );
                                                    let _ = rx_sender.send(ack_pkt);
                                                }
                                                if (flags & 0x01) != 0 {
                                                    in_sess.guest_seq = in_sess.guest_seq.wrapping_add(1);
                                                    in_sess.fin_from_guest = true;
                                                }
                                            }
                                            continue;
                                        }
                                    }

                                    // En Host-Only no enrutar conexiones TCP salientes hacia Internet
                                    if cfg.mode != NetMode::Nat {
                                        if (flags & 0x02) != 0 && (flags & 0x10) == 0 {
                                            let rst = build_tcp_packet(
                                                cfg.gateway_mac,
                                                src_mac,
                                                dst_ip,
                                                src_ip,
                                                dst_port,
                                                src_port,
                                                0,
                                                seq.wrapping_add(1),
                                                0x14,
                                                0,
                                                &[],
                                            );
                                            let _ = rx_sender.send(rst);
                                        }
                                        continue;
                                    }

                                    let key = (src_port, dst_ip, dst_port);

                                    if (flags & 0x02) != 0 && (flags & 0x10) == 0 {
                                        let target_addr = std::net::SocketAddr::new(
                                            std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                                                dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3],
                                            )),
                                            dst_port,
                                        );
                                        let (conn_tx, conn_rx) = mpsc::channel();
                                        std::thread::spawn(move || {
                                            let res = std::net::TcpStream::connect_timeout(
                                                &target_addr,
                                                std::time::Duration::from_millis(2500),
                                            )
                                            .ok()
                                            .inspect(|s| {
                                                let _ = s.set_nonblocking(true);
                                            });
                                            let _ = conn_tx.send(res);
                                        });
                                        let host_isn = 100_000u32;
                                        let session = TcpNatSession {
                                            stream: None,
                                            connecting_rx: Some(conn_rx),
                                            guest_mac: src_mac,
                                            guest_ip: src_ip,
                                            guest_port: src_port,
                                            remote_ip: dst_ip,
                                            remote_port: dst_port,
                                            guest_seq: seq.wrapping_add(1),
                                            host_seq: host_isn,
                                            tx_pending: Vec::new(),
                                            fin_pending: false,
                                            closed: false,
                                        };
                                        tcp_conns.insert(key, session);
                                    } else if let Some(session) = tcp_conns.get_mut(&key) {
                                        if (flags & 0x04) != 0 {
                                            session.closed = true;
                                            continue;
                                        }
                                        // Verificar seq para ignorar retransmisiones duplicadas
                                        if seq != session.guest_seq {
                                            if !payload.is_empty() || (flags & 0x01) != 0 {
                                                let dup_ack = build_tcp_packet(
                                                    cfg.gateway_mac,
                                                    session.guest_mac,
                                                    session.remote_ip,
                                                    session.guest_ip,
                                                    session.remote_port,
                                                    session.guest_port,
                                                    session.host_seq,
                                                    session.guest_seq,
                                                    0x10,
                                                    65535,
                                                    &[],
                                                );
                                                let _ = rx_sender.send(dup_ack);
                                            }
                                            continue;
                                        }
                                        // Encolar payload ANTES de FIN si llegan juntos (PSH|FIN)
                                        if !payload.is_empty() {
                                            session.tx_pending.extend_from_slice(payload);
                                            session.guest_seq =
                                                session.guest_seq.wrapping_add(payload.len() as u32);
                                            let ack_pkt = build_tcp_packet(
                                                cfg.gateway_mac,
                                                session.guest_mac,
                                                session.remote_ip,
                                                session.guest_ip,
                                                session.remote_port,
                                                session.guest_port,
                                                session.host_seq,
                                                session.guest_seq,
                                                0x10,
                                                65535,
                                                &[],
                                            );
                                            let _ = rx_sender.send(ack_pkt);
                                        }
                                        if (flags & 0x01) != 0 {
                                            session.guest_seq = session.guest_seq.wrapping_add(1);
                                            session.fin_pending = true;
                                        }
                                    }
                                }
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }

                if disconnected {
                    break;
                }

                // 2. Tramas entrantes leídas de la interfaz TAP
                #[cfg(target_os = "linux")]
                if let Some(ref mut tap) = tap_file {
                    use std::io::Read;
                    match tap.read(&mut tap_buf) {
                        Ok(n) if n > 0 => {
                            idle = false;
                            let _ = rx_sender.send(tap_buf[..n].to_vec());
                        }
                        _ => {}
                    }
                }

                // 3. Respuestas DNS upstream recibidas
                if let Some(ref s) = dns_socket {
                    while let Ok((n, _)) = s.recv_from(&mut dns_buf) {
                        idle = false;
                        if n >= 2 {
                            let tx_id = u16::from_be_bytes([dns_buf[0], dns_buf[1]]);
                            if let Some((src_mac, src_ip, src_port, orig_dst_ip, _)) =
                                pending_dns.remove(&tx_id)
                            {
                                let reply_frame = build_udp_packet(
                                    cfg.gateway_mac,
                                    src_mac,
                                    orig_dst_ip,
                                    src_ip,
                                    53,
                                    src_port,
                                    &dns_buf[..n],
                                );
                                let _ = rx_sender.send(reply_frame);
                            }
                        }
                    }
                }

                // 4. Completar conexiones TCP pendientes, vaciar tx_pending y leer datos del servidor remoto
                tcp_conns.retain(|_key, session| {
                    if session.closed {
                        return false;
                    }
                    if session.stream.is_none() {
                        if let Some(ref rx) = session.connecting_rx {
                            match rx.try_recv() {
                                Ok(Some(stream)) => {
                                    idle = false;
                                    session.stream = Some(stream);
                                    session.connecting_rx = None;
                                    let syn_ack = build_tcp_packet(
                                        cfg.gateway_mac,
                                        session.guest_mac,
                                        session.remote_ip,
                                        session.guest_ip,
                                        session.remote_port,
                                        session.guest_port,
                                        session.host_seq,
                                        session.guest_seq,
                                        0x12, // SYN | ACK
                                        65535,
                                        &[],
                                    );
                                    session.host_seq = session.host_seq.wrapping_add(1);
                                    let _ = rx_sender.send(syn_ack);
                                }
                                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => {
                                    let rst = build_tcp_packet(
                                        cfg.gateway_mac,
                                        session.guest_mac,
                                        session.remote_ip,
                                        session.guest_ip,
                                        session.remote_port,
                                        session.guest_port,
                                        0,
                                        session.guest_seq,
                                        0x14, // RST | ACK
                                        0,
                                        &[],
                                    );
                                    let _ = rx_sender.send(rst);
                                    return false;
                                }
                                Err(mpsc::TryRecvError::Empty) => return true,
                            }
                        } else {
                            return false;
                        }
                    }

                    let Some(ref mut stream) = session.stream else {
                        return false;
                    };

                    if !session.tx_pending.is_empty() {
                        use std::io::Write;
                        match stream.write(&session.tx_pending) {
                            Ok(0) => {
                                session.closed = true;
                                return false;
                            }
                            Ok(written) => {
                                idle = false;
                                session.tx_pending.drain(..written);
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => {
                                session.closed = true;
                                return false;
                            }
                        }
                    }

                    if session.fin_pending && session.tx_pending.is_empty() {
                        let fin_ack = build_tcp_packet(
                            cfg.gateway_mac,
                            session.guest_mac,
                            session.remote_ip,
                            session.guest_ip,
                            session.remote_port,
                            session.guest_port,
                            session.host_seq,
                            session.guest_seq,
                            0x11, // FIN | ACK
                            65535,
                            &[],
                        );
                        let _ = rx_sender.send(fin_ack);
                        return false;
                    }

                    use std::io::Read;
                    match stream.read(&mut tcp_buf) {
                        Ok(0) => {
                            idle = false;
                            let fin = build_tcp_packet(
                                cfg.gateway_mac,
                                session.guest_mac,
                                session.remote_ip,
                                session.guest_ip,
                                session.remote_port,
                                session.guest_port,
                                session.host_seq,
                                session.guest_seq,
                                0x11, // FIN | ACK
                                65535,
                                &[],
                            );
                            session.host_seq = session.host_seq.wrapping_add(1);
                            let _ = rx_sender.send(fin);
                            false
                        }
                        Ok(n) => {
                            idle = false;
                            let pkt = build_tcp_packet(
                                cfg.gateway_mac,
                                session.guest_mac,
                                session.remote_ip,
                                session.guest_ip,
                                session.remote_port,
                                session.guest_port,
                                session.host_seq,
                                session.guest_seq,
                                0x18, // PSH | ACK
                                65535,
                                &tcp_buf[..n],
                            );
                            session.host_seq = session.host_seq.wrapping_add(n as u32);
                            let _ = rx_sender.send(pkt);
                            true
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
                        Err(_) => false,
                    }
                });

                // 4b. Aceptar nuevas conexiones entrantes de Port-Forwarding (127.0.0.1:<host_port> -> guest)
                for (guest_port, listener) in &inbound_listeners {
                    while let Ok((stream, _peer)) = listener.accept() {
                        idle = false;
                        let _ = stream.set_nonblocking(true);
                        let mut attempts = 0u16;
                        let gateway_port = loop {
                            let cand = next_ephemeral;
                            next_ephemeral = if next_ephemeral >= 59999 { 41000 } else { next_ephemeral + 1 };
                            if !inbound_conns.contains_key(&(cand, *guest_port)) || attempts > 200 {
                                break cand;
                            }
                            attempts += 1;
                        };
                        let host_isn = 200_000u32.wrapping_add(gateway_port as u32);
                        let syn = build_tcp_packet(
                            cfg.gateway_mac,
                            observed_guest_mac,
                            cfg.gateway_ip,
                            cfg.guest_ip,
                            gateway_port,
                            *guest_port,
                            host_isn,
                            0,
                            0x02, // SYN
                            65535,
                            &[],
                        );
                        let _ = rx_sender.send(syn);
                        inbound_conns.insert(
                            (gateway_port, *guest_port),
                            TcpInboundSession {
                                stream,
                                gateway_port,
                                guest_port: *guest_port,
                                guest_mac: observed_guest_mac,
                                guest_ip: cfg.guest_ip,
                                host_seq: host_isn.wrapping_add(1),
                                guest_seq: 0,
                                established: false,
                                tx_to_host: Vec::new(),
                                fin_from_guest: false,
                                closed: false,
                            },
                        );
                    }
                }

                // 4c. Avanzar sesiones TCP entrantes (SSH / servicios en guest)
                inbound_conns.retain(|_, session| {
                    if session.closed {
                        return false;
                    }
                    if !session.established {
                        return true;
                    }
                    if !session.tx_to_host.is_empty() {
                        use std::io::Write;
                        match session.stream.write(&session.tx_to_host) {
                            Ok(0) => {
                                session.closed = true;
                                return false;
                            }
                            Ok(written) => {
                                idle = false;
                                session.tx_to_host.drain(..written);
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => {
                                session.closed = true;
                                return false;
                            }
                        }
                    }
                    if session.fin_from_guest && session.tx_to_host.is_empty() {
                        let fin_ack = build_tcp_packet(
                            cfg.gateway_mac,
                            session.guest_mac,
                            cfg.gateway_ip,
                            session.guest_ip,
                            session.gateway_port,
                            session.guest_port,
                            session.host_seq,
                            session.guest_seq,
                            0x11, // FIN | ACK
                            65535,
                            &[],
                        );
                        let _ = rx_sender.send(fin_ack);
                        return false;
                    }
                    use std::io::Read;
                    match session.stream.read(&mut tcp_buf) {
                        Ok(0) => {
                            idle = false;
                            let fin = build_tcp_packet(
                                cfg.gateway_mac,
                                session.guest_mac,
                                cfg.gateway_ip,
                                session.guest_ip,
                                session.gateway_port,
                                session.guest_port,
                                session.host_seq,
                                session.guest_seq,
                                0x11, // FIN | ACK
                                65535,
                                &[],
                            );
                            session.host_seq = session.host_seq.wrapping_add(1);
                            let _ = rx_sender.send(fin);
                            false
                        }
                        Ok(n) => {
                            idle = false;
                            let pkt = build_tcp_packet(
                                cfg.gateway_mac,
                                session.guest_mac,
                                cfg.gateway_ip,
                                session.guest_ip,
                                session.gateway_port,
                                session.guest_port,
                                session.host_seq,
                                session.guest_seq,
                                0x18, // PSH | ACK
                                65535,
                                &tcp_buf[..n],
                            );
                            session.host_seq = session.host_seq.wrapping_add(n as u32);
                            let _ = rx_sender.send(pkt);
                            true
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
                        Err(_) => false,
                    }
                });

                if idle {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });
}

// ─── Estado del Controlador VirtIO-Net ─────────────────────────────
pub struct VirtioNetState {
    pub iobase: u16,
    pub host_features: u32,
    pub guest_features: u32,
    pub queue_sel: u16,
    pub queues: [VirtQueue; 2],
    pub device_status: u8,
    pub isr_status: u8,
    pub irq_pulse: bool,
    pub mac: [u8; 6],
    pub net_cfg: NetConfig,
    pub rx_queue: VecDeque<Vec<u8>>,
    pub tx_sender: Option<mpsc::Sender<Vec<u8>>>,
    pub rx_receiver: Option<mpsc::Receiver<Vec<u8>>>,
}

impl Default for VirtioNetState {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioNetState {
    pub fn new() -> Self {
        Self::with_config(NetConfig::default_slirp())
    }

    pub fn with_config(net_cfg: NetConfig) -> Self {
        let (tx_sender, tx_receiver) = mpsc::channel();
        let (rx_sender, rx_receiver) = mpsc::channel();
        let mac = net_cfg.guest_mac;

        spawn_net_worker_with_config(tx_receiver, rx_sender, net_cfg.clone());

        Self {
            iobase: 0,
            host_features: VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
            guest_features: 0,
            queue_sel: 0,
            queues: [VirtQueue::default(); 2],
            device_status: 0,
            isr_status: 0,
            irq_pulse: false,
            mac,
            net_cfg,
            rx_queue: VecDeque::new(),
            tx_sender: Some(tx_sender),
            rx_receiver: Some(rx_receiver),
        }
    }

    pub fn reset(&mut self) {
        let base = self.iobase;
        let mac = self.mac;
        let tx_sender = self.tx_sender.clone();
        self.host_features = VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS;
        self.guest_features = 0;
        self.queue_sel = 0;
        self.queues = [VirtQueue::default(); 2];
        self.device_status = 0;
        self.isr_status = 0;
        self.irq_pulse = false;
        self.rx_queue.clear();
        self.iobase = base;
        self.mac = mac;
        self.tx_sender = tx_sender;
    }

    pub fn take_irq_pulse(&mut self) -> bool {
        let p = self.irq_pulse;
        self.irq_pulse = false;
        p
    }

    /// Procesa un ciclo de las virtqueues con la memoria física del guest.
    pub fn step(&mut self, mem: &GuestMemory) {
        if (self.device_status & VIRTIO_CONFIG_S_DRIVER_OK) == 0 {
            return;
        }

        let mut trigger_irq = false;

        // 1. Recibir tramas del worker en segundo plano (DNS o TAP)
        if let Some(ref rx_rx) = self.rx_receiver {
            while let Ok(frame) = rx_rx.try_recv() {
                self.rx_queue.push_back(frame);
            }
        }

        // 2. Procesar Queue 1 (TX: Guest -> Host) con tope de 64 paquetes por tick
        for _ in 0..64 {
            let Some(data) = self.queues[1].read_buffer(mem) else { break; };
            trigger_irq = true;
            if data.len() > 10 {
                let eth_frame = &data[10..];
                self.handle_tx_frame(eth_frame);
            }
        }

        // 3. Procesar Queue 0 (RX: Host -> Guest) con tope de 64 paquetes por tick
        for _ in 0..64 {
            let Some(frame) = self.rx_queue.front() else { break; };
            let mut pkt = Vec::with_capacity(10 + frame.len());
            pkt.extend_from_slice(&[0u8; 10]); // virtio_net_hdr (flags, gso, checksum)
            pkt.extend_from_slice(frame);

            if self.queues[0].write_buffer(mem, &pkt) {
                self.rx_queue.pop_front();
                trigger_irq = true;
            } else {
                break; // No hay descriptores RX disponibles puestos por el guest
            }
        }

        if trigger_irq {
            self.isr_status |= 0x01;
            self.irq_pulse = true;
        }
    }

    fn handle_tx_frame(&mut self, frame: &[u8]) {
        let tap_configured = self.net_cfg.tap_name.is_some()
            || self.net_cfg.mode == NetMode::Tap
            || std::env::var_os("TWO_FIVE_FIVE_TAP")
                .or_else(|| std::env::var_os("TFF_TAP"))
                .or_else(|| std::env::var_os("MI_VMM_TAP"))
                .is_some();
        if !tap_configured && self.net_cfg.mode != NetMode::Internal {
            if let Some(reply) = process_local_frame_with_cfg(frame, &self.net_cfg) {
                self.rx_queue.push_back(reply);
            }
        }

        // Reenviar al worker para TAP, port-forward o DNS/TCP upstream
        if let Some(ref tx_tx) = self.tx_sender {
            let _ = tx_tx.send(frame.to_vec());
        }
    }
}

#[allow(dead_code)]
pub fn process_local_frame(frame: &[u8]) -> Option<Vec<u8>> {
    process_local_frame_with_cfg(frame, &NetConfig::default_slirp())
}

pub fn process_local_frame_with_cfg(frame: &[u8], cfg: &NetConfig) -> Option<Vec<u8>> {
    if frame.len() < 14 {
        return None;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        0x0806 => handle_arp_with_cfg(frame, cfg),
        0x0800 => handle_ipv4_with_cfg(frame, cfg),
        _ => None,
    }
}

#[allow(dead_code)]
pub fn handle_arp(frame: &[u8]) -> Option<Vec<u8>> {
    handle_arp_with_cfg(frame, &NetConfig::default_slirp())
}

fn handle_arp_with_cfg(frame: &[u8], cfg: &NetConfig) -> Option<Vec<u8>> {
    if frame.len() < 42 {
        return None;
    }
    let opcode = u16::from_be_bytes([frame[20], frame[21]]);
    if opcode != 1 {
        return None;
    }
    let sender_mac: [u8; 6] = frame[22..28].try_into().ok()?;
    let sender_ip: [u8; 4] = frame[28..32].try_into().ok()?;
    let target_ip: [u8; 4] = frame[38..42].try_into().ok()?;

    if target_ip == cfg.gateway_ip || target_ip == cfg.dns_ip {
        crate::tui::log(&format!(
            "[NET] Solicitud ARP de {}.{}.{}.{} para {}.{}.{}.{} -> Respondiendo",
            sender_ip[0], sender_ip[1], sender_ip[2], sender_ip[3],
            target_ip[0], target_ip[1], target_ip[2], target_ip[3]
        ));
        Some(build_arp_reply(cfg.gateway_mac, target_ip, sender_mac, sender_ip))
    } else {
        None
    }
}

#[allow(dead_code)]
pub fn handle_ipv4(frame: &[u8]) -> Option<Vec<u8>> {
    handle_ipv4_with_cfg(frame, &NetConfig::default_slirp())
}

fn handle_ipv4_with_cfg(frame: &[u8], cfg: &NetConfig) -> Option<Vec<u8>> {
    if frame.len() < 34 {
        return None;
    }
    let ihl = ((frame[14] & 0x0F) * 4) as usize;
    let ip_total_len = u16::from_be_bytes([frame[16], frame[17]]) as usize;
    if ihl < 20 || ip_total_len < ihl || 14 + ip_total_len > frame.len() {
        return None;
    }
    let protocol = frame[23];
    let dst_ip: [u8; 4] = frame[30..34].try_into().ok()?;

    match protocol {
        1 => {
            // ICMP
            if dst_ip == cfg.gateway_ip || dst_ip == cfg.dns_ip {
                crate::tui::log("[NET] Solicitud ICMP Echo Request -> Respondiendo Echo Reply");
                build_icmp_echo_reply(&frame[..14 + ip_total_len])
            } else {
                None
            }
        }
        17 => {
            // UDP
            let udp_offset = 14 + ihl;
            if 14 + ip_total_len < udp_offset + 8 {
                return None;
            }
            let dst_port = u16::from_be_bytes([frame[udp_offset + 2], frame[udp_offset + 3]]);
            if dst_port == 67 {
                handle_dhcp_with_cfg(&frame[udp_offset + 8..14 + ip_total_len], frame, cfg)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[allow(dead_code)]
pub fn handle_dhcp(dhcp_payload: &[u8], full_frame: &[u8]) -> Option<Vec<u8>> {
    handle_dhcp_with_cfg(dhcp_payload, full_frame, &NetConfig::default_slirp())
}

fn handle_dhcp_with_cfg(dhcp_payload: &[u8], full_frame: &[u8], cfg: &NetConfig) -> Option<Vec<u8>> {
    if dhcp_payload.len() < 240 || dhcp_payload[0] != 1 {
        return None;
    }
    if dhcp_payload[236..240] != [0x63, 0x82, 0x53, 0x63] {
        return None;
    }

    let xid: [u8; 4] = dhcp_payload[4..8].try_into().ok()?;
    let client_mac: [u8; 6] = dhcp_payload[28..34].try_into().ok()?;

    let mut msg_type: Option<u8> = None;
    let mut idx = 240;
    while idx < dhcp_payload.len() {
        let tag = dhcp_payload[idx];
        if tag == 255 {
            break;
        }
        if tag == 0 {
            idx += 1;
            continue;
        }
        if idx + 1 >= dhcp_payload.len() {
            break;
        }
        let len = dhcp_payload[idx + 1] as usize;
        idx += 2;
        if idx + len > dhcp_payload.len() {
            break;
        }
        if tag == 53 && len >= 1 {
            msg_type = Some(dhcp_payload[idx]);
        }
        idx += len;
    }

    let response_type = match msg_type {
        Some(1) => {
            crate::tui::log("[NET] DHCP Discover recibido -> Enviando DHCPOFFER (10.0.2.15)");
            2 // DHCPOFFER
        }
        Some(3) => {
            crate::tui::log("[NET] DHCP Request recibido -> Enviando DHCPACK (10.0.2.15)");
            5 // DHCPACK
        }
        _ => return None,
    };

    let mut offer = vec![0u8; 240];
    offer[0] = 2; // BOOTREPLY
    offer[1] = 1; // Ethernet
    offer[2] = 6; // Hw len
    offer[3] = 0;
    offer[4..8].copy_from_slice(&xid);
    offer[16..20].copy_from_slice(&cfg.guest_ip);
    offer[20..24].copy_from_slice(&cfg.gateway_ip);
    offer[28..34].copy_from_slice(&client_mac);
    offer[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);

    let mut opt = Vec::new();
    opt.extend_from_slice(&[53, 1, response_type]);
    opt.extend_from_slice(&[54, 4]);
    opt.extend_from_slice(&cfg.gateway_ip);
    opt.extend_from_slice(&[51, 4]);
    opt.extend_from_slice(&86400u32.to_be_bytes()); // Lease time: 1 día
    opt.extend_from_slice(&[1, 4]);
    opt.extend_from_slice(&cfg.subnet_mask);
    opt.extend_from_slice(&[3, 4]);
    opt.extend_from_slice(&cfg.gateway_ip);
    opt.extend_from_slice(&[6, 4]);
    opt.extend_from_slice(&cfg.dns_ip);
    opt.push(255); // End

    offer.extend_from_slice(&opt);
    while offer.len() < 300 {
        offer.push(0);
    }

    let dst_mac = if full_frame[0..6] == BROADCAST_MAC {
        BROADCAST_MAC
    } else {
        client_mac
    };

    Some(build_udp_packet(
        cfg.gateway_mac,
        dst_mac,
        cfg.gateway_ip,
        [255, 255, 255, 255],
        67,
        68,
        &offer,
    ))
}

/// Dispositivo VirtIO-Net registrado en el bus I/O del VMM.
pub struct VirtioNetDevice {
    pub state: Arc<Mutex<VirtioNetState>>,
}

impl VirtioNetDevice {
    pub fn new(state: Arc<Mutex<VirtioNetState>>) -> Self {
        Self { state }
    }

    #[allow(dead_code)]
    pub fn set_iobase(&self, base: u16) {
        self.state.lock().unwrap().iobase = base;
    }
}

impl IoDevice for VirtioNetDevice {
    fn matches_port(&self, port: u16) -> bool {
        let base = self.state.lock().unwrap().iobase;
        base != 0 && port >= base && port < base + 32
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let mut st = self.state.lock().unwrap();
        let offset = port.saturating_sub(st.iobase);
        let mut res = vec![0u8; count];

        match offset {
            VIRTIO_PCI_HOST_FEATURES..=0x03 => {
                let bytes = st.host_features.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_HOST_FEATURES) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            VIRTIO_PCI_GUEST_FEATURES..=0x07 => {
                let bytes = st.guest_features.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_GUEST_FEATURES) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            VIRTIO_PCI_QUEUE_PFN..=0x0B => {
                let q_idx = st.queue_sel as usize;
                let pfn = if q_idx < st.queues.len() {
                    st.queues[q_idx].pfn
                } else {
                    0
                };
                let bytes = pfn.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_QUEUE_PFN) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            VIRTIO_PCI_QUEUE_NUM..=0x0D => {
                let q_idx = st.queue_sel as usize;
                let num = if q_idx < st.queues.len() {
                    st.queues[q_idx].size
                } else {
                    0
                };
                let bytes = num.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_QUEUE_NUM) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            VIRTIO_PCI_QUEUE_SEL..=0x0F => {
                let bytes = st.queue_sel.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_QUEUE_SEL) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            VIRTIO_PCI_STATUS => {
                res[0] = st.device_status;
            }
            VIRTIO_PCI_ISR => {
                res[0] = st.isr_status;
                st.isr_status = 0;
                st.irq_pulse = false;
            }
            // Device config space: MAC address (0x14..=0x19)
            0x14..=0x19 => {
                let shift = (offset - 0x14) as usize;
                for i in 0..count {
                    if shift + i < st.mac.len() {
                        res[i] = st.mac[shift + i];
                    }
                }
            }
            // Device config space: Status (0x1A..=0x1B) -> VIRTIO_NET_S_LINK_UP
            0x1A..=0x1B => {
                let bytes = VIRTIO_NET_S_LINK_UP.to_le_bytes();
                let shift = (offset - 0x1A) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            _ => {}
        }

        res
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let mut st = self.state.lock().unwrap();
        let offset = port.saturating_sub(st.iobase);

        match offset {
            VIRTIO_PCI_GUEST_FEATURES..=0x07 => {
                let mut bytes = st.guest_features.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_GUEST_FEATURES) as usize;
                for (i, &b) in data.iter().enumerate() {
                    if shift + i < bytes.len() {
                        bytes[shift + i] = b;
                    }
                }
                st.guest_features = u32::from_le_bytes(bytes);
            }
            VIRTIO_PCI_QUEUE_PFN..=0x0B => {
                let q_idx = st.queue_sel as usize;
                if q_idx < st.queues.len() {
                    let mut bytes = st.queues[q_idx].pfn.to_le_bytes();
                    let shift = (offset - VIRTIO_PCI_QUEUE_PFN) as usize;
                    for (i, &b) in data.iter().enumerate() {
                        if shift + i < bytes.len() {
                            bytes[shift + i] = b;
                        }
                    }
                    let new_pfn = u32::from_le_bytes(bytes);
                    if st.queues[q_idx].pfn != new_pfn {
                        st.queues[q_idx].pfn = new_pfn;
                        st.queues[q_idx].last_avail_idx = 0;
                        st.queues[q_idx].last_used_idx = 0;
                    }
                }
            }
            VIRTIO_PCI_QUEUE_SEL..=0x0F => {
                let mut bytes = st.queue_sel.to_le_bytes();
                let shift = (offset - VIRTIO_PCI_QUEUE_SEL) as usize;
                for (i, &b) in data.iter().enumerate() {
                    if shift + i < bytes.len() {
                        bytes[shift + i] = b;
                    }
                }
                st.queue_sel = u16::from_le_bytes(bytes);
            }
            VIRTIO_PCI_QUEUE_NOTIFY..=0x11 => {
                // Notificación recibida del guest
            }
            VIRTIO_PCI_STATUS => {
                let val = data[0];
                if val == 0 {
                    st.reset();
                } else {
                    st.device_status = val;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_checksum_computation() {
        // Cabecera IP típica de 20 bytes
        let header = [
            0x45, 0x00, 0x00, 0x3c, 0x1c, 0x46, 0x40, 0x00, 0x40, 0x06, 0x00, 0x00,
            0xac, 0x10, 0x0a, 0x63, 0xac, 0x10, 0x0a, 0x0c,
        ];
        let csum = ip_checksum(&header);
        assert_eq!(csum, 0xb1e6);
    }

    #[test]
    fn test_arp_reply_building() {
        let reply = build_arp_reply(GATEWAY_MAC, GATEWAY_IP, DEFAULT_GUEST_MAC, GUEST_IP);
        assert_eq!(reply.len(), 42);
        // Dest MAC
        assert_eq!(&reply[0..6], &DEFAULT_GUEST_MAC);
        // Src MAC
        assert_eq!(&reply[6..12], &GATEWAY_MAC);
        // EtherType ARP
        assert_eq!(u16::from_be_bytes([reply[12], reply[13]]), 0x0806);
        // Opcode 2 (Reply)
        assert_eq!(u16::from_be_bytes([reply[20], reply[21]]), 2);
        // Sender IP (Gateway)
        assert_eq!(&reply[28..32], &GATEWAY_IP);
        // Target IP (Guest)
        assert_eq!(&reply[38..42], &GUEST_IP);
    }

    #[test]
    fn test_udp_dhcp_offer_building() {
        let frame = build_udp_packet(
            GATEWAY_MAC,
            BROADCAST_MAC,
            GATEWAY_IP,
            [255, 255, 255, 255],
            67,
            68,
            &[1, 2, 3, 4],
        );
        assert!(frame.len() >= 42);
        assert_eq!(u16::from_be_bytes([frame[12], frame[13]]), 0x0800); // IPv4
        assert_eq!(frame[23], 17); // UDP
        assert_eq!(u16::from_be_bytes([frame[34], frame[36 - 1]]), 67); // Src port
        assert_eq!(u16::from_be_bytes([frame[36], frame[38 - 1]]), 68); // Dst port
    }

    #[test]
    fn test_virtio_net_ports_io() {
        let state = Arc::new(Mutex::new(VirtioNetState::new()));
        state.lock().unwrap().iobase = 0xC200;
        let mut dev = VirtioNetDevice::new(state);

        assert!(dev.matches_port(0xC200));
        assert!(dev.matches_port(0xC21F));
        assert!(!dev.matches_port(0xC220));

        // Leer features
        let feat = dev.read(0xC200, 4);
        assert_eq!(feat, (VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS).to_le_bytes());

        // Leer dirección MAC (offset 0x14)
        let mac = dev.read(0xC214, 6);
        assert_eq!(&mac, &DEFAULT_GUEST_MAC);

        // Leer estado de enlace (offset 0x1A)
        let status = dev.read(0xC21A, 2);
        assert_eq!(status, 1u16.to_le_bytes());
    }

    #[test]
    fn test_tcp_packet_building() {
        let payload = b"GET / HTTP/1.1\r\n\r\n";
        let pkt = build_tcp_packet(
            GATEWAY_MAC,
            DEFAULT_GUEST_MAC,
            [93, 184, 216, 34],
            GUEST_IP,
            80,
            12345,
            1000,
            2000,
            0x18, // PSH | ACK
            65535,
            payload,
        );

        // Longitud total: 14 eth + 20 ip + 20 tcp + 18 payload = 72 bytes
        assert_eq!(pkt.len(), 14 + 20 + 20 + payload.len());
        // Ethernet header
        assert_eq!(&pkt[0..6], &DEFAULT_GUEST_MAC);
        assert_eq!(&pkt[6..12], &GATEWAY_MAC);
        assert_eq!(u16::from_be_bytes([pkt[12], pkt[13]]), 0x0800);
        // IPv4 header
        assert_eq!(pkt[14], 0x45); // Version 4, IHL 5
        let total_len = u16::from_be_bytes([pkt[16], pkt[17]]) as usize;
        assert_eq!(total_len, 20 + 20 + payload.len());
        assert_eq!(pkt[23], 6); // TCP
        assert_eq!(&pkt[26..30], &[93, 184, 216, 34]);
        assert_eq!(&pkt[30..34], &GUEST_IP);
        // Verificar que el checksum IP sea válido (suma de words complemento a 1 debe ser 0xFFFF o 0x0000)
        let mut ip_sum = 0u32;
        for i in 0..10 {
            ip_sum += u16::from_be_bytes([pkt[14 + i * 2], pkt[14 + i * 2 + 1]]) as u32;
        }
        while (ip_sum >> 16) != 0 {
            ip_sum = (ip_sum & 0xFFFF) + (ip_sum >> 16);
        }
        assert_eq!(ip_sum as u16, 0xFFFF);

        // TCP header
        let tcp_off = 34;
        let src_port = u16::from_be_bytes([pkt[tcp_off], pkt[tcp_off + 1]]);
        let dst_port = u16::from_be_bytes([pkt[tcp_off + 2], pkt[tcp_off + 3]]);
        let seq = u32::from_be_bytes([pkt[tcp_off + 4], pkt[tcp_off + 5], pkt[tcp_off + 6], pkt[tcp_off + 7]]);
        let ack = u32::from_be_bytes([pkt[tcp_off + 8], pkt[tcp_off + 9], pkt[tcp_off + 10], pkt[tcp_off + 11]]);
        let data_off = pkt[tcp_off + 12] >> 4;
        let flags = pkt[tcp_off + 13];
        let window = u16::from_be_bytes([pkt[tcp_off + 14], pkt[tcp_off + 15]]);
        assert_eq!(src_port, 80);
        assert_eq!(dst_port, 12345);
        assert_eq!(seq, 1000);
        assert_eq!(ack, 2000);
        assert_eq!(data_off, 5);
        assert_eq!(flags, 0x18);
        assert_eq!(window, 65535);
        assert_eq!(&pkt[tcp_off + 20..], payload);
    }

    #[test]
    fn test_virtqueue_bounds_checking() {
        let mut ram = vec![0u8; 65536];
        let mem = GuestMemory::new(ram.as_mut_ptr(), ram.len());
        let mut vq = VirtQueue {
            size: 16,
            pfn: 1, // desc_addr = 0x1000
            last_avail_idx: 0,
            last_used_idx: 0,
        };
        // avail_addr = 0x1000 + 16 * 16 = 0x1100
        // Caso 1: head_idx >= vq.size (20 >= 16)
        mem.write_u16(0x1100 + 2, 1);
        mem.write_u16(0x1100 + 4, 20);

        let res = vq.read_buffer(&mem);
        assert_eq!(res, Some(vec![]));

        let mut vq_tx = VirtQueue {
            size: 16,
            pfn: 1,
            last_avail_idx: 0,
            last_used_idx: 0,
        };
        assert!(vq_tx.write_buffer(&mem, &[1, 2, 3, 4]));

        // Caso 2: flags con NEXT pero next >= vq.size
        let mut vq2 = VirtQueue {
            size: 16,
            pfn: 1,
            last_avail_idx: 0,
            last_used_idx: 0,
        };
        mem.write_u16(0x1100 + 2, 1);
        mem.write_u16(0x1100 + 4, 0); // head_idx = 0
        let desc_0 = 0x1000;
        mem.write_u32(desc_0, 0x2000); // buf_gpa
        mem.write_u32(desc_0 + 4, 0);
        mem.write_u32(desc_0 + 8, 4); // len = 4
        mem.write_u16(desc_0 + 12, VRING_DESC_F_NEXT);
        mem.write_u16(desc_0 + 14, 99); // next >= 16
        mem.write_u32(0x2000, 0x12345678);

        let res2 = vq2.read_buffer(&mem);
        assert_eq!(res2.unwrap().len(), 4);
    }

    #[test]
    fn test_inbound_tcp_port_forward_handshake_and_relay() {
        use std::io::{Read, Write};
        // Encontrar un puerto libre efímero en 127.0.0.1
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
        let host_port = probe.local_addr().unwrap().port();
        drop(probe);

        let mut cfg = NetConfig::default_slirp();
        cfg.port_forwards = vec![(host_port, 22)];
        let (tx_tx, tx_rx) = mpsc::channel();
        let (rx_tx, rx_rx) = mpsc::channel();
        spawn_net_worker_with_config(tx_rx, rx_tx, cfg.clone());

        // Esperar ligeramente a que el listener enlace
        let mut client = None;
        for _ in 0..30 {
            if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", host_port)) {
                client = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut client = client.expect("connect to forwarded host_port");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();

        // El worker debe inyectar un TCP SYN hacia el guest (puerto 22)
        let syn = rx_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("expected SYN frame");
        let gw_ephemeral = u16::from_be_bytes([syn[34], syn[35]]);
        let guest_dst_port = u16::from_be_bytes([syn[36], syn[37]]);
        let host_seq0 = u32::from_be_bytes([syn[38], syn[39], syn[40], syn[41]]);
        assert_eq!(guest_dst_port, 22);
        assert_eq!(syn[47], 0x02);

        // Guest responde SYN|ACK
        let guest_isn = 5000u32;
        let syn_ack = build_tcp_packet(
            cfg.guest_mac,
            cfg.gateway_mac,
            cfg.guest_ip,
            cfg.gateway_ip,
            22,
            gw_ephemeral,
            guest_isn,
            host_seq0.wrapping_add(1),
            0x12,
            65535,
            &[],
        );
        tx_tx.send(syn_ack).unwrap();

        // Worker debe responder ACK al guest
        let ack = rx_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("expected ACK frame");
        assert_eq!(ack[47] & 0x10, 0x10);

        // Host escribe datos al puerto redirigido -> guest debe recibir PSH|ACK
        client.write_all(b"SSH-2.0-Test\n").unwrap();
        let data_to_guest = rx_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("expected PSH|ACK to guest");
        let hdr_len = 14 + ((data_to_guest[14] & 0x0F) as usize) * 4 + ((data_to_guest[46] >> 4) as usize) * 4;
        assert_eq!(&data_to_guest[hdr_len..], b"SSH-2.0-Test\n");

        // Guest envía datos al host
        let host_seq_now = host_seq0.wrapping_add(1) + b"SSH-2.0-Test\n".len() as u32;
        let reply = build_tcp_packet(
            cfg.guest_mac,
            cfg.gateway_mac,
            cfg.guest_ip,
            cfg.gateway_ip,
            22,
            gw_ephemeral,
            guest_isn.wrapping_add(1),
            host_seq_now,
            0x18,
            65535,
            b"OK_GUEST\n",
        );
        tx_tx.send(reply).unwrap();

        let mut buf = [0u8; 64];
        let n = client.read(&mut buf).expect("client read");
        assert_eq!(&buf[..n], b"OK_GUEST\n");
    }
}
