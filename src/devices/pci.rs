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
            0x04 => self.command as u8,
            0x05 => (self.command >> 8) as u8,
            0x06 => self.status as u8,
            0x07 => (self.status >> 8) as u8,
            0x0E => self.header_type,
            // BAR registers: return sizing mask when value is all 1s
            0x10..=0x27 => {
                let bar_idx = ((offset - 0x10) / 4) as usize;
                let bar_byte = (offset - 0x10) % 4;
                if bar_idx < 6 && self.bar_masks[bar_idx] != 0 {
                    // Check if the stored BAR value is all 1s (sizing mode)
                    let bar_reg = 0x10 + (bar_idx as u8) * 4;
                    let stored = (self.config_regs[bar_reg as usize] as u32)
                        | ((self.config_regs[(bar_reg + 1) as usize] as u32) << 8)
                        | ((self.config_regs[(bar_reg + 2) as usize] as u32) << 16)
                        | ((self.config_regs[(bar_reg + 3) as usize] as u32) << 24);
                    if stored == 0xFFFFFFFF {
                        // Sizing mode: return mask
                        let mask = self.bar_masks[bar_idx];
                        ((mask >> (bar_byte * 8)) & 0xFF) as u8
                    } else {
                        self.config_regs[offset as usize]
                    }
                } else {
                    self.config_regs[offset as usize]
                }
            }
            _ => self.config_regs[offset as usize],
        }
    }

    fn set_byte(&mut self, offset: u8, value: u8) {
        match offset {
            0x04 => self.command = (self.command & 0xFF00) | value as u16,
            0x05 => self.command = (self.command & 0x00FF) | ((value as u16) << 8),
            0x06 => self.status = (self.status & 0xFF00) | value as u16,
            0x07 => self.status = (self.status & 0x00FF) | ((value as u16) << 8),
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
    /// (tarea 1) Última asignación detectada de los BARs VGA (dev 2:0):
    /// Some(base_gpa) cuando el guest escribe una dirección real de memoria.
    /// La consume el hook de `DeviceBus::out()` (devices/mod.rs).
    pub last_vga_lfb_bar_write: Option<u32>,
    pub last_vga_mmio_bar_write: Option<u32>,
}

impl PciBus {
    pub fn new() -> Self {
        eprintln!("[PCI] Bus PCI inicializado (bus=0, 32 slots × 8 funcs)");
        Self {
            addr_reg: 0,
            devices: vec![vec![None; 8]; 32],
            last_acpi_config_write: None,
            usb_uhci: None,
            last_vga_lfb_bar_write: None,
            last_vga_mmio_bar_write: None,
        }
    }

    /// Conecta el estado compartido del controlador USB UHCI
    /// (para notificar el I/O base asignado al BAR4)
    pub fn connect_usb(&mut self, state: Arc<Mutex<super::usb_uhci::UhciState>>) {
        self.usb_uhci = Some(state);
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
            // Detect writes to USB UHCI BAR4 (device 1, function 2, reg 0x20)
            if dev_num == 1 && fn_num == 2 && byte_off >= 0x20 && byte_off < 0x24 {
                let raw_bar = (dev.config_regs[0x20] as u32)
                    | ((dev.config_regs[0x21] as u32) << 8)
                    | ((dev.config_regs[0x22] as u32) << 16)
                    | ((dev.config_regs[0x23] as u32) << 24);
                // Skip sizing writes (all 1s) — only set iobase for real addresses
                if raw_bar != 0xFFFFFFFF && raw_bar & 1 != 0 {
                    let iobase = (raw_bar & 0xFFE0) as u16;
                    if iobase != 0 && iobase >= 0x400 {
                        usb_new_iobase = Some(iobase);
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
        if let Some(new_base) = usb_new_iobase {
            if let Some(ref usb_state) = self.usb_uhci {
                let mut state = usb_state.lock().unwrap();
                if state.iobase != new_base {
                    state.iobase = new_base;
                    eprintln!("[PCI] USB UHCI I/O base: 0x{:04X} (dev 1:2 BAR4)", new_base);
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
                eprintln!(
                    "[PCI] CONFIG ADDR → bus={:#x} dev={:#x} fn={:#x} reg_off={:#x}",
                    (addr >> 16) & 0xFF,
                    (addr >> 11) & 0x1F,
                    (addr >> 8) & 0x07,
                    addr & 0xFC
                );
            }
            PORT_DATA | 0xCFD | 0xCFE | 0xCFF => {
                if self.access_enabled() {
                    self.write_bytes(port, data);
                    eprintln!(
                        "[PCI] CONFIG WRITE → data={}",
                        data.iter().map(|b| format!("{:02x}", b)).collect::<String>()
                    );
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
            let result = self.read_bytes(port, count);
            if result != vec![0xFF; count] {
                eprintln!(
                    "[PCI] CONFIG READ → reg_off={:#x} data=[{}]",
                    self.addr_reg & 0xFC,
                    result.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
                );
            }
            result
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
        let mut ide = PciDevice::new(0x8086, 0x7010, 0x01, 0x01, 0x80);
        // prog_if 0x80 = legacy IDE mode (both channels, interrupt mode)
        ide.revision = 0x00;
        // BAR0 (reg 0x10): Primary command block I/O (0x1F0), 8 bytes → mask 0xFFFFFFF8
        ide.set_bar_mask(0x10, 0xFFFFFFF8);
        // BAR1 (reg 0x14): Primary control block I/O (0x3F6), 4 bytes → mask 0xFFFFFFFC
        ide.set_bar_mask(0x14, 0xFFFFFFFC);
        // BAR2 (reg 0x18): Secondary command block I/O (0x170), 8 bytes → mask 0xFFFFFFF8
        ide.set_bar_mask(0x18, 0xFFFFFFF8);
        // BAR3 (reg 0x1C): Secondary control block I/O (0x376), 4 bytes → mask 0xFFFFFFFC
        ide.set_bar_mask(0x1C, 0xFFFFFFFC);
        // BAR4 (reg 0x20): Bus master I/O, 16 bytes → mask 0xFFFFFFF0
        ide.set_bar_mask(0x20, 0xFFFFFFF0);
        self.add_device(1, 1, ide);

        // Function 2: USB UHCI
        let mut usb = PciDevice::new(0x8086, 0x7020, 0x0C, 0x03, 0x00);
        usb.revision = 0x00;
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
}
