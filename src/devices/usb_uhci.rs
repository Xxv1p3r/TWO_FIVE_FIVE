//! Emulación mínima del controlador USB UHCI (Intel 82801) del PIIX3.
//!
//! SeaBIOS inicializa el UHCI siguiendo este flujo:
//! 1. Busca PCI device con class 0C/03 (USB UHCI)
//! 2. Lee BAR4 para obtener el I/O base
//! 3. Habilita bus mastering (PCI command register)
//! 4. Reset: escribe USBLEGSUP(0xC0)=RWC, luego USBCMD=HCRESET, USBINTR=0, USBCMD=0
//! 5. Configura: alloca framelist, escribe USBSOF=64, USBFLBASEADD=framelist, USBFRNUM=0
//! 6. Inicia: USBCMD = RS | CF | MAXP
//! 7. Escanea puertos: USBPORTSC1/2 para detectar dispositivos
//! 8. Si no hay dispositivos, apaga el controlador (USBCMD=0)
//!
//! Para que SeaBIOS continúe el boot sin USB real, basta con:
//! - Aceptar todas las escrituras de registros (no hacer nada)
//! - Devolver valores "no device connected" en puertos raíz
//! - USBSTS con HCHalted=1 inicialmente, luego 0 cuando se inicia

use super::IoDevice;
use std::sync::{Arc, Mutex};

/// Registros UHCI relativos al I/O base
#[allow(dead_code)]
const USBCMD: u16 = 0x00;
const USBSTS: u16 = 0x02;
#[allow(dead_code)]
const USBINTR: u16 = 0x04;
#[allow(dead_code)]
const USBFRNUM: u16 = 0x06;
#[allow(dead_code)]
const USBFLBASEADD: u16 = 0x08;
const USBFLBASEADD_HI: u16 = 0x0A;
#[allow(dead_code)]
const USBSOF: u16 = 0x0C;
const USBPORTSC1: u16 = 0x10;
const USBPORTSC2: u16 = 0x12;

/// Bits USBCMD
const USBCMD_RS: u16 = 0x0001;
#[allow(dead_code)]
const USBCMD_HCRESET: u16 = 0x0002;

/// Bits USBSTS
const USBSTS_HCH: u16 = 0x0020;

/// Bits USBPORTSC
#[allow(dead_code)]
const USBPORTSC_CCS: u16 = 0x0001;
const USBPORTSC_RD: u16 = 0x0080;

/// Estado interno del controlador UHCI
#[derive(Debug, Clone)]
pub struct UhciState {
    usb_cmd: u16,
    usb_sts: u16,
    usb_intr: u16,
    usb_frnum: u16,
    usb_flbase: u32,
    usb_sof: u8,
    pub iobase: u16,
    port_status: [u16; 2],
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
            port_status: [USBPORTSC_RD, USBPORTSC_RD],
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
    pub fn set_iobase(&self, base: u16) {
        let mut s = self.state.lock().unwrap();
        s.iobase = base;
    }

    /// Devuelve el I/O base actual (0 = no asignado)
    pub fn iobase(&self) -> u16 {
        self.state.lock().unwrap().iobase
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
        let val = data[0] as u16;

        match off {
            USBCMD => {
                if val & USBCMD_HCRESET != 0 {
                    s.usb_cmd = 0;
                    s.usb_sts = USBSTS_HCH;
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
                s.usb_sts = (s.usb_sts & !val) | USBSTS_HCH;
                if s.usb_cmd & USBCMD_RS == 0 {
                    s.usb_sts |= USBSTS_HCH;
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
            USBPORTSC1 => { s.port_status[0] = (val & !0x02) | USBPORTSC_RD; }
            USBPORTSC2 => { s.port_status[1] = (val & !0x02) | USBPORTSC_RD; }
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

        // Initial state: halted
        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH != 0, "Should be halted initially");

        // Start the controller
        dev.write(0x1F00, &[0x01, 0x00]); // USBCMD = RS
        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH == 0, "Should not be halted when running");

        // Stop the controller
        dev.write(0x1F00, &[0x00, 0x00]); // USBCMD = 0
        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH != 0, "Should be halted when stopped");
    }

    #[test]
    fn uhci_no_device_connected() {
        let mut dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // Port 1 should report no device (CCS=0)
        let ps1 = u16::from_le_bytes([dev.read(0x1F10, 2)[0], dev.read(0x1F10, 2)[1]]);
        assert!(ps1 & USBPORTSC_CCS == 0, "Port 1 should have no device");

        let ps2 = u16::from_le_bytes([dev.read(0x1F12, 2)[0], dev.read(0x1F12, 2)[1]]);
        assert!(ps2 & USBPORTSC_CCS == 0, "Port 2 should have no device");
    }

    #[test]
    fn uhci_host_reset() {
        let mut dev = UsbUhci::new();
        dev.set_iobase(0x1F00);

        // Start controller
        dev.write(0x1F00, &[0x01, 0x00]);

        // Issue host reset
        dev.write(0x1F00, &[0x02, 0x00]); // USBCMD_HCRESET

        let sts = u16::from_le_bytes([dev.read(0x1F02, 2)[0], dev.read(0x1F02, 2)[1]]);
        assert!(sts & USBSTS_HCH != 0, "Should be halted after host reset");
    }
}
