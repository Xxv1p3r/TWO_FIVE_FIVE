//! Dispositivos legacy que SeaBIOS espera encontrar:
//! - DebugCon (0x402): salida de log de SeaBIOS (CONFIG_DEBUG_SERIAL_PORT).
//! - CMOS/RTC (0x70/0x71): mapa de memoria del sistema, hora, estado de boot.

use super::IoDevice;
use crate::guest_mem::GuestMemory;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

// ─── DebugCon: puerto de salida de caracteres del BIOS ─────────────
// Additionally mirrors output to VGA text buffer at 0xB8000 so BIOS
// messages appear on the display.
pub struct DebugCon {
    /// Cursor row in VGA text buffer (0-24)
    vga_row: usize,
    /// Cursor column in VGA text buffer (0-79)
    vga_col: usize,
    /// Memoria del guest con bounds-check (tarea 18): sustituye al puntero
    /// crudo que antes se guardaba como usize para cruzar hilos.
    guest_mem: Option<Arc<GuestMemory>>,
    /// (tarea 13) Mirror de la salida del BIOS al buffer de texto VGA
    /// (0xB8000). Por defecto OFF: el guest ya pinta su propia pantalla vía
    /// INT 10h y el espejo duplicaba/pisaba caracteres. Se activa con
    /// MI_VMM_MIRROR_BIOS=1 para depurar arranques que no llegan a pintar.
    mirror_vga: bool,
}

impl DebugCon {
    pub const PORT: u16 = 0x402;
    const VGA_BASE: usize = 0xB8000;
    const VGA_COLS: usize = 80;
    const VGA_ROWS: usize = 25;

    pub fn new() -> Self {
        Self {
            vga_row: 0,
            vga_col: 0,
            guest_mem: None,
            mirror_vga: std::env::var("MI_VMM_MIRROR_BIOS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }

    /// (tarea 13) ¿Está activo el espejo VGA de la salida del BIOS?
    pub fn mirror_enabled(&self) -> bool {
        self.mirror_vga
    }

    /// Set the guest memory handle for VGA mirroring.
    pub fn set_guest_mem(&mut self, mem: Arc<GuestMemory>) {
        self.guest_mem = Some(mem);
    }

    /// Reset del DebugCon: vuelve el cursor del espejo VGA al inicio.
    pub fn reset(&mut self) {
        self.vga_row = 0;
        self.vga_col = 0;
    }

    /// Write a character to the VGA text buffer at current cursor position.
    /// Todas las escrituras van por `GuestMemory` con bounds-check (tarea 18).
    fn vga_putchar(&mut self, ch: u8) {
        let Some(mem) = self.guest_mem.as_ref() else { return };

        match ch {
            b'\n' => {
                self.vga_col = 0;
                self.vga_row += 1;
            }
            b'\r' => {
                self.vga_col = 0;
            }
            b'\t' => {
                self.vga_col = (self.vga_col + 8) & !7;
            }
            _ => {
                let off = Self::VGA_BASE + (self.vga_row * Self::VGA_COLS + self.vga_col) * 2;
                // Fuera de rango → write() devuelve false y no toca memoria.
                mem.write(off, ch);
                mem.write(off + 1, 0x07); // light gray on black
                self.vga_col += 1;
            }
        }
        // Scroll up if past bottom
        if self.vga_row >= Self::VGA_ROWS {
            // Copy rows 1-24 to rows 0-23. Los rangos se solapan, así que se
            // hace con un buffer intermedio y copias acotadas (sin memmove
            // crudo).
            let row_bytes = Self::VGA_COLS * 2;
            let dst = Self::VGA_BASE;
            let src = Self::VGA_BASE + row_bytes;
            let count = (Self::VGA_ROWS - 1) * row_bytes;
            let mut scratch = [0u8; (Self::VGA_ROWS - 1) * Self::VGA_COLS * 2];
            if mem.copy_from(src, &mut scratch) == count {
                mem.copy_to(dst, &scratch[..count]);
            }
            // Clear last row
            for i in 0..Self::VGA_COLS {
                let off = Self::VGA_BASE + (Self::VGA_ROWS - 1) * Self::VGA_COLS * 2 + i * 2;
                mem.write(off, b' ');
                mem.write(off + 1, 0x07);
            }
            self.vga_row = Self::VGA_ROWS - 1;
        }
    }
}

impl Default for DebugCon {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for DebugCon {
    fn matches_port(&self, port: u16) -> bool {
        port == Self::PORT
    }

    fn write(&mut self, _port: u16, data: &[u8]) {
        // (tarea 13) Mirror to VGA text buffer: solo opt-in vía MI_VMM_MIRROR_BIOS=1
        if self.mirror_vga {
            for &b in data {
                self.vga_putchar(b);
            }
        }
        // Also write to stderr
        use std::io::Write;
        let mut err = std::io::stderr();
        for &b in data {
            if b == b'\n' {
                let _ = err.write_all(b"\n[BIOS] ");
            } else {
                let _ = err.write_all(&[b]);
            }
        }
        let _ = err.flush();
    }

    fn read(&mut self, _port: u16, _count: usize) -> Vec<u8> {
        // QEMU_DEBUGCON_READBACK = 0xE9 (paravirt.h). SeaBIOS's
        // qemu_debug_preinit() reads 0x402 and if it doesn't return 0xE9
        // it sets DebugOutputPort=0, disabling ALL QEMU debug output.
        vec![0xE9]
    }
}


// ─── POST codes: puerto 0x80, indicador de progreso del BIOS ───────
pub struct PostCode {
    pub last: u8,
}

impl PostCode {
    pub const PORT: u16 = 0x80;
    pub fn new() -> Self {
        Self { last: 0 }
    }

    pub fn reset(&mut self) {
        self.last = 0;
    }
}

impl Default for PostCode {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for PostCode {
    fn matches_port(&self, port: u16) -> bool {
        port == Self::PORT
    }

    fn write(&mut self, _port: u16, data: &[u8]) {
        if let Some(&v) = data.first() {
            self.last = v;
            eprintln!("[POST] código 0x{:02X}", v);
        }
    }

    fn read(&mut self, _port: u16, _count: usize) -> Vec<u8> {
        vec![self.last]
    }
}
// ─── CMOS/RTC: 0x70 = índice, 0x71 = dato ──────────────────────────
pub struct CmosRtc {
    index: u8,
    /// Start time for RTC emulation
    rtc_start: std::time::Instant,
    /// Status C: UF (Update Finished) interrupt pending bit
    status_c: u8,
    /// Track the last second we saw (to detect second transitions)
    last_second: u64,
    /// CMOS RAM (128 bytes) — writable registers
    ram: [u8; 128],
}

impl CmosRtc {
    pub const PORT_INDEX: u16 = 0x70;
    pub const PORT_DATA: u16 = 0x71;

    pub fn new() -> Self {
        Self::with_ram_size(512 * 1024 * 1024)
    }

    pub fn with_ram_size(ram_size: u64) -> Self {
        let mut ram = [0u8; 128];
        ram[0x0F] = 0x00; // Shutdown status: normal boot
        ram[0x10] = 0x00; // Equipment byte: 0 floppies
        ram[0x14] = 0x23; // Equipment: 80x25 color, keyboard installed

        // Base memory: 640 KB (0x0280)
        ram[0x15] = 0x80;
        ram[0x16] = 0x02;

        // Extended memory 1MB-16MB in KB (max 15MB = 15360 KB = 0x3C00)
        let ext_16m = if ram_size > 16 * 1024 * 1024 {
            15 * 1024 // 15360 KB = 0x3C00
        } else if ram_size > 1024 * 1024 {
            ((ram_size - 1024 * 1024) / 1024) as u16
        } else {
            0
        };
        ram[0x17] = ext_16m as u8;
        ram[0x18] = (ext_16m >> 8) as u8;
        ram[0x30] = ext_16m as u8;
        ram[0x31] = (ext_16m >> 8) as u8;

        // Extended memory >16MB in 64KB chunks
        let ext_above_16m = if ram_size > 16 * 1024 * 1024 {
            ((ram_size - 16 * 1024 * 1024) / 65536) as u16
        } else {
            0
        };
        ram[0x34] = ext_above_16m as u8;
        ram[0x35] = (ext_above_16m >> 8) as u8;

        Self {
            index: 0,
            rtc_start: std::time::Instant::now(),
            status_c: 0,
            last_second: 0,
            ram,
        }
    }

    /// Reset del RTC/CMOS: limpia el estado volátil (registro índice,
    /// Status C, byte de shutdown) pero conserva la hora y la RAM CMOS
    /// respaldada por batería (persisten entre resets en hardware real).
    pub fn reset(&mut self) {
        self.index = 0;
        self.status_c = 0;
        self.last_second = self.current_second();
        // Shutdown status: boot normal (evita que SeaBIOS entre en la
        // rutina de shutdown tras un reset).
        self.ram[0x0F] = 0x00;
    }

    /// Convert a binary value to BCD (Binary Coded Decimal).
    fn to_bcd(val: u8) -> u8 {
        ((val / 10) << 4) | (val % 10)
    }

    /// Check for RTC update cycle: set UF bit when second transitions.
    /// This is called on every read to simulate the RTC update interrupt.
    fn update_rtc_cycle(&mut self) {
        let current_second = self.rtc_start.elapsed().as_secs();
        if current_second != self.last_second {
            // Second transition detected: set UF (Update Finished) bit in Status C
            self.status_c |= 0x10; // bit 4 = UF
            self.last_second = current_second;
        }
    }

    /// Current second (real time, BCD-decoded)
    fn current_second(&self) -> u64 {
        self.rtc_start.elapsed().as_secs()
    }

    /// Valores CMOS que reportamos al guest.
    fn read_reg(&mut self, reg: u8) -> u8 {
        match reg {
            // RTC: tiempo real emulado
            0x00 => { // Seconds
                self.update_rtc_cycle();
                let elapsed = self.current_second() % 60;
                Self::to_bcd(elapsed as u8)
            }
            0x02 => { // Minutes
                let elapsed = self.current_second() / 60;
                Self::to_bcd((elapsed % 60) as u8)
            }
            0x04 => { // Hours
                let elapsed = self.current_second() / 3600;
                Self::to_bcd((elapsed % 24) as u8)
            }
            0x06 => 1, // Day of week (Monday)
            0x07 => 1, // Day of month
            0x08 => 1, // Month
            0x09 => 25, // Year (2025)
            0x01..=0x03 | 0x05 => 0,
            0x0A => {
                // Status A: bit 7 = UIP (Update In Progress)
                // We set UIP=1 for a brief window (~244μs) before the second changes,
                // then clear it. This allows SeaBIOS pmtimer calibration to complete.
                let elapsed_ns = self.rtc_start.elapsed().as_nanos() as u64;
                let nanos_in_second = elapsed_ns % 1_000_000_000;
                // UIP=1 during the last 244μs before second boundary
                let uip = if nanos_in_second >= 999_756_000 { 0x80 } else { 0x00 };
                uip | 0x26 // dividers: 32768 Hz, 22-stage divider
            }
            0x0B => 0x02, // Status B: 24h, BCD
            0x0C => {
                // Status C: read clears the register
                let val = self.status_c;
                self.status_c = 0; // Clear all pending interrupts on read
                val
            }
            0x0D => 0x80, // Status D: batería OK
            0x0F => 0,    // Shutdown status: boot normal (soft reset)
            0x3D..=0x3F => 0, // sin option ROMs
            // Para registros de memoria (0x14-0x18, 0x30-0x35) y registros escribibles:
            _ => self.ram[reg as usize],
        }
    }
}

impl Default for CmosRtc {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for CmosRtc {
    fn matches_port(&self, port: u16) -> bool {
        port == Self::PORT_INDEX || port == Self::PORT_DATA
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if port == Self::PORT_INDEX && !data.is_empty() {
            self.index = data[0] & 0x7F; // bit 7 = NMI mask, ignorado
        }
        if port == Self::PORT_DATA && !data.is_empty() {
            // Store value in CMOS RAM
            let idx = self.index as usize;
            if idx < 128 {
                self.ram[idx] = data[0];
            }
        }
    }

    fn read(&mut self, port: u16, _count: usize) -> Vec<u8> {
        if port == Self::PORT_DATA {
            let val = self.read_reg(self.index);
           // Track which CMOS registers are being polled (using atomics, not static mut)
            static CMOS_REG_COUNTS: [AtomicU32; 128] = [const { AtomicU32::new(0) }; 128];
            static CMOS_TOTAL: AtomicU32 = AtomicU32::new(0);
            let idx = (self.index & 0x7F) as usize;
            if idx < 128 {
                CMOS_REG_COUNTS[idx].fetch_add(1, Ordering::Relaxed);
            }
            let total = CMOS_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
            if total == 500 || total == 1000 || total == 1500 {
                // Find top CMOS registers
                let mut regs: Vec<(u8, u32)> = Vec::new();
                for i in 0..128u8 {
                    let c = CMOS_REG_COUNTS[i as usize].load(Ordering::Relaxed);
                    if c > 0 {
                        regs.push((i, c));
                    }
                }
               regs.sort_by(|a, b| b.1.cmp(&a.1));
                let top: Vec<String> = regs.iter().take(10)
                    .map(|(r, c)| format!("0x{:02X}:{}", r, c))
                    .collect();
                eprintln!("[CMOS] total={}, top: {}", total, top.join(", "));     
            }
            vec![val]
        } else {
            vec![self.index]
        }
    }
}

// ─── A20 Gate: System Control Port A (0x92) ──────────────────────
/// Controla la línea A20 y el reset rápido del sistema.
/// - Bit 0: A20 gate (1 = habilitado, permite acceder a memoria >1MB)
/// - Bit 1: Reset CPU (escribir 1 genera reset)
pub struct A20Gate {
    /// Estado del puerto: bit 0 = A20 habilitado
    state: u8,
}

impl A20Gate {
    pub const PORT: u16 = 0x92;

    pub fn new() -> Self {
        // Bit 0 = A20 gate (0=disabled, 1=enabled), Bit 1 = fast reset.
        // QEMU default: A20 disabled (bit 0 = 0). SeaBIOS enables it early.
        Self { state: 0x02 }
    }

    /// ¿El A20 está habilitado?
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.state & 0x01 != 0
    }

    /// Reset del A20: vuelve al estado inicial (A20 deshabilitado, como
    /// un reset del chipset).
    pub fn reset(&mut self) {
        self.state = 0x02;
    }
}

impl Default for A20Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for A20Gate {
    fn matches_port(&self, port: u16) -> bool {
        port == Self::PORT
    }

    fn write(&mut self, _port: u16, data: &[u8]) {
        if let Some(&val) = data.first() {
            self.state = val;
            // Bit 1 = reset rápido. Ignorado.
        }
    }

    fn read(&mut self, _port: u16, _count: usize) -> Vec<u8> {
       static A20_READ_LOG: AtomicU32 = AtomicU32::new(0);
        let count = A20_READ_LOG.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 10 {
            eprintln!("[A20] read state=0x{:02X}", self.state);
        }
        vec![self.state]
    }
}


// ─── PIIX3 ACPI/PM I/O ports ───────────────────────────────────
// QEMU PIIX4 PM layout (piix4.c):
//   PCI config reg 0x40: PM I/O base (masked with 0xFFC0)
//   PCI config reg 0x80 bit 0: PM I/O space enable
//   PM I/O region: 64 bytes from pm_base
//     base+0x00: PM1a Event Status (2 bytes)
//     base+0x02: PM1a Event Enable (2 bytes)
//     base+0x04: PM1a Control (2 bytes)
//     base+0x08: PM Timer (4 bytes, 24-bit @ 3.579545 MHz)
//   GPE0: hardcoded at 0xAFE0 (4 bytes)
//   SMBus: PCI reg 0x90 base, reg 0xD2 enable
//
// SeaBIOS source (pciinit.c):
//   piix4_pm_config_setup():
//     pci_config_writel(bdf, PIIX_PMBASE, acpi_pm_base | 1);   // reg 0x40
//     pci_config_writeb(bdf, PIIX_PMREGMISC, 0x01);            // reg 0x80
//   piix4_pm_setup():
//     pmtimer_setup(acpi_pm_base + 0x08);  // PM Timer at base+8

/// Frecuencia del PM Timer en Hz (ACPI spec 3.0, section 4.2.4).
const PM_TIMER_HZ: u64 = 3_579_545;

pub struct AcpiPm {
    /// PM1a Event Status (base+0x00, 2 bytes)
    pm1a_sts: u16,
    /// PM1a Event Enable (base+0x02, 2 bytes)
    pm1a_en: u16,
    /// PM1a Control (base+0x04, 2 bytes)
    pm1a_cnt: u16,
    /// PM Timer start time
    pm_timer_start: Instant,
    /// Last PM Timer value returned (to ensure monotonic increase)
    pm_timer_last: u32,
    /// PM I/O base address (configured via PCI config reg 0x40)
    pm_base: u16,
    /// PM I/O space enabled (PCI config reg 0x80 bit 0)
    pm_enabled: bool,
    /// GPE0 Status (4 bytes at 0xAFE0)
    gpe0_sts: u8,
    /// GPE0 Enable (4 bytes at 0xAFE0)
    gpe0_en: u8,
    /// SMBus I/O base (from PCI config reg 0x90)
    smb_base: u16,
    /// SMBus enabled (PCI config reg 0xD2)
    smb_enabled: bool,
    /// Legacy compat registers (0xB0-0xB7)
    legacy_regs: [u8; 8],
    /// Contador de reads para debug
    read_count: u32,
    /// SLP_EN visto en PM1a_CNT (bit 13): el guest evaluó _S5 y pidió
    /// apagado limpio. El VMM lo consulta para terminar la VM.
    sleep_requested: bool,
}

impl AcpiPm {
    pub fn new() -> Self {
        Self {
            pm1a_sts: 0,
            pm1a_en: 0,
            pm1a_cnt: 0,
            pm_timer_start: Instant::now(),
            pm_timer_last: 0,
            pm_base: 0xB0,  // default QEMU base
            pm_enabled: false,
            gpe0_sts: 0,
            gpe0_en: 0,
            smb_base: 0,
            smb_enabled: false,
            legacy_regs: [0u8; 8],
            read_count: 0,
            sleep_requested: false,
        }
    }

    /// True si el guest escribió SLP_EN en PM1a_CNT (apagado limpio vía _S5).
    pub fn sleep_requested(&self) -> bool {
        self.sleep_requested
    }

    /// Llamado por DeviceBus cuando SeaBIOS escribe al PCI config del PIIX3 ACPI.
    /// reg 0x40: PM I/O base, reg 0x80: PM enable, reg 0x90: SMBus base, reg 0xD2: SMBus enable.
    pub fn update_pci_config(&mut self, reg_off: u8, value: u32) {
        match reg_off {
            0x40 => {
                let new_base = (value & 0xFFC0) as u16;
                if new_base != self.pm_base {
                    eprintln!("[ACPI] PM I/O base configurado: 0x{:04X}", new_base);
                    self.pm_base = new_base;
                }
            }
            0x80 => {
                let enabled = (value as u8) & 0x01 != 0;
                if enabled != self.pm_enabled {
                    eprintln!("[ACPI] PM I/O space {}", if enabled { "HABILITADO" } else { "DESHABILITADO" });
                    self.pm_enabled = enabled;
                }
            }
            0x90 => {
                let new_base = (value as u16) & 0xFFC0;
                if new_base != self.smb_base {
                    eprintln!("[ACPI] SMBus I/O base configurado: 0x{:04X}", new_base);
                    self.smb_base = new_base;
                }
            }
            0xD2 => {
                let enabled = (value as u8) & 0x01 != 0;
                if enabled != self.smb_enabled {
                    eprintln!("[ACPI] SMBus {}", if enabled { "HABILITADO" } else { "DESHABILITADO" });
                    self.smb_enabled = enabled;
                }
            }
            _ => {}
        }
    }

    /// Reset del bloque ACPI/PM: vuelve a los valores iniciales (el PM I/O
    /// base vuelve al legacy 0xB0 y queda deshabilitado hasta que SeaBIOS lo
    /// reprograme por PCI config).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Retorna el PM Timer actual (24-bit counter).
    /// Uses stored counter that always advances to ensure tight polling loops
    /// eventually see a different value.
    fn pm_timer_value(&mut self) -> u32 {
        let elapsed_ns = self.pm_timer_start.elapsed().as_nanos() as u64;
        let ticks = ((elapsed_ns * PM_TIMER_HZ as u64) / 1_000_000_000) as u32;
        let val = ticks & 0x00FFFFFF;
        // Ensure monotonically increasing: use max of (real value, last + 1)
        // This prevents both oscillation AND backwards movement when
        // Instant::now() returns the same nanosecond in a tight loop.
        let next = self.pm_timer_last.wrapping_add(1) & 0x00FFFFFF;
        self.pm_timer_last = std::cmp::max(val, next);
        self.pm_timer_last
    }
}

impl Default for AcpiPm {
    fn default() -> Self { Self::new() }
}

impl IoDevice for AcpiPm {
    fn matches_port(&self, port: u16) -> bool {
        // Rango legacy hardcoded (0xB0-0xB7) — fallback
        if matches!(port, 0xB0..=0xB7) {
            return true;
        }
        // Rango dinámico PM I/O (64 bytes desde pm_base)
        if self.pm_enabled && port >= self.pm_base && port < self.pm_base.wrapping_add(64) {
            return true;
        }
        // GPE0 en 0xAFE0-0xAFE3
        if matches!(port, 0xAFE0..=0xAFE3) {
            return true;
        }
        // SMBus I/O (16 bytes desde smb_base)
        if self.smb_enabled && port >= self.smb_base && port < self.smb_base.wrapping_add(16) {
            return true;
        }
        false
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let val = data[0];

        // GPE0 registers (0xAFE0-0xAFE3)
        if matches!(port, 0xAFE0..=0xAFE3) {
            let off = (port - 0xAFE0) as usize;
            if off == 0 { self.gpe0_sts &= !val; } // Write-1-to-clear
            if off == 1 { self.gpe0_en = val; }
            if off == 2 { self.gpe0_sts &= !val; }
            if off == 3 { self.gpe0_en = val; }
            return;
        }

        // SMBus registers — silently ignore writes
        if self.smb_enabled && port >= self.smb_base && port < self.smb_base.wrapping_add(16) {
            return;
        }

        // Legacy range 0xB0-0xB7
        if matches!(port, 0xB0..=0xB7) {
            let off = (port - 0xB0) as usize;
            if off <= 3 {
                // PM1a Event Status (0xB0-0xB3): write-1-to-clear
                self.legacy_regs[off] &= !val;
            } else {
                // PM1a Control and other regs: direct write
                self.legacy_regs[off] = val;
            }
            return;
        }

        // Dynamic PM I/O range
        if self.pm_enabled && port >= self.pm_base && port < self.pm_base.wrapping_add(64) {
            let off = port - self.pm_base;
            match off {
                0x00..=0x01 => {
                    // PM1a Event Status (2 bytes at base+0x00/0x01): write-1-to-clear
                    let bit_shift = (off * 8) as u16;
                    self.pm1a_sts &= !((val as u16) << bit_shift);
                }
                0x02..=0x03 => {
                  // PM1a Event Enable (2 bytes at base+0x02/0x03)
                    if off == 0x02 {
                        self.pm1a_en = (self.pm1a_en & 0xFF00) | (val as u16);
                    } else {
                        self.pm1a_en = (self.pm1a_en & 0x00FF) | ((val as u16) << 8);
                    }
                }
                0x04..=0x05 => {
                    // PM1a Control: accepts writes, register is read-only.
                    // El guest puede escribir 1 o 2 bytes en un solo OUT
                    // (outw/outl a 0x604): aplicar cada byte a su offset.
                    for (i, &b) in data.iter().enumerate() {
                        match off + i as u16 {
                            0x04 => self.pm1a_cnt = (self.pm1a_cnt & 0xFF00) | (b as u16),
                            0x05 => self.pm1a_cnt = (self.pm1a_cnt & 0x00FF) | ((b as u16) << 8),
                            _ => {}
                        }
                    }
                    // SLP_TYP (bits 12:10) + SLP_EN (bit 13): el guest
                    // evaluó _S5 y escribe SLP_EN para apagar la máquina.
                    // Se detecta aquí para que el VMM salga limpiamente.
                    if self.pm1a_cnt & (1 << 13) != 0 {
                        eprintln!(
                            "[ACPI] SLP_EN detectado (PM1a_CNT=0x{:04X}) — apagado limpio solicitado",
                            self.pm1a_cnt
                        );
                        self.sleep_requested = true;
                    }
                }
                _ => {} // Other PM registers: ignore
            }
            return;
        }
    }

    fn read(&mut self, port: u16, _count: usize) -> Vec<u8> {
        self.read_count += 1;

        // GPE0 registers (0xAFE0-0xAFE3)
        if matches!(port, 0xAFE0..=0xAFE3) {
            let off = (port - 0xAFE0) as usize;
            return vec![match off {
                0 => self.gpe0_sts,
                1 => self.gpe0_en,
                2 => self.gpe0_sts,
                3 => self.gpe0_en,
                _ => 0,
            }];
        }

        // SMBus registers — return 0
        if self.smb_enabled && port >= self.smb_base && port < self.smb_base.wrapping_add(16) {
            return vec![0];
        }

        // Legacy range 0xB0-0xB7
        if matches!(port, 0xB0..=0xB7) {
            return vec![self.legacy_regs[(port - 0xB0) as usize]];
        }

        // Dynamic PM I/O range
        if self.pm_enabled && port >= self.pm_base && port < self.pm_base.wrapping_add(64) {
            let off = port - self.pm_base;
            let result = match off {
                0x00 => (self.pm1a_sts & 0xFF) as u8,
                0x01 => ((self.pm1a_sts >> 8) & 0xFF) as u8,
                0x02 => (self.pm1a_en & 0xFF) as u8,
                0x03 => ((self.pm1a_en >> 8) & 0xFF) as u8,
                0x04 => (self.pm1a_cnt & 0xFF) as u8,
                0x05 => ((self.pm1a_cnt >> 8) & 0xFF) as u8,
                0x08 => {
                    // PM Timer: 24-bit counter, 4-byte read
                    let val = self.pm_timer_value();
                    return vec![val as u8, (val >> 8) as u8, (val >> 16) as u8, 0];
                }
                _ => 0,
            };
            if self.read_count <= 10 {
                eprintln!("[ACPI] PM READ base+{:#x} = 0x{:02X}", off, result);
            }
            return vec![result];
        }

        // Fallback
        vec![0]
    }
}

// ─── Controladora floppy (FDC) — stub sin drives ──────────────────
/// Puertos 0x3F0-0x3F7 (FDC primario), emulando una controladora SIN
/// unidades conectadas (como QEMU con `-fda none`):
/// - DOR (0x3F2): acepta escrituras y permite readback.
/// - MSR (0x3F4): 0x00 → RQM=0/DRQ=0, "controladora no lista". SeaBIOS
///   aborta el probe del floppy y sigue con el resto del boot.
/// - FIFO (0x3F5): acepta comandos sin eco.
/// - DIR (0x3F7 lectura): 0x80 = disk change (sin disco).
/// Sin esto, el `OUT 0x3F2` del guest queda "no manejado" y el estado
/// del probe queda indefinido (KVM InternalError posterior).
pub struct FloppyStub {
    dor: u8,
}

impl FloppyStub {
    pub fn new() -> Self {
        Self { dor: 0x0C } // valor reset típico: motor off, DMA+reset ready
    }

    pub fn reset(&mut self) {
        self.dor = 0x0C;
    }
}

impl Default for FloppyStub {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_pm() -> AcpiPm {
        let mut pm = AcpiPm::new();
        // SeaBIOS programa el PIIX por PCI config (reg 0x40 base, 0x80 enable).
        pm.update_pci_config(0x40, 0x600 | 1);
        pm.update_pci_config(0x80, 0x01);
        pm
    }

    /// Linux escribe SLP_EN en PM1a_CNT (0x604) tras evaluar _S5 → apagado.
    #[test]
    fn slp_en_detects_clean_shutdown_request() {
        let mut pm = configured_pm();
        assert!(!pm.sleep_requested());
        // outw(SLP_TYP=0 | SLP_EN=0x2000, 0x604) como un único OUT de 2 bytes.
        pm.write(0x604, &[0x00, 0x20]);
        assert!(pm.sleep_requested(), "SLP_EN debe marcar el apagado");
        assert_eq!(pm.pm1a_cnt, 0x2000);
        // Reset del bloque PM (reboot/arranque limpio) borra el flag.
        pm.reset();
        assert!(!pm.sleep_requested());
    }

    /// Un outw también puede llegar dividido en dos OUT de 1 byte.
    #[test]
    fn pm1a_cnt_accepts_split_byte_writes() {
        let mut pm = configured_pm();
        pm.write(0x604, &[0x00]);
        pm.write(0x605, &[0x20]);
        assert_eq!(pm.pm1a_cnt, 0x2000);
        assert!(pm.sleep_requested());
    }

    /// Escrituras sin SLP_EN (p. ej. SCI enable) no marcan el apagado.
    #[test]
    fn pm1a_cnt_without_slp_en_ignored() {
        let mut pm = configured_pm();
        pm.write(0x604, &[0x01, 0x00]);
        assert!(!pm.sleep_requested());
    }
}

impl IoDevice for FloppyStub {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port, 0x3F0..=0x3F5 | 0x3F7)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        match port {
            0x3F2 => { if !data.is_empty() { self.dor = data[0]; } }
            // DSR/FIFO/CCR: ignorar (sin controladora activa)
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let v = match port {
            0x3F2 => self.dor,
            0x3F4 => 0x00, // MSR: no RQM, no DIO, no DRQ, no BSY
            0x3F5 => 0x00, // FIFO: sin datos
            0x3F7 => 0x80, // DIR: disk changed (sin disco)
            _ => 0x00,
        };
        vec![v; count]
    }
}

// ─── Platform stubs: LPT (parallel) + COM2-4 ──────────────────────
/// Puertos LPT1 (0x378-0x37A), LPT2 (0x278-0x27A)
/// y COM2 (0x2F8), COM3 (0x3E8), COM4 (0x2E8).
/// SeaBIOS sondea estos puertos; si no responden, asume que no existen.
pub struct PlatformStubs {
    /// LPT data port latch (para que el probe write/readback funcione)
    lpt1_data: u8,
    lpt2_data: u8,
}

impl PlatformStubs {
    pub fn new() -> Self {
        Self { lpt1_data: 0, lpt2_data: 0 }
    }

    pub fn reset(&mut self) {
        self.lpt1_data = 0;
        self.lpt2_data = 0;
    }
}

impl Default for PlatformStubs {
    fn default() -> Self { Self::new() }
}

impl IoDevice for PlatformStubs {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port,
            0x378..=0x37A | 0x278..=0x27A |  // LPT1/LPT2
            0x2F8..=0x2FF | 0x3E8..=0x3EF | 0x2E8..=0x2EF  // COM2/3/4
        )
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        match port {
            0x378 => { self.lpt1_data = data[0]; }
            0x278 => { self.lpt2_data = data[0]; }
            _ => {} // COM/LPT control: ignore
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        match port {
            // LPT data readback (SeaBIOS escribe 0xAA y espera leer 0xAA)
            0x378 => vec![self.lpt1_data; count],
            0x278 => vec![self.lpt2_data; count],
            // LPT status: bit 7 = not busy, bits 4-3 = ACK/error flags
            0x379 | 0x279 => vec![0xDF; count], // all lines high
            // LPT control: bits 0-2 active, bit 3 = IRQ enable (low)
            0x37A | 0x27A => vec![0x04; count],
            // COM IER (offset1): no interrupts
            0x2F9 | 0x3E9 | 0x2E9 => vec![0x00; count],
            // COM IIR/FCR (offset2): no interrupt pending, 16550 FIFO
            0x2FA | 0x3EA | 0x2EA => vec![0xC1; count],
            // COM LSR: THRE(bit5) + TEMT(bit6) = transmitter empty
            0x2FD | 0x3FD | 0x2ED => vec![0x60; count],
            // COM MSR (offset6): DSR+CTS+DCD ready
            0x2FE | 0x3EE | 0x2EE => vec![0xB0; count],
            _ => vec![0x00; count],
        }
    }
}

// ─── APM / SMI Device (0xB2/0xB3) — Item 23 (SMM) ────────────────
/// Emulación del puerto de control APM (0xB2) y datos (0xB3).
/// Usado por el firmware/SO para disparar System Management Interrupts (SMI)
/// y habilitar/deshabilitar ACPI según el valor de SMI_CMD en la FADT.
pub struct ApmSmiDevice {
    pub cmd: u8,
    pub data: u8,
    pub smi_requested: bool,
}

impl ApmSmiDevice {
    pub const PORT_CMD: u16 = 0xB2;
    pub const PORT_DATA: u16 = 0xB3;

    pub fn new() -> Self {
        Self {
            cmd: 0,
            data: 0,
            smi_requested: false,
        }
    }

    pub fn reset(&mut self) {
        self.cmd = 0;
        self.data = 0;
        self.smi_requested = false;
    }

    pub fn take_smi(&mut self) -> bool {
        let r = self.smi_requested;
        self.smi_requested = false;
        r
    }
}

impl Default for ApmSmiDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for ApmSmiDevice {
    fn matches_port(&self, port: u16) -> bool {
        port == Self::PORT_CMD || port == Self::PORT_DATA
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if port == Self::PORT_CMD {
            self.cmd = data[0];
            self.smi_requested = true;
        } else if port == Self::PORT_DATA {
            self.data = data[0];
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let val = if port == Self::PORT_CMD { self.cmd } else { self.data };
        vec![val; count]
    }
}

// ─── TSS Real para prevención de Triple Fault (Item 23) ────────────
/// Configura una estructura Task State Segment (TSS) de 32 bits real en memoria del guest.
///
/// Provee una pila limpia aislada (`ESP0`) y descriptores adecuados para que un Double
/// Fault (#DF, Vector 8) no se convierta instantáneamente en un Triple Fault (#TF)
/// si ocurre corrupción o desbordamiento de pila en el guest.
pub fn init_guest_tss(guest_mem: &mut [u8], tss_addr: usize, stack_addr: usize) -> bool {
    let tss_size = 104usize;
    if tss_addr + tss_size > guest_mem.len() || stack_addr > guest_mem.len() {
        return false;
    }
    guest_mem[tss_addr..tss_addr + tss_size].fill(0);
    // ESP0 en offset 4 (u32 LE)
    let sp0 = (stack_addr as u32).to_le_bytes();
    guest_mem[tss_addr + 4..tss_addr + 8].copy_from_slice(&sp0);
    // SS0 en offset 8 (selector de datos de kernel 0x10)
    let ss0 = 0x10u16.to_le_bytes();
    guest_mem[tss_addr + 8..tss_addr + 10].copy_from_slice(&ss0);
    // I/O Map Base Address en offset 102 = 104 (fin de TSS, sin mapa I/O bitmap)
    let iomap_base = 104u16.to_le_bytes();
    guest_mem[tss_addr + 102..tss_addr + 104].copy_from_slice(&iomap_base);
    true
}
