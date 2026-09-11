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

#[allow(dead_code)]
pub const VRAM_SIZE: usize = 16 * 1024 * 1024; // 16 MiB
#[allow(dead_code)]
pub const VRAM_DEFAULT_BASE_GPA: u64 = 0xE800_0000;

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
    // VRAM pointer
    pub vram_ptr: *mut u8,
    pub vram_size: usize,
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
            vram_ptr,
            vram_size,
        }
    }

    pub fn is_vbe_enabled(&self) -> bool {
        self.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_ENABLED != 0
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
}

impl IoDevice for VgaDevice {
    fn matches_port(&self, port: u16) -> bool {
        matches!(
            port,
            0x1CE | 0x1CF | 0x3C0..=0x3CF | 0x3D4 | 0x3D5 | 0x3DA | 0x3BA
        )
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap();

        match port {
            // Bochs Dispi Index (0x1CE)
            0x1CE => {
                let idx = if data.len() >= 2 {
                    u16::from_le_bytes([data[0], data[1]])
                } else {
                    data[0] as u16
                };
                state.dispi_index = idx;
            }
            // Bochs Dispi Data (0x1CF)
            0x1CF => {
                let val = if data.len() >= 2 {
                    u16::from_le_bytes([data[0], data[1]])
                } else {
                    data[0] as u16
                };
                let idx = state.dispi_index;
                if (idx as usize) < state.dispi_regs.len() {
                    match idx {
                        VBE_DISPI_INDEX_ID => {
                            if (VBE_DISPI_ID0..=VBE_DISPI_ID5).contains(&val) {
                                state.dispi_regs[idx as usize] = val;
                            }
                        }
                        VBE_DISPI_INDEX_XRES => {
                            state.dispi_regs[idx as usize] = val.clamp(320, 2560);
                        }
                        VBE_DISPI_INDEX_YRES => {
                            state.dispi_regs[idx as usize] = val.clamp(200, 1600);
                        }
                        VBE_DISPI_INDEX_BPP => {
                            if matches!(val, 4 | 8 | 15 | 16 | 24 | 32) {
                                state.dispi_regs[idx as usize] = val;
                            }
                        }
                        VBE_DISPI_INDEX_ENABLE => {
                            state.dispi_regs[idx as usize] = val;
                            eprintln!(
                                "[VGA] VBE Enable=0x{:02X}: {}x{}@{}bpp",
                                val,
                                state.dispi_regs[VBE_DISPI_INDEX_XRES as usize],
                                state.dispi_regs[VBE_DISPI_INDEX_YRES as usize],
                                state.dispi_regs[VBE_DISPI_INDEX_BPP as usize],
                            );
                        }
                        _ => {
                            state.dispi_regs[idx as usize] = val;
                        }
                    }
                }
            }
            // VGA Standard Registers
            0x3C0 => {
                if !state.attr_flipflop {
                    state.attr_index = data[0] & 0x1F;
                } else {
                    let idx = state.attr_index as usize;
                    state.attr_regs[idx] = data[0];
                }
                state.attr_flipflop = !state.attr_flipflop;
            }
            0x3C2 => state.misc_output = data[0],
            0x3C4 => state.seq_index = data[0],
            0x3C5 => {
                let idx = state.seq_index as usize;
                state.seq_regs[idx] = data[0];
            }
            0x3C7 => {
                state.dac_read_index = data[0];
                state.dac_sub_index = 0;
            }
            0x3C8 => {
                state.dac_write_index = data[0];
                state.dac_sub_index = 0;
            }
            0x3C9 => {
                let idx = (state.dac_write_index as usize) * 3 + (state.dac_sub_index as usize);
                if idx < state.dac_palette.len() {
                    state.dac_palette[idx] = data[0];
                }
                state.dac_sub_index += 1;
                if state.dac_sub_index >= 3 {
                    state.dac_sub_index = 0;
                    state.dac_write_index = state.dac_write_index.wrapping_add(1);
                }
            }
            0x3CE => state.grc_index = data[0],
            0x3CF => {
                let idx = state.grc_index as usize;
                state.grc_regs[idx] = data[0];
            }
            0x3D4 => state.crtc_index = data[0],
            0x3D5 => {
                let idx = state.crtc_index as usize;
                state.crtc_regs[idx] = data[0];
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let mut state = self.state.lock().unwrap();
        match port {
            0x1CE => {
                let idx = state.dispi_index;
                if count >= 2 {
                    idx.to_le_bytes().to_vec()
                } else {
                    vec![idx as u8]
                }
            }
            0x1CF => {
                let idx = state.dispi_index;
                let val = if (idx as usize) < state.dispi_regs.len() {
                    state.dispi_regs[idx as usize]
                } else {
                    0xFFFF
                };
                if count >= 2 {
                    val.to_le_bytes().to_vec()
                } else {
                    vec![val as u8]
                }
            }
            0x3C0 => vec![state.attr_index],
            0x3C1 => vec![state.attr_regs[state.attr_index as usize]],
            0x3C2 | 0x3CC => vec![state.misc_output],
            0x3C4 => vec![state.seq_index],
            0x3C5 => vec![state.seq_regs[state.seq_index as usize]],
            0x3C9 => {
                let idx = (state.dac_read_index as usize) * 3 + (state.dac_sub_index as usize);
                let val = if idx < state.dac_palette.len() {
                    state.dac_palette[idx]
                } else {
                    0
                };
                state.dac_sub_index += 1;
                if state.dac_sub_index >= 3 {
                    state.dac_sub_index = 0;
                    state.dac_read_index = state.dac_read_index.wrapping_add(1);
                }
                vec![val]
            }
            0x3CE => vec![state.grc_index],
            0x3CF => vec![state.grc_regs[state.grc_index as usize]],
            0x3D4 => vec![state.crtc_index],
            0x3D5 => vec![state.crtc_regs[state.crtc_index as usize]],
            0x3DA | 0x3BA => {
                state.attr_flipflop = false;
                state.status1_toggle ^= 0x09; // Toggle bit 3 (VSync) and bit 0 (display enable)
                vec![state.status1_toggle]
            }
            _ => vec![0x00; count],
        }
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

        let st = state.lock().unwrap();
        assert!(st.is_vbe_enabled());
        let (w, h, bpp) = st.get_resolution();
        assert_eq!((w, h, bpp), (1024, 768, 32));
    }
}
