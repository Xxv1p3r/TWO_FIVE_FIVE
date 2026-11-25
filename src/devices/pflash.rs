//! Emulación de memoria Flash paralela CFI (Common Flash Interface) para UEFI / OVMF (Item 22).
//!
//! En sistemas UEFI (EDK II / OVMF), el firmware requiere una partición de flash de sólo
//! lectura (CODE) y una partición persistente de lectura/escritura (VARS / VarStore).
//! OVMF accede al VarStore utilizando el conjunto de comandos Intel Command Set 0x0001 (StrataFlash).

use std::fs::File;
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PFlashMode {
    ReadArray,
    StatusRegister,
    CfiQuery,
    BlockEraseWaitingConfirm,
    ProgramWord,
}

#[allow(dead_code)]
pub struct ParallelFlash {
    pub name: String,
    pub data: Vec<u8>,
    pub block_size: usize,
    pub readonly: bool,
    pub mode: PFlashMode,
    pub status: u8,
    pub dirty: bool,
    pub file_path: Option<String>,
}

impl ParallelFlash {
    pub const STATUS_READY: u8 = 0x80;
    pub const STATUS_ERASE_ERROR: u8 = 0x20;
    #[allow(dead_code)]
    pub const STATUS_PROGRAM_ERROR: u8 = 0x10;

    /// Crea una nueva instancia de flash respaldada en memoria.
    pub fn new(name: &str, size: usize, block_size: usize, readonly: bool) -> Self {
        Self {
            name: name.to_string(),
            data: vec![0xFF; size], // Flash virgen borrada = todos los bits a 1 (0xFF)
            block_size: block_size.max(1),
            readonly,
            mode: PFlashMode::ReadArray,
            status: Self::STATUS_READY,
            dirty: false,
            file_path: None,
        }
    }

    /// Carga el contenido de la flash desde un archivo host (ej. OVMF_VARS.fd o OVMF_CODE.fd).
    pub fn load_file(name: &str, path: &str, block_size: usize, readonly: bool) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        Ok(Self {
            name: name.to_string(),
            data,
            block_size: block_size.max(1),
            readonly,
            mode: PFlashMode::ReadArray,
            status: Self::STATUS_READY,
            dirty: false,
            file_path: Some(path.to_string()),
        })
    }

    /// Lee datos de la flash considerando el modo actual del autómata CFI.
    pub fn read(&self, offset: usize, count: usize) -> Vec<u8> {
        let mut res = vec![0xFF; count];
        if offset >= self.data.len() {
            return res;
        }

        match self.mode {
            PFlashMode::ReadArray => {
                let end = offset.saturating_add(count).min(self.data.len());
                let slice = &self.data[offset..end];
                res[..slice.len()].copy_from_slice(slice);
            }
            PFlashMode::StatusRegister => {
                res.fill(self.status);
            }
            PFlashMode::CfiQuery => {
                // Tabla CFI estándar de 8 bits (Intel Command Set 0x0001) para OVMF QemuFlash
                let dev_size_log2 = (self.data.len().max(1).ilog2() as u8).max(16);
                let num_blocks = ((self.data.len() / self.block_size.max(1)).saturating_sub(1)) as u16;
                let blk_sz_256 = (self.block_size / 256) as u16;
                for (i, b) in res.iter_mut().enumerate() {
                    let addr = offset + i;
                    *b = match addr & 0xFF {
                        0x00 => 0x89, // Intel Manufacturer ID
                        0x01 => 0x18, // Device ID
                        0x10 => b'Q',
                        0x11 => b'R',
                        0x12 => b'Y',
                        0x13 => 0x01, // Primary Vendor Command Set: Intel 0x0001
                        0x14 => 0x00,
                        0x15 => 0x31, // Extended table addr
                        0x16 => 0x00,
                        0x20 => b'Q', // Alias de ancho 16-bit por compatibilidad
                        0x22 => b'R',
                        0x24 => b'Y',
                        0x26 => 0x01,
                        0x27 => dev_size_log2,
                        0x28 => 0x00, // x8/x16 interface
                        0x29 => 0x00,
                        0x2A => 0x05, // Max write buffer size = 32 bytes (2^5)
                        0x2B => 0x00,
                        0x2C => 0x01, // 1 erase block region
                        0x2D => (num_blocks & 0xFF) as u8,
                        0x2E => (num_blocks >> 8) as u8,
                        0x2F => (blk_sz_256 & 0xFF) as u8,
                        0x30 => (blk_sz_256 >> 8) as u8,
                        _ => 0x00,
                    };
                }
            }
            _ => {
                res.fill(self.status);
            }
        }
        res
    }

    /// Escribe o envía un comando CFI a la flash.
    /// Devuelve true si los datos persistentes cambiaron.
    pub fn write(&mut self, offset: usize, data: &[u8]) -> bool {
        if data.is_empty() || offset >= self.data.len() {
            return false;
        }

        let cmd = data[0];
        if self.readonly {
            match cmd {
                0xFF | 0x00 => self.mode = PFlashMode::ReadArray,
                0x70 => self.mode = PFlashMode::StatusRegister,
                0x98 | 0x90 => self.mode = PFlashMode::CfiQuery,
                _ => {}
            }
            return false;
        }

        let mut changed = false;

        match self.mode {
            PFlashMode::ReadArray | PFlashMode::StatusRegister | PFlashMode::CfiQuery => {
                match cmd {
                    0xFF | 0x00 => {
                        self.mode = PFlashMode::ReadArray;
                    }
                    0x70 => {
                        self.mode = PFlashMode::StatusRegister;
                    }
                    0x50 => {
                        self.status = Self::STATUS_READY;
                    }
                    0x98 | 0x90 => {
                        self.mode = PFlashMode::CfiQuery;
                    }
                    0x20 => {
                        self.mode = PFlashMode::BlockEraseWaitingConfirm;
                    }
                    0x40 | 0x10 => {
                        self.mode = PFlashMode::ProgramWord;
                    }
                    _ => {}
                }
            }
            PFlashMode::BlockEraseWaitingConfirm => {
                if cmd == 0xD0 {
                    let bsz = self.block_size.max(1);
                    let block_start = (offset / bsz) * bsz;
                    let block_end = block_start.saturating_add(bsz).min(self.data.len());
                    if block_start < self.data.len() {
                        self.data[block_start..block_end].fill(0xFF);
                        self.dirty = true;
                        changed = true;
                    }
                    self.status = Self::STATUS_READY;
                } else {
                    self.status |= Self::STATUS_ERASE_ERROR;
                }
                self.mode = PFlashMode::StatusRegister;
            }
            PFlashMode::ProgramWord => {
                let end = offset.saturating_add(data.len()).min(self.data.len());
                for (i, &b) in data.iter().take(end.saturating_sub(offset)).enumerate() {
                    let target = &mut self.data[offset + i];
                    let new_val = *target & b;
                    if *target != new_val {
                        *target = new_val;
                        self.dirty = true;
                        changed = true;
                    }
                }
                self.status = Self::STATUS_READY;
                self.mode = PFlashMode::StatusRegister;
            }
        }

        changed
    }

    /// Sincroniza las modificaciones al archivo en el disco host de forma atómica (fsync + rename).
    pub fn flush_to_disk(&mut self) -> std::io::Result<()> {
        if !self.dirty || self.readonly {
            return Ok(());
        }

        if let Some(ref path) = self.file_path {
            let tmp_path = format!("{}.tmp", path);
            {
                let mut file = File::create(&tmp_path)?;
                file.write_all(&self.data)?;
                file.sync_all()?;
            }
            std::fs::rename(&tmp_path, path)?;
            self.dirty = false;
            eprintln!("[PFLASH] VarStore guardado en disco: {} ({} bytes)", path, self.data.len());
        }
        Ok(())
    }
}
