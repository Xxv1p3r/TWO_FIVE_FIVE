//! PCI Config Space emulado con soporte multifunción.
//!
//! QEMU organiza el PIIX3 como dispositivo multifunción en 00:01.x:
//!   Func 0: ISA Bridge    (8086:7000, class 06/01)
//!   Func 1: IDE Controller (8086:7010, class 01/01)
//!   Func 2: USB UHCI      (8086:7020, class 0C/03)
//!   Func 3: ACPI/PM       (8086:7113, class 06/80)
//!
//! El i440FX host bridge (00:00.0) tiene registros PAM (0x80-0x8F)
//! que controlan si la memoria 0xC0000-0xFFFFF es RAM o ROM.
//! SeaBIOS escribe estos registros y los lee de vuelta para verificar.

use super::IoDevice;
use std::sync::{Arc, Mutex};

// ─── Puertos de acceso a config space ──────────────────────────────
const PORT_ADDR: u16 = 0xCF8;
const PORT_DATA: u16 = 0xCFC;

// ─── PCI Header Type ──────────────────────────────────────────────
const HEADER_TYPE_MULTIFUNCTION: u8 = 0x80;

/// Un dispositivo PCI individual.
#[derive(Debug, Clone)]
pub struct PciDevice {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,     // Byte 0x0E
    #[allow(dead_code)]
    pub bist: u8,            // Byte 0x0F
    #[allow(dead_code)]
    pub latency_timer: u8,   // Byte 0x0D
    #[allow(dead_code)]
    pub cache_line_size: u8, // Byte 0x0C
    #[allow(dead_code)]
    pub interrupt_line: u8,  // Byte 0x3C
    #[allow(dead_code)]
    pub interrupt_pin: u8,   // Byte 0x3D
    command: u16,
    status: u16,
    /// Registro de configuración extendido (para PAM, subsystem IDs, etc.)
    pub config_regs: [u8; 256],
    /// BAR sizing masks: indexed by BAR register offset (0x10, 0x14, 0x18, 0x1C, 0x20, 0x24)
    /// Value 0 = BAR not implemented. Non-zero = mask returned during sizing (write 0xFFFFFFFF → read mask).
    bar_masks: [u32; 6],
}

impl PciDevice {
    fn new(vendor: u16, device: u16, class: u8, subclass: u8, prog_if: u8) -> Self {
        let mut config_regs = [0u8; 256];
        // Vendor/Device
        config_regs[0] = vendor as u8;
        config_regs[1] = (vendor >> 8) as u8;
        config_regs[2] = device as u8;
        config_regs[3] = (device >> 8) as u8;
        // Class/Subclass/ProgIf/Revision
        config_regs[8] = 0x01; // revision
        config_regs[9] = prog_if;
        config_regs[0x0A] = subclass;
        config_regs[0x0B] = class;
        Self {
            vendor_id: vendor,
            device_id: device,
            class_code: class,
            subclass,
            prog_if,
            revision: 0x01,
            header_type: 0x00,
            bist: 0x00,
            latency_timer: 0x00,
            cache_line_size: 0x00,
            interrupt_line: 0x00,
            interrupt_pin: 0x00,
            command: 0x00,
            status: 0x00,
            config_regs,
            bar_masks: [0u32; 6],
        }
    }

    /// Set BAR mask for sizing: reg_off is 0x10..0x24 (6 BARs)
    pub fn set_bar_mask(&mut self, reg_off: u8, mask: u32) {
        let idx = ((reg_off - 0x10) / 4) as usize;
        if idx < 6 {
            self.bar_masks[idx] = mask;
        }
    }

    fn get_byte(&self, offset: u8) -> u8 {
        match offset {
            0x00 => self.vendor_id as u8,
            0x01 => (self.vendor_id >> 8) as u8,
            0x02 => self.device_id as u8,
            0x03 => (self.device_id >> 8) as u8,
            0x04 => self.command as u8,
            0x05 => (self.command >> 8) as u8,
            0x06 => self.status as u8,
            0x07 => (self.status >> 8) as u8,
            0x08 => self.revision,
            0x09 => self.prog_if,
            0x0A => self.subclass,
            0x0B => self.class_code,
            0x0E => self.header_type,
            // BAR registers: return sizing mask when value is all 1s
            0x10..=0x27 => {
                let bar_idx = ((offset - 0x10) / 4) as usize;
                let bar_byte = (offset - 0x10) % 4;
                if bar_idx < 6 && self.bar_masks[bar_idx] != 0 {
                    // Check if the stored BAR value is all 1s (sizing mode)
                    let mask = self.bar_masks[bar_idx];
                    let bar_reg = 0x10 + (bar_idx as u8) * 4;
                    let stored = (self.config_regs[bar_reg as usize] as u32)
                        | ((self.config_regs[(bar_reg + 1) as usize] as u32) << 8)
                        | ((self.config_regs[(bar_reg + 2) as usize] as u32) << 16)
                        | ((self.config_regs[(bar_reg + 3) as usize] as u32) << 24);
                    if stored == 0xFFFFFFFF {
                        // Sizing mode: return mask
                        ((mask >> (bar_byte * 8)) & 0xFF) as u8
                    } else if bar_byte == 0 {
                        (self.config_regs[offset as usize] & (mask as u8)) | ((mask & 0x0F) as u8)
                    } else {
                        self.config_regs[offset as usize]
                    }
                } else {
                    // BAR no implementado: debe devolver 0x00 según especificación PCI
                    0x00
                }
            }
            // Expansion ROM Base Address (0x30..0x33): devolvemos 0 para indicar
            // al BIOS que el dispositivo no expone ROM vía PCI BAR (se carga vía fw_cfg o C0000).
            0x30..=0x33 => 0x00,
            _ => self.config_regs[offset as usize],
        }
    }

    fn set_byte(&mut self, offset: u8, value: u8) {
        match offset {
            0x04 => self.command = (self.command & 0xFF00) | value as u16,
            0x05 => self.command = (self.command & 0x00FF) | ((value as u16) << 8),
            0x06 => self.status = (self.status & 0xFF00) | value as u16,
            0x07 => self.status = (self.status & 0x00FF) | ((value as u16) << 8),
            // BAR registers (0x10..=0x27): solo escribir si el BAR está implementado
            0x10..=0x27 => {
                let bar_idx = ((offset - 0x10) / 4) as usize;
                if bar_idx < 6 && self.bar_masks[bar_idx] != 0 {
                    self.config_regs[offset as usize] = value;
                }
            }
            // Expansion ROM BAR (0x30..0x33): inmutable a 0 (sin ROM PCI)
            0x30..=0x33 => {}
            // Allow writing to config registers 0x00-0xFF
            _ => {
                self.config_regs[offset as usize] = value;
                // Keep vendor/device/ident fields immutable
                if offset < 0x04 {
                    // vendor/device: restore original
                }
            }
        }
    }
}

/// "Bus PCI" con soporte multifunción (8 devices × 8 functions).
pub struct PciBus {
    addr_reg: u32,
    devices: Vec<Vec<Option<PciDevice>>>,
    pub last_acpi_config_write: Option<(u8, u32)>,
    /// USB UHCI state compartido (para notificar I/O base asignado)
    usb_uhci: Option<Arc<Mutex<super::usb_uhci::UhciState>>>,
    /// VirtIO Serial state compartido (para notificar I/O base asignado)
    virtio_serial: Option<Arc<Mutex<super::virtio_serial::VirtioSerialState>>>,
    /// VirtIO Net state compartido (para notificar I/O base asignado)
    virtio_net: Option<Arc<Mutex<super::virtio_net::VirtioNetState>>>,
    /// (tarea 1) Última asignación detectada de los BARs VGA (dev 2:0):
    /// Some(base_gpa) cuando el guest escribe una dirección real de memoria.
    /// La consume el hook de `DeviceBus::out()` (devices/mod.rs).
    pub last_vga_lfb_bar_write: Option<u32>,
    pub last_vga_mmio_bar_write: Option<u32>,
    /// Última asignación detectada de BAR4 del controlador IDE PIIX3 (dev 1:1, BMDMA)
    pub last_ide_bmdma_bar_write: Option<u16>,
    /// Última asignación detectada de BAR5 del controlador SATA AHCI (dev 5:0, MMIO 4 KiB)
    pub last_ahci_bar_write: Option<u32>,
    /// VMMDev state compartido (VirtualBox Guest Additions)
    vmmdev: Option<Arc<Mutex<super::vmmdev::VmmDevState>>>,
    /// Última asignación detectada de BAR0 de VMMDev (dev 6:0, I/O 32 bytes)
    pub last_vmmdev_io_bar_write: Option<u16>,
    /// Última asignación detectada de BAR1 de VMMDev (dev 6:0, MMIO 16 KiB)
    pub last_vmmdev_mmio_bar_write: Option<u32>,
    /// AC'97 Audio state compartido
    ac97: Option<Arc<Mutex<super::ac97::Ac97State>>>,
    /// Última asignación detectada de BAR0 de AC'97 (dev 7:0, NAMBAR 256 bytes)
    pub last_ac97_nam_bar_write: Option<u16>,
    /// Última asignación detectada de BAR1 de AC'97 (dev 7:0, NABMBAR 64 bytes)
    pub last_ac97_nabm_bar_write: Option<u16>,
}

impl PciBus {
    pub fn new() -> Self {
        eprintln!("[PCI] Bus PCI inicializado (bus=0, 32 slots × 8 funcs)");
        Self {
            addr_reg: 0,
            devices: vec![vec![None; 8]; 32],
            last_acpi_config_write: None,
            usb_uhci: None,
            virtio_serial: None,
            virtio_net: None,
            vmmdev: None,
            ac97: None,
            last_vga_lfb_bar_write: None,
            last_vga_mmio_bar_write: None,
            last_ide_bmdma_bar_write: None,
            last_ahci_bar_write: None,
            last_vmmdev_io_bar_write: None,
            last_vmmdev_mmio_bar_write: None,
            last_ac97_nam_bar_write: None,
            last_ac97_nabm_bar_write: None,
        }
    }

    /// Conecta el estado compartido del controlador USB UHCI
    /// (para notificar el I/O base asignado al BAR4)
    pub fn connect_usb(&mut self, state: Arc<Mutex<super::usb_uhci::UhciState>>) {
        self.usb_uhci = Some(state);
    }

    /// Conecta el estado compartido del controlador VirtIO Serial
    /// (para notificar el I/O base asignado al BAR0)
    pub fn connect_virtio_serial(&mut self, state: Arc<Mutex<super::virtio_serial::VirtioSerialState>>) {
        self.virtio_serial = Some(state);
    }

    /// Conecta el estado compartido del controlador VirtIO Net
    /// (para notificar el I/O base asignado al BAR0)
    pub fn connect_virtio_net(&mut self, state: Arc<Mutex<super::virtio_net::VirtioNetState>>) {
        self.virtio_net = Some(state);
    }

    /// Obtiene el I/O base asignado al USB UHCI (si disponible)
    /// (helper de diagnóstico; el I/O base lo asigna write_bytes vía BAR4)
    #[allow(dead_code)]
    pub fn usb_iobase(&self) -> Option<u16> {
        self.usb_uhci.as_ref().map(|s| {
            let state = s.lock().unwrap();
            if state.iobase == 0 { None } else { Some(state.iobase) }
        }).and_then(|x| x)
    }

    /// Devuelve la línea IRQ asignada al controlador USB UHCI (dev 1:2).
    /// SeaBIOS asigna PIRQD -> IRQ 11 y lo escribe en el registro de config 0x3C.
    pub fn usb_irq_line(&self) -> u8 {
        if let Some(dev) = self.devices[1][2].as_ref() {
            let irq = dev.config_regs[0x3C];
            if irq != 0 && irq != 0xFF {
                return irq;
            }
        }
        11 // Fallback PIIX3 USB INTD#
    }

    /// Devuelve la línea IRQ asignada al controlador VirtIO Serial (dev 3:0).
    pub fn virtio_serial_irq_line(&self) -> u8 {
        if let Some(dev) = self.devices[3][0].as_ref() {
            let irq = dev.config_regs[0x3C];
            if irq != 0 && irq != 0xFF {
                return irq;
            }
        }
        10 // Default IRQ 10
    }

    /// Devuelve la línea IRQ asignada al controlador VirtIO Net (dev 4:0).
    pub fn virtio_net_irq_line(&self) -> u8 {
        if let Some(dev) = self.devices[4][0].as_ref() {
            let irq = dev.config_regs[0x3C];
            if irq != 0 && irq != 0xFF {
                return irq;
            }
        }
        9 // Default IRQ 9 (PIRQA / INTA#)
    }

    /// Devuelve la línea IRQ asignada al controlador SATA AHCI (dev 5:0).
    pub fn ahci_irq_line(&self) -> u8 {
        if let Some(dev) = self.devices[5][0].as_ref() {
            let irq = dev.config_regs[0x3C];
            if irq != 0 && irq != 0xFF {
                return irq;
            }
        }
        10 // Default IRQ 10
    }

    /// Conecta el estado compartido del dispositivo VirtualBox VMMDev
    pub fn connect_vmmdev(&mut self, state: Arc<Mutex<super::vmmdev::VmmDevState>>) {
        self.vmmdev = Some(state);
    }

    /// Devuelve la línea IRQ asignada al dispositivo VirtualBox VMMDev (dev 6:0).
    pub fn vmmdev_irq_line(&self) -> u8 {
        if let Some(dev) = self.devices[6][0].as_ref() {
            let irq = dev.config_regs[0x3C];
            if irq != 0 && irq != 0xFF {
                return irq;
            }
        }
        11 // Default IRQ 11 (shared with USB UHCI)
    }

    /// Conecta el estado compartido del controlador de audio AC'97
    pub fn connect_ac97(&mut self, state: Arc<Mutex<super::ac97::Ac97State>>) {
        self.ac97 = Some(state);
    }

    /// Devuelve la línea IRQ asignada al controlador de audio AC'97 (dev 7:0).
    pub fn ac97_irq_line(&self) -> u8 {
        if let Some(dev) = self.devices[7][0].as_ref() {
            let irq = dev.config_regs[0x3C];
            if irq != 0 && irq != 0xFF {
                return irq;
            }
        }
        5 // Default IRQ 5 (Standard PC Audio IRQ)
    }

    /// Registra un dispositivo en un slot/función específicos.
    pub fn add_device(&mut self, slot: usize, function: usize, dev: PciDevice) {
        eprintln!(
            "[PCI] Slot {}:{} vendor={:#06x} device={:#06x} class=0x{:02X}/0x{:02X} prog_if={:#04x}",
            slot, function, dev.vendor_id, dev.device_id, dev.class_code, dev.subclass, dev.prog_if
        );
        if slot < self.devices.len() && function < 8 {
            self.devices[slot][function] = Some(dev);
        }
    }

    fn device_at(&self, addr: u32) -> Option<&PciDevice> {
        let bus_num = ((addr >> 16) & 0xFF) as usize;
        let dev_num = ((addr >> 11) & 0x1F) as usize;
        let fn_num = ((addr >> 8) & 0x07) as usize;
        if bus_num != 0 || dev_num >= self.devices.len() {
            return None;
        }
        self.devices[dev_num][fn_num].as_ref()
    }

    fn device_at_mut(&mut self, addr: u32) -> Option<&mut PciDevice> {
        let bus_num = ((addr >> 16) & 0xFF) as usize;
        let dev_num = ((addr >> 11) & 0x1F) as usize;
        let fn_num = ((addr >> 8) & 0x07) as usize;
        if bus_num != 0 || dev_num >= self.devices.len() {
            return None;
        }
        self.devices[dev_num][fn_num].as_mut()
    }

    fn read_bytes(&self, port: u16, count: usize) -> Vec<u8> {
        let addr = self.addr_reg;
        let lane = (port & 0x03) as u8;
        let byte_off = (addr & 0xFC) as u8 + lane;

        let mut result = vec![0u8; count];
        if let Some(dev) = self.device_at(addr) {
            for (i, item) in result.iter_mut().enumerate() {
                *item = dev.get_byte(byte_off.wrapping_add(i as u8));
            }
        } else {
            result.fill(0xFF);
        }
        result
    }

    fn write_bytes(&mut self, port: u16, data: &[u8]) {
        let addr = self.addr_reg;
        let lane = (port & 0x03) as u8;
        let byte_off = (addr & 0xFC) as u8 + lane;
        let dev_num = ((addr >> 11) & 0x1F) as usize;
        let fn_num = ((addr >> 8) & 0x07) as usize;

        // Collect side-effects while dev is borrowed, then apply after drop
        let mut acpi_write: Option<(u8, u32)> = None;
        let mut usb_new_iobase: Option<u16> = None;
        let mut virtio_serial_new_iobase: Option<u16> = None;
        let mut virtio_net_new_iobase: Option<u16> = None;
        let mut ide_bmdma_new_iobase: Option<u16> = None;
        let mut ahci_new_mmio_base: Option<u32> = None;
        let mut vmmdev_new_iobase: Option<u16> = None;
        let mut vmmdev_new_mmio_base: Option<u32> = None;
        let mut ac97_new_nam_iobase: Option<u16> = None;
        let mut ac97_new_nabm_iobase: Option<u16> = None;
        let mut vga_bar_write: Option<(u8, u32)> = None; // (tarea 1: reg_off, base)

        if let Some(dev) = self.device_at_mut(addr) {
            for (i, &b) in data.iter().enumerate() {
                dev.set_byte(byte_off.wrapping_add(i as u8), b);
            }
            // (tarea 1) Detectar asignaciones de los BARs VGA (dev 2:0):
            // BAR0 (0x10) = framebuffer lineal 16 MiB, BAR2 (0x18) = MMIO 4 KiB.
            // Igual que con el USB: ignoramos las escrituras de sizing
            // (0xFFFFFFFF) y solo registramos direcciones reales de memoria.
            if dev_num == 2 && fn_num == 0 {
                for reg_off in [0x10u8, 0x18u8] {
                    if byte_off >= reg_off && byte_off < reg_off + 4 {
                        let raw = (dev.config_regs[reg_off as usize] as u32)
                            | ((dev.config_regs[reg_off as usize + 1] as u32) << 8)
                            | ((dev.config_regs[reg_off as usize + 2] as u32) << 16)
                            | ((dev.config_regs[reg_off as usize + 3] as u32) << 24);
                        if raw != 0xFFFFFFFF && raw & 1 == 0 {
                            let base = raw & 0xFFFF_FFF0;
                            vga_bar_write = Some((reg_off, base));
                            eprintln!(
                                "[PCI] VGA BAR{} asignado: GPA {:#x}",
                                if reg_off == 0x10 { 0 } else { 2 },
                                base
                            );
                        }
                    }
                }
            }
            // Detect writes to PIIX3 ACPI PM config registers (device 1, function 3)
            if dev_num == 1 && fn_num == 3 {
                let reg = byte_off;
                if matches!(reg, 0x40 | 0x80 | 0x90 | 0xD2) {
                    let mut val_bytes = [0u8; 4];
                    for (i, item) in val_bytes.iter_mut().enumerate() {
                        *item = dev.get_byte(reg.wrapping_add(i as u8));
                    }
                    acpi_write = Some((reg, u32::from_le_bytes(val_bytes)));
                }
            }
            // Detect writes to IDE Controller BAR4 (device 1, function 1, reg 0x20)
            if dev_num == 1 && fn_num == 1 && byte_off >= 0x20 && byte_off < 0x24 {
                let raw_bar = (dev.config_regs[0x20] as u32)
                    | ((dev.config_regs[0x21] as u32) << 8)
                    | ((dev.config_regs[0x22] as u32) << 16)
                    | ((dev.config_regs[0x23] as u32) << 24);
                if raw_bar != 0xFFFFFFFF && raw_bar != 0 {
                    let iobase = (raw_bar & 0xFFF0) as u16;
                    if iobase != 0 && iobase >= 0x400 {
                        ide_bmdma_new_iobase = Some(iobase);
                    }
                }
            }
            // Detect writes to USB UHCI BAR4 (device 1, function 2, reg 0x20)
            if dev_num == 1 && fn_num == 2 && byte_off >= 0x20 && byte_off < 0x24 {
                let raw_bar = (dev.config_regs[0x20] as u32)
                    | ((dev.config_regs[0x21] as u32) << 8)
                    | ((dev.config_regs[0x22] as u32) << 16)
                    | ((dev.config_regs[0x23] as u32) << 24);
                // Skip sizing writes (all 1s) and 0 — only set iobase for real addresses
                if raw_bar != 0xFFFFFFFF && raw_bar != 0 {
                    let iobase = (raw_bar & 0xFFE0) as u16;
                    if iobase != 0 && iobase >= 0x400 {
                        usb_new_iobase = Some(iobase);
                    }
                }
            }
            // Detect writes to VirtIO Serial BAR0 (device 3, function 0, reg 0x10)
            if dev_num == 3 && fn_num == 0 && byte_off >= 0x10 && byte_off < 0x14 {
                let raw_bar = (dev.config_regs[0x10] as u32)
                    | ((dev.config_regs[0x11] as u32) << 8)
                    | ((dev.config_regs[0x12] as u32) << 16)
                    | ((dev.config_regs[0x13] as u32) << 24);
                if raw_bar != 0xFFFFFFFF && raw_bar != 0 {
                    let iobase = (raw_bar & 0xFFC0) as u16;
                    if iobase != 0 && iobase >= 0x400 {
                        virtio_serial_new_iobase = Some(iobase);
                    }
                }
            }
            // Detect writes to VirtIO Net BAR0 (device 4, function 0, reg 0x10)
            if dev_num == 4 && fn_num == 0 && byte_off >= 0x10 && byte_off < 0x14 {
                let raw_bar = (dev.config_regs[0x10] as u32)
                    | ((dev.config_regs[0x11] as u32) << 8)
                    | ((dev.config_regs[0x12] as u32) << 16)
                    | ((dev.config_regs[0x13] as u32) << 24);
                if raw_bar != 0xFFFFFFFF && raw_bar != 0 {
                    let iobase = (raw_bar & 0xFFE0) as u16;
                    if iobase != 0 && iobase >= 0x400 {
                        virtio_net_new_iobase = Some(iobase);
                    }
                }
            }
            // Detect writes to SATA AHCI BAR5 (device 5, function 0, reg 0x24)
            if dev_num == 5 && fn_num == 0 && byte_off >= 0x24 && byte_off < 0x28 {
                let raw_bar = (dev.config_regs[0x24] as u32)
                    | ((dev.config_regs[0x25] as u32) << 8)
                    | ((dev.config_regs[0x26] as u32) << 16)
                    | ((dev.config_regs[0x27] as u32) << 24);
                if raw_bar != 0xFFFFFFFF && raw_bar != 0 && (raw_bar & 1) == 0 {
                    let base = raw_bar & 0xFFFF_F000;
                    ahci_new_mmio_base = Some(base);
                }
            }
            // Detect writes to VMMDev (device 6, function 0)
            if dev_num == 6 && fn_num == 0 {
                // BAR0 (reg 0x10): I/O space 32 bytes
                if byte_off >= 0x10 && byte_off < 0x14 {
                    let raw_bar = (dev.config_regs[0x10] as u32)
                        | ((dev.config_regs[0x11] as u32) << 8)
                        | ((dev.config_regs[0x12] as u32) << 16)
                        | ((dev.config_regs[0x13] as u32) << 24);
                    if raw_bar != 0xFFFFFFFF && raw_bar != 0 && (raw_bar & 1) != 0 {
                        let iobase = (raw_bar & 0xFFE0) as u16;
                        if iobase != 0 && iobase >= 0x400 {
                            vmmdev_new_iobase = Some(iobase);
                        }
                    }
                }
                // BAR1 (reg 0x14): MMIO space 16 KiB
                if byte_off >= 0x14 && byte_off < 0x18 {
                    let raw_bar = (dev.config_regs[0x14] as u32)
                        | ((dev.config_regs[0x15] as u32) << 8)
                        | ((dev.config_regs[0x16] as u32) << 16)
                        | ((dev.config_regs[0x17] as u32) << 24);
                    if raw_bar != 0xFFFFFFFF && raw_bar != 0 && (raw_bar & 1) == 0 {
                        let base = raw_bar & 0xFFFF_C000;
                        vmmdev_new_mmio_base = Some(base);
                    }
                }
            }
            // Detect writes to AC'97 Audio (device 7, function 0)
            if dev_num == 7 && fn_num == 0 {
                // BAR0 (reg 0x10): NAMBAR I/O space 256 bytes
                if byte_off >= 0x10 && byte_off < 0x14 {
                    let raw_bar = (dev.config_regs[0x10] as u32)
                        | ((dev.config_regs[0x11] as u32) << 8)
                        | ((dev.config_regs[0x12] as u32) << 16)
                        | ((dev.config_regs[0x13] as u32) << 24);
                    if raw_bar != 0xFFFFFFFF && raw_bar != 0 && (raw_bar & 1) != 0 {
                        let iobase = (raw_bar & 0xFF00) as u16;
                        if iobase != 0 && iobase >= 0x400 {
                            ac97_new_nam_iobase = Some(iobase);
                        }
                    }
                }
                // BAR1 (reg 0x14): NABMBAR I/O space 64 bytes
                if byte_off >= 0x14 && byte_off < 0x18 {
                    let raw_bar = (dev.config_regs[0x14] as u32)
                        | ((dev.config_regs[0x15] as u32) << 8)
                        | ((dev.config_regs[0x16] as u32) << 16)
                        | ((dev.config_regs[0x17] as u32) << 24);
                    if raw_bar != 0xFFFFFFFF && raw_bar != 0 && (raw_bar & 1) != 0 {
                        let iobase = (raw_bar & 0xFFC0) as u16;
                        if iobase != 0 && iobase >= 0x400 {
                            ac97_new_nabm_iobase = Some(iobase);
                        }
                    }
                }
            }
        }
        // Apply side-effects after mutable borrow on dev is released
        if let Some((reg_off, base)) = vga_bar_write {
            if reg_off == 0x10 {
                self.last_vga_lfb_bar_write = Some(base);
            } else {
                self.last_vga_mmio_bar_write = Some(base);
            }
        }
        if let Some(aw) = acpi_write {
            self.last_acpi_config_write = Some(aw);
        }
        if let Some(new_base) = ide_bmdma_new_iobase {
            self.last_ide_bmdma_bar_write = Some(new_base);
            eprintln!("[PCI] IDE BMDMA I/O base: 0x{:04X} (dev 1:1 BAR4)", new_base);
        }
        if let Some(new_base) = usb_new_iobase {
            if let Some(ref usb_state) = self.usb_uhci {
                let mut state = usb_state.lock().unwrap();
                if state.iobase != new_base {
                    state.iobase = new_base;
                    eprintln!("[PCI] USB UHCI I/O base: 0x{:04X} (dev 1:2 BAR4)", new_base);
                }
            }
        }
        if let Some(new_base) = virtio_serial_new_iobase {
            if let Some(ref vs_state) = self.virtio_serial {
                let mut state = vs_state.lock().unwrap();
                if state.iobase != new_base {
                    state.iobase = new_base;
                    eprintln!("[PCI] VirtIO Serial I/O base: 0x{:04X} (dev 3:0 BAR0)", new_base);
                }
            }
        }
        if let Some(new_base) = virtio_net_new_iobase {
            if let Some(ref vn_state) = self.virtio_net {
                let mut state = vn_state.lock().unwrap();
                if state.iobase != new_base {
                    state.iobase = new_base;
                    eprintln!("[PCI] VirtIO Net I/O base: 0x{:04X} (dev 4:0 BAR0)", new_base);
                }
            }
        }
        if let Some(new_base) = ahci_new_mmio_base {
            self.last_ahci_bar_write = Some(new_base);
            eprintln!("[PCI] AHCI BAR5 MMIO base: 0x{:08X} (dev 5:0)", new_base);
        }
        if let Some(new_base) = vmmdev_new_iobase {
            self.last_vmmdev_io_bar_write = Some(new_base);
            if let Some(ref vmm_state) = self.vmmdev {
                let mut state = vmm_state.lock().unwrap();
                if state.iobase != new_base {
                    state.iobase = new_base;
                    eprintln!("[PCI] VMMDev I/O base: 0x{:04X} (dev 6:0 BAR0)", new_base);
                }
            }
        }
        if let Some(new_base) = vmmdev_new_mmio_base {
            self.last_vmmdev_mmio_bar_write = Some(new_base);
            if let Some(ref vmm_state) = self.vmmdev {
                let mut state = vmm_state.lock().unwrap();
                if state.mmio_base != new_base {
                    state.mmio_base = new_base;
                    eprintln!("[PCI] VMMDev MMIO base: 0x{:08X} (dev 6:0 BAR1)", new_base);
                }
            }
        }
        if let Some(new_base) = ac97_new_nam_iobase {
            self.last_ac97_nam_bar_write = Some(new_base);
            if let Some(ref ac97_state) = self.ac97 {
                let mut state = ac97_state.lock().unwrap();
                if state.nambar != new_base {
                    state.nambar = new_base;
                    eprintln!("[PCI] AC'97 NAMBAR I/O base: 0x{:04X} (dev 7:0 BAR0)", new_base);
                }
            }
        }
        if let Some(new_base) = ac97_new_nabm_iobase {
            self.last_ac97_nabm_bar_write = Some(new_base);
            if let Some(ref ac97_state) = self.ac97 {
                let mut state = ac97_state.lock().unwrap();
                if state.nabmbar != new_base {
                    state.nabmbar = new_base;
                    eprintln!("[PCI] AC'97 NABMBAR I/O base: 0x{:04X} (dev 7:0 BAR1)", new_base);
                }
            }
        }
    }

    fn access_enabled(&self) -> bool {
        (self.addr_reg & (1 << 31)) != 0
    }
}

impl Default for PciBus {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for PciBus {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port, PORT_ADDR | PORT_DATA | 0xCFD | 0xCFE | 0xCFF)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        match port {
            PORT_ADDR => {
                let addr = (data[0] as u32)
                    | ((data.get(1).copied().unwrap_or(0) as u32) << 8)
                    | ((data.get(2).copied().unwrap_or(0) as u32) << 16)
                    | ((data.get(3).copied().unwrap_or(0x80) as u32) << 24);
                self.addr_reg = addr & 0x80FF_FFFC;
            }
            PORT_DATA | 0xCFD | 0xCFE | 0xCFF => {
                if self.access_enabled() {
                    self.write_bytes(port, data);
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        if matches!(port, PORT_DATA | 0xCFD | 0xCFE | 0xCFF) {
            if !self.access_enabled() {
                return vec![0xFF; count];
            }
            self.read_bytes(port, count)
        } else {
            vec![
                self.addr_reg as u8,
                ((self.addr_reg >> 8) & 0xFF) as u8,
                ((self.addr_reg >> 16) & 0xFF) as u8,
                ((self.addr_reg >> 24) & 0x8F) as u8,
            ]
        }
    }
}

// ────────────────────────────────────────────────────────────────────
//  Builder: crea el bus con layout PIIX3 multifunción QEMU-compat
// ────────────────────────────────────────────────────────────────────

impl PciBus {
    /// Crea un bus PCI preconfigurado con QEMU-compatible i440FX + PIIX3.
    pub fn with_legacy_ide() -> Self {
        let mut bus = Self::new();
        bus.populate_legacy_devices();
        bus
    }

    /// Reset del bus PCI (reset del chipset): reconstruye el árbol de
    /// dispositivos con su estado inicial (command/status a 0, BARs sin
    /// asignar, PAM en ROM). Conserva el estado compartido del USB UHCI
    /// (el Arc, que `connect_usb` ya enlazó); el UHCI se resetea aparte.
    pub fn reset(&mut self) {
        self.addr_reg = 0;
        self.last_acpi_config_write = None;
        self.last_vga_lfb_bar_write = None;
        self.last_vga_mmio_bar_write = None;
        self.last_ahci_bar_write = None;
        self.devices = vec![vec![None; 8]; 32];
        self.populate_legacy_devices();
    }

    /// Puebla el bus con el layout QEMU-compatible i440FX + PIIX3.
    /// Reutilizado por `with_legacy_ide()` y por `reset()` para reconstruir
    /// el árbol de dispositivos desde cero tras un reset del guest.
    fn populate_legacy_devices(&mut self) {

        // ── Device 0: i440FX Host Bridge (8086:1237) ──
        let mut host = PciDevice::new(0x8086, 0x1237, 0x06, 0x00, 0x00);
        host.revision = 0x02;
        // PAM registers (0x80-0x8F) control memory caching/shadowing.
        // SeaBIOS writes to these during relocation. We persist them so
        // read-back returns the written value.
        // Initial state: all segments are ROM (0x00).
        // Subsystem IDs (0x2C/0x2E): QEMU's i440FX reports 1af4:1100.
        // SeaBIOS's qemu_detect() REQUIRES these to enable PF_QEMU —
        // without them it silently disables fw_cfg, e820 and debug output!
        host.config_regs[0x2C] = 0xF4; // subsystem vendor 0x1AF4 (Red Hat) low
        host.config_regs[0x2D] = 0x1A; // subsystem vendor high
        host.config_regs[0x2E] = 0x00; // subsystem device 0x1100 (QEMU VM) low
        host.config_regs[0x2F] = 0x11; // subsystem device high
        self.add_device(0, 0, host);

        // ── Device 1: PIIX3 multifunction ──
        // QEMU PIIX3 = 82371AB/EB/MB, device IDs:
        //   Func 0: 8086:7000 (ISA Bridge)
        //   Func 1: 8086:7010 (IDE Controller)
        //   Func 2: 8086:7020 (USB UHCI)
        //   Func 3: 8086:7113 (ACPI/Power Management)

        // Function 0: ISA Bridge
        let mut isa = PciDevice::new(0x8086, 0x7000, 0x06, 0x01, 0x00);
        isa.header_type = HEADER_TYPE_MULTIFUNCTION; // bit 7 = multifunction
        isa.revision = 0x00;
        self.add_device(1, 0, isa);

        // Function 1: IDE Controller (PIIX3)
        // prog_if 0x80 = Legacy compatibility mode (canales fijos en ISA 0x1F0 y 0x170),
        // con soporte Bus Master DMA (bit 7 = 1)
        let mut ide = PciDevice::new(0x8086, 0x7010, 0x01, 0x01, 0x80);
        ide.revision = 0x00;
        // BAR4 (reg 0x20): Bus master I/O, 16 bytes → mask 0xFFFF_FFF1
        ide.set_bar_mask(0x20, 0xFFFF_FFF1);
        ide.config_regs[0x20] = 0x01;
        ide.config_regs[0x21] = 0xC0; // default 0xC001
        self.add_device(1, 1, ide);

        // Function 2: USB UHCI
        let mut usb = PciDevice::new(0x8086, 0x7020, 0x0C, 0x03, 0x00);
        usb.revision = 0x00;
        usb.interrupt_pin = 0x04; // INTD# (PIIX3 USB)
        usb.config_regs[0x3D] = 0x04; // Interrupt Pin INTD#
        usb.config_regs[0x3C] = 11;   // Default IRQ line 11 (PIIX3 PIRQD)
        // BAR4 (reg 0x20): I/O, 32 bytes → mask 0xFFFFFFE1
        usb.set_bar_mask(0x20, 0xFFFFFFE1);
        self.add_device(1, 2, usb);

        // Function 3: ACPI/PM
        let mut acpi = PciDevice::new(0x8086, 0x7113, 0x06, 0x80, 0x00);
        acpi.revision = 0x00;
        // No BARs - PM I/O base configured via PCI config regs 0x40/0x80
        self.add_device(1, 3, acpi);

        // ── Device 2: Bochs VBE Display Adapter (1234:1111) ──
        let mut vga = PciDevice::new(0x1234, 0x1111, 0x03, 0x00, 0x00);
        vga.revision = 0x02;
        // BAR0 (reg 0x10): 16 MiB Framebuffer Memory (prefetchable)
        vga.set_bar_mask(0x10, 0xFF00_0008);
        // BAR2 (reg 0x18): 4 KiB MMIO
        vga.set_bar_mask(0x18, 0xFFFF_F000);
        self.add_device(2, 0, vga);

        // ── Device 3: VirtIO-Serial Controller (1AF4:1003) ──
        let mut vs = PciDevice::new(0x1AF4, 0x1003, 0x07, 0x80, 0x00);
        vs.revision = 0x00;
        vs.config_regs[8] = 0x00;
        vs.interrupt_pin = 0x01; // INTA#
        vs.config_regs[0x2C] = 0xF4; // Subsystem Vendor ID low
        vs.config_regs[0x2D] = 0x1A; // Subsystem Vendor ID high
        vs.config_regs[0x2E] = 0x03; // Subsystem Device ID low
        vs.config_regs[0x2F] = 0x00; // Subsystem Device ID high
        vs.config_regs[0x3D] = 0x01; // Interrupt Pin INTA#
        vs.config_regs[0x3C] = 10;   // Default IRQ line 10
        // BAR0 (reg 0x10): I/O space, 64 bytes -> mask 0xFFFFFFC1
        vs.set_bar_mask(0x10, 0xFFFFFFC1);
        self.add_device(3, 0, vs);

        // ── Device 4: VirtIO-Net Ethernet Controller (1AF4:1000) ──
        let mut vn = PciDevice::new(0x1AF4, 0x1000, 0x02, 0x00, 0x00);
        vn.revision = 0x00;
        vn.config_regs[8] = 0x00;
        vn.interrupt_pin = 0x01; // INTA#
        vn.config_regs[0x2C] = 0xF4; // Subsystem Vendor ID low
        vn.config_regs[0x2D] = 0x1A; // Subsystem Vendor ID high
        vn.config_regs[0x2E] = 0x01; // Subsystem Device ID low (0x0001 = Net)
        vn.config_regs[0x2F] = 0x00; // Subsystem Device ID high
        vn.config_regs[0x3D] = 0x01; // Interrupt Pin INTA#
        vn.config_regs[0x3C] = 9;    // Default IRQ line 9
        // BAR0 (reg 0x10): I/O space, 32 bytes -> mask 0xFFFFFFE1
        vn.set_bar_mask(0x10, 0xFFFFFFE1);
        self.add_device(4, 0, vn);

        // ── Device 5: Intel ICH8M AHCI Controller (8086:2829) ──
        let mut ahci = PciDevice::new(0x8086, 0x2829, 0x01, 0x06, 0x01);
        ahci.revision = 0x02;
        ahci.interrupt_pin = 0x01; // INTA#
        ahci.config_regs[0x2C] = 0x86; // Subsystem Vendor ID low (Intel)
        ahci.config_regs[0x2D] = 0x80; // Subsystem Vendor ID high
        ahci.config_regs[0x2E] = 0x29; // Subsystem Device ID low (ICH8M)
        ahci.config_regs[0x2F] = 0x28; // Subsystem Device ID high
        ahci.config_regs[0x3D] = 0x01; // Interrupt Pin INTA#
        ahci.config_regs[0x3C] = 10;   // Default IRQ line 10
        // BAR5 (reg 0x24): 4 KiB MMIO non-prefetchable -> mask 0xFFFFF000
        ahci.set_bar_mask(0x24, 0xFFFF_F000);
        self.add_device(5, 0, ahci);

        // ── Device 6: VirtualBox VMMDev (80EE:CAFE) ──
        let mut vmmdev = PciDevice::new(0x80EE, 0xCAFE, 0x08, 0x80, 0x00);
        vmmdev.revision = 0x00;
        vmmdev.interrupt_pin = 0x01; // INTA#
        vmmdev.config_regs[0x2C] = 0xEE; // Subsystem Vendor ID low (InnoTek/VirtualBox)
        vmmdev.config_regs[0x2D] = 0x80; // Subsystem Vendor ID high
        vmmdev.config_regs[0x2E] = 0xFE; // Subsystem Device ID low (VMMDev)
        vmmdev.config_regs[0x2F] = 0xCA; // Subsystem Device ID high
        vmmdev.config_regs[0x3D] = 0x01; // Interrupt Pin INTA#
        vmmdev.config_regs[0x3C] = 11;   // Default IRQ line 11 (shared with USB UHCI)
        // BAR0 (reg 0x10): 32 bytes I/O space -> mask 0xFFFF_FFE1
        vmmdev.set_bar_mask(0x10, 0xFFFF_FFE1);
        // BAR1 (reg 0x14): 16 KiB MMIO -> mask 0xFFFF_C000
        vmmdev.set_bar_mask(0x14, 0xFFFF_C000);
        self.add_device(6, 0, vmmdev);

        // ── Device 7: Intel 82801AA AC'97 Audio Controller (8086:2415) ──
        let mut ac97 = PciDevice::new(0x8086, 0x2415, 0x04, 0x01, 0x00);
        ac97.revision = 0x01;
        ac97.interrupt_pin = 0x01; // INTA#
        ac97.config_regs[0x2C] = 0x86; // Subsystem Vendor ID low (Intel)
        ac97.config_regs[0x2D] = 0x80; // Subsystem Vendor ID high
        ac97.config_regs[0x2E] = 0x00; // Subsystem Device ID low
        ac97.config_regs[0x2F] = 0x00; // Subsystem Device ID high
        ac97.config_regs[0x3D] = 0x01; // Interrupt Pin INTA#
        ac97.config_regs[0x3C] = 5;    // Default IRQ line 5 (Audio)
        // BAR0 (reg 0x10): NAMBAR 256 bytes I/O space -> mask 0xFFFF_FF01
        ac97.set_bar_mask(0x10, 0xFFFF_FF01);
        // BAR1 (reg 0x14): NABMBAR 64 bytes I/O space -> mask 0xFFFF_FFC1
        ac97.set_bar_mask(0x14, 0xFFFF_FFC1);
        self.add_device(7, 0, ac97);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_addr(bus: &mut PciBus, addr: u32) {
        bus.write(PORT_ADDR, &addr.to_le_bytes());
    }

    #[test]
    fn multifunction_device_detected() {
        let mut bus = PciBus::with_legacy_ide();
        // Read func 0 of device 1
        set_addr(&mut bus, 0x8000_0800 | (1 << 11) | (0 << 8));
        let id = bus.read(PORT_DATA, 4);
        let vendor = u16::from_le_bytes([id[0], id[1]]);
        let device = u16::from_le_bytes([id[2], id[3]]);
        assert_eq!((vendor, device), (0x8086, 0x7000));

        // Read func 1 of device 1
        set_addr(&mut bus, 0x8000_0800 | (1 << 11) | (1 << 8));
        let id = bus.read(PORT_DATA, 4);
        let vendor = u16::from_le_bytes([id[0], id[1]]);
        let device = u16::from_le_bytes([id[2], id[3]]);
        assert_eq!((vendor, device), (0x8086, 0x7010));
    }

    #[test]
    fn header_type_multifunction() {
        let mut bus = PciBus::with_legacy_ide();
        set_addr(&mut bus, 0x8000_0800 | (1 << 11) | (0 << 8) | 0x0E);
        let ht = bus.read(PORT_DATA + 2, 1);
        assert_eq!(ht[0], HEADER_TYPE_MULTIFUNCTION);
    }

    #[test]
    fn command_register_writable() {
        let mut bus = PciBus::with_legacy_ide();
        set_addr(&mut bus, 0x8000_0800 | (1 << 11) | (1 << 8) | 0x04);
        bus.write(PORT_DATA, &[0x07, 0x00, 0x00, 0x00]);
        assert_eq!(bus.read(PORT_DATA, 4)[0], 0x07);
    }

    #[test]
    fn class_code_readable() {
        let mut bus = PciBus::with_legacy_ide();
        set_addr(&mut bus, 0x8000_0800 | (1 << 11) | (1 << 8) | 0x08);
        let d = bus.read(PORT_DATA, 4);
        assert_eq!(d[3], 0x01); // class: Mass Storage
        assert_eq!(d[2], 0x01); // subclass: IDE
    }

    #[test]
    fn virtio_serial_device_detected() {
        let mut bus = PciBus::with_legacy_ide();
        // Read device 3 func 0
        set_addr(&mut bus, 0x8000_0800 | (3 << 11) | (0 << 8));
        let id = bus.read(PORT_DATA, 4);
        let vendor = u16::from_le_bytes([id[0], id[1]]);
        let device = u16::from_le_bytes([id[2], id[3]]);
        assert_eq!((vendor, device), (0x1AF4, 0x1003));

        // Read revision and class code (offset 0x08)
        set_addr(&mut bus, 0x8000_0800 | (3 << 11) | (0 << 8) | 0x08);
        let rev_class = bus.read(PORT_DATA, 4);
        assert_eq!(rev_class[0], 0x00, "VirtIO legacy revision must be 0x00");

        // Read BAR0 mask in sizing mode
        set_addr(&mut bus, 0x8000_0800 | (3 << 11) | (0 << 8) | 0x10);
        bus.write(PORT_DATA, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let mask = bus.read(PORT_DATA, 4);
        assert_eq!(u32::from_le_bytes([mask[0], mask[1], mask[2], mask[3]]), 0xFFFFFFC1);
    }

    #[test]
    fn virtio_net_device_detected() {
        let mut bus = PciBus::with_legacy_ide();
        // Read device 4 func 0
        set_addr(&mut bus, 0x8000_0000 | (4 << 11) | (0 << 8));
        let id = bus.read(PORT_DATA, 4);
        let vendor = u16::from_le_bytes([id[0], id[1]]);
        let device = u16::from_le_bytes([id[2], id[3]]);
        assert_eq!((vendor, device), (0x1AF4, 0x1000));

        // Read revision and class code (offset 0x08): revision 0, class 0x02, subclass 0x00
        set_addr(&mut bus, 0x8000_0000 | (4 << 11) | (0 << 8) | 0x08);
        let rev_class = bus.read(PORT_DATA, 4);
        assert_eq!(rev_class[0], 0x00, "VirtIO legacy revision must be 0x00");
        assert_eq!(rev_class[2], 0x00, "Subclass must be 0x00 (Ethernet)");
        assert_eq!(rev_class[3], 0x02, "Class must be 0x02 (Network)");

        // Read BAR0 mask in sizing mode (32 bytes I/O space -> 0xFFFFFFE1)
        set_addr(&mut bus, 0x8000_0000 | (4 << 11) | (0 << 8) | 0x10);
        bus.write(PORT_DATA, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let mask = bus.read(PORT_DATA, 4);
        assert_eq!(u32::from_le_bytes([mask[0], mask[1], mask[2], mask[3]]), 0xFFFFFFE1);
    }

    #[test]
    fn vmmdev_device_detected() {
        let mut bus = PciBus::with_legacy_ide();
        // Read device 6 func 0
        set_addr(&mut bus, 0x8000_0000 | (6 << 11) | (0 << 8));
        let id = bus.read(PORT_DATA, 4);
        let vendor = u16::from_le_bytes([id[0], id[1]]);
        let device = u16::from_le_bytes([id[2], id[3]]);
        assert_eq!((vendor, device), (0x80EE, 0xCAFE));

        // Read revision and class code (offset 0x08): revision 0, class 0x08, subclass 0x80
        set_addr(&mut bus, 0x8000_0000 | (6 << 11) | (0 << 8) | 0x08);
        let rev_class = bus.read(PORT_DATA, 4);
        assert_eq!(rev_class[0], 0x00, "VMMDev revision must be 0x00");
        assert_eq!(rev_class[2], 0x80, "Subclass must be 0x80 (Other)");
        assert_eq!(rev_class[3], 0x08, "Class must be 0x08 (Generic System Peripheral)");

        // Read BAR0 mask in sizing mode (32 bytes I/O space -> 0xFFFF_FFE1)
        set_addr(&mut bus, 0x8000_0000 | (6 << 11) | (0 << 8) | 0x10);
        bus.write(PORT_DATA, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let mask0 = bus.read(PORT_DATA, 4);
        assert_eq!(u32::from_le_bytes([mask0[0], mask0[1], mask0[2], mask0[3]]), 0xFFFF_FFE1);

        // Read BAR1 mask in sizing mode (16 KiB MMIO -> 0xFFFF_C000)
        set_addr(&mut bus, 0x8000_0000 | (6 << 11) | (0 << 8) | 0x14);
        bus.write(PORT_DATA, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let mask1 = bus.read(PORT_DATA, 4);
        assert_eq!(u32::from_le_bytes([mask1[0], mask1[1], mask1[2], mask1[3]]), 0xFFFF_C000);
    }

    #[test]
    fn ac97_device_detected() {
        let mut bus = PciBus::with_legacy_ide();
        // Read device 7 func 0
        set_addr(&mut bus, 0x8000_0000 | (7 << 11) | (0 << 8));
        let id = bus.read(PORT_DATA, 4);
        let vendor = u16::from_le_bytes([id[0], id[1]]);
        let device = u16::from_le_bytes([id[2], id[3]]);
        assert_eq!((vendor, device), (0x8086, 0x2415));

        // Read revision and class code (offset 0x08): revision 1, class 0x04, subclass 0x01
        set_addr(&mut bus, 0x8000_0000 | (7 << 11) | (0 << 8) | 0x08);
        let rev_class = bus.read(PORT_DATA, 4);
        assert_eq!(rev_class[0], 0x01, "AC97 revision must be 0x01");
        assert_eq!(rev_class[2], 0x01, "Subclass must be 0x01 (Audio Controller)");
        assert_eq!(rev_class[3], 0x04, "Class must be 0x04 (Multimedia)");

        // Read BAR0 mask in sizing mode (256 bytes I/O space -> 0xFFFF_FF01)
        set_addr(&mut bus, 0x8000_0000 | (7 << 11) | (0 << 8) | 0x10);
        bus.write(PORT_DATA, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let mask0 = bus.read(PORT_DATA, 4);
        assert_eq!(u32::from_le_bytes([mask0[0], mask0[1], mask0[2], mask0[3]]), 0xFFFF_FF01);

        // Read BAR1 mask in sizing mode (64 bytes I/O space -> 0xFFFF_FFC1)
        set_addr(&mut bus, 0x8000_0000 | (7 << 11) | (0 << 8) | 0x14);
        bus.write(PORT_DATA, &[0xFF, 0xFF, 0xFF, 0xFF]);
        let mask1 = bus.read(PORT_DATA, 4);
        assert_eq!(u32::from_le_bytes([mask1[0], mask1[1], mask1[2], mask1[3]]), 0xFFFF_FFC1);
    }
}
