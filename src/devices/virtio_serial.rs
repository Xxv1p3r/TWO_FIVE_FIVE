//! Emulación de dispositivo PCI VirtIO-Serial con soporte del protocolo SPICE vdagent.
//!
//! Permite la negociación de resolución dinámica en caliente con sistemas operativos
//! invitados modernos (Linux Mint, Kali, Ubuntu, Fedora, Debian, etc.) que cuenten con
//! el servicio estándar `spice-vdagent` escuchando en `/dev/virtio-ports/com.redhat.spice.0`.

use super::IoDevice;
use crate::guest_mem::GuestMemory;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ─── Constantes VirtIO Legacy PCI ──────────────────────────────────
pub const VIRTIO_PCI_HOST_FEATURES: u16 = 0x00;
pub const VIRTIO_PCI_GUEST_FEATURES: u16 = 0x04;
pub const VIRTIO_PCI_QUEUE_PFN: u16 = 0x08;
pub const VIRTIO_PCI_QUEUE_NUM: u16 = 0x0C;
pub const VIRTIO_PCI_QUEUE_SEL: u16 = 0x0E;
pub const VIRTIO_PCI_QUEUE_NOTIFY: u16 = 0x10;
pub const VIRTIO_PCI_STATUS: u16 = 0x12;
pub const VIRTIO_PCI_ISR: u16 = 0x13;

pub const VIRTIO_CONSOLE_F_SIZE: u32 = 1 << 0;
pub const VIRTIO_CONSOLE_F_MULTIPORT: u32 = 1 << 1;

#[allow(dead_code)]
pub const VIRTIO_CONFIG_S_ACKNOWLEDGE: u8 = 1;
#[allow(dead_code)]
pub const VIRTIO_CONFIG_S_DRIVER: u8 = 2;
pub const VIRTIO_CONFIG_S_DRIVER_OK: u8 = 4;
#[allow(dead_code)]
pub const VIRTIO_CONFIG_S_FAILED: u8 = 128;

// ─── Constantes de Control VirtIO-Console ───────────────────────────
pub const VIRTIO_CONSOLE_DEVICE_READY: u16 = 0;
pub const VIRTIO_CONSOLE_PORT_ADD: u16 = 1;
pub const VIRTIO_CONSOLE_PORT_READY: u16 = 3;
pub const VIRTIO_CONSOLE_PORT_OPEN: u16 = 6;
pub const VIRTIO_CONSOLE_PORT_NAME: u16 = 7;

// ─── Constantes del Protocolo SPICE vdagent ─────────────────────────
pub const VDP_CLIENT_PORT: u32 = 1;
pub const VD_AGENT_PROTOCOL: u32 = 1;
pub const VD_AGENT_MONITORS_CONFIG: u32 = 2;
pub const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
pub const VD_AGENT_CAP_MONITORS_CONFIG: u32 = 1;

pub const QUEUE_SIZE: u16 = 32;

// Banderas de descriptor VirtIO
const VRING_DESC_F_NEXT: u16 = 0x0001;
const VRING_DESC_F_WRITE: u16 = 0x0002;

/// Estructura que representa una cola VirtQueue (Split Virtqueue Legacy).
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

    /// Escribe datos en una cadena de descriptores disponible (Host -> Guest).
    /// Devuelve true si se escribieron datos con éxito.
    pub fn write_buffer(&mut self, mem: &GuestMemory, data: &[u8]) -> bool {
        if !self.is_ready() || data.is_empty() {
            return false;
        }

        let avail_addr = self.avail_addr();
        let avail_idx = mem.read_u16(avail_addr + 2);
        if avail_idx == self.last_avail_idx {
            return false; // No hay buffers disponibles puestos por el guest
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

    /// Lee datos de una cadena de descriptores disponible (Guest -> Host).
    /// Devuelve los bytes leídos si había un buffer listo.
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

        // Registrar elemento en el Used Ring
        let used_addr = self.used_addr();
        let used_idx = mem.read_u16(used_addr + 2);
        let used_slot = (used_idx % self.size) as usize;
        let elem_addr = used_addr + 4 + 8 * used_slot;

        mem.write_u32(elem_addr, head_idx as u32);
        mem.write_u32(elem_addr + 4, data.len() as u32);
        mem.write_u16(used_addr + 2, used_idx.wrapping_add(1));
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);

        Some(data)
    }
}

/// Mensaje de control VirtIO-Console empaquetado.
pub fn build_console_control(id: u32, event: u16, value: u16, extra: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(8 + extra.len());
    msg.extend_from_slice(&id.to_le_bytes());
    msg.extend_from_slice(&event.to_le_bytes());
    msg.extend_from_slice(&value.to_le_bytes());
    msg.extend_from_slice(extra);
    msg
}

/// Mensaje SPICE VDAgentMonitorsConfig empaquetado para 1 monitor (56 bytes en total).
pub fn build_spice_monitors_config(w: u32, h: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(56);
    // 1. VDIChunkHeader (8 bytes)
    msg.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
    msg.extend_from_slice(&48u32.to_le_bytes()); // Tamaño de VDAgentMessage (20 + 28 = 48)

    // 2. VDAgentMessage (20 bytes)
    msg.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
    msg.extend_from_slice(&VD_AGENT_MONITORS_CONFIG.to_le_bytes());
    msg.extend_from_slice(&0u64.to_le_bytes()); // opaque
    msg.extend_from_slice(&28u32.to_le_bytes()); // payload size

    // 3. VDAgentMonitorsConfig (28 bytes)
    msg.extend_from_slice(&1u32.to_le_bytes()); // num_of_monitors = 1
    msg.extend_from_slice(&0u32.to_le_bytes()); // flags = 0
    msg.extend_from_slice(&h.to_le_bytes());
    msg.extend_from_slice(&w.to_le_bytes());
    msg.extend_from_slice(&32u32.to_le_bytes()); // depth = 32
    msg.extend_from_slice(&0i32.to_le_bytes());  // x = 0
    msg.extend_from_slice(&0i32.to_le_bytes());  // y = 0

    msg
}

/// Mensaje SPICE VDAgentAnnounceCapabilities empaquetado (36 bytes en total).
pub fn build_spice_capabilities() -> Vec<u8> {
    let mut msg = Vec::with_capacity(36);
    // VDIChunkHeader (8 bytes)
    msg.extend_from_slice(&VDP_CLIENT_PORT.to_le_bytes());
    msg.extend_from_slice(&28u32.to_le_bytes()); // VDAgentMessage + 8 bytes payload = 28

    // VDAgentMessage (20 bytes)
    msg.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
    msg.extend_from_slice(&VD_AGENT_ANNOUNCE_CAPABILITIES.to_le_bytes());
    msg.extend_from_slice(&0u64.to_le_bytes());
    msg.extend_from_slice(&8u32.to_le_bytes());

    // VDAgentAnnounceCapabilities payload (8 bytes)
    msg.extend_from_slice(&0u32.to_le_bytes()); // request = 0
    msg.extend_from_slice(&(1u32 << VD_AGENT_CAP_MONITORS_CONFIG).to_le_bytes()); // caps

    msg
}

/// Estado interno del controlador VirtIO-Serial.
#[derive(Debug)]
pub struct VirtioSerialState {
    pub iobase: u16,
    pub host_features: u32,
    pub guest_features: u32,
    pub queue_sel: u16,
    pub queues: [VirtQueue; 4],
    pub device_status: u8,
    pub isr_status: u8,
    pub irq_pulse: bool,

    // Estado del protocolo VirtIO-Console multiport
    pub guest_device_ready: bool,
    pub guest_port_ready: bool,
    pub guest_port_open: bool,

    // Colas de transmisión pendientes
    pub control_tx_pending: VecDeque<Vec<u8>>,
    pub data_tx_pending: VecDeque<Vec<u8>>,

    // Resolución dinámica deseada
    pub desired_resolution: Option<(u32, u32)>,
    pub last_sent_resolution: Option<(u32, u32)>,
}

impl Default for VirtioSerialState {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioSerialState {
    pub fn new() -> Self {
        Self {
            iobase: 0,
            host_features: VIRTIO_CONSOLE_F_MULTIPORT | VIRTIO_CONSOLE_F_SIZE,
            guest_features: 0,
            queue_sel: 0,
            queues: [VirtQueue::default(); 4],
            device_status: 0,
            isr_status: 0,
            irq_pulse: false,
            guest_device_ready: false,
            guest_port_ready: false,
            guest_port_open: false,
            control_tx_pending: VecDeque::new(),
            data_tx_pending: VecDeque::new(),
            desired_resolution: None,
            last_sent_resolution: None,
        }
    }

    pub fn reset(&mut self) {
        let base = self.iobase;
        *self = Self::new();
        self.iobase = base;
    }

    /// Solicita un cambio de resolución dinámica hacia el agente en el sistema operativo invitado.
    pub fn request_resolution(&mut self, width: u32, height: u32) {
        let clamped_w = (width.clamp(320, 2560) & !1) as u32;
        let clamped_h = height.clamp(200, 1600) as u32;
        self.desired_resolution = Some((clamped_w, clamped_h));

        if self.guest_port_open && self.last_sent_resolution != Some((clamped_w, clamped_h)) {
            self.last_sent_resolution = Some((clamped_w, clamped_h));
            self.data_tx_pending
                .push_back(build_spice_monitors_config(clamped_w, clamped_h));
        }
    }

    /// Comprueba y extrae el pulso de interrupción pendiente.
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

        // 1. Procesar Queue 3 (Control TX: Guest -> Host)
        while let Some(pkt) = self.queues[3].read_buffer(mem) {
            trigger_irq = true;
            if pkt.len() >= 8 {
                let id = u32::from_le_bytes([pkt[0], pkt[1], pkt[2], pkt[3]]);
                let event = u16::from_le_bytes([pkt[4], pkt[5]]);
                let value = u16::from_le_bytes([pkt[6], pkt[7]]);

                match event {
                    VIRTIO_CONSOLE_DEVICE_READY => {
                        self.guest_device_ready = value == 1;
                        if self.guest_device_ready {
                            crate::tui::log("[VIRTIO-SERIAL] Guest kernel DEVICE_READY. Anunciando puerto 0...");
                            // Anunciar el puerto 0
                            self.control_tx_pending.push_back(build_console_control(
                                0,
                                VIRTIO_CONSOLE_PORT_ADD,
                                1,
                                &[],
                            ));
                        }
                    }
                    VIRTIO_CONSOLE_PORT_READY => {
                        if id == 0 {
                            self.guest_port_ready = value == 1;
                            if self.guest_port_ready {
                                crate::tui::log("[VIRTIO-SERIAL] Guest kernel PORT_READY para puerto 0. Configurando nombre com.redhat.spice.0...");
                                self.control_tx_pending.push_back(build_console_control(
                                    0,
                                    VIRTIO_CONSOLE_PORT_NAME,
                                    1,
                                    b"com.redhat.spice.0\0",
                                ));
                                self.control_tx_pending.push_back(build_console_control(
                                    0,
                                    VIRTIO_CONSOLE_PORT_OPEN,
                                    1,
                                    &[],
                                ));
                            }
                        }
                    }
                    VIRTIO_CONSOLE_PORT_OPEN => {
                        if id == 0 {
                            let open = value == 1;
                            self.guest_port_open = open;
                            crate::tui::log(&format!("[VIRTIO-SERIAL] Guest agent PORT_OPEN={}", open));
                            if open {
                                // spice-vdagent abrió el puerto: anunciar capacidades y resolución
                                self.data_tx_pending.push_back(build_spice_capabilities());
                                if let Some((w, h)) = self.desired_resolution {
                                    self.last_sent_resolution = Some((w, h));
                                    self.data_tx_pending
                                        .push_back(build_spice_monitors_config(w, h));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // 2. Enviar mensajes pendientes en Queue 2 (Control RX: Host -> Guest)
        while let Some(msg) = self.control_tx_pending.front() {
            if self.queues[2].write_buffer(mem, msg) {
                self.control_tx_pending.pop_front();
                trigger_irq = true;
            } else {
                break;
            }
        }

        // 3. Procesar Queue 1 (Port TX: Guest -> Host data)
        while let Some(data) = self.queues[1].read_buffer(mem) {
            trigger_irq = true;
            // Si el agente envía VD_AGENT_ANNOUNCE_CAPABILITIES, responder con la configuración inicial
            if data.len() >= 28 {
                let msg_type = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
                if msg_type == VD_AGENT_ANNOUNCE_CAPABILITIES {
                    if let Some((w, h)) = self.desired_resolution {
                        self.last_sent_resolution = Some((w, h));
                        self.data_tx_pending
                            .push_back(build_spice_monitors_config(w, h));
                    }
                }
            }
        }

        // 4. Enviar mensajes pendientes en Queue 0 (Port RX: Host -> Guest data)
        while let Some(msg) = self.data_tx_pending.front() {
            if self.queues[0].write_buffer(mem, msg) {
                self.data_tx_pending.pop_front();
                trigger_irq = true;
            } else {
                break;
            }
        }

        if trigger_irq {
            self.isr_status |= 0x01;
            self.irq_pulse = true;
        }
    }
}

/// Dispositivo VirtIO-Serial registrado en el bus I/O.
pub struct VirtioSerialDevice {
    pub state: Arc<Mutex<VirtioSerialState>>,
}

impl VirtioSerialDevice {
    pub fn new(state: Arc<Mutex<VirtioSerialState>>) -> Self {
        Self { state }
    }
}

impl IoDevice for VirtioSerialDevice {
    fn matches_port(&self, port: u16) -> bool {
        let base = self.state.lock().unwrap().iobase;
        base != 0 && port >= base && port < base + 64
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
                // Al leer el registro ISR, se limpia a 0 según especificación VirtIO
                res[0] = st.isr_status;
                st.isr_status = 0;
            }
            // Device-specific config (offset 0x14):
            // cols: 80, rows: 25, max_nr_ports: 1
            0x14..=0x15 => {
                let bytes = 80u16.to_le_bytes();
                let shift = (offset - 0x14) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            0x16..=0x17 => {
                let bytes = 25u16.to_le_bytes();
                let shift = (offset - 0x16) as usize;
                for i in 0..count {
                    if shift + i < bytes.len() {
                        res[i] = bytes[shift + i];
                    }
                }
            }
            0x18..=0x1B => {
                // max_nr_ports = 1 (genera exactamente 4 colas: 2 de datos + 2 de control)
                let bytes = 1u32.to_le_bytes();
                let shift = (offset - 0x18) as usize;
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
                // Notificación de la cola escrita por el guest
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
    fn test_spice_monitors_config_format() {
        let msg = build_spice_monitors_config(1024, 768);
        assert_eq!(msg.len(), 56);
        // VDIChunkHeader
        assert_eq!(u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]), 1);
        assert_eq!(u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]), 48);
        // VDAgentMessage
        assert_eq!(u32::from_le_bytes([msg[8], msg[9], msg[10], msg[11]]), 1);
        assert_eq!(u32::from_le_bytes([msg[12], msg[13], msg[14], msg[15]]), 2);
        // Monitors payload
        assert_eq!(u32::from_le_bytes([msg[28], msg[29], msg[30], msg[31]]), 1); // num monitors
        assert_eq!(u32::from_le_bytes([msg[36], msg[37], msg[38], msg[39]]), 768); // height
        assert_eq!(u32::from_le_bytes([msg[40], msg[41], msg[42], msg[43]]), 1024); // width
    }

    #[test]
    fn test_virtio_serial_ports_io() {
        let state = Arc::new(Mutex::new(VirtioSerialState::new()));
        state.lock().unwrap().iobase = 0xC100;
        let mut dev = VirtioSerialDevice::new(state);

        assert!(dev.matches_port(0xC100));
        assert!(dev.matches_port(0xC13F));
        assert!(!dev.matches_port(0xC140));

        // Leer features
        let feat = dev.read(0xC100, 4);
        assert_eq!(feat, (VIRTIO_CONSOLE_F_MULTIPORT | VIRTIO_CONSOLE_F_SIZE).to_le_bytes());

        // Seleccionar Queue 2 y configurar PFN
        dev.write(0xC10E, &2u16.to_le_bytes());
        dev.write(0xC108, &0x1234u32.to_le_bytes());

        let pfn = dev.read(0xC108, 4);
        assert_eq!(pfn, 0x1234u32.to_le_bytes());
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
