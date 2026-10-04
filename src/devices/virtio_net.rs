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
}

impl Default for VirtQueue {
    fn default() -> Self {
        Self {
            size: QUEUE_SIZE,
            pfn: 0,
            last_avail_idx: 0,
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

        // Registrar elemento en el Used Ring
        let used_addr = self.used_addr();
        let used_idx = mem.read_u16(used_addr + 2);
        let used_slot = (used_idx % self.size) as usize;
        let elem_addr = used_addr + 4 + 8 * used_slot;

        mem.write_u32(elem_addr, head_idx as u32);
        mem.write_u32(elem_addr + 4, bytes_written as u32);
        mem.write_u16(used_addr + 2, used_idx.wrapping_add(1));
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

            if (flags & VRING_DESC_F_WRITE) == 0 && buf_len > 0 {
                let mut chunk = vec![0u8; buf_len];
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
        let used_idx = mem.read_u16(used_addr + 2);
        let used_slot = (used_idx % self.size) as usize;
        let elem_addr = used_addr + 4 + 8 * used_slot;

        mem.write_u32(elem_addr, head_idx as u32);
        mem.write_u32(elem_addr + 4, 0);
        mem.write_u16(used_addr + 2, used_idx.wrapping_add(1));
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

// ─── Worker de Red Asíncrono en Segundo Plano ──────────────────────
fn spawn_net_worker(
    tx_receiver: mpsc::Receiver<Vec<u8>>,
    rx_sender: mpsc::Sender<Vec<u8>>,
) {
    std::thread::Builder::new()
        .name("virtio-net-worker".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            let mut tap_file = if let Ok(tap_name) = std::env::var("MI_VMM_TAP") {
                eprintln!("[VIRTIO-NET] Intentando conectar a interfaz TAP '{}'...", tap_name);
                open_tap_device(&tap_name)
            } else {
                None
            };
            #[cfg(not(target_os = "linux"))]
            let mut tap_file: Option<std::fs::File> = None;

            if tap_file.is_some() {
                eprintln!("[VIRTIO-NET] Interfaz TAP inicializada y conectada.");
            } else {
                eprintln!("[VIRTIO-NET] Stack de Red Integrado activo (DHCP 10.0.2.15, Gateway 10.0.2.2, DNS Proxy).");
            }

            // Socket UDP para reenvío DNS upstream hacia 8.8.8.8:53
            let dns_socket = std::net::UdpSocket::bind("0.0.0.0:0").ok();
            if let Some(ref s) = dns_socket {
                let _ = s.set_nonblocking(true);
            }

            let mut pending_dns = HashMap::new();
            let mut tap_buf = [0u8; 2048];
            let mut dns_buf = [0u8; 2048];
            let mut tcp_buf = [0u8; 4096];

            struct TcpNatSession {
                stream: std::net::TcpStream,
                guest_mac: [u8; 6],
                guest_ip: [u8; 4],
                guest_port: u16,
                remote_ip: [u8; 4],
                remote_port: u16,
                guest_seq: u32,
                host_seq: u32,
                closed: bool,
            }

            let mut tcp_conns: HashMap<(u16, [u8; 4], u16), TcpNatSession> = HashMap::new();

            loop {
                let mut idle = true;

                // 1. Tramas salientes enviadas por el guest
                while let Ok(frame) = tx_receiver.try_recv() {
                    idle = false;
                    #[cfg(target_os = "linux")]
                    if let Some(ref mut tap) = tap_file {
                        use std::io::Write;
                        let _ = tap.write_all(&frame);
                        continue;
                    }

                    // En modo sin TAP: inspeccionar tráfico IP (TCP y UDP)
                    if frame.len() >= 34 {
                        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
                        if ethertype == 0x0800 {
                            let ihl = ((frame[14] & 0x0F) * 4) as usize;
                            let protocol = frame[23];
                            let src_mac: [u8; 6] = frame[6..12].try_into().unwrap_or(DEFAULT_GUEST_MAC);
                            let src_ip: [u8; 4] = frame[26..30].try_into().unwrap_or(GUEST_IP);
                            let dst_ip: [u8; 4] = frame[30..34].try_into().unwrap_or(GATEWAY_IP);

                            // 1a. Tráfico UDP (consultas DNS y paquetes salientes)
                            if protocol == 17 && frame.len() >= 14 + ihl + 8 {
                                let udp_off = 14 + ihl;
                                let src_port = u16::from_be_bytes([frame[udp_off], frame[udp_off + 1]]);
                                let dst_port = u16::from_be_bytes([frame[udp_off + 2], frame[udp_off + 3]]);
                                if dst_port == 53 {
                                    let dns_payload = &frame[udp_off + 8..];
                                    if dns_payload.len() >= 2 {
                                        let tx_id = u16::from_be_bytes([dns_payload[0], dns_payload[1]]);
                                        pending_dns.insert(tx_id, (src_mac, src_ip, src_port, dst_ip));
                                        if let Some(ref s) = dns_socket {
                                            let _ = s.send_to(dns_payload, "8.8.8.8:53");
                                        }
                                    }
                                }
                            }

                            // 1b. Tráfico TCP (NAT en modo usuario completo)
                            if protocol == 6 && frame.len() >= 14 + ihl + 20 {
                                let tcp_off = 14 + ihl;
                                let src_port = u16::from_be_bytes([frame[tcp_off], frame[tcp_off + 1]]);
                                let dst_port = u16::from_be_bytes([frame[tcp_off + 2], frame[tcp_off + 3]]);
                                let seq = u32::from_be_bytes([frame[tcp_off + 4], frame[tcp_off + 5], frame[tcp_off + 6], frame[tcp_off + 7]]);
                                let _ack = u32::from_be_bytes([frame[tcp_off + 8], frame[tcp_off + 9], frame[tcp_off + 10], frame[tcp_off + 11]]);
                                let data_offset = ((frame[tcp_off + 12] >> 4) * 4) as usize;
                                let flags = frame[tcp_off + 13];
                                let payload_off = tcp_off + data_offset;
                                let payload = if frame.len() > payload_off { &frame[payload_off..] } else { &[] };

                                let key = (src_port, dst_ip, dst_port);

                                if (flags & 0x02) != 0 && (flags & 0x10) == 0 {
                                    // SYN saliente: crear conexión en el host
                                    let target_addr = std::net::SocketAddr::new(
                                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(dst_ip[0], dst_ip[1], dst_ip[2], dst_ip[3])),
                                        dst_port,
                                    );
                                    if let Ok(stream) = std::net::TcpStream::connect_timeout(&target_addr, std::time::Duration::from_millis(800)) {
                                        let _ = stream.set_nonblocking(true);
                                        let host_isn = 100_000u32;
                                        let session = TcpNatSession {
                                            stream,
                                            guest_mac: src_mac,
                                            guest_ip: src_ip,
                                            guest_port: src_port,
                                            remote_ip: dst_ip,
                                            remote_port: dst_port,
                                            guest_seq: seq.wrapping_add(1),
                                            host_seq: host_isn.wrapping_add(1),
                                            closed: false,
                                        };
                                        let syn_ack = build_tcp_packet(
                                            GATEWAY_MAC,
                                            src_mac,
                                            dst_ip,
                                            src_ip,
                                            dst_port,
                                            src_port,
                                            host_isn,
                                            seq.wrapping_add(1),
                                            0x12, // SYN | ACK
                                            65535,
                                            &[],
                                        );
                                        let _ = rx_sender.send(syn_ack);
                                        tcp_conns.insert(key, session);
                                    } else {
                                        // Conexión fallida: responder con RST | ACK
                                        let rst = build_tcp_packet(
                                            GATEWAY_MAC,
                                            src_mac,
                                            dst_ip,
                                            src_ip,
                                            dst_port,
                                            src_port,
                                            0,
                                            seq.wrapping_add(1),
                                            0x14, // RST | ACK
                                            0,
                                            &[],
                                        );
                                        let _ = rx_sender.send(rst);
                                    }
                                } else if let Some(session) = tcp_conns.get_mut(&key) {
                                    if (flags & 0x04) != 0 {
                                        // RST recibido del guest
                                        session.closed = true;
                                    } else if (flags & 0x01) != 0 {
                                        // FIN recibido del guest
                                        session.guest_seq = session.guest_seq.wrapping_add(1);
                                        let fin_ack = build_tcp_packet(
                                            GATEWAY_MAC,
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
                                        session.closed = true;
                                    } else if !payload.is_empty() {
                                        use std::io::Write;
                                        let _ = session.stream.write_all(payload);
                                        session.guest_seq = session.guest_seq.wrapping_add(payload.len() as u32);
                                        // Enviar ACK inmediato
                                        let ack_pkt = build_tcp_packet(
                                            GATEWAY_MAC,
                                            session.guest_mac,
                                            session.remote_ip,
                                            session.guest_ip,
                                            session.remote_port,
                                            session.guest_port,
                                            session.host_seq,
                                            session.guest_seq,
                                            0x10, // ACK
                                            65535,
                                            &[],
                                        );
                                        let _ = rx_sender.send(ack_pkt);
                                    }
                                }
                            }
                        }
                    }
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

                // 3. Respuestas DNS upstream recibidas de 8.8.8.8:53
                if let Some(ref s) = dns_socket {
                    while let Ok((n, _)) = s.recv_from(&mut dns_buf) {
                        idle = false;
                        if n >= 2 {
                            let tx_id = u16::from_be_bytes([dns_buf[0], dns_buf[1]]);
                            if let Some((src_mac, src_ip, src_port, orig_dst_ip)) = pending_dns.remove(&tx_id) {
                                let reply_frame = build_udp_packet(
                                    GATEWAY_MAC,
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

                // 4. Datos entrantes TCP desde los sockets de host (NAT modo usuario)
                tcp_conns.retain(|_key, session| {
                    if session.closed {
                        return false;
                    }
                    use std::io::Read;
                    match session.stream.read(&mut tcp_buf) {
                        Ok(0) => {
                            // Cierre por parte del servidor remoto (EOF)
                            idle = false;
                            let fin = build_tcp_packet(
                                GATEWAY_MAC,
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
                                GATEWAY_MAC,
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

                if idle {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        })
        .expect("Error al inicializar hilo virtio-net-worker");
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
        let (tx_sender, tx_receiver) = mpsc::channel();
        let (rx_sender, rx_receiver) = mpsc::channel();

        spawn_net_worker(tx_receiver, rx_sender);

        Self {
            iobase: 0,
            host_features: VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS,
            guest_features: 0,
            queue_sel: 0,
            queues: [VirtQueue::default(); 2],
            device_status: 0,
            isr_status: 0,
            irq_pulse: false,
            mac: DEFAULT_GUEST_MAC,
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

        // 2. Procesar Queue 1 (TX: Guest -> Host)
        while let Some(data) = self.queues[1].read_buffer(mem) {
            trigger_irq = true;
            if data.len() > 10 {
                let eth_frame = &data[10..];
                self.handle_tx_frame(eth_frame);
            }
        }

        // 3. Procesar Queue 0 (RX: Host -> Guest)
        while let Some(frame) = self.rx_queue.front() {
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
        if frame.len() < 14 {
            return;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

        match ethertype {
            0x0806 => {
                if let Some(reply) = self.handle_arp(frame) {
                    self.rx_queue.push_back(reply);
                }
            }
            0x0800 => {
                if let Some(reply) = self.handle_ipv4(frame) {
                    self.rx_queue.push_back(reply);
                }
            }
            _ => {}
        }

        // Reenviar al worker para TAP o DNS upstream
        if let Some(ref tx_tx) = self.tx_sender {
            let _ = tx_tx.send(frame.to_vec());
        }
    }

    fn handle_arp(&self, frame: &[u8]) -> Option<Vec<u8>> {
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

        if target_ip == GATEWAY_IP || target_ip == DNS_IP {
            crate::tui::log(&format!(
                "[VIRTIO-NET] Solicitud ARP de {}.{}.{}.{} para {}.{}.{}.{} -> Respondiendo",
                sender_ip[0], sender_ip[1], sender_ip[2], sender_ip[3],
                target_ip[0], target_ip[1], target_ip[2], target_ip[3]
            ));
            Some(build_arp_reply(GATEWAY_MAC, target_ip, sender_mac, sender_ip))
        } else {
            None
        }
    }

    fn handle_ipv4(&self, frame: &[u8]) -> Option<Vec<u8>> {
        if frame.len() < 34 {
            return None;
        }
        let ihl = ((frame[14] & 0x0F) * 4) as usize;
        if frame.len() < 14 + ihl {
            return None;
        }
        let protocol = frame[23];
        let dst_ip: [u8; 4] = frame[30..34].try_into().ok()?;

        match protocol {
            1 => {
                // ICMP
                if dst_ip == GATEWAY_IP || dst_ip == DNS_IP {
                    crate::tui::log("[VIRTIO-NET] Solicitud ICMP Echo Request -> Respondiendo Echo Reply");
                    build_icmp_echo_reply(frame)
                } else {
                    None
                }
            }
            17 => {
                // UDP
                let udp_offset = 14 + ihl;
                if frame.len() < udp_offset + 8 {
                    return None;
                }
                let dst_port = u16::from_be_bytes([frame[udp_offset + 2], frame[udp_offset + 3]]);
                if dst_port == 67 {
                    self.handle_dhcp(&frame[udp_offset + 8..], frame)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn handle_dhcp(&self, dhcp_payload: &[u8], full_frame: &[u8]) -> Option<Vec<u8>> {
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
                crate::tui::log("[VIRTIO-NET] DHCP Discover recibido -> Enviando DHCPOFFER (10.0.2.15)");
                2 // DHCPOFFER
            }
            Some(3) => {
                crate::tui::log("[VIRTIO-NET] DHCP Request recibido -> Enviando DHCPACK (10.0.2.15)");
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
        offer[16..20].copy_from_slice(&GUEST_IP);
        offer[20..24].copy_from_slice(&GATEWAY_IP);
        offer[28..34].copy_from_slice(&client_mac);
        offer[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);

        let mut opt = Vec::new();
        opt.extend_from_slice(&[53, 1, response_type]);
        opt.extend_from_slice(&[54, 4]);
        opt.extend_from_slice(&GATEWAY_IP);
        opt.extend_from_slice(&[51, 4]);
        opt.extend_from_slice(&86400u32.to_be_bytes()); // Lease time: 1 día
        opt.extend_from_slice(&[1, 4]);
        opt.extend_from_slice(&SUBNET_MASK);
        opt.extend_from_slice(&[3, 4]);
        opt.extend_from_slice(&GATEWAY_IP);
        opt.extend_from_slice(&[6, 4]);
        opt.extend_from_slice(&DNS_IP);
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
            GATEWAY_MAC,
            dst_mac,
            GATEWAY_IP,
            [255, 255, 255, 255],
            67,
            68,
            &offer,
        ))
    }
}

/// Dispositivo VirtIO-Net registrado en el bus I/O del VMM.
pub struct VirtioNetDevice {
    pub state: Arc<Mutex<VirtioNetState>>,
}

impl VirtioNetDevice {
    pub fn new(state: Arc<Mutex<VirtioNetState>>) -> Self {
        Self { state }
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
        };
        assert!(vq_tx.write_buffer(&mem, &[1, 2, 3, 4]));

        // Caso 2: flags con NEXT pero next >= vq.size
        let mut vq2 = VirtQueue {
            size: 16,
            pfn: 1,
            last_avail_idx: 0,
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
}
