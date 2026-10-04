//! Emulación del controlador USB UHCI (Intel 82801) del PIIX3 y despacho de DMA.
//!
//! SeaBIOS y el kernel de Linux inicializan el UHCI siguiendo este flujo:
//! 1. SeaBIOS busca dispositivo PCI con class 0C/03 (USB UHCI) en 00:01.2.
//! 2. Lee BAR4 para obtener el I/O base y habilita Bus Mastering.
//! 3. Resetea el host controller (USBCMD=HCRESET).
//! 4. Configura Frame List Base (USBFLBASEADD) con tabla de 1024 punteros físicos.
//! 5. Inicia el scheduler: USBCMD = RS (Run).
//! 6. Detecta dispositivo en el Puerto 1 (USBPORTSC1 con CCS=1).
//! 7. Resetea el Puerto 1 (PR=1, luego PR=0) -> puerto habilitado (PE=1).
//! 8. El kernel Linux (uhci-hcd + usbhid) despacha peticiones mediante
//!    Queue Heads (QH) y Transfer Descriptors (TD) en memoria física del guest.
//! 9. El endpoint 1 (Interrupt IN) reporta eventos de entrada absolutos (0..32767)
//!    de la tableta gráfica USB (QEMU USB Tablet) sin captura ni desfase de cursor.

use super::IoDevice;
use super::usb_tablet::UsbTablet;
use crate::guest_mem::GuestMemory;
use std::sync::{Arc, Mutex};

/// Registros UHCI relativos al I/O base
const USBCMD: u16 = 0x00;
const USBSTS: u16 = 0x02;
const USBINTR: u16 = 0x04;
const USBFRNUM: u16 = 0x06;
const USBFLBASEADD: u16 = 0x08;
const USBFLBASEADD_HI: u16 = 0x0A;
const USBSOF: u16 = 0x0C;
const USBPORTSC1: u16 = 0x10;
const USBPORTSC2: u16 = 0x12;

/// Bits USBCMD
pub const USBCMD_RS: u16 = 0x0001;
pub const USBCMD_HCRESET: u16 = 0x0002;

/// Bits USBSTS
pub const USBSTS_USBINT: u16 = 0x0001;
pub const USBSTS_ERROR: u16 = 0x0002;
pub const USBSTS_HCH: u16 = 0x0020;

/// Bits USBPORTSC
pub const USBPORTSC_CCS: u16 = 0x0001;  // Current Connect Status
pub const USBPORTSC_CSC: u16 = 0x0002;  // Connect Status Change (R/WC)
pub const USBPORTSC_PE: u16 = 0x0004;   // Port Enable
pub const USBPORTSC_PEC: u16 = 0x0008;  // Port Enable Change (R/WC)
pub const USBPORTSC_RD: u16 = 0x0080;   // Resume Detect (siempre 1 en PIIX3)
pub const USBPORTSC_PR: u16 = 0x0200;   // Port Reset

/// Estado interno del controlador UHCI
#[derive(Debug, Clone)]
pub struct UhciState {
    pub usb_cmd: u16,
    pub usb_sts: u16,
    pub usb_intr: u16,
    pub usb_frnum: u16,
    pub usb_flbase: u32,
    pub usb_sof: u8,
    pub iobase: u16,
    pub port_status: [u16; 2],
    pub tablet: UsbTablet,
    pub irq_pulse: bool,
}

impl Default for UhciState {
    fn default() -> Self {
        Self {
            usb_cmd: 0,
            usb_sts: USBSTS_HCH,
            usb_intr: 0,
            usb_frnum: 0,
            usb_flbase: 0,
            usb_sof: 64,
            iobase: 0,
            // Puerto 1: UsbTablet conectada inicialmente
            port_status: [USBPORTSC_CCS | USBPORTSC_CSC | USBPORTSC_RD, USBPORTSC_RD],
            tablet: UsbTablet::new(),
            irq_pulse: false,
        }
    }
}

impl UhciState {
    /// Inyecta un evento de ratón absoluto (X, Y en 0..32767, botones en bits 0..2, rueda).
    pub fn inject_tablet_event(&mut self, x: u16, y: u16, buttons: u8, wheel: i8) {
        self.tablet.inject_event(x, y, buttons, wheel);
    }

    /// Comprueba si la línea de interrupción del UHCI está activa.
    pub fn is_irq_asserted(&self) -> bool {
        let ioc_enabled = (self.usb_intr & 0x0004) != 0;
        let int_status = (self.usb_sts & USBSTS_USBINT) != 0;
        let err_enabled = (self.usb_intr & 0x0001) != 0;
        let err_status = (self.usb_sts & USBSTS_ERROR) != 0;
        (ioc_enabled && int_status) || (err_enabled && err_status)
    }

    /// Extrae el flag de pulso de interrupción (para generar flancos frescos en KVM).
    pub fn take_irq_pulse(&mut self) -> bool {
        let p = self.irq_pulse;
        self.irq_pulse = false;
        p
    }

    /// Procesa un Transfer Descriptor (TD) individual.
    /// Devuelve `(siguiente_enlace, sigue_activo)`.
    fn process_td(&mut self, mem: &GuestMemory, td_addr: usize) -> (u32, bool) {
        let link = mem.read_u32(td_addr);
        let mut status = mem.read_u32(td_addr + 4);
        let token = mem.read_u32(td_addr + 8);
        let buffer = mem.read_u32(td_addr + 12);

        // Si no está activo (bit 23 == 0), ya fue procesado
        if status & 0x0080_0000 == 0 {
            return (link, false);
        }

        // Si el puerto 1 no está habilitado (PE == 0), no podemos comunicarnos con el dispositivo
        if (self.port_status[0] & USBPORTSC_PE) == 0 {
            return (link, true);
        }

        let dev_addr = ((token >> 8) & 0x7F) as u8;
        // Si no coincide la dirección del dispositivo con la de nuestra tableta, ignorar
        if dev_addr != self.tablet.address && (self.tablet.pending_address == 0 || dev_addr != 0) {
            return (link, true);
        }

        let pid = (token & 0xFF) as u8;
        let endpoint = ((token >> 15) & 0x0F) as u8;
        let max_len_raw = (token >> 21) & 0x7FF;
        let max_len = if max_len_raw == 0x7FF { 0 } else { (max_len_raw + 1) as usize };

        match pid {
            0x2D => {
                // SETUP PID (siempre 8 bytes)
                let mut setup_bytes = [0u8; 8];
                mem.copy_from(buffer as usize, &mut setup_bytes);
                self.tablet.handle_setup(&setup_bytes);

                // TD completado: limpiar bit activo (bit 23) y escribir longitud real (8 bytes -> actlen 7)
                status = (status & !0x0080_07FF) | 7;
                mem.write_u32(td_addr + 4, status);

                if status & 0x0100_0000 != 0 {
                    // IOC (Interrupt On Complete)
                    self.usb_sts |= USBSTS_USBINT;
                    self.irq_pulse = true;
                }
                (link, false)
            }
            0x69 => {
                // IN PID (transferencia hacia el host)
                if let Some(data) = self.tablet.handle_in(endpoint, max_len) {
                    let actual_len = data.len();
                    if actual_len > 0 {
                        mem.copy_to(buffer as usize, &data);
                    }
                    let actlen_field = if actual_len == 0 {
                        0x7FF
                    } else {
                        ((actual_len - 1) & 0x7FF) as u32
                    };
                    status = (status & !0x0080_07FF) | actlen_field;
                    mem.write_u32(td_addr + 4, status);

                    if status & 0x0100_0000 != 0 {
                        // IOC
                        self.usb_sts |= USBSTS_USBINT;
                        self.irq_pulse = true;
                    }
                    (link, false)
                } else {
                    // NAK: el dispositivo no tiene datos preparados (p. ej. EP 1 sin eventos pendientes)
                    // El TD sigue activo en memoria y no avanzamos la cola este frame
                    (link, true)
                }
            }
            0xE1 => {
                // OUT PID (transferencia desde el host)
                if max_len > 0 {
                    let mut out_data = vec![0u8; max_len];
                    mem.copy_from(buffer as usize, &mut out_data);
                    self.tablet.handle_out(endpoint, &out_data);
                } else {
                    self.tablet.handle_out(endpoint, &[]);
                }
                let actlen_field = if max_len == 0 {
                    0x7FF
                } else {
                    ((max_len - 1) & 0x7FF) as u32
                };
                status = (status & !0x0080_07FF) | actlen_field;
                mem.write_u32(td_addr + 4, status);

                if status & 0x0100_0000 != 0 {
                    // IOC
                    self.usb_sts |= USBSTS_USBINT;
                    self.irq_pulse = true;
                }
                (link, false)
            }
            _ => (link, false),
        }
    }

    /// Ejecuta un tick de 1 ms del scheduler UHCI (recorre Frame List, Queue Heads y TDs).
    pub fn step(&mut self, mem: &GuestMemory) {
        // Solo procesar si el controlador está en marcha (Run/Stop bit = 1)
        if (self.usb_cmd & USBCMD_RS) == 0 {
            return;
        }

        // Incrementar contador de frames (0..1023)
        self.usb_frnum = (self.usb_frnum + 1) & 0x7FF;
        let frame_idx = (self.usb_frnum & 0x3FF) as usize;

        let flbase = (self.usb_flbase & 0xFFFF_F000) as usize;
        if flbase == 0 {
            return;
        }

        let entry_addr = flbase + frame_idx * 4;
        let mut link = mem.read_u32(entry_addr);

        // Límite horizontal para evitar bucles infinitos en punteros circulares
        let mut horiz_limit = 128;
        while (link & 1) == 0 && horiz_limit > 0 {
            horiz_limit -= 1;
            let target_addr = (link & 0xFFFF_FFF0) as usize;
            if target_addr == 0 {
                break;
            }

            if (link & 2) != 0 {
                // Queue Head (QH)
                let horizontal_link = mem.read_u32(target_addr);
                let mut elem_link = mem.read_u32(target_addr + 4);

                // Recorrer los TDs en el elemento vertical de la cola
                let mut vert_limit = 64;
                while (elem_link & 1) == 0 && vert_limit > 0 {
                    vert_limit -= 1;
                    let elem_addr = (elem_link & 0xFFFF_FFF0) as usize;
                    if elem_addr == 0 {
                        break;
                    }

                    if (elem_link & 2) != 0 {
                        // Enlace a otra QH en vertical no soportado, salir
                        break;
                    }

                    let (next_link, still_active) = self.process_td(mem, elem_addr);
                    if still_active {
                        // El TD no completó (p. ej. NAK), detener procesamiento en esta cola
                        break;
                    }
                    // Actualizar element link de la QH para avanzar la cola
                    mem.write_u32(target_addr + 4, next_link);
                    elem_link = next_link;
                }

                link = horizontal_link;
            } else {
                // Transfer Descriptor (TD) directo en la lista de frames
                let (next_link, still_active) = self.process_td(mem, target_addr);
                if still_active {
                    break;
                }
                link = next_link;
            }
        }
    }
}

/// Emulación del controlador USB UHCI del PIIX3.
/// Se comparte entre el PciBus (que asigna el I/O base via BAR4) y
/// el DeviceBus (que despacha los accesos I/O).
pub struct UsbUhci {
    pub state: Arc<Mutex<UhciState>>,
}

impl UsbUhci {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(UhciState::default())),
        }
    }

    /// Notificado por PciBus cuando SeaBIOS escribe al BAR4 del dispositivo USB.
    #[allow(dead_code)]
    pub fn set_iobase(&self, base: u16) {
        let mut s = self.state.lock().unwrap();
        s.iobase = base;
    }

    /// Reset del controlador (reset del chipset): vuelve al estado inicial.
    pub fn reset(&self) {
        let mut s = self.state.lock().unwrap();
        *s = UhciState::default();
    }

    /// Devuelve el I/O base actual (0 = no asignado).
    #[allow(dead_code)]
    pub fn iobase(&self) -> u16 {
        self.state.lock().unwrap().iobase
    }

    /// Ejecuta un tick del scheduler UHCI.
    pub fn step(&self, mem: &GuestMemory) {
        let mut s = self.state.lock().unwrap();
        s.step(mem);
    }

    /// Comprueba si la interrupción USB está activa.
    pub fn is_irq_asserted(&self) -> bool {
        let s = self.state.lock().unwrap();
        s.is_irq_asserted()
    }

    /// Consume el pulso de interrupción pendiente.
    pub fn take_irq_pulse(&self) -> bool {
        let mut s = self.state.lock().unwrap();
        s.take_irq_pulse()
    }

    /// Inyecta un evento en la tableta USB.
    pub fn inject_tablet_event(&self, x: u16, y: u16, buttons: u8, wheel: i8) {
        let mut s = self.state.lock().unwrap();
        s.inject_tablet_event(x, y, buttons, wheel);
    }
}

impl Default for UsbUhci {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for UsbUhci {
    fn matches_port(&self, port: u16) -> bool {
        let s = self.state.lock().unwrap();
        if s.iobase == 0 {
            return false;
        }
        port >= s.iobase && port < s.iobase + 0x20
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let mut s = self.state.lock().unwrap();
        let off = port - s.iobase;
        let val = if data.len() >= 2 {
            u16::from_le_bytes([data[0], data[1]])
        } else {
            data[0] as u16
        };

        match off {
            USBCMD => {
                if val & USBCMD_HCRESET != 0 {
                    s.usb_cmd = 0;
                    s.usb_sts = USBSTS_HCH;
                    s.usb_intr = 0;
                    s.usb_frnum = 0;
                    s.usb_flbase = 0;
                    s.usb_sof = 64;
                    s.port_status = [USBPORTSC_CCS | USBPORTSC_CSC | USBPORTSC_RD, USBPORTSC_RD];
                    s.tablet.reset();
                    s.irq_pulse = false;
                } else {
                    s.usb_cmd = val;
                    if val & USBCMD_RS != 0 {
                        s.usb_sts &= !USBSTS_HCH;
                    } else {
                        s.usb_sts |= USBSTS_HCH;
                    }
                }
            }
            USBSTS => {
                // Bits 0..5 son R/WC (Write 1 to clear)
                s.usb_sts &= !(val & 0x003F);
                if s.usb_cmd & USBCMD_RS == 0 {
                    s.usb_sts |= USBSTS_HCH;
                } else {
                    s.usb_sts &= !USBSTS_HCH;
                }
            }
            USBINTR => { s.usb_intr = val; }
            USBFRNUM => { s.usb_frnum = val; }
            USBFLBASEADD => {
                if data.len() >= 2 {
                    let low = u16::from_le_bytes([data[0], data.get(1).copied().unwrap_or(0)]);
                    s.usb_flbase = (s.usb_flbase & 0xFFFF0000) | low as u32;
                }
            }
            USBFLBASEADD_HI => {
                let high = val;
                s.usb_flbase = (s.usb_flbase & 0x0000FFFF) | ((high as u32) << 16);
            }
            USBSOF => { s.usb_sof = val as u8; }
            USBPORTSC1 => {
                // Bits R/WC: CSC (bit 1), PEC (bit 3)
                if val & USBPORTSC_CSC != 0 {
                    s.port_status[0] &= !USBPORTSC_CSC;
                }
                if val & USBPORTSC_PEC != 0 {
                    s.port_status[0] &= !USBPORTSC_PEC;
                }

                let was_reset = (s.port_status[0] & USBPORTSC_PR) != 0;
                let now_reset = (val & USBPORTSC_PR) != 0;

                if now_reset {
                    s.port_status[0] |= USBPORTSC_PR;
                    s.port_status[0] &= !USBPORTSC_PE;
                } else if was_reset && !now_reset {
                    // Secuencia de reset completada -> Puerto habilitado y PEC activado
                    s.port_status[0] &= !USBPORTSC_PR;
                    s.port_status[0] |= USBPORTSC_PE | USBPORTSC_PEC;
                    s.tablet.reset();
                    eprintln!("[UHCI] Port 1 reset completado, UsbTablet habilitado (PE=1)");
                } else if val & USBPORTSC_PE != 0 {
                    s.port_status[0] |= USBPORTSC_PE;
                } else {
                    s.port_status[0] &= !USBPORTSC_PE;
                }

                // Preservar siempre CCS (1) y RD (0x0080)
                s.port_status[0] = (s.port_status[0] & !0x0081) | USBPORTSC_RD | USBPORTSC_CCS;
            }
            USBPORTSC2 => {
                if val & USBPORTSC_CSC != 0 {
                    s.port_status[1] &= !USBPORTSC_CSC;
                }
                if val & USBPORTSC_PEC != 0 {
                    s.port_status[1] &= !USBPORTSC_PEC;
                }
                // Puerto 2 sin dispositivo
                s.port_status[1] = (s.port_status[1] & 0x0200) | USBPORTSC_RD;
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let s = self.state.lock().unwrap();
        let off = port - s.iobase;
        let result = match off {
            USBCMD => s.usb_cmd.to_le_bytes(),
            USBSTS => {
                let mut sts = s.usb_sts;
                if s.usb_cmd & USBCMD_RS != 0 {
                    sts &= !USBSTS_HCH;
                }
                sts.to_le_bytes()
            }
            USBINTR => s.usb_intr.to_le_bytes(),
            USBFRNUM => s.usb_frnum.to_le_bytes(),
            USBFLBASEADD => {
                let bytes = s.usb_flbase.to_le_bytes();
                [bytes[0], bytes[1]]
            }
            USBFLBASEADD_HI => {
                let bytes = s.usb_flbase.to_le_bytes();
                [bytes[2], bytes[3]]
            }
            USBSOF => [s.usb_sof, 0],
            USBPORTSC1 => s.port_status[0].to_le_bytes(),
            USBPORTSC2 => s.port_status[1].to_le_bytes(),
            _ => [0xFFu8; 2],
        };
        result.iter().take(count).copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uhci_start_stop_cycle() {
        let mut dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // Estado inicial: halted
        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH != 0, "Should be halted initially");

        // Iniciar el controlador
        dev.write(0x1F00, &[0x01, 0x00]); // USBCMD = RS
        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH == 0, "Should not be halted when running");

        // Detener el controlador
        dev.write(0x1F00, &[0x00, 0x00]); // USBCMD = 0
        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH != 0, "Should be halted when stopped");
    }

    #[test]
    fn uhci_port_device_connection() {
        let mut dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // Puerto 1 debe reportar tableta conectada (CCS=1)
        let ps1 = u16::from_le_bytes([dev.read(0x1F10, 2)[0], dev.read(0x1F10, 2)[1]]);
        assert!(ps1 & USBPORTSC_CCS != 0, "Port 1 should have UsbTablet connected");

        // Puerto 2 debe reportar sin dispositivo (CCS=0)
        let ps2 = u16::from_le_bytes([dev.read(0x1F12, 2)[0], dev.read(0x1F12, 2)[1]]);
        assert!(ps2 & USBPORTSC_CCS == 0, "Port 2 should have no device");
    }

    #[test]
    fn uhci_port_reset_sequence() {
        let mut dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // 1. Activar Port Reset (PR = 1)
        dev.write(0x1F10, &[0x00, 0x02]); // 0x0200
        let ps1 = u16::from_le_bytes([dev.read(0x1F10, 2)[0], dev.read(0x1F10, 2)[1]]);
        assert!(ps1 & USBPORTSC_PR != 0, "Port 1 should be in reset");
        assert!(ps1 & USBPORTSC_PE == 0, "Port 1 should be disabled during reset");

        // 2. Liberar Port Reset (PR = 0)
        dev.write(0x1F10, &[0x00, 0x00]);
        let ps1 = u16::from_le_bytes([dev.read(0x1F10, 2)[0], dev.read(0x1F10, 2)[1]]);
        assert!(ps1 & USBPORTSC_PR == 0, "Port 1 reset should be released");
        assert!(ps1 & USBPORTSC_PE != 0, "Port 1 should be enabled after reset");
        assert!(ps1 & USBPORTSC_PEC != 0, "Port 1 PEC should be set after reset");
    }

    #[test]
    fn uhci_dma_control_transfer() {
        let mut mem_backing = vec![0u8; 0x10000];
        let guest_mem = GuestMemory::new(mem_backing.as_mut_ptr(), mem_backing.len());

        let dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // Habilitar puerto 1 (reset sequence)
        let mut dev_io: Box<dyn IoDevice> = Box::new(dev);
        dev_io.write(0x1F10, &[0x00, 0x02]); // PR = 1
        dev_io.write(0x1F10, &[0x00, 0x00]); // PR = 0 -> PE = 1

        // Iniciar UHCI
        dev_io.write(0x1F00, &[0x01, 0x00]); // USBCMD = RS
        dev_io.write(0x1F04, &[0x04, 0x00]); // USBINTR = IOC enable

        // Configurar Frame List en GPA 0x1000
        dev_io.write(0x1F08, &[0x00, 0x10]); // FLBASE low
        dev_io.write(0x1F0A, &[0x00, 0x00]); // FLBASE hi

        // Frame List entry 0 -> Queue Head en GPA 0x2000 (bit 1 = QH)
        guest_mem.write_u32(0x1000, 0x2002);

        // QH en 0x2000:
        // link = Terminate (0x01)
        // element = TD1 en 0x3000 (bit 1 = 0 -> TD)
        guest_mem.write_u32(0x2000, 0x0001);
        guest_mem.write_u32(0x2004, 0x3000);

        // TD1 en 0x3000: SETUP packet para GET_DESCRIPTOR (Device, 18 bytes)
        // setup_packet en 0x4000: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00]
        let setup_data = [0x80u8, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];
        guest_mem.copy_to(0x4000, &setup_data);

        // TD1 fields:
        // link -> TD2 en 0x3010
        guest_mem.write_u32(0x3000, 0x3010);
        // status = Active (0x0080_0000)
        guest_mem.write_u32(0x3004, 0x0080_0000);
        // token: PID=0x2D (SETUP), dev=0, ep=0, max_len=8 (encoded 7 << 21)
        guest_mem.write_u32(0x3008, 0x2D | (7 << 21));
        // buffer = 0x4000
        guest_mem.write_u32(0x300C, 0x4000);

        // TD2 en 0x3010: IN packet para recibir los 18 bytes en 0x5000
        // link = Terminate (0x01)
        guest_mem.write_u32(0x3010, 0x0001);
        // status = Active + IOC (0x0180_0000)
        guest_mem.write_u32(0x3014, 0x0180_0000);
        // token: PID=0x69 (IN), dev=0, ep=0, max_len=18 (encoded 17 << 21)
        guest_mem.write_u32(0x3018, 0x69 | (17 << 21));
        // buffer = 0x5000
        guest_mem.write_u32(0x301C, 0x5000);

        // Ejecutar frame UHCI
        // Se ejecuta a través del estado interno
        // Nota: dev_io es UsbUhci
        // Re-creamos referencia o invocamos step
        // Recuperamos UsbUhci
        drop(dev_io);
        let mut dev2 = UsbUhci::new();
        dev2.set_iobase(0x1F00);
        dev2.write(0x1F10, &[0x00, 0x02]);
        dev2.write(0x1F10, &[0x00, 0x00]);
        dev2.write(0x1F00, &[0x01, 0x00]);
        dev2.write(0x1F04, &[0x04, 0x00]);
        dev2.write(0x1F08, &[0x00, 0x10]);
        dev2.write(0x1F0A, &[0x00, 0x00]);

        // Asegurar que el frame actual es 0
        {
            let mut s = dev2.state.lock().unwrap();
            s.usb_frnum = 0x7FF; // Al hacer step incrementará a 0
        }

        dev2.step(&guest_mem);

        // Verificar que TD1 se completó (inactivo)
        let td1_status = guest_mem.read_u32(0x3004);
        assert_eq!(td1_status & 0x0080_0000, 0, "TD1 should be inactive");

        // Verificar que TD2 se completó (inactivo)
        let td2_status = guest_mem.read_u32(0x3014);
        assert_eq!(td2_status & 0x0080_0000, 0, "TD2 should be inactive");

        // Verificar datos recibidos en 0x5000: Device Descriptor (18 bytes, bLength=18, bDescriptorType=1)
        let b_length = guest_mem.read_u8(0x5000);
        let b_type = guest_mem.read_u8(0x5001);
        assert_eq!(b_length, 18, "Device descriptor length");
        assert_eq!(b_type, 0x01, "Device descriptor type");

        // Verificar interrupción IOC generada
        assert!(dev2.is_irq_asserted(), "IRQ should be asserted by IOC");
    }

    #[test]
    fn uhci_dma_interrupt_in_events() {
        let mut mem_backing = vec![0u8; 0x10000];
        let guest_mem = GuestMemory::new(mem_backing.as_mut_ptr(), mem_backing.len());

        let dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // Habilitar puerto 1
        dev.state.lock().unwrap().port_status[0] = USBPORTSC_CCS | USBPORTSC_PE | USBPORTSC_RD;
        dev.state.lock().unwrap().usb_cmd = USBCMD_RS;
        dev.state.lock().unwrap().usb_intr = 0x0004; // IOC
        dev.state.lock().unwrap().usb_flbase = 0x1000;
        dev.state.lock().unwrap().usb_frnum = 0x7FF;

        // Frame List entry 0 -> TD en 0x3000 (direct TD, bit 1 = 0)
        guest_mem.write_u32(0x1000, 0x3000);

        // TD en 0x3000: Interrupt IN en EP 1, max 8 bytes
        guest_mem.write_u32(0x3000, 0x0001); // Terminate
        guest_mem.write_u32(0x3004, 0x0180_0000); // Active + IOC
        // PID=0x69 (IN), dev=0, ep=1, max_len=8 (encoded 7 << 21)
        guest_mem.write_u32(0x3008, 0x69 | (1 << 15) | (7 << 21));
        guest_mem.write_u32(0x300C, 0x5000);

        // 1. Step sin eventos -> Debe hacer NAK y el TD debe seguir activo
        dev.step(&guest_mem);
        let td_status = guest_mem.read_u32(0x3004);
        assert_ne!(td_status & 0x0080_0000, 0, "TD should remain active on NAK");
        assert!(!dev.is_irq_asserted(), "No IRQ on NAK");

        // 2. Inyectar evento de tableta (X=12345, Y=23456, Botón 1)
        dev.inject_tablet_event(12345, 23456, 1, 0);

        // Restablecer frame counter a 0x7FF para que apunte de nuevo al frame 0
        dev.state.lock().unwrap().usb_frnum = 0x7FF;

        // 3. Step con evento pendiente -> TD debe completarse
        dev.step(&guest_mem);
        let td_status = guest_mem.read_u32(0x3004);
        assert_eq!(td_status & 0x0080_0000, 0, "TD should be inactive after report transfer");
        assert!(dev.is_irq_asserted(), "IRQ should be asserted after report transfer");

        // Verificar reporte en memoria 0x5000 (6 bytes: buttons, x_lo, x_hi, y_lo, y_hi, wheel)
        let mut report = [0u8; 6];
        guest_mem.copy_from(0x5000, &mut report);
        assert_eq!(report[0], 1); // Botón 1
        assert_eq!(u16::from_le_bytes([report[1], report[2]]), 12345);
        assert_eq!(u16::from_le_bytes([report[3], report[4]]), 23456);
        assert_eq!(report[5], 0);
    }
}
