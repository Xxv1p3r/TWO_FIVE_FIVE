#![allow(dead_code)]
//! Controlador de Audio Intel 82801AA AC'97 (`8086:2415`).
//!
//! Emula el controlador de audio AC'97 utilizado por VirtualBox (`DevIchAc97.cpp`).
//! Es compatible con Linux ALSA (`snd-intel8x0`), Windows XP, Windows 7, y otros
//! sistemas operativos para PC.
//!
//! Registros expuestos vía PCI:
//! - BAR0 (NAMBAR): 256 bytes de espacio I/O para el mezclador y códec AC'97
//!   (SigmaTel STAC9700, AC'97 revisión 2.3 con soporte VRA).
//! - BAR1 (NABMBAR): 64 bytes de espacio I/O para Bus Master DMA (streams
//!   PCM In, PCM Out, Mic In y registros de control/estado global).

use std::sync::{Arc, Mutex};
use crate::guest_mem::GuestMemory;

// ─── Identificadores PCI ─────────────────────────────────────────────────────

pub const AC97_VENDOR_ID: u16 = 0x8086;
pub const AC97_DEVICE_ID: u16 = 0x2415;

// ─── Códec AC'97 / NAMBAR (Offsets de puertos en BAR0) ───────────────────────

pub const AC97_RESET: u16 = 0x00;
pub const AC97_MASTER_VOLUME_MUTE: u16 = 0x02;
pub const AC97_HEADPHONE_VOLUME_MUTE: u16 = 0x04;
pub const AC97_MASTER_VOLUME_MONO_MUTE: u16 = 0x06;
pub const AC97_PC_BEEP_VOLUME_MUTE: u16 = 0x0A;
pub const AC97_PHONE_VOLUME_MUTE: u16 = 0x0C;
pub const AC97_MIC_VOLUME_MUTE: u16 = 0x0E;
pub const AC97_LINE_IN_VOLUME_MUTE: u16 = 0x10;
pub const AC97_CD_VOLUME_MUTE: u16 = 0x12;
pub const AC97_AUX_VOLUME_MUTE: u16 = 0x16;
pub const AC97_PCM_OUT_VOLUME_MUTE: u16 = 0x18;
pub const AC97_RECORD_SELECT: u16 = 0x1A;
pub const AC97_RECORD_GAIN_MUTE: u16 = 0x1C;
pub const AC97_RECORD_GAIN_MIC_MUTE: u16 = 0x1E;
pub const AC97_GENERAL_PURPOSE: u16 = 0x20;
pub const AC97_3D_CONTROL: u16 = 0x22;
pub const AC97_POWERDOWN_CTRL_STAT: u16 = 0x26;
pub const AC97_EXTENDED_AUDIO_ID: u16 = 0x28;
pub const AC97_EXTENDED_AUDIO_CTRL_STAT: u16 = 0x2A;
pub const AC97_PCM_FRONT_DAC_RATE: u16 = 0x2C;
pub const AC97_PCM_SURROUND_DAC_RATE: u16 = 0x2E;
pub const AC97_PCM_LFE_DAC_RATE: u16 = 0x30;
pub const AC97_PCM_LR_ADC_RATE: u16 = 0x32;
pub const AC97_MIC_ADC_RATE: u16 = 0x34;
pub const AC97_VENDOR_ID1: u16 = 0x7C;
pub const AC97_VENDOR_ID2: u16 = 0x7E;

// Códec SigmaTel STAC9700
pub const SIGMATEL_VENDOR_ID1: u16 = 0x8384;
pub const SIGMATEL_VENDOR_ID2: u16 = 0x7600;

// Capacidades de códec extendido (EAID / EACS)
pub const AC97_EAID_REV1: u16 = 1 << 11; // 0x0800: Revisión 1.x / 2.3
pub const AC97_EACS_VRA: u16 = 0x0001;   // Variable Rate Audio (bit 0)
pub const AC97_EACS_VRM: u16 = 0x0008;   // Variable Rate Mic Audio (bit 3)
pub const AC97_EAID: u16 = AC97_EAID_REV1 | AC97_EACS_VRA | AC97_EACS_VRM; // 0x0809
pub const AC97_EAID_REV23: u16 = AC97_EAID;

// ─── Bus Master DMA / NABMBAR (Offsets en BAR1) ──────────────────────────────

// Desplazamientos base de streams dentro de NABMBAR:
// 0x00: PCM In (PI)
// 0x10: PCM Out (PO)
// 0x20: Mic In (MC)
pub const NABM_OFF_PI: u16 = 0x00;
pub const NABM_OFF_PO: u16 = 0x10;
pub const NABM_OFF_MC: u16 = 0x20;

// Registros relativos al inicio de cada stream
pub const NABM_REG_BDBAR: u16 = 0x00; // Buffer Descriptor Base Address (32 bits)
pub const NABM_REG_CIV: u16 = 0x04;   // Current Index Value (8 bits)
pub const NABM_REG_LVI: u16 = 0x05;   // Last Valid Index (8 bits)
pub const NABM_REG_SR: u16 = 0x06;    // Status Register (16 bits)
pub const NABM_REG_PICB: u16 = 0x08;  // Position In Current Buffer (16 bits)
pub const NABM_REG_PIV: u16 = 0x0A;   // Prefetch Index Value (8 bits)
pub const NABM_REG_CR: u16 = 0x0B;    // Control Register (8 bits)

// Bits del Status Register (SR)
pub const AC97_SR_DCH: u16 = 1 << 0;   // DMA Controller Halted (RO)
pub const AC97_SR_CELV: u16 = 1 << 1;  // Current Equals Last Valid (RO)
pub const AC97_SR_LVBCI: u16 = 1 << 2; // Last Valid Buffer Completion Interrupt (W1C)
pub const AC97_SR_BCIS: u16 = 1 << 3;  // Buffer Completion Interrupt Status (W1C)
pub const AC97_SR_FIFOE: u16 = 1 << 4; // FIFO Error (W1C)

pub const AC97_SR_RO_MASK: u16 = AC97_SR_DCH | AC97_SR_CELV;
pub const AC97_SR_WCLEAR_MASK: u16 = AC97_SR_FIFOE | AC97_SR_BCIS | AC97_SR_LVBCI;

// Bits del Control Register (CR)
pub const AC97_CR_RPBM: u8 = 1 << 0;  // Run/Pause Bus Master (1=Run, 0=Pause)
pub const AC97_CR_RR: u8 = 1 << 1;    // Reset Registers
pub const AC97_CR_LVBIE: u8 = 1 << 2; // Last Valid Buffer Interrupt Enable
pub const AC97_CR_FEIE: u8 = 1 << 3;  // FIFO Error Interrupt Enable
pub const AC97_CR_IOCE: u8 = 1 << 4;  // Interrupt On Completion Enable

// Registros globales de NABMBAR
pub const NABM_REG_GLOB_CNT: u16 = 0x2C; // Global Control (32 bits)
pub const NABM_REG_GLOB_STA: u16 = 0x30; // Global Status (32 bits)
pub const NABM_REG_ACC_SEMA: u16 = 0x34; // Codec Access Semaphore (8 bits)

// Bits y máscaras de GLOB_STA (Global Status Register)
pub const GLOB_STA_GSCI: u32 = 1 << 0;
pub const GLOB_STA_MIINT: u32 = 1 << 1;
pub const GLOB_STA_POINT: u32 = 1 << 2;
pub const GLOB_STA_PIINT: u32 = 1 << 3;
pub const GLOB_STA_PCR: u32 = 1 << 8; // Primary Codec Ready (1 = listo)

pub const AC97_GS_GSCI: u32 = 1 << 0;
pub const AC97_GS_MIINT: u32 = 1 << 1;
pub const AC97_GS_MOINT: u32 = 1 << 2;
pub const AC97_GS_PIINT: u32 = 1 << 5;
pub const AC97_GS_POINT: u32 = 1 << 6;
pub const AC97_GS_MINT: u32 = 1 << 7;
pub const AC97_GS_S0CR: u32 = 1 << 8;
pub const AC97_GS_S1CR: u32 = 1 << 9;
pub const AC97_GS_S0R1: u32 = 1 << 10;
pub const AC97_GS_S1R1: u32 = 1 << 11;
pub const AC97_GS_B1S12: u32 = 1 << 12;
pub const AC97_GS_B2S12: u32 = 1 << 13;
pub const AC97_GS_B3S12: u32 = 1 << 14;
pub const AC97_GS_RCS: u32 = 1 << 15;
pub const AC97_GS_AD3: u32 = 1 << 16;
pub const AC97_GS_MD3: u32 = 1 << 17;

pub const AC97_GS_VALID_MASK: u32 = (1 << 18) - 1;
pub const AC97_GS_WCLEAR_MASK: u32 = AC97_GS_RCS | AC97_GS_S1R1 | AC97_GS_S0R1 | AC97_GS_GSCI;
pub const AC97_GS_RO_MASK: u32 = (1 << 14)
    | (1 << 13)
    | (1 << 12)
    | (1 << 9)
    | GLOB_STA_PCR
    | (1 << 7)
    | (1 << 6)
    | (1 << 5)
    | (1 << 4)
    | (1 << 3)
    | (1 << 2)
    | (1 << 1);

// ─── Estructuras de datos ────────────────────────────────────────────────────

/// Estado de un canal DMA (stream)
#[derive(Clone, Debug)]
pub struct Ac97Stream {
    pub bdbar: u32,
    pub civ: u8,
    pub lvi: u8,
    pub sr: u16,
    pub picb: u16,
    pub piv: u8,
    pub cr: u8,
}

impl Ac97Stream {
    pub fn new() -> Self {
        Self {
            bdbar: 0,
            civ: 0,
            lvi: 0,
            sr: AC97_SR_DCH, // Halted inicialmente
            picb: 0,
            piv: 0,
            cr: 0,
        }
    }

    pub fn reset(&mut self) {
        self.bdbar = 0;
        self.civ = 0;
        self.lvi = 0;
        self.sr = AC97_SR_DCH;
        self.picb = 0;
        self.piv = 0;
        self.cr = 0;
    }
}

/// Estado compartido del controlador AC'97
pub struct Ac97State {
    pub nambar: u16,
    pub nabmbar: u16,
    pub mixer: [u16; 128],
    pub streams: [Ac97Stream; 3], // 0: PI, 1: PO, 2: MC
    pub glob_cnt: u32,
    pub glob_sta: u32,
    pub cas: u8,
    pub irq_asserted: bool,
}

impl Ac97State {
    pub fn new() -> Self {
        let mut s = Self {
            nambar: 0,
            nabmbar: 0,
            mixer: [0u16; 128],
            streams: [Ac97Stream::new(), Ac97Stream::new(), Ac97Stream::new()],
            glob_cnt: 0,
            glob_sta: GLOB_STA_PCR, // Códec primario listo
            cas: 0,
            irq_asserted: false,
        };
        s.reset_mixer();
        s
    }

    pub fn reset(&mut self) {
        self.reset_mixer();
        for st in &mut self.streams {
            st.reset();
        }
        self.glob_cnt = 0;
        self.glob_sta = GLOB_STA_PCR;
        self.cas = 0;
        self.irq_asserted = false;
    }

    pub fn reset_mixer(&mut self) {
        self.mixer.fill(0);
        self.mixer[(AC97_RESET / 2) as usize] = 0x0000;
        self.mixer[(AC97_MASTER_VOLUME_MUTE / 2) as usize] = 0x8000;
        self.mixer[(AC97_HEADPHONE_VOLUME_MUTE / 2) as usize] = 0x8000;
        self.mixer[(AC97_MASTER_VOLUME_MONO_MUTE / 2) as usize] = 0x8000;
        self.mixer[(AC97_PHONE_VOLUME_MUTE / 2) as usize] = 0x8008;
        self.mixer[(AC97_MIC_VOLUME_MUTE / 2) as usize] = 0x8008;
        self.mixer[(AC97_CD_VOLUME_MUTE / 2) as usize] = 0x8808;
        self.mixer[(AC97_AUX_VOLUME_MUTE / 2) as usize] = 0x8808;
        self.mixer[(AC97_PCM_OUT_VOLUME_MUTE / 2) as usize] = 0x8808;
        self.mixer[(AC97_RECORD_GAIN_MUTE / 2) as usize] = 0x8000;
        self.mixer[(AC97_RECORD_GAIN_MIC_MUTE / 2) as usize] = 0x8000;
        self.mixer[(AC97_POWERDOWN_CTRL_STAT / 2) as usize] = 0x000F; // Todos los bloques encendidos
        self.mixer[(AC97_EXTENDED_AUDIO_ID / 2) as usize] = AC97_EAID;
        self.mixer[(AC97_EXTENDED_AUDIO_CTRL_STAT / 2) as usize] = AC97_EACS_VRA | AC97_EACS_VRM;
        self.mixer[(AC97_PCM_FRONT_DAC_RATE / 2) as usize] = 0xBB80; // 48000 Hz
        self.mixer[(AC97_PCM_SURROUND_DAC_RATE / 2) as usize] = 0xBB80;
        self.mixer[(AC97_PCM_LFE_DAC_RATE / 2) as usize] = 0xBB80;
        self.mixer[(AC97_PCM_LR_ADC_RATE / 2) as usize] = 0xBB80;
        self.mixer[(AC97_MIC_ADC_RATE / 2) as usize] = 0xBB80;
        self.mixer[(AC97_VENDOR_ID1 / 2) as usize] = SIGMATEL_VENDOR_ID1;
        self.mixer[(AC97_VENDOR_ID2 / 2) as usize] = SIGMATEL_VENDOR_ID2;
    }

    /// Actualiza el estado de la línea de interrupción evaluando los streams
    pub fn update_irq(&mut self) {
        let mut irq = false;
        // Evaluar PCM In (Stream 0)
        let s0 = &self.streams[0];
        let s0_irq = ((s0.cr & AC97_CR_IOCE) != 0 && (s0.sr & AC97_SR_BCIS) != 0)
            || ((s0.cr & AC97_CR_LVBIE) != 0 && (s0.sr & AC97_SR_LVBCI) != 0)
            || ((s0.cr & AC97_CR_FEIE) != 0 && (s0.sr & AC97_SR_FIFOE) != 0);

        // Evaluar PCM Out (Stream 1)
        let s1 = &self.streams[1];
        let s1_irq = ((s1.cr & AC97_CR_IOCE) != 0 && (s1.sr & AC97_SR_BCIS) != 0)
            || ((s1.cr & AC97_CR_LVBIE) != 0 && (s1.sr & AC97_SR_LVBCI) != 0)
            || ((s1.cr & AC97_CR_FEIE) != 0 && (s1.sr & AC97_SR_FIFOE) != 0);

        // Evaluar Mic In (Stream 2)
        let s2 = &self.streams[2];
        let s2_irq = ((s2.cr & AC97_CR_IOCE) != 0 && (s2.sr & AC97_SR_BCIS) != 0)
            || ((s2.cr & AC97_CR_LVBIE) != 0 && (s2.sr & AC97_SR_LVBCI) != 0)
            || ((s2.cr & AC97_CR_FEIE) != 0 && (s2.sr & AC97_SR_FIFOE) != 0);

        if s0_irq {
            self.glob_sta |= GLOB_STA_PIINT;
            irq = true;
        } else {
            self.glob_sta &= !GLOB_STA_PIINT;
        }

        if s1_irq {
            self.glob_sta |= GLOB_STA_POINT;
            irq = true;
        } else {
            self.glob_sta &= !GLOB_STA_POINT;
        }

        if s2_irq {
            self.glob_sta |= GLOB_STA_MIINT;
            irq = true;
        } else {
            self.glob_sta &= !GLOB_STA_MIINT;
        }

        self.irq_asserted = irq;
    }
}

/// Dispositivo AC'97 conectado a los puertos de I/O
pub struct Ac97Device {
    pub state: Arc<Mutex<Ac97State>>,
}

impl Ac97Device {
    pub fn new(state: Arc<Mutex<Ac97State>>) -> Self {
        Self { state }
    }

    pub fn set_nambar(&mut self, base: u16) {
        self.state.lock().unwrap().nambar = base;
    }

    pub fn set_nabmbar(&mut self, base: u16) {
        self.state.lock().unwrap().nabmbar = base;
    }

    pub fn matches_port(&self, port: u16) -> bool {
        let state = self.state.lock().unwrap();
        let nam = state.nambar;
        let nabm = state.nabmbar;
        (nam != 0 && port >= nam && port < nam + 256) || (nabm != 0 && port >= nabm && port < nabm + 64)
    }

    pub fn is_nambar_reg_mapped(reg_offset: u16) -> bool {
        matches!(
            reg_offset,
            AC97_RESET
                | AC97_MASTER_VOLUME_MUTE
                | AC97_HEADPHONE_VOLUME_MUTE
                | AC97_MASTER_VOLUME_MONO_MUTE
                | AC97_PC_BEEP_VOLUME_MUTE
                | AC97_PHONE_VOLUME_MUTE
                | AC97_MIC_VOLUME_MUTE
                | AC97_LINE_IN_VOLUME_MUTE
                | AC97_CD_VOLUME_MUTE
                | AC97_AUX_VOLUME_MUTE
                | AC97_PCM_OUT_VOLUME_MUTE
                | AC97_RECORD_SELECT
                | AC97_RECORD_GAIN_MUTE
                | AC97_RECORD_GAIN_MIC_MUTE
                | AC97_GENERAL_PURPOSE
                | AC97_3D_CONTROL
                | AC97_POWERDOWN_CTRL_STAT
                | AC97_EXTENDED_AUDIO_ID
                | AC97_EXTENDED_AUDIO_CTRL_STAT
                | AC97_PCM_FRONT_DAC_RATE
                | AC97_PCM_SURROUND_DAC_RATE
                | AC97_PCM_LFE_DAC_RATE
                | AC97_PCM_LR_ADC_RATE
                | AC97_MIC_ADC_RATE
                | AC97_VENDOR_ID1
                | AC97_VENDOR_ID2
        )
    }

    pub fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let mut state = self.state.lock().unwrap();
        let nam = state.nambar;
        let nabm = state.nabmbar;

        // 1. Acceso a NAMBAR (Mezclador / Códec)
        if nam != 0 && port >= nam && port < nam + 256 {
            state.cas = 0;
            let off = (port - nam) as usize;
            if off < 128 {
                let reg_offset = (off & !1) as u16;
                if Self::is_nambar_reg_mapped(reg_offset) {
                    let reg_idx = (reg_offset / 2) as usize;
                    let val = state.mixer[reg_idx];
                    let bytes = val.to_le_bytes();
                    if off % 2 == 0 {
                        if count == 1 {
                            return vec![bytes[0]];
                        } else if count == 2 {
                            return bytes.to_vec();
                        } else if count == 4 && off + 2 < 128 && Self::is_nambar_reg_mapped((off + 2) as u16) {
                            let val2 = state.mixer[reg_idx + 1];
                            let bytes2 = val2.to_le_bytes();
                            return vec![bytes[0], bytes[1], bytes2[0], bytes2[1]];
                        }
                    } else if count == 1 {
                        return vec![bytes[1]];
                    }
                }
            }
            return vec![0xFF; count];
        }

        // 2. Acceso a NABMBAR (Bus Master DMA)
        if nabm != 0 && port >= nabm && port < nabm + 64 {
            let off = port - nabm;
            let val = match off {
                // Global Registers
                NABM_REG_GLOB_CNT => state.glob_cnt,
                NABM_REG_GLOB_STA => state.glob_sta,
                NABM_REG_ACC_SEMA => state.cas as u32,

                // Stream Registers (PI: 0x00..0x0F, PO: 0x10..0x1F, MC: 0x20..0x2F)
                0x00..=0x2F => {
                    let st_idx = (off / 0x10) as usize;
                    let reg_off = off % 0x10;
                    if st_idx < 3 {
                        let st = &state.streams[st_idx];
                        match reg_off {
                            NABM_REG_BDBAR => st.bdbar,
                            NABM_REG_CIV => st.civ as u32,
                            NABM_REG_LVI => st.lvi as u32,
                            NABM_REG_SR => st.sr as u32,
                            NABM_REG_PICB => st.picb as u32,
                            NABM_REG_PIV => st.piv as u32,
                            NABM_REG_CR => st.cr as u32,
                            _ => 0,
                        }
                    } else {
                        0
                    }
                }
                _ => 0,
            };

            let bytes = val.to_le_bytes();
            if count <= 4 {
                return bytes[..count].to_vec();
            }
            return vec![0x00; count];
        }

        vec![0xFF; count]
    }

    pub fn write(&mut self, port: u16, data: &[u8], mem: Option<&GuestMemory>) {
        let mut state = self.state.lock().unwrap();
        let nam = state.nambar;
        let nabm = state.nabmbar;

        // 1. Escritura en NAMBAR (Mezclador / Códec)
        if nam != 0 && port >= nam && port < nam + 256 {
            state.cas = 0;
            let off = (port - nam) as usize;
            if off < 128 && data.len() >= 2 && Self::is_nambar_reg_mapped(off as u16) {
                let reg_idx = off / 2;
                let val = u16::from_le_bytes([data[0], data[1]]);
                match off as u16 {
                    AC97_RESET => {
                        state.reset_mixer();
                    }
                    AC97_EXTENDED_AUDIO_CTRL_STAT => {
                        state.mixer[reg_idx] = val & (AC97_EACS_VRA | AC97_EACS_VRM);
                    }
                    AC97_PCM_FRONT_DAC_RATE
                    | AC97_PCM_SURROUND_DAC_RATE
                    | AC97_PCM_LFE_DAC_RATE
                    | AC97_PCM_LR_ADC_RATE => {
                        // Soportar tasas de muestreo si VRA está activo
                        if (state.mixer[(AC97_EXTENDED_AUDIO_CTRL_STAT / 2) as usize] & AC97_EACS_VRA) != 0 {
                            state.mixer[reg_idx] = val.clamp(8000, 48000);
                        }
                    }
                    AC97_MIC_ADC_RATE => {
                        // Soportar tasa MIC si VRM o VRA está activo
                        if (state.mixer[(AC97_EXTENDED_AUDIO_CTRL_STAT / 2) as usize] & (AC97_EACS_VRA | AC97_EACS_VRM)) != 0 {
                            state.mixer[reg_idx] = val.clamp(8000, 48000);
                        }
                    }
                    AC97_EXTENDED_AUDIO_ID | AC97_VENDOR_ID1 | AC97_VENDOR_ID2 => {
                        // Read-only
                    }
                    _ => {
                        state.mixer[reg_idx] = val;
                    }
                }
            }
            return;
        }

        // 2. Escritura en NABMBAR (Bus Master DMA)
        if nabm != 0 && port >= nabm && port < nabm + 64 {
            let off = port - nabm;
            let val = match data.len() {
                1 => data[0] as u32,
                2 => u16::from_le_bytes([data[0], data[1]]) as u32,
                _ => u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
            };

            match off {
                NABM_REG_GLOB_CNT => {
                    state.glob_cnt = val;
                    if (val & 0x02) != 0 {
                        // Cold reset
                        state.reset();
                    }
                }
                NABM_REG_GLOB_STA => {
                    // Bits WC son Write-1-to-clear
                    state.glob_sta &= !(val & AC97_GS_WCLEAR_MASK);
                    // Preservar los bits válidos distintos de RO/WC (como AD3 y MD3)
                    state.glob_sta |= (val & !(AC97_GS_WCLEAR_MASK | AC97_GS_RO_MASK)) & AC97_GS_VALID_MASK;
                }
                NABM_REG_ACC_SEMA => {
                    state.cas = (val & 0x01) as u8;
                }

                0x00..=0x2F => {
                    let st_idx = (off / 0x10) as usize;
                    let reg_off = off % 0x10;
                    if st_idx < 3 {
                        match reg_off {
                            NABM_REG_BDBAR => {
                                state.streams[st_idx].bdbar = val & !0x07; // 8-byte aligned
                            }
                            NABM_REG_LVI => {
                                let lvi = (val & 0x1F) as u8;
                                state.streams[st_idx].lvi = lvi;
                                if state.streams[st_idx].sr & AC97_SR_CELV != 0 {
                                    state.streams[st_idx].sr &= !(AC97_SR_DCH | AC97_SR_CELV);
                                }
                            }
                            NABM_REG_SR => {
                                // Status register: CELV (bit 1) y DCH (bit 0) son Read-Only. Solo FIFOE | BCIS | LVBCI (bits 4, 3, 2) son W1C.
                                let clear_mask = (val as u16) & AC97_SR_WCLEAR_MASK;
                                state.streams[st_idx].sr &= !clear_mask;
                                state.update_irq();
                            }
                            NABM_REG_CR => {
                                let cr = val as u8;
                                if (cr & AC97_CR_RR) != 0 {
                                    state.streams[st_idx].reset();
                                } else {
                                    let was_running = (state.streams[st_idx].cr & AC97_CR_RPBM) != 0;
                                    state.streams[st_idx].cr = cr;
                                    let now_running = (cr & AC97_CR_RPBM) != 0;

                                    if !was_running && now_running {
                                        state.streams[st_idx].sr &= !AC97_SR_DCH;
                                        // Ejecutar DMA para avanzar el stream
                                        if let Some(guest_mem) = mem {
                                            Self::process_dma_stream(&mut state.streams[st_idx], guest_mem);
                                            state.update_irq();
                                        }
                                    } else if !now_running {
                                        state.streams[st_idx].sr |= AC97_SR_DCH;
                                        state.update_irq();
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Procesa la lista de descriptores de buffer DMA de un stream
    fn process_dma_stream(st: &mut Ac97Stream, mem: &GuestMemory) {
        if st.bdbar == 0 || (st.cr & AC97_CR_RPBM) == 0 {
            return;
        }

        // Cada Buffer Descriptor Entry (BDLE) tiene 8 bytes:
        // - 0..4: Dirección física de muestras (GPA)
        // - 4..6: Número de muestras (16 bits)
        // - 6..8: Banderas (bit 15 = IOC, bit 14 = BUP)
        let bd_addr = st.bdbar as u64 + (st.civ as u64) * 8;
        let mut entry = [0u8; 8];
        if mem.read_bytes(bd_addr, &mut entry).is_ok() {
            let buf_addr = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let samples = u16::from_le_bytes([entry[4], entry[5]]);
            let flags = u16::from_le_bytes([entry[6], entry[7]]);
            let ioc = (flags & 0x8000) != 0;

            if buf_addr != 0 && samples > 0 {
                // Buffer válido: se procesa
                st.picb = samples;

                // Si IOC (Interrupt On Completion) está activado en el descriptor:
                if ioc {
                    st.sr |= AC97_SR_BCIS;
                }

                // Avanzar índice circular CIV
                if st.civ == st.lvi {
                    // Alcanzó el último buffer válido: detener DMA
                    st.sr |= AC97_SR_CELV | AC97_SR_LVBCI | AC97_SR_DCH;
                } else {
                    st.civ = (st.civ + 1) % 32;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ac97_initial_state_and_mixer_reset() {
        let state = Arc::new(Mutex::new(Ac97State::new()));
        let mut dev = Ac97Device::new(state.clone());
        dev.set_nambar(0xE000);
        dev.set_nabmbar(0xE100);

        assert!(dev.matches_port(0xE000));
        assert!(dev.matches_port(0xE0FE));
        assert!(dev.matches_port(0xE100));
        assert!(dev.matches_port(0xE13F));
        assert!(!dev.matches_port(0xE200));

        // Leer Vendor ID de SigmaTel (0x7C y 0x7E)
        let v1_bytes = dev.read(0xE07C, 2);
        let v1 = u16::from_le_bytes([v1_bytes[0], v1_bytes[1]]);
        assert_eq!(v1, SIGMATEL_VENDOR_ID1);

        let v2_bytes = dev.read(0xE07E, 2);
        let v2 = u16::from_le_bytes([v2_bytes[0], v2_bytes[1]]);
        assert_eq!(v2, SIGMATEL_VENDOR_ID2);

        // Leer Primary Codec Ready en GLOB_STA (offset 0x30 en NABMBAR)
        let sta_bytes = dev.read(0xE130, 4);
        let sta = u32::from_le_bytes([sta_bytes[0], sta_bytes[1], sta_bytes[2], sta_bytes[3]]);
        assert_eq!(sta & GLOB_STA_PCR, GLOB_STA_PCR);
    }

    #[test]
    fn test_ac97_dma_stream_execution() {
        let state = Arc::new(Mutex::new(Ac97State::new()));
        let mut dev = Ac97Device::new(state.clone());
        dev.set_nambar(0xE000);
        dev.set_nabmbar(0xE100);

        let mut raw_mem = vec![0u8; 8192];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let bd_table_gpa = 0x1000u32;
        let pcm_buf_gpa = 0x1800u32;
        let samples = 1024u16;
        let flags = 0x8000u16; // IOC = 1

        // Escribir BDLE 0
        let _ = guest_mem.write_bytes(bd_table_gpa as u64 + 0, &pcm_buf_gpa.to_le_bytes());
        let _ = guest_mem.write_bytes(bd_table_gpa as u64 + 4, &samples.to_le_bytes());
        let _ = guest_mem.write_bytes(bd_table_gpa as u64 + 6, &flags.to_le_bytes());

        // Configurar Stream 1 (PO - PCM Out): NABMBAR + 0x10
        // 1. Configurar BDBAR
        dev.write(0xE110, &bd_table_gpa.to_le_bytes(), Some(&guest_mem));
        // 2. Configurar LVI = 0
        dev.write(0xE115, &[0u8], Some(&guest_mem));
        // 3. Configurar CR: RPBM (bit 0) | IOCE (bit 4) = 0x11
        dev.write(0xE11B, &[0x11u8], Some(&guest_mem));

        let s = state.lock().unwrap();
        // Verificar que el stream ejecutó DMA y activó la interrupción
        assert_eq!(s.streams[1].sr & AC97_SR_BCIS, AC97_SR_BCIS);
        assert_eq!(s.streams[1].sr & AC97_SR_CELV, AC97_SR_CELV);
        assert!(s.irq_asserted);
        assert_eq!(s.glob_sta & GLOB_STA_POINT, GLOB_STA_POINT);
    }

    #[test]
    fn test_ac97_eaid_and_eacs_defaults() {
        let state = Arc::new(Mutex::new(Ac97State::new()));
        let mut dev = Ac97Device::new(state.clone());
        dev.set_nambar(0xE000);
        dev.set_nabmbar(0xE100);

        // EAID debe ser 0x0809 (REV1 | VRA | VRM)
        assert_eq!(AC97_EAID, 0x0809);
        let eaid_bytes = dev.read(0xE000 + AC97_EXTENDED_AUDIO_ID, 2);
        let eaid = u16::from_le_bytes([eaid_bytes[0], eaid_bytes[1]]);
        assert_eq!(eaid, 0x0809);

        // EACS por defecto en reset debe ser 0x0009 (VRA | VRM)
        let eacs_bytes = dev.read(0xE000 + AC97_EXTENDED_AUDIO_CTRL_STAT, 2);
        let eacs = u16::from_le_bytes([eacs_bytes[0], eacs_bytes[1]]);
        assert_eq!(eacs, 0x0009);

        // Escritura en EACS enmascara solo bits soportados (VRA | VRM)
        dev.write(0xE000 + AC97_EXTENDED_AUDIO_CTRL_STAT, &0xFFFFu16.to_le_bytes(), None);
        let eacs_after = dev.read(0xE000 + AC97_EXTENDED_AUDIO_CTRL_STAT, 2);
        assert_eq!(u16::from_le_bytes([eacs_after[0], eacs_after[1]]), 0x0009);
    }

    #[test]
    fn test_ac97_sr_write_ro_and_w1c() {
        let state = Arc::new(Mutex::new(Ac97State::new()));
        let mut dev = Ac97Device::new(state.clone());
        dev.set_nambar(0xE000);
        dev.set_nabmbar(0xE100);

        // Forzar todos los bits en SR del stream 1 (PCM Out: 0xE116)
        {
            let mut s = state.lock().unwrap();
            s.streams[1].sr = AC97_SR_DCH | AC97_SR_CELV | AC97_SR_LVBCI | AC97_SR_BCIS | AC97_SR_FIFOE;
        }

        // Escribir 0xFFFF (todos 1): solo FIFOE, BCIS, LVBCI deben limpiarse (W1C).
        // CELV y DCH deben conservarse por ser RO.
        dev.write(0xE110 + NABM_REG_SR, &0xFFFFu16.to_le_bytes(), None);

        {
            let s = state.lock().unwrap();
            let sr = s.streams[1].sr;
            assert_eq!(sr & AC97_SR_FIFOE, 0, "FIFOE no fue limpiado por W1C");
            assert_eq!(sr & AC97_SR_BCIS, 0, "BCIS no fue limpiado por W1C");
            assert_eq!(sr & AC97_SR_LVBCI, 0, "LVBCI no fue limpiado por W1C");
            assert_ne!(sr & AC97_SR_CELV, 0, "CELV fue modificado pero es RO");
            assert_ne!(sr & AC97_SR_DCH, 0, "DCH fue modificado pero es RO");
        }

        // Escribir específicamente los bits RO CELV | DCH (0x0003)
        dev.write(0xE110 + NABM_REG_SR, &[0x03, 0x00], None);
        {
            let s = state.lock().unwrap();
            let sr = s.streams[1].sr;
            assert_ne!(sr & AC97_SR_CELV, 0, "CELV no debe limpiarse por escritura");
            assert_ne!(sr & AC97_SR_DCH, 0, "DCH no debe limpiarse por escritura");
        }
    }

    #[test]
    fn test_ac97_unmapped_nambar_reads() {
        let state = Arc::new(Mutex::new(Ac97State::new()));
        let mut dev = Ac97Device::new(state.clone());
        dev.set_nambar(0xE000);
        dev.set_nabmbar(0xE100);

        // Registro no mapeado dentro de 0x00..0x7E (offset 0x36)
        let r36 = dev.read(0xE036, 2);
        assert_eq!(r36, vec![0xFF, 0xFF]);

        // Lectura de 4 bytes en offset no mapeado 0x36
        let r36_4 = dev.read(0xE036, 4);
        assert_eq!(r36_4, vec![0xFF, 0xFF, 0xFF, 0xFF]);

        // Registro no mapeado (offset 0x08)
        let r08 = dev.read(0xE008, 2);
        assert_eq!(r08, vec![0xFF, 0xFF]);

        // Fuera de registro (offset 0x80 >= 128)
        let r80 = dev.read(0xE080, 2);
        assert_eq!(r80, vec![0xFF, 0xFF]);

        // Offset 0xFE (fuera de registros de mezclador)
        let rfe = dev.read(0xE0FE, 2);
        assert_eq!(rfe, vec![0xFF, 0xFF]);

        // Lectura de 1 byte fuera de registro
        let r80_1 = dev.read(0xE080, 1);
        assert_eq!(r80_1, vec![0xFF]);
    }

    #[test]
    fn test_ac97_glob_sta_write() {
        let state = Arc::new(Mutex::new(Ac97State::new()));
        let mut dev = Ac97Device::new(state.clone());
        dev.set_nambar(0xE000);
        dev.set_nabmbar(0xE100);

        // Estado inicial: PCR activo (bit 8)
        {
            let s = state.lock().unwrap();
            assert_eq!(s.glob_sta & GLOB_STA_PCR, GLOB_STA_PCR);
        }

        // Forzar bit GSCI (bit 0, WC)
        {
            let mut s = state.lock().unwrap();
            s.glob_sta |= AC97_GS_GSCI;
        }

        // Escribir GLOB_STA activando bits RW (AD3: bit 16, MD3: bit 17) y limpiando GSCI (bit 0)
        let write_val = AC97_GS_AD3 | AC97_GS_MD3 | AC97_GS_GSCI;
        dev.write(0xE130, &write_val.to_le_bytes(), None);

        let s = state.lock().unwrap();
        // Bits AD3 y MD3 deben preservarse
        assert_eq!(s.glob_sta & AC97_GS_AD3, AC97_GS_AD3);
        assert_eq!(s.glob_sta & AC97_GS_MD3, AC97_GS_MD3);
        // Bit RO GLOB_STA_PCR debe mantenerse
        assert_eq!(s.glob_sta & GLOB_STA_PCR, GLOB_STA_PCR);
        // Bit WC AC97_GS_GSCI debe haberse limpiado
        assert_eq!(s.glob_sta & AC97_GS_GSCI, 0);
    }
}
