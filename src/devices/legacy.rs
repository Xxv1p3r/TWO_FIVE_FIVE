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
    line_buf: Vec<u8>,
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
            line_buf: Vec::with_capacity(256),
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
        if crate::tui::is_active() {
            for &b in data {
                if b == b'\n' {
                    let s = format!("[BIOS] {}", String::from_utf8_lossy(&self.line_buf));
                    self.line_buf.clear();
                    crate::tui::log(s);
                } else if b != b'\r' {
                    self.line_buf.push(b);
                }
            }
            return;
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
// ─── Algoritmo civil gregoriano (Howard Hinnant) para fecha UTC ───────
fn civil_from_days(days: i64) -> (u16, u8, u8) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 142456) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as u16, m as u8, d as u8)
}

// ─── CMOS/RTC: Motorola MC146818 con extensiones PIIX4 (DevRTC.cpp) ───
/// Emula el reloj de tiempo real MC146818 y la RAM NVRAM CMOS con dos bancos
/// de 128 bytes (256 bytes totales). Banco 0 en puertos 0x70/0x71 y Banco 1 en
/// puertos 0x72/0x73, idéntico a VirtualBox (`DevRTC.cpp`).
pub struct CmosRtc {
    /// cmos_index[0] para Banco 0 (0..127), cmos_index[1] para Banco 1 (0..127)
    index: [u8; 2],
    /// Start time for RTC emulation
    rtc_start: Instant,
    /// Unix timestamp base at start (seconds)
    rtc_base_unix_secs: u64,
    /// Status C: UF (Update Finished) interrupt pending bit
    status_c: u8,
    /// Track the last second we saw (to detect second transitions)
    last_second: u64,
    /// CMOS RAM (256 bytes: 0..128 Banco 0, 128..256 Banco 1)
    ram: [u8; 256],
}

impl CmosRtc {
    pub const PORT_INDEX: u16 = 0x70;
    pub const PORT_DATA: u16 = 0x71;
    pub const PORT_INDEX_EXT: u16 = 0x72;
    pub const PORT_DATA_EXT: u16 = 0x73;

    pub fn new() -> Self {
        Self::with_ram_size(512 * 1024 * 1024)
    }

    pub fn with_ram_size(ram_size: u64) -> Self {
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(1774828800);

        let mut ram = [0u8; 256];
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
            (((ram_size - 16 * 1024 * 1024) / 65536) as u64).min(0xFFFF) as u16
        } else {
            0
        };
        ram[0x34] = ext_above_16m as u8;
        ram[0x35] = (ext_above_16m >> 8) as u8;

        // High memory above 4GB in 64KB chunks (SeaBIOS standard: 0x5b, 0x5c, 0x5d)
        if ram_size > 0xE000_0000 {
            let high_chunks = ((ram_size - 0xE000_0000) / 65536) as u32;
            ram[0x5B] = high_chunks as u8;
            ram[0x5C] = (high_chunks >> 8) as u8;
            ram[0x5D] = (high_chunks >> 16) as u8;
        }

        // Boot sequence: SeaBIOS / VirtualBox standard
        // 0x38: Boot sequence (0x31: CD-ROM then HDD)
        // 0x3D: 1st boot drive (0x20 = CDROM, 0x02 = HDD)
        ram[0x38] = 0x31;
        ram[0x3D] = 0x20;

        // Century bytes (0x32: Century BCD, 0x37: IBM PS/2 Date Century BCD)
        let days = (unix_secs / 86400) as i64;
        let (cur_y, _, _) = civil_from_days(days);
        let century_bcd = Self::to_bcd((cur_y / 100) as u8);
        ram[0x32] = century_bcd;
        ram[0x37] = century_bcd;

        // Calculate standard PC/AT CMOS checksum across 0x10..=0x2D
        let mut sum: u16 = 0;
        for i in 0x10..=0x2D {
            sum = sum.wrapping_add(ram[i] as u16);
        }
        ram[0x2E] = (sum >> 8) as u8;
        ram[0x2F] = (sum & 0xFF) as u8;

        Self {
            index: [0, 0],
            rtc_start: Instant::now(),
            rtc_base_unix_secs: unix_secs,
            status_c: 0,
            last_second: 0,
            ram,
        }
    }

    /// Reset del RTC/CMOS: limpia el estado volátil (registro índice,
    /// Status C, byte de shutdown) pero conserva la hora y la RAM CMOS
    /// respaldada por batería (persisten entre resets en hardware real).
    pub fn reset(&mut self) {
        self.index = [0, 0];
        self.status_c = 0;
        self.last_second = self.rtc_start.elapsed().as_secs();
        // Shutdown status: boot normal
        self.ram[0x0F] = 0x00;
    }

    /// Convert a binary value to BCD (Binary Coded Decimal).
    fn to_bcd(val: u8) -> u8 {
        ((val / 10) << 4) | (val % 10)
    }

    /// Recalcula el checksum estándar PC/AT (suma 16-bit de 0x10..=0x2D)
    pub fn recalc_crc(&mut self) {
        let mut sum: u16 = 0;
        for i in 0x10..=0x2D {
            sum = sum.wrapping_add(self.ram[i] as u16);
        }
        self.ram[0x2E] = (sum >> 8) as u8;
        self.ram[0x2F] = (sum & 0xFF) as u8;
    }

    /// Retorna los componentes actuales del calendario UTC (año, mes, día, hora, min, seg, día_semana)
    fn current_utc(&self) -> (u16, u8, u8, u8, u8, u8, u8) {
        let s = self.rtc_base_unix_secs + self.rtc_start.elapsed().as_secs();
        let sec = (s % 60) as u8;
        let min = ((s / 60) % 60) as u8;
        let hour = ((s / 3600) % 24) as u8;
        let days = (s / 86400) as i64;
        let wday = ((days + 4) % 7) as u8 + 1; // 1=Sunday..7=Saturday
        let (year, month, day) = civil_from_days(days);
        (year, month, day, hour, min, sec, wday)
    }

    /// Check for RTC update cycle: set UF bit when second transitions.
    fn update_rtc_cycle(&mut self) {
        let current_second = self.rtc_start.elapsed().as_secs();
        if current_second != self.last_second {
            // Second transition detected: set UF (Update Finished) bit in Status C
            self.status_c |= 0x10; // bit 4 = UF
            self.last_second = current_second;
        }
    }

    /// Valores CMOS que reportamos al guest para Banco 0.
    fn read_reg(&mut self, reg: u8) -> u8 {
        let (year, month, day, hour, min, sec, wday) = self.current_utc();
        match reg {
            // RTC: tiempo real UTC emulado
            0x00 => { // Seconds
                self.update_rtc_cycle();
                Self::to_bcd(sec)
            }
            0x02 => Self::to_bcd(min), // Minutes
            0x04 => Self::to_bcd(hour), // Hours
            0x06 => wday,               // Day of week (1..=7)
            0x07 => Self::to_bcd(day),  // Day of month
            0x08 => Self::to_bcd(month), // Month
            0x09 => Self::to_bcd((year % 100) as u8), // Year
            0x01..=0x03 | 0x05 => 0,
            0x0A => {
                // Status A: bit 7 = UIP (Update In Progress)
                let elapsed_ns = self.rtc_start.elapsed().as_nanos() as u64;
                let nanos_in_second = elapsed_ns % 1_000_000_000;
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
            0x0F => 0x00, // Shutdown status: boot normal (soft reset)
            0x32 | 0x37 => Self::to_bcd((year / 100) as u8),
            // Registros de memoria, configuración y NVRAM de Banco 0:
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
        matches!(port, 0x70..=0x73)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let val = data[0];
        match port {
            0x70 => {
                self.index[0] = val & 0x7F; // bit 7 = NMI mask
            }
            0x71 => {
                let idx = self.index[0] as usize;
                if idx < 128 {
                    self.ram[idx] = val;
                    if (0x10..=0x2D).contains(&idx) {
                        self.recalc_crc();
                    }
                }
            }
            0x72 => {
                self.index[1] = val & 0x7F;
            }
            0x73 => {
                let idx = 128 + (self.index[1] as usize);
                if idx < 256 {
                    self.ram[idx] = val;
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, _count: usize) -> Vec<u8> {
        match port {
            0x70 | 0x72 => vec![0xFF], // Motorola MC146818 & VirtualBox DevRTC line 373: index ports return 0xFF on read
            0x71 => {
                let val = self.read_reg(self.index[0]);
                vec![val]
            }
            0x73 => {
                let idx = 128 + (self.index[1] as usize);
                vec![self.ram[idx]]
            }
            _ => vec![0xFF],
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

    /// Simula la pulsación del botón de encendido ACPI (PWRBTN_STS).
    /// Devuelve true si la interrupción SCI debe dispararse (PWRBTN_EN == 1).
    pub fn trigger_power_button(&mut self) -> bool {
        self.pm1a_sts |= 0x0100;
        (self.pm1a_en & 0x0100) != 0
    }

    /// Fuerza la solicitud de apagado limpio.
    pub fn request_shutdown(&mut self) {
        self.sleep_requested = true;
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
                0x08..=0x0B => {
                    // PM Timer: 24-bit counter, 4-byte read or sub-byte reads
                    let val = self.pm_timer_value();
                    let shift = (off - 0x08) * 8;
                    if _count <= 1 {
                        return vec![(val >> shift) as u8];
                    }
                    let mut bytes = Vec::with_capacity(_count);
                    for i in 0.._count {
                        bytes.push(((val >> ((off - 0x08 + i as u16) * 8)) & 0xFF) as u8);
                    }
                    return bytes;
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

    #[test]
    fn test_cmos_dual_bank_and_crc() {
        let mut cmos = CmosRtc::with_ram_size(1024 * 1024 * 1024);
        // Ports 0x70 and 0x72 return 0xFF on read
        assert_eq!(cmos.read(0x70, 1), vec![0xFF]);
        assert_eq!(cmos.read(0x72, 1), vec![0xFF]);

        // Century bytes in BCD (20 for 20xx)
        cmos.write(0x70, &[0x32]);
        assert_eq!(cmos.read(0x71, 1), vec![0x20]);
        cmos.write(0x70, &[0x37]);
        assert_eq!(cmos.read(0x71, 1), vec![0x20]);

        // Verify checksum at 0x2E/0x2F is non-zero and updates
        cmos.write(0x70, &[0x2E]);
        let crc_hi_orig = cmos.read(0x71, 1)[0];
        cmos.write(0x70, &[0x2F]);
        let crc_lo_orig = cmos.read(0x71, 1)[0];
        let orig_crc = ((crc_hi_orig as u16) << 8) | (crc_lo_orig as u16);
        assert!(orig_crc > 0);

        // Modifying register 0x12 modifies CRC
        cmos.write(0x70, &[0x12]);
        cmos.write(0x71, &[0x55]);
        cmos.write(0x70, &[0x2E]);
        let crc_hi_new = cmos.read(0x71, 1)[0];
        cmos.write(0x70, &[0x2F]);
        let crc_lo_new = cmos.read(0x71, 1)[0];
        let new_crc = ((crc_hi_new as u16) << 8) | (crc_lo_new as u16);
        assert_ne!(orig_crc, new_crc);

        // Extended Bank 1 via ports 0x72/0x73
        cmos.write(0x72, &[0x10]);
        cmos.write(0x73, &[0xBE]);
        cmos.write(0x72, &[0x10]);
        assert_eq!(cmos.read(0x73, 1), vec![0xBE]);
        assert_eq!(cmos.ram[128 + 0x10], 0xBE);
    }

    #[test]
    fn test_cmos_ram_size_above_4gb_saturates() {
        let cmos = CmosRtc::with_ram_size(8 * 1024 * 1024 * 1024);
        assert_eq!(cmos.ram[0x34], 0xFF);
        assert_eq!(cmos.ram[0x35], 0xFF);
    }

    #[test]
    fn test_dma_controller_address_count_and_page_registers() {
        let mut dma = I8237Dma::new();
        // Page register 0x81 (Channel 2 Floppy)
        dma.write(0x81, &[0x1F]);
        assert_eq!(dma.read(0x81, 1), vec![0x1F]);
        assert_eq!(dma.channels[2].page, 0x1F);

        // Channel 2 Address write with flip-flop (low byte then high byte)
        dma.write(0x0C, &[0]); // Clear flip-flop
        dma.write(0x04, &[0x34]); // Low byte
        dma.write(0x04, &[0x12]); // High byte
        assert_eq!(dma.channels[2].base_addr, 0x1234);

        // Readback Channel 2 Address
        dma.write(0x0C, &[0]); // Clear flip-flop
        assert_eq!(dma.read(0x04, 1), vec![0x34]);
        assert_eq!(dma.read(0x04, 1), vec![0x12]);

        // Channel 6 (16-bit DMA 2) at port 0xC8
        dma.write(0xD8, &[0]); // Clear flip-flop DMA2
        dma.write(0xC8, &[0x78]);
        dma.write(0xC8, &[0x56]);
        assert_eq!(dma.channels[6].base_addr, 0x5678);
    }

    #[test]
    fn test_acpi_pm_timer_multibyte_read() {
        let mut pm = configured_pm();
        // 4-byte read of PM Timer at base+0x08
        let bytes4 = pm.read(0x608, 4);
        assert_eq!(bytes4.len(), 4);

        // Single byte read at base+0x08 and base+0x09
        let b0 = pm.read(0x608, 1);
        let b1 = pm.read(0x609, 1);
        assert_eq!(b0.len(), 1);
        assert_eq!(b1.len(), 1);
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
            0x2E..=0x2F | 0x4E..=0x4F |      // Super I/O probe (ITE/Winbond)
            0xF0..=0xF1 |                     // Math coprocessor / FPU status/reset
            0x378..=0x37A | 0x278..=0x27A |  // LPT1/LPT2
            0x2F8..=0x2FF | 0x3E8..=0x3EF | 0x2E8..=0x2EF |  // COM2/3/4
            0xCFB                             // PCI configuration BIOS access
        )
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        match port {
            0x378 => { self.lpt1_data = data[0]; }
            0x278 => { self.lpt2_data = data[0]; }
            _ => {} // COM/LPT/SuperIO/FPU/CFB: ignore
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        match port {
            // Super I/O probe: return 0xFF (device not present)
            0x2E..=0x2F | 0x4E..=0x4F => vec![0xFF; count],
            // FPU status / clear: return 0x00
            0xF0..=0xF1 => vec![0x00; count],
            // PCI config BIOS mechanism
            0xCFB => vec![0x00; count],
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

// ─── Controlador DMA Intel 8237A (DMA1 + DMA2 + Registros de Página) ──
/// Emula los dos controladores DMA Intel 8237A en cascada (DMA1 canales 0-3 en
/// puertos 0x00..0x0F, DMA2 canales 4-7 en puertos 0xC0..0xDF) y los registros
/// de página ISA en puertos 0x81..0x8F, idéntico a VirtualBox (`DevDMA.cpp`).
#[derive(Debug, Clone)]
pub struct DmaChannel {
    pub base_addr: u16,
    pub cur_addr: u16,
    pub base_count: u16,
    pub cur_count: u16,
    pub mode: u8,
    pub page: u8,
}

impl DmaChannel {
    pub fn new() -> Self {
        Self {
            base_addr: 0,
            cur_addr: 0,
            base_count: 0,
            cur_count: 0,
            mode: 0,
            page: 0,
        }
    }
}

pub struct I8237Dma {
    pub channels: [DmaChannel; 8],
    /// Byte pointer flip-flop: false = low byte, true = high byte
    pub flip_flop: [bool; 2],
    /// Command registers (DMA1, DMA2)
    pub command: [u8; 2],
    /// Status registers (DMA1, DMA2)
    pub status: [u8; 2],
    /// Channel mask registers (4 bits each, bit set = masked)
    pub mask: [u8; 2],
    /// Page registers for address lines A16..A23 (ports 0x80..0x8F)
    pub page_regs: [u8; 16],
}

impl I8237Dma {
    pub fn new() -> Self {
        Self {
            channels: [
                DmaChannel::new(), DmaChannel::new(), DmaChannel::new(), DmaChannel::new(),
                DmaChannel::new(), DmaChannel::new(), DmaChannel::new(), DmaChannel::new(),
            ],
            flip_flop: [false, false],
            command: [0, 0],
            status: [0, 0],
            mask: [0x0F, 0x0F], // All channels masked on reset
            page_regs: [0; 16],
        }
    }

    pub fn reset(&mut self) {
        self.flip_flop = [false, false];
        self.command = [0, 0];
        self.status = [0, 0];
        self.mask = [0x0F, 0x0F];
        self.page_regs = [0; 16];
    }
}

impl Default for I8237Dma {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for I8237Dma {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port, 0x00..=0x0F | 0x81..=0x8F | 0xC0..=0xDF)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let val = data[0];

        // 1. Registros de Página ISA (0x81..0x8F)
        if (0x81..=0x8F).contains(&port) {
            let p = (port & 0x0F) as usize;
            self.page_regs[p] = val;
            match port {
                0x87 => self.channels[0].page = val,
                0x83 => self.channels[1].page = val,
                0x81 => self.channels[2].page = val,
                0x82 => self.channels[3].page = val,
                0x8B => self.channels[5].page = val,
                0x89 => self.channels[6].page = val,
                0x8A => self.channels[7].page = val,
                _ => {}
            }
            return;
        }

        // 2. DMA 1 (8-bit: 0x00..0x0F)
        if port <= 0x0F {
            match port {
                0x00..=0x07 => {
                    let ch = (port >> 1) as usize;
                    let is_count = (port & 1) != 0;
                    if !self.flip_flop[0] {
                        if is_count {
                            self.channels[ch].base_count = (self.channels[ch].base_count & 0xFF00) | (val as u16);
                            self.channels[ch].cur_count = self.channels[ch].base_count;
                        } else {
                            self.channels[ch].base_addr = (self.channels[ch].base_addr & 0xFF00) | (val as u16);
                            self.channels[ch].cur_addr = self.channels[ch].base_addr;
                        }
                        self.flip_flop[0] = true;
                    } else {
                        if is_count {
                            self.channels[ch].base_count = (self.channels[ch].base_count & 0x00FF) | ((val as u16) << 8);
                            self.channels[ch].cur_count = self.channels[ch].base_count;
                        } else {
                            self.channels[ch].base_addr = (self.channels[ch].base_addr & 0x00FF) | ((val as u16) << 8);
                            self.channels[ch].cur_addr = self.channels[ch].base_addr;
                        }
                        self.flip_flop[0] = false;
                    }
                }
                0x08 => self.command[0] = val,
                0x0A => {
                    let ch = (val & 0x03) as usize;
                    if val & 0x04 != 0 {
                        self.mask[0] |= 1 << ch;
                    } else {
                        self.mask[0] &= !(1 << ch);
                    }
                }
                0x0B => {
                    let ch = (val & 0x03) as usize;
                    self.channels[ch].mode = val;
                }
                0x0C => self.flip_flop[0] = false,
                0x0D => {
                    self.flip_flop[0] = false;
                    self.command[0] = 0;
                    self.status[0] = 0;
                    self.mask[0] = 0x0F;
                }
                0x0E => self.mask[0] = 0,
                0x0F => self.mask[0] = val & 0x0F,
                _ => {}
            }
            return;
        }

        // 3. DMA 2 (16-bit: 0xC0..0xDF)
        if (0xC0..=0xDF).contains(&port) {
            let reg = ((port - 0xC0) >> 1) as u8;
            match reg {
                0x00..=0x07 => {
                    let ch = 4 + (reg >> 1) as usize;
                    let is_count = (reg & 1) != 0;
                    if !self.flip_flop[1] {
                        if is_count {
                            self.channels[ch].base_count = (self.channels[ch].base_count & 0xFF00) | (val as u16);
                            self.channels[ch].cur_count = self.channels[ch].base_count;
                        } else {
                            self.channels[ch].base_addr = (self.channels[ch].base_addr & 0xFF00) | (val as u16);
                            self.channels[ch].cur_addr = self.channels[ch].base_addr;
                        }
                        self.flip_flop[1] = true;
                    } else {
                        if is_count {
                            self.channels[ch].base_count = (self.channels[ch].base_count & 0x00FF) | ((val as u16) << 8);
                            self.channels[ch].cur_count = self.channels[ch].base_count;
                        } else {
                            self.channels[ch].base_addr = (self.channels[ch].base_addr & 0x00FF) | ((val as u16) << 8);
                            self.channels[ch].cur_addr = self.channels[ch].base_addr;
                        }
                        self.flip_flop[1] = false;
                    }
                }
                0x08 => self.command[1] = val,
                0x0A => {
                    let ch = (val & 0x03) as usize;
                    if val & 0x04 != 0 {
                        self.mask[1] |= 1 << ch;
                    } else {
                        self.mask[1] &= !(1 << ch);
                    }
                }
                0x0B => {
                    let ch = 4 + (val & 0x03) as usize;
                    self.channels[ch].mode = val;
                }
                0x0C => self.flip_flop[1] = false,
                0x0D => {
                    self.flip_flop[1] = false;
                    self.command[1] = 0;
                    self.status[1] = 0;
                    self.mask[1] = 0x0F;
                }
                0x0E => self.mask[1] = 0,
                0x0F => self.mask[1] = val & 0x0F,
                _ => {}
            }
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        // 1. Registros de Página ISA (0x81..0x8F)
        if (0x81..=0x8F).contains(&port) {
            let p = (port & 0x0F) as usize;
            return vec![self.page_regs[p]; count];
        }

        // 2. DMA 1 (8-bit: 0x00..0x0F)
        if port <= 0x0F {
            let val = match port {
                0x00..=0x07 => {
                    let ch = (port >> 1) as usize;
                    let is_count = (port & 1) != 0;
                    if !self.flip_flop[0] {
                        self.flip_flop[0] = true;
                        if is_count { self.channels[ch].cur_count as u8 } else { self.channels[ch].cur_addr as u8 }
                    } else {
                        self.flip_flop[0] = false;
                        if is_count { (self.channels[ch].cur_count >> 8) as u8 } else { (self.channels[ch].cur_addr >> 8) as u8 }
                    }
                }
                0x08 => self.status[0],
                0x0F => self.mask[0],
                _ => 0,
            };
            return vec![val; count];
        }

        // 3. DMA 2 (16-bit: 0xC0..0xDF)
        if (0xC0..=0xDF).contains(&port) {
            let reg = ((port - 0xC0) >> 1) as u8;
            let val = match reg {
                0x00..=0x07 => {
                    let ch = 4 + (reg >> 1) as usize;
                    let is_count = (reg & 1) != 0;
                    if !self.flip_flop[1] {
                        self.flip_flop[1] = true;
                        if is_count { self.channels[ch].cur_count as u8 } else { self.channels[ch].cur_addr as u8 }
                    } else {
                        self.flip_flop[1] = false;
                        if is_count { (self.channels[ch].cur_count >> 8) as u8 } else { (self.channels[ch].cur_addr >> 8) as u8 }
                    }
                }
                0x08 => self.status[1],
                0x0F => self.mask[1],
                _ => 0,
            };
            return vec![val; count];
        }

        vec![0xFF; count]
    }
}
