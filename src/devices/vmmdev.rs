//! Controlador VMMDev (VirtualBox Guest Additions Integration Device).
//!
//! Emula el dispositivo PCI `80EE:CAFE` de VirtualBox (`VMMDev`), que implementa
//! la interfaz entre el hipervisor y los controladores de integración del huésped
//! (`vboxguest`, herramientas del sistema y agentes de usuario).
//!
//! Características emuladas:
//! - BAR0 (I/O, 32 bytes):
//!   * Offset 0: Puerto de despacho de peticiones GPA (VMMDev Request).
//!   * Offset 8: Puerto rápido de acuse de eventos/IRQ (Fast Request IRQ Ack).
//! - BAR1 (MMIO, 16 KiB): VMMDev RAM (estructura `VMMDevMemory`, versión 1).
//! - Tipos de petición soportados:
//!   * `VMMDevReq_GetHostVersion` (4): Informa versión de VirtualBox (7.2.20) y capacidades.
//!   * `VMMDevReq_GetHostTime` (10): Sincronización horaria en milisegundos UTC.
//!   * `VMMDevReq_GetMouseStatus` (1) y `VMMDevReq_SetMouseStatus` (2): Integración de ratón absoluto.
//!   * `VMMDevReq_AcknowledgeEvents` (41): Lectura y confirmación de eventos del host.
//!   * `VMMDevReq_CtlGuestFilterMask` (42): Configuración de filtros de eventos del huésped.
//!   * `VMMDevReq_ReportGuestInfo` (50) y `ReportGuestInfo2` (58): Registro del SO huésped.
//!   * `VMMDevReq_GetDisplayChangeRequest` (51) y `GetDisplayChangeRequest2` (54): Redimensión dinámica de pantalla.
//!   * `VMMDevReq_ReportGuestCapabilities` (55) y `SetGuestCapabilities` (56): Capacidades activas.
//!   * `VMMDevReq_VideoModeSupported` (52): Verificación de modos gráficos.
//!   * Peticiones HGCM (60..=64): Devolución segura `VERR_NOT_SUPPORTED` (-37).

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::guest_mem::GuestMemory;

// ─── Constantes del protocolo VMMDev de VirtualBox ───────────────────────────

#[allow(dead_code)]
pub const VMMDEV_VENDOR_ID: u16 = 0x80EE;
#[allow(dead_code)]
pub const VMMDEV_DEVICE_ID: u16 = 0xCAFE;

#[allow(dead_code)]
pub const VMMDEV_VERSION: u32 = 0x00010004;
pub const VMMDEV_REQUEST_HEADER_VERSION: u32 = 0x10001;
pub const VMMDEV_MEMORY_VERSION: u32 = 1;
pub const VMMDEV_RAM_SIZE: usize = 0x4000; // 16 KiB

// Puertos relativos a BAR0
pub const VMMDEV_PORT_OFF_REQUEST: u16 = 0;
pub const VMMDEV_PORT_OFF_REQUEST_FAST: u16 = 8;

// Códigos de petición (VMMDevRequestType)
#[allow(dead_code)]
pub const VMMDEV_REQ_INVALID: u32 = 0;
pub const VMMDEV_REQ_GET_MOUSE_STATUS: u32 = 1;
pub const VMMDEV_REQ_SET_MOUSE_STATUS: u32 = 2;
#[allow(dead_code)]
pub const VMMDEV_REQ_SET_POINTER_SHAPE: u32 = 3;
pub const VMMDEV_REQ_GET_HOST_VERSION: u32 = 4;
pub const VMMDEV_REQ_IDLE: u32 = 5;
pub const VMMDEV_REQ_GET_HOST_TIME: u32 = 10;
#[allow(dead_code)]
pub const VMMDEV_REQ_GET_HYPERVISOR_INFO: u32 = 20;
#[allow(dead_code)]
pub const VMMDEV_REQ_SET_HYPERVISOR_INFO: u32 = 21;
#[allow(dead_code)]
pub const VMMDEV_REQ_REGISTER_PATCH_MEMORY: u32 = 22;
#[allow(dead_code)]
pub const VMMDEV_REQ_DEREGISTER_PATCH_MEMORY: u32 = 23;
#[allow(dead_code)]
pub const VMMDEV_REQ_SET_POWER_STATUS: u32 = 30;
pub const VMMDEV_REQ_ACKNOWLEDGE_EVENTS: u32 = 41;
pub const VMMDEV_REQ_CTL_GUEST_FILTER_MASK: u32 = 42;
pub const VMMDEV_REQ_REPORT_GUEST_INFO: u32 = 50;
pub const VMMDEV_REQ_GET_DISPLAY_CHANGE_REQUEST: u32 = 51;
pub const VMMDEV_REQ_VIDEO_MODE_SUPPORTED: u32 = 52;
#[allow(dead_code)]
pub const VMMDEV_REQ_GET_HEIGHT_REDUCTION: u32 = 53;
pub const VMMDEV_REQ_GET_DISPLAY_CHANGE_REQUEST2: u32 = 54;
pub const VMMDEV_REQ_REPORT_GUEST_CAPABILITIES: u32 = 55;
pub const VMMDEV_REQ_SET_GUEST_CAPABILITIES: u32 = 56;
pub const VMMDEV_REQ_REPORT_GUEST_INFO2: u32 = 58;
pub const VMMDEV_REQ_REPORT_GUEST_STATUS: u32 = 59;
pub const VMMDEV_REQ_REPORT_GUEST_USER_STATE: u32 = 74;

// HGCM requests
pub const VMMDEV_REQ_HGCM_CONNECT: u32 = 60;
pub const VMMDEV_REQ_HGCM_DISCONNECT: u32 = 61;
pub const VMMDEV_REQ_HGCM_CALL32: u32 = 62;
pub const VMMDEV_REQ_HGCM_CALL64: u32 = 63;
pub const VMMDEV_REQ_HGCM_CANCEL: u32 = 64;

// Eventos de VMMDev
#[allow(dead_code)]
pub const VMMDEV_EVENT_MOUSE_CAPABILITIES_CHANGED: u32 = 1 << 0;
#[allow(dead_code)]
pub const VMMDEV_EVENT_HGCM: u32 = 1 << 1;
pub const VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST: u32 = 1 << 2;
#[allow(dead_code)]
pub const VMMDEV_EVENT_SEAMLESS_MODE_CHANGE_REQUEST: u32 = 1 << 5;
#[allow(dead_code)]
pub const VMMDEV_EVENT_BALLOON_CHANGE_REQUEST: u32 = 1 << 6;
#[allow(dead_code)]
pub const VMMDEV_EVENT_STATISTICS_INTERVAL_CHANGE_REQUEST: u32 = 1 << 7;
pub const VMMDEV_EVENT_MOUSE_POSITION_CHANGED: u32 = 1 << 9;

// Características de ratón
pub const VMMDEV_MOUSE_GUEST_CAN_ABSOLUTE: u32 = 1 << 0;
pub const VMMDEV_MOUSE_HOST_WANTS_ABSOLUTE: u32 = 1 << 1;
#[allow(dead_code)]
pub const VMMDEV_MOUSE_GUEST_NEEDS_HOST_CURSOR: u32 = 1 << 2;
#[allow(dead_code)]
pub const VMMDEV_MOUSE_HOST_CANNOT_HWPOINTER: u32 = 1 << 3;
#[allow(dead_code)]
pub const VMMDEV_MOUSE_NEW_PROTOCOL: u32 = 1 << 4;
pub const VMMDEV_MOUSE_HOST_HAS_ABS_DEV: u32 = 1 << 6;

// Características del host
#[allow(dead_code)]
pub const VMMDEV_HVF_HGCM_PHYS_PAGE_LIST: u32 = 1 << 0;
pub const VMMDEV_HVF_HGCM: u32 = 0x02;
pub const VMMDEV_HVF_HGCM_CONTIG_PAGE_LIST: u32 = 0x08;
pub const VMMDEV_HVF_FAST_IRQ_ACK: u32 = 1 << 31;
pub const VMMDEV_VERSION_1_03: u32 = 0x00010003;

// Códigos de retorno VBox
pub const VINF_SUCCESS: i32 = 0;
pub const VERR_NOT_IMPLEMENTED: i32 = -12;
pub const VERR_NOT_SUPPORTED: i32 = -37;
#[allow(dead_code)]
pub const VERR_INVALID_PARAMETER: i32 = -2;

/// Información reportada por el Guest OS
#[derive(Debug, Clone, Default)]
pub struct VmmGuestInfo {
    pub os_type: u32,
    pub interface_version: u32,
    pub major: u16,
    pub minor: u16,
    pub build: u32,
    pub revision: u32,
    pub name: String,
}

/// Estado interno compartido del dispositivo VMMDev
pub struct VmmDevState {
    pub iobase: u16,
    pub mmio_base: u32,
    pub ram: Vec<u8>,
    pub host_event_flags: u32,
    pub guest_filter_mask: u32,
    pub guest_caps: u32,
    pub mouse_features: u32,
    pub mouse_x: i32,
    pub mouse_y: i32,
    pub additions_active: bool,
    pub additions_ok: bool,
    pub guest_info: VmmGuestInfo,
    pub display_change_req: Option<(u32, u32, u32)>, // width, height, bpp
    pub irq_asserted: bool,
}

impl VmmDevState {
    pub fn new() -> Self {
        let mut ram = vec![0u8; VMMDEV_RAM_SIZE];
        // Inicializar VMMDevMemory
        let size_bytes = (VMMDEV_RAM_SIZE as u32).to_le_bytes();
        let ver_bytes = VMMDEV_MEMORY_VERSION.to_le_bytes();
        ram[0..4].copy_from_slice(&size_bytes);
        ram[4..8].copy_from_slice(&ver_bytes);
        ram[8..12].copy_from_slice(&0u32.to_le_bytes()); // fHaveEvents = 0
        ram[12..16].copy_from_slice(&0u32.to_le_bytes()); // guest event mask

        Self {
            iobase: 0,
            mmio_base: 0,
            ram,
            host_event_flags: 0,
            guest_filter_mask: 0,
            guest_caps: 0,
            mouse_features: VMMDEV_MOUSE_HOST_WANTS_ABSOLUTE | VMMDEV_MOUSE_HOST_HAS_ABS_DEV,
            mouse_x: 0x8000,
            mouse_y: 0x8000,
            additions_active: false,
            additions_ok: false,
            guest_info: VmmGuestInfo::default(),
            display_change_req: None,
            irq_asserted: false,
        }
    }

    pub fn reset(&mut self) {
        let size_bytes = (VMMDEV_RAM_SIZE as u32).to_le_bytes();
        let ver_bytes = VMMDEV_MEMORY_VERSION.to_le_bytes();
        self.ram.fill(0);
        self.ram[0..4].copy_from_slice(&size_bytes);
        self.ram[4..8].copy_from_slice(&ver_bytes);

        self.host_event_flags = 0;
        self.guest_filter_mask = 0;
        self.guest_caps = 0;
        self.mouse_features = VMMDEV_MOUSE_HOST_WANTS_ABSOLUTE | VMMDEV_MOUSE_HOST_HAS_ABS_DEV;
        self.mouse_x = 0x8000;
        self.mouse_y = 0x8000;
        self.additions_active = false;
        self.additions_ok = false;
        self.guest_info = VmmGuestInfo::default();
        self.display_change_req = None;
        self.irq_asserted = false;
    }

    /// Emite un evento del host hacia el huésped y actualiza la bandera de interrupción
    pub fn raise_event(&mut self, event: u32) {
        self.host_event_flags |= event;
        let host_events_bytes = self.host_event_flags.to_le_bytes();
        self.ram[8] = 1; // fHaveEvents = true
        self.ram[8..12].copy_from_slice(&host_events_bytes);
        if (self.host_event_flags & self.guest_filter_mask) != 0 {
            self.irq_asserted = true;
        }
    }

    /// Actualiza la posición absoluta del cursor del ratón (0..0xFFFF)
    pub fn update_mouse_position(&mut self, x: i32, y: i32) {
        self.mouse_x = x;
        self.mouse_y = y;
        if (self.mouse_features & VMMDEV_MOUSE_GUEST_CAN_ABSOLUTE) != 0 {
            self.raise_event(VMMDEV_EVENT_MOUSE_POSITION_CHANGED);
        }
    }

    /// Notifica una petición de cambio de resolución gráfica hacia el huésped
    pub fn request_display_resize(&mut self, width: u32, height: u32, bpp: u32) {
        self.display_change_req = Some((width, height, bpp));
        self.raise_event(VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST);
    }
}

/// Dispositivo VMMDev conectado al bus de E/S y MMIO
pub struct VmmDevDevice {
    pub state: Arc<Mutex<VmmDevState>>,
}

impl VmmDevDevice {
    pub fn new(state: Arc<Mutex<VmmDevState>>) -> Self {
        Self { state }
    }

    pub fn set_iobase(&mut self, base: u16) {
        self.state.lock().unwrap().iobase = base;
    }

    pub fn set_mmio_base(&mut self, base: u32) {
        self.state.lock().unwrap().mmio_base = base;
    }

    pub fn matches_port(&self, port: u16) -> bool {
        let iobase = self.state.lock().unwrap().iobase;
        iobase != 0 && port >= iobase && port < iobase + 32
    }

    pub fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let mut state = self.state.lock().unwrap();
        let iobase = state.iobase;
        let off = port.saturating_sub(iobase);

        if off == VMMDEV_PORT_OFF_REQUEST_FAST {
            // Fast request IRQ ack (igual que VMMDevReq_AcknowledgeEvents)
            if !state.additions_ok || count != 4 {
                return vec![0xFF; count];
            }
            let pending = state.host_event_flags & state.guest_filter_mask;
            state.host_event_flags &= !state.guest_filter_mask;
            let host_events_bytes = state.host_event_flags.to_le_bytes();
            state.ram[8..12].copy_from_slice(&host_events_bytes);
            state.ram[8] = 0; // fHaveEvents = false
            state.irq_asserted = false;
            let bytes = pending.to_le_bytes();
            return bytes.to_vec();
        }

        vec![0xFF; count]
    }

    pub fn write(&mut self, port: u16, data: &[u8], mem: Option<&GuestMemory>) {
        let iobase = self.state.lock().unwrap().iobase;
        let off = port.saturating_sub(iobase);

        if off == VMMDEV_PORT_OFF_REQUEST && data.len() >= 4 {
            let gpa = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if let Some(guest_mem) = mem {
                self.process_request(gpa, guest_mem);
            }
        }
    }

    /// Despacha peticiones MMIO recibidas en la ventana BAR1
    pub fn mmio_read(&self, addr: u64, size: usize) -> Option<Vec<u8>> {
        let state = self.state.lock().unwrap();
        let mmio_base = state.mmio_base as u64;
        if mmio_base != 0 && addr >= mmio_base && addr < mmio_base + VMMDEV_RAM_SIZE as u64 {
            let off = (addr - mmio_base) as usize;
            let end = (off + size).min(state.ram.len());
            let mut res = vec![0xFF; size];
            if off < state.ram.len() {
                let copy_len = end - off;
                res[..copy_len].copy_from_slice(&state.ram[off..end]);
            }
            return Some(res);
        }
        None
    }

    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        let mmio_base = state.mmio_base as u64;
        if mmio_base != 0 && addr >= mmio_base && addr < mmio_base + VMMDEV_RAM_SIZE as u64 {
            let off = (addr - mmio_base) as usize;
            if off < state.ram.len() {
                let copy_len = data.len().min(state.ram.len() - off);
                state.ram[off..off + copy_len].copy_from_slice(&data[..copy_len]);
            }
            return true;
        }
        false
    }

    /// Procesa una estructura `VMMDevRequestHeader` almacenada en la memoria física del huésped
    fn process_request(&self, gpa: u32, mem: &GuestMemory) {
        if gpa == 0 {
            return;
        }
        let mut hdr_bytes = [0u8; 24];
        if mem.read_bytes(gpa as u64, &mut hdr_bytes).is_err() {
            return;
        }

        let size = u32::from_le_bytes([hdr_bytes[0], hdr_bytes[1], hdr_bytes[2], hdr_bytes[3]]);
        let version = u32::from_le_bytes([hdr_bytes[4], hdr_bytes[5], hdr_bytes[6], hdr_bytes[7]]);
        let request_type = u32::from_le_bytes([hdr_bytes[8], hdr_bytes[9], hdr_bytes[10], hdr_bytes[11]]);

        if size < 24 || version != VMMDEV_REQUEST_HEADER_VERSION {
            return;
        }

        let mut rc = VINF_SUCCESS;
        let mut state = self.state.lock().unwrap();

        match request_type {
            VMMDEV_REQ_GET_HOST_VERSION => {
                // Major 7, Minor 2, Build 20, Revision 161567, Features
                let major = 7u16;
                let minor = 2u16;
                let build = 20u32;
                let revision = 161567u32;
                let features = VMMDEV_HVF_FAST_IRQ_ACK | VMMDEV_HVF_HGCM | VMMDEV_HVF_HGCM_CONTIG_PAGE_LIST;

                let _ = mem.write_bytes(gpa as u64 + 24, &major.to_le_bytes());
                let _ = mem.write_bytes(gpa as u64 + 26, &minor.to_le_bytes());
                let _ = mem.write_bytes(gpa as u64 + 28, &build.to_le_bytes());
                let _ = mem.write_bytes(gpa as u64 + 32, &revision.to_le_bytes());
                let _ = mem.write_bytes(gpa as u64 + 36, &features.to_le_bytes());
            }

            VMMDEV_REQ_GET_HOST_TIME => {
                let ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let _ = mem.write_bytes(gpa as u64 + 24, &ms.to_le_bytes());
            }

            VMMDEV_REQ_GET_MOUSE_STATUS => {
                let feat = state.mouse_features;
                let x = state.mouse_x;
                let y = state.mouse_y;
                let _ = mem.write_bytes(gpa as u64 + 24, &feat.to_le_bytes());
                let _ = mem.write_bytes(gpa as u64 + 28, &x.to_le_bytes());
                let _ = mem.write_bytes(gpa as u64 + 32, &y.to_le_bytes());
            }

            VMMDEV_REQ_SET_MOUSE_STATUS => {
                let mut buf = [0u8; 4];
                if mem.read_bytes(gpa as u64 + 24, &mut buf).is_ok() {
                    let feat = u32::from_le_bytes(buf);
                    state.mouse_features = feat | VMMDEV_MOUSE_HOST_WANTS_ABSOLUTE | VMMDEV_MOUSE_HOST_HAS_ABS_DEV;
                }
            }

            VMMDEV_REQ_ACKNOWLEDGE_EVENTS => {
                let pending = state.host_event_flags & state.guest_filter_mask;
                state.host_event_flags &= !state.guest_filter_mask;
                let host_events_bytes = state.host_event_flags.to_le_bytes();
                state.ram[8..12].copy_from_slice(&host_events_bytes);
                state.ram[8] = 0; // fHaveEvents = false
                state.irq_asserted = false;
                let _ = mem.write_bytes(gpa as u64 + 24, &pending.to_le_bytes());
            }

            VMMDEV_REQ_CTL_GUEST_FILTER_MASK => {
                let mut buf = [0u8; 8];
                if mem.read_bytes(gpa as u64 + 24, &mut buf).is_ok() {
                    let or_mask = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let not_mask = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    state.guest_filter_mask = (state.guest_filter_mask | or_mask) & !not_mask;
                }
            }

            VMMDEV_REQ_REPORT_GUEST_INFO => {
                let mut buf = [0u8; 8];
                if mem.read_bytes(gpa as u64 + 24, &mut buf).is_ok() {
                    state.guest_info.interface_version = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    state.guest_info.os_type = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    state.additions_active = true;
                    if state.guest_info.interface_version >= VMMDEV_VERSION_1_03 {
                        state.additions_ok = true;
                    }
                    eprintln!(
                        "[VMMDev] Guest Additions registradas (if_ver={:#x}, os_type={:#x})",
                        state.guest_info.interface_version, state.guest_info.os_type
                    );
                }
            }

            VMMDEV_REQ_REPORT_GUEST_INFO2 => {
                let mut buf = [0u8; 144];
                if mem.read_bytes(gpa as u64 + 24, &mut buf).is_ok() {
                    state.guest_info.major = u16::from_le_bytes([buf[0], buf[1]]);
                    state.guest_info.minor = u16::from_le_bytes([buf[2], buf[3]]);
                    state.guest_info.build = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    state.guest_info.revision = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
                    let name_end = buf[16..144].iter().position(|&c| c == 0).unwrap_or(128);
                    state.guest_info.name = String::from_utf8_lossy(&buf[16..16 + name_end]).to_string();
                    state.additions_active = true;
                    if state.guest_info.major > 1
                        || (state.guest_info.major == 1 && state.guest_info.minor >= 3)
                        || state.guest_info.interface_version >= VMMDEV_VERSION_1_03
                    {
                        state.additions_ok = true;
                    }
                    eprintln!(
                        "[VMMDev] Guest Additions v{}.{}.{} rev {} ('{}') conectadas",
                        state.guest_info.major,
                        state.guest_info.minor,
                        state.guest_info.build,
                        state.guest_info.revision,
                        state.guest_info.name
                    );
                }
            }

            VMMDEV_REQ_REPORT_GUEST_CAPABILITIES => {
                let caps = state.guest_caps;
                let _ = mem.write_bytes(gpa as u64 + 24, &caps.to_le_bytes());
            }

            VMMDEV_REQ_SET_GUEST_CAPABILITIES => {
                let mut buf = [0u8; 8];
                if mem.read_bytes(gpa as u64 + 24, &mut buf).is_ok() {
                    let or_mask = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let not_mask = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    state.guest_caps = (state.guest_caps | or_mask) & !not_mask;
                }
            }

            VMMDEV_REQ_GET_DISPLAY_CHANGE_REQUEST => {
                if let Some((w, h, bpp)) = state.display_change_req.take() {
                    let _ = mem.write_bytes(gpa as u64 + 24, &w.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 28, &h.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 32, &bpp.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 36, &VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST.to_le_bytes());
                } else {
                    let zero = 0u32.to_le_bytes();
                    let _ = mem.write_bytes(gpa as u64 + 24, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 28, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 32, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 36, &zero);
                }
            }

            VMMDEV_REQ_GET_DISPLAY_CHANGE_REQUEST2 => {
                if let Some((w, h, bpp)) = state.display_change_req.take() {
                    let _ = mem.write_bytes(gpa as u64 + 24, &w.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 28, &h.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 32, &bpp.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 36, &VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST.to_le_bytes());
                    let _ = mem.write_bytes(gpa as u64 + 40, &0u32.to_le_bytes()); // display 0
                } else {
                    let zero = 0u32.to_le_bytes();
                    let _ = mem.write_bytes(gpa as u64 + 24, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 28, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 32, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 36, &zero);
                    let _ = mem.write_bytes(gpa as u64 + 40, &zero);
                }
            }

            VMMDEV_REQ_VIDEO_MODE_SUPPORTED => {
                // Siempre responder que el modo de video está soportado
                let _ = mem.write_bytes(gpa as u64 + 36, &1u32.to_le_bytes());
            }

            VMMDEV_REQ_IDLE | VMMDEV_REQ_REPORT_GUEST_STATUS | VMMDEV_REQ_REPORT_GUEST_USER_STATE => {
                // Peticiones no-op que finalizan con éxito
            }

            VMMDEV_REQ_HGCM_CONNECT
            | VMMDEV_REQ_HGCM_DISCONNECT
            | VMMDEV_REQ_HGCM_CALL32
            | VMMDEV_REQ_HGCM_CALL64
            | VMMDEV_REQ_HGCM_CANCEL => {
                // Canal HGCM no configurado: devolver VERR_NOT_SUPPORTED de forma limpia
                rc = VERR_NOT_SUPPORTED;
            }

            _ => {
                // Peticiones no implementadas
                rc = VERR_NOT_IMPLEMENTED;
            }
        }

        // Actualizar código de estado `rc` en la cabecera
        let _ = mem.write_bytes(gpa as u64 + 12, &rc.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vmmdev_initial_state_and_ram() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let _dev = VmmDevDevice::new(state.clone());

        let s = state.lock().unwrap();
        assert_eq!(s.iobase, 0);
        assert_eq!(s.mmio_base, 0);
        assert_eq!(&s.ram[0..4], &(VMMDEV_RAM_SIZE as u32).to_le_bytes());
        assert_eq!(&s.ram[4..8], &VMMDEV_MEMORY_VERSION.to_le_bytes());
        assert_eq!(s.ram[8], 0); // fHaveEvents = false
        assert_eq!(s.mouse_features & VMMDEV_MOUSE_HOST_WANTS_ABSOLUTE, VMMDEV_MOUSE_HOST_WANTS_ABSOLUTE);
    }

    #[test]
    fn test_vmmdev_process_get_host_version() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        let mut raw_mem = vec![0u8; 4096];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let gpa = 0x100u32;
        // Construir VMMDevRequestHeader para VMMDevReq_GetHostVersion
        let size = 40u32;
        let version = VMMDEV_REQUEST_HEADER_VERSION;
        let req_type = VMMDEV_REQ_GET_HOST_VERSION;

        let _ = guest_mem.write_bytes(gpa as u64 + 0, &size.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 4, &version.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 8, &req_type.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 12, &(-1i32).to_le_bytes()); // rc previo

        // Disparar OUT a BAR0 + 0 con el GPA
        dev.write(0xD000, &gpa.to_le_bytes(), Some(&guest_mem));

        // Verificar respuesta
        let mut rc_bytes = [0u8; 4];
        let _ = guest_mem.read_bytes(gpa as u64 + 12, &mut rc_bytes);
        assert_eq!(i32::from_le_bytes(rc_bytes), VINF_SUCCESS);

        let mut major_bytes = [0u8; 2];
        let _ = guest_mem.read_bytes(gpa as u64 + 24, &mut major_bytes);
        assert_eq!(u16::from_le_bytes(major_bytes), 7);

        let mut minor_bytes = [0u8; 2];
        let _ = guest_mem.read_bytes(gpa as u64 + 26, &mut minor_bytes);
        assert_eq!(u16::from_le_bytes(minor_bytes), 2);

        let mut feat_bytes = [0u8; 4];
        let _ = guest_mem.read_bytes(gpa as u64 + 36, &mut feat_bytes);
        assert_eq!(
            u32::from_le_bytes(feat_bytes),
            VMMDEV_HVF_FAST_IRQ_ACK | VMMDEV_HVF_HGCM | VMMDEV_HVF_HGCM_CONTIG_PAGE_LIST
        );
    }

    #[test]
    fn test_vmmdev_process_get_host_time() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        let mut raw_mem = vec![0u8; 4096];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let gpa = 0x200u32;
        let size = 32u32;
        let version = VMMDEV_REQUEST_HEADER_VERSION;
        let req_type = VMMDEV_REQ_GET_HOST_TIME;

        let _ = guest_mem.write_bytes(gpa as u64 + 0, &size.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 4, &version.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 8, &req_type.to_le_bytes());

        dev.write(0xD000, &gpa.to_le_bytes(), Some(&guest_mem));

        let mut time_bytes = [0u8; 8];
        let _ = guest_mem.read_bytes(gpa as u64 + 24, &mut time_bytes);
        let time_ms = u64::from_le_bytes(time_bytes);
        assert!(time_ms > 1_600_000_000_000); // Timestamp contemporáneo válido
    }

    #[test]
    fn test_vmmdev_events_and_fast_irq_ack() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        // Habilitar guest additions y filtrar el evento deseado
        {
            let mut s = state.lock().unwrap();
            s.additions_ok = true;
            s.guest_filter_mask = VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST;
        }

        // Activar un evento
        state.lock().unwrap().raise_event(VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST);
        assert!(state.lock().unwrap().irq_asserted);

        // Verificar layout V1_03 u32HostEvents en ram[8..12]
        let ram_events = {
            let s = state.lock().unwrap();
            u32::from_le_bytes([s.ram[8], s.ram[9], s.ram[10], s.ram[11]])
        };
        assert_eq!(ram_events, VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST);

        // Leer el puerto rápido de acuse (BAR0 + 8)
        let ack_bytes = dev.read(0xD008, 4);
        let ack_val = u32::from_le_bytes([ack_bytes[0], ack_bytes[1], ack_bytes[2], ack_bytes[3]]);
        assert_eq!(ack_val, VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST);

        // Verificar que la IRQ se desasertó y el evento se limpió
        assert!(!state.lock().unwrap().irq_asserted);
        let ram_events_cleared = {
            let s = state.lock().unwrap();
            u32::from_le_bytes([s.ram[8], s.ram[9], s.ram[10], s.ram[11]])
        };
        assert_eq!(ram_events_cleared, 0);
    }

    #[test]
    fn test_vmmdev_fast_ack_without_additions_ok() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        state.lock().unwrap().guest_filter_mask = VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST;
        state.lock().unwrap().raise_event(VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST);
        assert!(state.lock().unwrap().irq_asserted);

        // additions_ok es false -> debe retornar 0xFFFFFFFF y no alterar el estado
        let ack_bytes = dev.read(0xD008, 4);
        assert_eq!(u32::from_le_bytes([ack_bytes[0], ack_bytes[1], ack_bytes[2], ack_bytes[3]]), u32::MAX);
        assert!(state.lock().unwrap().irq_asserted);
        assert_ne!(state.lock().unwrap().host_event_flags, 0);
    }

    #[test]
    fn test_vmmdev_fast_ack_count_not_four_no_state_leak() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        {
            let mut s = state.lock().unwrap();
            s.additions_ok = true;
            s.guest_filter_mask = VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST;
        }
        state.lock().unwrap().raise_event(VMMDEV_EVENT_DISPLAY_CHANGE_REQUEST);
        assert!(state.lock().unwrap().irq_asserted);

        // Lectura de 2 bytes (count != 4): debe retornar 0xFFFF sin alterar el estado
        let ack_bytes = dev.read(0xD008, 2);
        assert_eq!(ack_bytes, vec![0xFF, 0xFF]);
        assert!(state.lock().unwrap().irq_asserted);
        assert_ne!(state.lock().unwrap().host_event_flags, 0);

        // Lectura de 1 byte
        let ack_bytes_1 = dev.read(0xD008, 1);
        assert_eq!(ack_bytes_1, vec![0xFF]);
        assert!(state.lock().unwrap().irq_asserted);
    }

    #[test]
    fn test_vmmdev_reset_clears_guest_filter_mask() {
        let mut state = VmmDevState::new();
        state.guest_filter_mask = 0x1234_5678;
        state.additions_ok = true;
        state.reset();

        assert_eq!(state.guest_filter_mask, 0);
        assert!(!state.additions_ok);
    }

    #[test]
    fn test_vmmdev_unknown_request_returns_not_implemented() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        let mut raw_mem = vec![0u8; 4096];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let gpa = 0x300u32;
        let size = 24u32;
        let version = VMMDEV_REQUEST_HEADER_VERSION;
        let unknown_req_type = 99999u32;

        let _ = guest_mem.write_bytes(gpa as u64 + 0, &size.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 4, &version.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 8, &unknown_req_type.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 12, &0i32.to_le_bytes());

        dev.write(0xD000, &gpa.to_le_bytes(), Some(&guest_mem));

        let mut rc_bytes = [0u8; 4];
        let _ = guest_mem.read_bytes(gpa as u64 + 12, &mut rc_bytes);
        assert_eq!(i32::from_le_bytes(rc_bytes), VERR_NOT_IMPLEMENTED);
    }

    #[test]
    fn test_vmmdev_guest_info_version_activation() {
        let state = Arc::new(Mutex::new(VmmDevState::new()));
        let mut dev = VmmDevDevice::new(state.clone());
        dev.set_iobase(0xD000);

        let mut raw_mem = vec![0u8; 4096];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        // 1. ReportGuestInfo con version antigua < 1.03 (0x00010000)
        let gpa = 0x400u32;
        let size = 32u32;
        let version = VMMDEV_REQUEST_HEADER_VERSION;
        let req_type = VMMDEV_REQ_REPORT_GUEST_INFO;
        let old_if_ver = 0x0001_0000u32;
        let os_type = 0x100u32;

        let _ = guest_mem.write_bytes(gpa as u64 + 0, &size.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 4, &version.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 8, &req_type.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 24, &old_if_ver.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa as u64 + 28, &os_type.to_le_bytes());

        dev.write(0xD000, &gpa.to_le_bytes(), Some(&guest_mem));
        assert!(!state.lock().unwrap().additions_ok);

        // 2. ReportGuestInfo con version >= 1.03 (0x00010003)
        let new_if_ver = VMMDEV_VERSION_1_03;
        let _ = guest_mem.write_bytes(gpa as u64 + 24, &new_if_ver.to_le_bytes());
        dev.write(0xD000, &gpa.to_le_bytes(), Some(&guest_mem));
        assert!(state.lock().unwrap().additions_ok);

        // 3. Reset y probar ReportGuestInfo2
        state.lock().unwrap().reset();
        assert!(!state.lock().unwrap().additions_ok);

        let gpa2 = 0x500u32;
        let size2 = 168u32;
        let req_type2 = VMMDEV_REQ_REPORT_GUEST_INFO2;
        let _ = guest_mem.write_bytes(gpa2 as u64 + 0, &size2.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa2 as u64 + 4, &version.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa2 as u64 + 8, &req_type2.to_le_bytes());

        // major = 1, minor = 2 (< 1.03)
        let _ = guest_mem.write_bytes(gpa2 as u64 + 24, &1u16.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa2 as u64 + 26, &2u16.to_le_bytes());
        dev.write(0xD000, &gpa2.to_le_bytes(), Some(&guest_mem));
        assert!(!state.lock().unwrap().additions_ok);

        // major = 7, minor = 2 (>= 1.03)
        let _ = guest_mem.write_bytes(gpa2 as u64 + 24, &7u16.to_le_bytes());
        let _ = guest_mem.write_bytes(gpa2 as u64 + 26, &2u16.to_le_bytes());
        dev.write(0xD000, &gpa2.to_le_bytes(), Some(&guest_mem));
        assert!(state.lock().unwrap().additions_ok);
    }
}
