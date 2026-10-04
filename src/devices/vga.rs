//! Emulación de Bochs VBE Display / Standard VGA Controller.
//!
//! Puertos:
//!   0x1CE / 0x1CF: Interfaz Bochs VBE Dispi (Index / Data)
//!   0x3C0 .. 0x3DF: Registros estándar VGA (CRT Controller, DAC, Sequencer, etc.)

use super::IoDevice;
use std::sync::{Arc, Mutex};

// ─── Bochs Dispi Register Constants ────────────────────────────────
pub const VBE_DISPI_INDEX_ID: u16 = 0x00;
pub const VBE_DISPI_INDEX_XRES: u16 = 0x01;
pub const VBE_DISPI_INDEX_YRES: u16 = 0x02;
pub const VBE_DISPI_INDEX_BPP: u16 = 0x03;
pub const VBE_DISPI_INDEX_ENABLE: u16 = 0x04;
#[allow(dead_code)]
pub const VBE_DISPI_INDEX_BANK: u16 = 0x05;
pub const VBE_DISPI_INDEX_VIRT_WIDTH: u16 = 0x06;
pub const VBE_DISPI_INDEX_VIRT_HEIGHT: u16 = 0x07;
#[allow(dead_code)]
pub const VBE_DISPI_INDEX_X_OFFSET: u16 = 0x08;
#[allow(dead_code)]
pub const VBE_DISPI_INDEX_Y_OFFSET: u16 = 0x09;
pub const VBE_DISPI_INDEX_VIDEO_MEMORY_64K: u16 = 0x0A;

pub const VBE_DISPI_ID0: u16 = 0xB0C0;
pub const VBE_DISPI_ID5: u16 = 0xB0C5;

pub const VBE_DISPI_ENABLED: u16 = 0x01;
#[allow(dead_code)]
pub const VBE_DISPI_GETCAPS: u16 = 0x02;
#[allow(dead_code)]
pub const VBE_DISPI_8BIT_DAC: u16 = 0x20;
#[allow(dead_code)]
pub const VBE_DISPI_LFB_ENABLED: u16 = 0x40;
#[allow(dead_code)]
pub const VBE_DISPI_NOCLEARMEM: u16 = 0x80;

pub const VBE_DISPI_MAX_XRES: u16 = 2560;
pub const VBE_DISPI_MAX_YRES: u16 = 1600;
pub const VBE_DISPI_MAX_BPP: u16 = 32;

#[allow(dead_code)]
pub const VRAM_SIZE: usize = 16 * 1024 * 1024; // 16 MiB
#[allow(dead_code)]
pub const VRAM_DEFAULT_BASE_GPA: u64 = 0xE800_0000;

/// (tarea 1) Tamaño de la ventana MMIO del BAR2 (registros dispi).
pub const VGA_MMIO_BAR_SIZE: u64 = 0x1000;

// ─── Estado interno compartido de VGA / VBE ────────────────────────
pub struct VgaState {
    pub dispi_index: u16,
    pub dispi_regs: [u16; 16],
    // Standard VGA legacy registers
    pub crtc_index: u8,
    pub crtc_regs: [u8; 256],
    pub seq_index: u8,
    pub seq_regs: [u8; 256],
    pub grc_index: u8,
    pub grc_regs: [u8; 256],
    pub attr_index: u8,
    pub attr_flipflop: bool,
    pub attr_regs: [u8; 256],
    pub dac_read_index: u8,
    pub dac_write_index: u8,
    pub dac_sub_index: u8,
    pub dac_palette: [u8; 768],
    pub misc_output: u8,
    pub status1_toggle: u8,
    pub vram_ptr: *mut u8,
    pub vram_size: usize,
    pub fallback_vram_ptr: *mut u8,
    // (tarea 1) Direcciones GPA asignadas por el guest vía config space PCI:
    // BAR0 = framebuffer lineal (16 MiB), BAR2 = registros dispi MMIO (4 KiB).
    // El LFB arranca apuntando a la VRAM que mapea main.rs en el slot 2.
    pub lfb_base: Option<u64>,
    pub mmio_base: Option<u64>,
    pub vbe_active: bool,
}

// Safety: VgaState pointer is managed and accessed with synchronization
unsafe impl Send for VgaState {}
unsafe impl Sync for VgaState {}

impl VgaState {
    pub fn new(vram_ptr: *mut u8, vram_size: usize) -> Self {
        let mut dispi_regs = [0u16; 16];
        dispi_regs[VBE_DISPI_INDEX_ID as usize] = VBE_DISPI_ID5;
        dispi_regs[VBE_DISPI_INDEX_XRES as usize] = 640;
        dispi_regs[VBE_DISPI_INDEX_YRES as usize] = 480;
        dispi_regs[VBE_DISPI_INDEX_BPP as usize] = 32;
        dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] = 0;
        dispi_regs[VBE_DISPI_INDEX_VIRT_WIDTH as usize] = 640;
        dispi_regs[VBE_DISPI_INDEX_VIRT_HEIGHT as usize] = 480;
        dispi_regs[VBE_DISPI_INDEX_VIDEO_MEMORY_64K as usize] = (vram_size / (64 * 1024)) as u16;

        Self {
            dispi_index: 0,
            dispi_regs,
            crtc_index: 0,
            crtc_regs: [0u8; 256],
            seq_index: 0,
            seq_regs: [0u8; 256],
            grc_index: 0,
            grc_regs: [0u8; 256],
            attr_index: 0,
            attr_flipflop: false,
            attr_regs: [0u8; 256],
            dac_read_index: 0,
            dac_write_index: 0,
            dac_sub_index: 0,
            dac_palette: [0u8; 768],
            misc_output: 0x23, // 80x25 text mode defaults
            status1_toggle: 0,
            lfb_base: Some(VRAM_DEFAULT_BASE_GPA),
            mmio_base: None, // BAR2 sin asignar hasta el POST del BIOS
            vram_ptr,
            vram_size,
            fallback_vram_ptr: std::ptr::null_mut(),
            vbe_active: false,
        }
    }

    /// Reset del estado VGA/VBE: vuelve a los registros de arranque
    /// (modo texto 80x25, VBE deshabilitado). La VRAM física se conserva.
    pub fn reset(&mut self) {
        let ptr = self.vram_ptr;
        let size = self.vram_size;
        *self = Self::new(ptr, size);
    }

    pub fn is_vbe_enabled(&self) -> bool {
        self.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_ENABLED != 0
    }

    /// (tarea 2) Detecta si el guest programó un modo gráfico VGA estándar
    /// (13h planar/packed, 12h/10h planar, 4/5 CGA) a través de los registros
    /// legacy (Sequencer / Graphics Controller). El renderizador de
    /// display.rs decide con esto si debe decodificar VRAM planar.
    ///
    /// Criterio (hardware real): bit 0 del registro Mode Control del
    /// Attribute Controller (0x3C0/idx 0x10) es "Graphics Mode": 1 =
    /// gráfico, 0 = texto. Como el modo 13h programa el Memory Mode del
    /// Sequencer con 0x0E (bit Alpha-Dis a 0), también aceptamos chain-4
    /// (bit 3 de 0x3C5/idx 4) como señal inequívoca de modo gráfico. Con
    /// registros sin tocar (a cero) devolvemos false y el renderizador
    /// asume el modo texto 80x25 clásico.
    ///
    /// Si VBE ya estuvo activo o configurado en alta resolución, NO se
    /// interpreta como gráfico VGA estándar (evita decodificar VRAM linear
    /// como planos CGA/planar durante un cambio de resolución KMS con ENABLE=0).
    pub fn is_standard_vga_graphics(&self) -> bool {
        if self.vbe_active {
            return false;
        }
        self.attr_regs[0x10] & 0x01 != 0 || self.seq_regs[0x04] & 0x08 != 0
    }

    /// (tarea 2) Geometría (ancho, alto) en píxeles del modo gráfico
    /// estándar actual, derivada de los registros del CRTC:
    ///   - Chain-4 (13h): CRTC Offset (0x13) en doubleswords × 4, 200 líneas.
    ///   - Planar (12h/10h/0Eh): (Horizontal Display End + 1) caracteres × 8
    ///     píxeles de ancho y Vertical Display End + 1 líneas de alto
    ///     (con bits 8/9 extendidos en el registro Overflow).
    pub fn standard_vga_geometry(&self) -> (usize, usize) {
        let offset = ((self.crtc_regs[0x13] as usize) << 2)
            | (((self.crtc_regs[0x14] as usize) >> 6) << 8);
        let hde_chars = self.crtc_regs[0x01] as usize + 1;
        if self.seq_regs[0x04] & 0x08 != 0 {
            // Chain-4 (modo 13h): 320 por defecto como el modo estándar
            let w = if offset >= 320 { offset } else { 320 };
            (w, 200)
        } else {
            // Planar: ancho = caracteres visibles × 8 px (640 en modo 12h)
            let w = if hde_chars >= 10 { hde_chars * 8 } else { 640 };
            // Alto = VDE + 1, con bits 8/9 en Overflow (CRTC 0x07: bits 1 y 6)
            let mut vde = self.crtc_regs[0x12] as usize;
            if self.crtc_regs[0x07] & 0x02 != 0 {
                vde |= 0x100;
            }
            if self.crtc_regs[0x07] & 0x40 != 0 {
                vde |= 0x200;
            }
            let h = if vde + 1 >= 100 { vde + 1 } else { 400 };
            (w, h)
        }
    }

    pub fn get_resolution(&self) -> (usize, usize, usize) {
        let x = self.dispi_regs[VBE_DISPI_INDEX_XRES as usize] as usize;
        let y = self.dispi_regs[VBE_DISPI_INDEX_YRES as usize] as usize;
        let bpp = self.dispi_regs[VBE_DISPI_INDEX_BPP as usize] as usize;
        let x = if x == 0 { 640 } else { x };
        let y = if y == 0 { 480 } else { y };
        let bpp = if bpp == 0 { 32 } else { bpp };
        (x, y, bpp)
    }

    /// (tarea 1) Escribe un registro dispi por índice. Lógica compartida por
    /// el acceso vía puertos (0x1CE/0x1CF) y vía MMIO (BAR2).
    pub fn dispi_write_reg(&mut self, idx: u16, val: u16) {
        if (idx as usize) < self.dispi_regs.len() {
            match idx {
                VBE_DISPI_INDEX_ID => {
                    if (VBE_DISPI_ID0..=VBE_DISPI_ID5).contains(&val) {
                        self.dispi_regs[idx as usize] = val;
                    }
                }
                VBE_DISPI_INDEX_XRES => {
                    self.dispi_regs[idx as usize] = val.clamp(320, 2560);
                }
                VBE_DISPI_INDEX_YRES => {
                    self.dispi_regs[idx as usize] = val.clamp(200, 1600);
                }
                VBE_DISPI_INDEX_BPP => {
                    if matches!(val, 4 | 8 | 15 | 16 | 24 | 32) {
                        self.dispi_regs[idx as usize] = val;
                    }
                }
                VBE_DISPI_INDEX_ENABLE => {
                    self.dispi_regs[idx as usize] = val;
                    if (val & VBE_DISPI_ENABLED) != 0 {
                        self.vbe_active = true;
                    }
                    // Según especificación Bochs VBE: si VBE_DISPI_NOCLEARMEM (0x80)
                    // NO está activo al habilitar VBE, la memoria de vídeo se limpia a 0
                    // para evitar mostrar artefactos visuales de modos previos o memoria residual.
                    if (val & VBE_DISPI_ENABLED) != 0 && (val & VBE_DISPI_NOCLEARMEM) == 0 {
                        if !self.vram_ptr.is_null() && self.vram_size > 0 {
                            unsafe {
                                std::ptr::write_bytes(self.vram_ptr, 0, self.vram_size);
                            }
                        }
                    }
                    eprintln!(
                        "[VGA] VBE Enable=0x{:02X}: {}x{}@{}bpp",
                        val,
                        self.dispi_regs[VBE_DISPI_INDEX_XRES as usize],
                        self.dispi_regs[VBE_DISPI_INDEX_YRES as usize],
                        self.dispi_regs[VBE_DISPI_INDEX_BPP as usize],
                    );
                }
                _ => {
                    self.dispi_regs[idx as usize] = val;
                }
            }
        }
    }

    /// (tarea 1) Lee un registro dispi por índice (0xFFFF si no existe).
    pub fn dispi_read_reg(&self, idx: u16) -> u16 {
        let getcaps = (self.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_GETCAPS) != 0;
        match idx {
            VBE_DISPI_INDEX_XRES if getcaps => VBE_DISPI_MAX_XRES,
            VBE_DISPI_INDEX_YRES if getcaps => VBE_DISPI_MAX_YRES,
            VBE_DISPI_INDEX_BPP if getcaps => VBE_DISPI_MAX_BPP,
            _ => {
                if (idx as usize) < self.dispi_regs.len() {
                    self.dispi_regs[idx as usize]
                } else {
                    0xFFFF
                }
            }
        }
    }

    /// Escritura en puertos de E/S VGA / VBE. Compartida entre el despachador
    /// de puertos (IN/OUT) y el acceso MMIO a registros VGA (BAR2 + 0x400).
    pub fn port_write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        match port {
            // Bochs Dispi Index (0x1CE)
            0x1CE => {
                let idx = if data.len() >= 2 {
                    u16::from_le_bytes([data[0], data[1]])
                } else {
                    data[0] as u16
                };
                self.dispi_index = idx;
            }
            // Bochs Dispi Data (0x1CF)
            0x1CF => {
                let val = if data.len() >= 2 {
                    u16::from_le_bytes([data[0], data[1]])
                } else {
                    data[0] as u16
                };
                let idx = self.dispi_index;
                self.dispi_write_reg(idx, val);
            }
            // VGA Standard Registers
            0x3C0 => {
                if !self.attr_flipflop {
                    self.attr_index = data[0] & 0x1F;
                } else {
                    let idx = self.attr_index as usize;
                    self.attr_regs[idx] = data[0];
                }
                self.attr_flipflop = !self.attr_flipflop;
            }
            0x3C2 => self.misc_output = data[0],
            0x3C4 => self.seq_index = data[0],
            0x3C5 => {
                let idx = self.seq_index as usize;
                self.seq_regs[idx] = data[0];
            }
            0x3C7 => {
                self.dac_read_index = data[0];
                self.dac_sub_index = 0;
            }
            0x3C8 => {
                self.dac_write_index = data[0];
                self.dac_sub_index = 0;
            }
            0x3C9 => {
                let idx = (self.dac_write_index as usize) * 3 + (self.dac_sub_index as usize);
                if idx < self.dac_palette.len() {
                    self.dac_palette[idx] = data[0];
                }
                self.dac_sub_index += 1;
                if self.dac_sub_index >= 3 {
                    self.dac_sub_index = 0;
                    self.dac_write_index = self.dac_write_index.wrapping_add(1);
                }
            }
            0x3CE => self.grc_index = data[0],
            0x3CF => {
                let idx = self.grc_index as usize;
                self.grc_regs[idx] = data[0];
            }
            0x3D4 => self.crtc_index = data[0],
            0x3D5 => {
                let idx = self.crtc_index as usize;
                self.crtc_regs[idx] = data[0];
            }
            _ => {}
        }
    }

    /// Lectura en puertos de E/S VGA / VBE. Compartida entre el despachador
    /// de puertos (IN/OUT) y el acceso MMIO a registros VGA (BAR2 + 0x400).
    pub fn port_read(&mut self, port: u16, count: usize) -> Vec<u8> {
        match port {
            0x1CE => {
                let idx = self.dispi_index;
                if count >= 2 {
                    idx.to_le_bytes().to_vec()
                } else {
                    vec![idx as u8]
                }
            }
            0x1CF => {
                let idx = self.dispi_index;
                let val = self.dispi_read_reg(idx);
                if count >= 2 {
                    val.to_le_bytes().to_vec()
                } else {
                    vec![val as u8]
                }
            }
            0x3C0 => vec![self.attr_index],
            0x3C1 => vec![self.attr_regs[self.attr_index as usize]],
            0x3C2 | 0x3CC => vec![self.misc_output],
            0x3C4 => vec![self.seq_index],
            0x3C5 => vec![self.seq_regs[self.seq_index as usize]],
            0x3C9 => {
                let idx = (self.dac_read_index as usize) * 3 + (self.dac_sub_index as usize);
                let val = if idx < self.dac_palette.len() {
                    self.dac_palette[idx]
                } else {
                    0
                };
                self.dac_sub_index += 1;
                if self.dac_sub_index >= 3 {
                    self.dac_sub_index = 0;
                    self.dac_read_index = self.dac_read_index.wrapping_add(1);
                }
                vec![val]
            }
            0x3CE => vec![self.grc_index],
            0x3CF => vec![self.grc_regs[self.grc_index as usize]],
            0x3D4 => vec![self.crtc_index],
            0x3D5 => vec![self.crtc_regs[self.crtc_index as usize]],
            0x3DA | 0x3BA => {
                self.attr_flipflop = false;
                self.status1_toggle ^= 0x09; // Toggle bit 3 (VSync) and bit 0 (display enable)
                vec![self.status1_toggle]
            }
            _ => vec![0x00; count],
        }
    }
}

pub struct VgaDevice {
    pub state: Arc<Mutex<VgaState>>,
}

impl VgaDevice {
    pub fn new(vram_ptr: *mut u8, vram_size: usize) -> (Self, Arc<Mutex<VgaState>>) {
        let state = Arc::new(Mutex::new(VgaState::new(vram_ptr, vram_size)));
        (
            Self {
                state: state.clone(),
            },
            state,
        )
    }

    /// Reset del controlador VGA: reinicia los registros internos (el Arc
    /// compartido con el display se conserva, así la ventana sigue viva).
    pub fn reset(&self) {
        self.state.lock().unwrap().reset();
    }

    /// (tarea 1) Registra la GPA que el guest asignó al BAR0 (framebuffer
    /// lineal). `bar_raw` es el valor del config space tras el POST; 0 = sin
    /// asignar.
    pub fn set_lfb_bar(&self, bar_raw: u32) {
        let base = (bar_raw & 0xFFFF_FFF0) as u64;
        self.state.lock().unwrap().lfb_base = if bar_raw == 0 { None } else { Some(base) };
        eprintln!("[VGA] BAR0 (LFB) asignado: GPA {:#x}", base);
    }

    /// (tarea 1) Registra la GPA del BAR2 (MMIO dispi, 4 KiB).
    pub fn set_mmio_bar(&self, bar_raw: u32) {
        let base = (bar_raw & 0xFFFF_FFF0) as u64;
        self.state.lock().unwrap().mmio_base = if bar_raw == 0 { None } else { Some(base) };
        eprintln!("[VGA] BAR2 (MMIO) asignado: GPA {:#x}", base);
    }

    /// (tarea 1) Re-apunta el buffer host del framebuffer. Necesario cuando
    /// el guest asigna el BAR0 dentro de la ventana de RAM de KVM (slot 2):
    /// esos accesos son RAM normal y NO generan exits MMIO, así que el
    /// renderizador debe leer donde el guest escribe realmente.
    pub fn set_vram_host_ptr(&self, ptr: *mut u8) {
        self.state.lock().unwrap().vram_ptr = ptr;
    }

    /// Guarda un puntero fallback (ej. GPA 0xE0000000) por si el guest
    /// o software legacy escribe en la dirección histórica fija de VBE.
    pub fn set_fallback_vram_ptr(&self, ptr: *mut u8) {
        self.state.lock().unwrap().fallback_vram_ptr = ptr;
    }

    /// (tarea 1) Escritura MMIO del guest. Devuelve true si la dirección
    /// pertenece a una ventana VGA (BAR2 dispi o BAR0 framebuffer).
    ///
    /// Soporta el layout PCI estándar de Bochs/QEMU:
    ///   - BAR2 + 0x500..0x520: registros Bochs Dispi (usados por `bochs-drm` en Linux)
    ///   - BAR2 + 0x400..0x420: registros VGA estándar (0x400 + port - 0x3C0)
    ///   - BAR2 + 0x600..0x610: registros de extensión QEMU
    ///   - BAR2 + 0x000..0x020: offset directo Dispi legacy (compatibilidad retroactiva)
    pub fn mmio_write(&self, addr: u64, data: &[u8]) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(base) = state.mmio_base {
            if addr >= base && addr + data.len() as u64 <= base + VGA_MMIO_BAR_SIZE {
                let off = (addr - base) as usize;
                // Dispi registers en 0x500..0x520 (layout bochs-drm del kernel Linux)
                if off >= 0x500 && off < 0x520 {
                    let dispi_off = off - 0x500;
                    let mut i = 0;
                    while i < data.len() {
                        let pos = dispi_off + i;
                        if pos % 2 == 0 && i + 1 < data.len() {
                            let reg = (pos / 2) as u16;
                            let val = u16::from_le_bytes([data[i], data[i + 1]]);
                            state.dispi_write_reg(reg, val);
                            i += 2;
                        } else if pos % 2 == 0 {
                            let reg = (pos / 2) as u16;
                            let cur = state.dispi_read_reg(reg);
                            let val = (cur & 0xFF00) | (data[i] as u16);
                            state.dispi_write_reg(reg, val);
                            i += 1;
                        } else {
                            let reg = (pos / 2) as u16;
                            let cur = state.dispi_read_reg(reg);
                            let val = (cur & 0x00FF) | ((data[i] as u16) << 8);
                            state.dispi_write_reg(reg, val);
                            i += 1;
                        }
                    }
                    return true;
                }
                // Puertos VGA estándar en 0x400..0x420 (0x400 + port - 0x3C0)
                if off >= 0x400 && off < 0x420 {
                    let port_base = 0x3C0 + (off - 0x400) as u16;
                    for (i, &byte) in data.iter().enumerate() {
                        let port = port_base + i as u16;
                        state.port_write(port, &[byte]);
                    }
                    return true;
                }
                // Extensión QEMU en 0x600..0x610 (ignorar escrituras)
                if off >= 0x600 && off < 0x610 {
                    return true;
                }
                // Offset directo legacy en 0x000..0x020 (para compatibilidad de tests previos)
                if off < 0x20 {
                    let mut i = 0;
                    while i < data.len() {
                        let pos = off + i;
                        if pos % 2 == 0 && i + 1 < data.len() {
                            let reg = (pos / 2) as u16;
                            let val = u16::from_le_bytes([data[i], data[i + 1]]);
                            state.dispi_write_reg(reg, val);
                            i += 2;
                        } else if pos % 2 == 0 {
                            let reg = (pos / 2) as u16;
                            let cur = state.dispi_read_reg(reg);
                            let val = (cur & 0xFF00) | (data[i] as u16);
                            state.dispi_write_reg(reg, val);
                            i += 1;
                        } else {
                            let reg = (pos / 2) as u16;
                            let cur = state.dispi_read_reg(reg);
                            let val = (cur & 0x00FF) | ((data[i] as u16) << 8);
                            state.dispi_write_reg(reg, val);
                            i += 1;
                        }
                    }
                    return true;
                }
                return true;
            }
        }
        // BAR0: framebuffer lineal → VRAM cruda (byte a byte).
        if let Some(base) = state.lfb_base {
            if addr >= base && addr + data.len() as u64 <= base + state.vram_size as u64 {
                let off = (addr - base) as usize;
                if !state.vram_ptr.is_null() && off + data.len() <= state.vram_size {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data.as_ptr(),
                            state.vram_ptr.add(off),
                            data.len(),
                        );
                    }
                }
                return true;
            }
        }
        false
    }

    /// (tarea 1) Lectura MMIO del guest: Some(bytes) si la dirección cae en
    /// una ventana VGA (exactamente `size` bytes); None si no es nuestra.
    ///
    /// Soporta el layout PCI estándar de Bochs/QEMU:
    ///   - BAR2 + 0x500..0x520: registros Bochs Dispi (usados por `bochs-drm` en Linux)
    ///   - BAR2 + 0x400..0x420: registros VGA estándar (0x400 + port - 0x3C0)
    ///   - BAR2 + 0x600..0x610: registros de extensión QEMU (0 = no extensión)
    ///   - BAR2 + 0x000..0x020: offset directo Dispi legacy (compatibilidad retroactiva)
    pub fn mmio_read(&self, addr: u64, size: usize) -> Option<Vec<u8>> {
        let mut state = self.state.lock().unwrap();
        if let Some(base) = state.mmio_base {
            if addr >= base && addr + size as u64 <= base + VGA_MMIO_BAR_SIZE {
                let off = (addr - base) as usize;
                // Dispi registers en 0x500..0x520 (layout bochs-drm del kernel Linux)
                if off >= 0x500 && off < 0x520 {
                    let dispi_off = off - 0x500;
                    let mut out = vec![0u8; size];
                    for (i, b) in out.iter_mut().enumerate() {
                        let pos = dispi_off + i;
                        let reg = pos / 2;
                        let byte = pos % 2;
                        *b = if reg < state.dispi_regs.len() {
                            (state.dispi_regs[reg] >> (byte * 8)) as u8
                        } else {
                            0xFF
                        };
                    }
                    return Some(out);
                }
                // Puertos VGA estándar en 0x400..0x420
                if off >= 0x400 && off < 0x420 {
                    let port_base = 0x3C0 + (off - 0x400) as u16;
                    let mut out = Vec::with_capacity(size);
                    for i in 0..size {
                        let port = port_base + i as u16;
                        let bytes = state.port_read(port, 1);
                        out.push(bytes.first().copied().unwrap_or(0xFF));
                    }
                    return Some(out);
                }
                // Extensión QEMU en 0x600..0x610 (retorna 0 para indicar que no hay extensión)
                if off >= 0x600 && off < 0x610 {
                    return Some(vec![0u8; size]);
                }
                // Offset directo legacy en 0x000..0x020 (para compatibilidad de tests previos)
                if off < 0x20 {
                    let mut out = vec![0u8; size];
                    for (i, b) in out.iter_mut().enumerate() {
                        let pos = off + i;
                        let reg = pos / 2;
                        let byte = pos % 2;
                        *b = if reg < state.dispi_regs.len() {
                            (state.dispi_regs[reg] >> (byte * 8)) as u8
                        } else {
                            0xFF
                        };
                    }
                    return Some(out);
                }
                return Some(vec![0xFF; size]);
            }
        }
        if let Some(base) = state.lfb_base {
            if addr >= base && addr + size as u64 <= base + state.vram_size as u64 {
                let off = (addr - base) as usize;
                if state.vram_ptr.is_null() || off + size > state.vram_size {
                    return Some(vec![0xFF; size]);
                }
                let src = unsafe { std::slice::from_raw_parts(state.vram_ptr.add(off), size) };
                return Some(src.to_vec());
            }
        }
        None
    }
}

impl IoDevice for VgaDevice {
    fn matches_port(&self, port: u16) -> bool {
        matches!(
            port,
            0x1CE | 0x1CF | 0x3C0..=0x3CF | 0x3D4 | 0x3D5 | 0x3DA | 0x3BA
        )
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        self.state.lock().unwrap().port_write(port, data);
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        self.state.lock().unwrap().port_read(port, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vbe_dispi_id_read_write() {
        let mut buf = vec![0u8; 1024];
        let (mut vga, _) = VgaDevice::new(buf.as_mut_ptr(), buf.len());

        // Write index 0 (ID)
        vga.write(0x1CE, &[0x00, 0x00]);
        let id_bytes = vga.read(0x1CF, 2);
        let id = u16::from_le_bytes([id_bytes[0], id_bytes[1]]);
        assert_eq!(id, VBE_DISPI_ID5);

        // Write ID0 (0xB0C0)
        vga.write(0x1CF, &VBE_DISPI_ID0.to_le_bytes());
        let id_bytes2 = vga.read(0x1CF, 2);
        let id2 = u16::from_le_bytes([id_bytes2[0], id_bytes2[1]]);
        assert_eq!(id2, VBE_DISPI_ID0);
    }

    #[test]
    fn vbe_dispi_resolution_config() {
        let mut buf = vec![0u8; 1024];
        let (mut vga, state) = VgaDevice::new(buf.as_mut_ptr(), buf.len());

        // Set XRES = 1024
        vga.write(0x1CE, &[VBE_DISPI_INDEX_XRES as u8, 0]);
        vga.write(0x1CF, &1024u16.to_le_bytes());

        // Set YRES = 768
        vga.write(0x1CE, &[VBE_DISPI_INDEX_YRES as u8, 0]);
        vga.write(0x1CF, &768u16.to_le_bytes());

        // Set BPP = 32
        vga.write(0x1CE, &[VBE_DISPI_INDEX_BPP as u8, 0]);
        vga.write(0x1CF, &32u16.to_le_bytes());

        // Set ENABLE = 1
        vga.write(0x1CE, &[VBE_DISPI_INDEX_ENABLE as u8, 0]);
        vga.write(0x1CF, &[VBE_DISPI_ENABLED as u8, 0]);

        // (tarea 15) VIRT_WIDTH para double-buffering/panning
        vga.write(0x1CE, &[VBE_DISPI_INDEX_VIRT_WIDTH as u8, 0]);
        vga.write(0x1CF, &2048u16.to_le_bytes());

        let st = state.lock().unwrap();
        assert!(st.is_vbe_enabled());
        let (w, h, bpp) = st.get_resolution();
        assert_eq!((w, h, bpp), (1024, 768, 32));
        assert_eq!(st.dispi_regs[VBE_DISPI_INDEX_VIRT_WIDTH as usize], 2048);
    }

    #[test]
    fn standard_vga_graphics_mode_detection() {
        // (tarea 2) Detección de modos gráficos estándar vía el Sequencer
        let mut buf = vec![0u8; 1024];
        let (mut vga, state) = VgaDevice::new(buf.as_mut_ptr(), buf.len());

        // Sin programar → modo texto (por defecto)
        assert!(!state.lock().unwrap().is_standard_vga_graphics());

        // Programar modo 13h: Memory Mode = 0x0E (alpha dis + chain4)
        vga.write(0x3C4, &[0x04]);
        vga.write(0x3C5, &[0x0E]);
        {
            let st = state.lock().unwrap();
            assert!(st.is_standard_vga_graphics());
            assert_eq!(st.standard_vga_geometry(), (320, 200));
        }

        // Volver a modo texto: sin chain-4 y AC Mode Control bit0 = 0
        vga.write(0x3C5, &[0x02]);
        vga.write(0x3C0, &[0x10]); // índice del Mode Control del AC
        vga.write(0x3C0, &[0x00]); // bit0 = 0 → texto
        assert!(!state.lock().unwrap().is_standard_vga_graphics());

        // Modo planar 12h (valores reales del vgabios): SR4=0x07 (sin
        // chain-4), AC Mode Control bit0=1 (gráfico), GRC5=0x20, CRTC
        // HDE=0x4F (80 chars × 8 px = 640), VDE=449 con bit8 en Overflow,
        // Offset=0x28 → 640x450 visible
        vga.write(0x3C4, &[0x04]);
        vga.write(0x3C5, &[0x07]);
        vga.write(0x3C0, &[0x10]);
        vga.write(0x3C0, &[0x01]);
        vga.write(0x3CE, &[0x05]);
        vga.write(0x3CF, &[0x20]);
        vga.write(0x3D4, &[0x01]);
        vga.write(0x3D5, &[0x4F]);
        vga.write(0x3D4, &[0x12]);
        vga.write(0x3D5, &[0xC1]);
        vga.write(0x3D4, &[0x07]);
        vga.write(0x3D5, &[0x02]); // bit1 = VDE[8]
        vga.write(0x3D4, &[0x13]);
        vga.write(0x3D5, &[0x28]);
        {
            let st = state.lock().unwrap();
            assert!(st.is_standard_vga_graphics());
            assert_eq!(st.standard_vga_geometry(), (640, 450));
        }
    }

    #[test]
    fn vbe_mmio_bar_access() {
        // (tarea 1) Acceso a los registros dispi vía BAR2 MMIO
        let mut buf = vec![0u8; 1024];
        let (vga, state) = VgaDevice::new(buf.as_mut_ptr(), buf.len());
        vga.set_mmio_bar(0xF000_0000);

        // XRES (reg 1) y ENABLE (reg 4) vía MMIO (offset = reg*2)
        vga.mmio_write(0xF000_0002, &1024u16.to_le_bytes());
        vga.mmio_write(0xF000_0008, &[VBE_DISPI_ENABLED as u8, 0]);

        {
            let st = state.lock().unwrap();
            assert_eq!(st.dispi_regs[VBE_DISPI_INDEX_XRES as usize], 1024);
            assert!(st.is_vbe_enabled());
        }

        // Lectura del ID vía MMIO (offset 0x00)
        let id = vga.mmio_read(0xF000_0000, 2).unwrap();
        assert_eq!(u16::from_le_bytes([id[0], id[1]]), VBE_DISPI_ID5);

        // Fuera de la ventana MMIO → no es cosa nuestra
        assert!(vga.mmio_read(0xF100_0000, 2).is_none());
    }

    #[test]
    fn lfb_mmio_write_readback() {
        // (tarea 1) Escritura al framebuffer vía BAR0 MMIO y lectura de vuelta
        let mut buf = vec![0xAAu8; 4096];
        let (vga, _) = VgaDevice::new(buf.as_mut_ptr(), buf.len());
        vga.set_lfb_bar(0xC000_0000);

        vga.mmio_write(0xC000_0100, &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(buf[0x100..0x104], [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(buf[0x104], 0xAA); // no pisa vecinos

        let rd = vga.mmio_read(0xC000_0100, 4).unwrap();
        assert_eq!(rd, vec![0x11, 0x22, 0x33, 0x44]);

        // Fuera de la ventana del LFB → None
        assert!(vga.mmio_read(0xC100_0000, 4).is_none());
    }

    #[test]
    fn vbe_mmio_bochs_offset_access() {
        // Acceso MMIO con el layout Bochs / QEMU usado por el kernel Linux (bochs-drm)
        let mut buf = vec![0u8; 1024];
        let (vga, state) = VgaDevice::new(buf.as_mut_ptr(), buf.len());
        vga.set_mmio_bar(0xFE01_F000);

        // 1. Lectura del ID en offset 0x500 (bochs-drm probe)
        let id_bytes = vga.mmio_read(0xFE01_F000 + 0x500, 2).unwrap();
        let id = u16::from_le_bytes([id_bytes[0], id_bytes[1]]);
        assert_eq!(id, VBE_DISPI_ID5);

        // 2. Escritura de XRES (1024) en offset 0x502
        vga.mmio_write(0xFE01_F000 + 0x502, &1024u16.to_le_bytes());
        let xres_bytes = vga.mmio_read(0xFE01_F000 + 0x502, 2).unwrap();
        assert_eq!(u16::from_le_bytes([xres_bytes[0], xres_bytes[1]]), 1024);

        // 3. Escritura de YRES (768) en offset 0x504
        vga.mmio_write(0xFE01_F000 + 0x504, &768u16.to_le_bytes());

        // 4. Habilitar VBE en offset 0x508
        vga.mmio_write(0xFE01_F000 + 0x508, &[VBE_DISPI_ENABLED as u8, 0]);
        {
            let st = state.lock().unwrap();
            assert!(st.is_vbe_enabled());
            assert!(st.vbe_active);
            // Cuando VBE está activo, is_standard_vga_graphics DEBE ser false
            // incluso si el Sequencer tiene chain-4 activo (evita artefactos durante modeset)
            assert!(!st.is_standard_vga_graphics());
        }

        // 5. Lectura de extensión QEMU en offset 0x600 (debe devolver 0 = no extensión)
        let qext = vga.mmio_read(0xFE01_F000 + 0x600, 4).unwrap();
        assert_eq!(qext, vec![0, 0, 0, 0]);

        // 6. Escritura y lectura de registro VGA vía MMIO en 0x400 (0x400 + port - 0x3c0)
        // CRTC index (port 0x3D4) = 0x400 + 0x3D4 - 0x3C0 = 0x414
        vga.mmio_write(0xFE01_F000 + 0x414, &[0x13]);
        let crtc_idx = vga.mmio_read(0xFE01_F000 + 0x414, 1).unwrap();
        assert_eq!(crtc_idx[0], 0x13);
    }

    #[test]
    fn test_vbe_dispi_getcaps() {
        let mut buf = vec![0u8; 1024];
        let mut vga = VgaState::new(buf.as_mut_ptr(), buf.len());

        // Normal mode: reading XRES/YRES/BPP returns configured values (640, 480, 32)
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_XRES), 640);
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_YRES), 480);
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_BPP), 32);

        // Enable GETCAPS flag
        vga.dispi_write_reg(VBE_DISPI_INDEX_ENABLE, VBE_DISPI_GETCAPS);
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_XRES), VBE_DISPI_MAX_XRES);
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_YRES), VBE_DISPI_MAX_YRES);
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_BPP), VBE_DISPI_MAX_BPP);

        // Clearing GETCAPS restores configured resolution
        vga.dispi_write_reg(VBE_DISPI_INDEX_ENABLE, 0);
        assert_eq!(vga.dispi_read_reg(VBE_DISPI_INDEX_XRES), 640);
    }
}
