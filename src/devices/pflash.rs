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
            block_size,
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
            block_size,
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
                let end = (offset + count).min(self.data.len());
                let slice = &self.data[offset..end];
                res[..slice.len()].copy_from_slice(slice);
            }
            PFlashMode::StatusRegister => {
                // Devolver el registro de estado replicado en cada byte leído
                res.fill(self.status);
            }
            PFlashMode::CfiQuery => {
                // Firma básica CFI 'Q', 'R', 'Y' en offsets normalizados
                for (i, b) in res.iter_mut().enumerate() {
                    let addr = offset + i;
                    *b = match addr & 0xFF {
                        0x20 => b'Q',
                        0x22 => b'R',
                        0x24 => b'Y',
                        0x26 => 0x01, // Intel Command Set ID LSB
                        0x28 => 0x00, // MSB
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
        if self.readonly {
            // Flash de solo lectura: ignora comandos de borrado/programación
            return false;
        }

        if data.is_empty() {
            return false;
        }

        let cmd = data[0];
        let mut changed = false;

        match self.mode {
            PFlashMode::ReadArray | PFlashMode::StatusRegister | PFlashMode::CfiQuery => {
                match cmd {
                    0xFF | 0x00 => {
                        // Reset / Read Array
                        self.mode = PFlashMode::ReadArray;
                    }
                    0x70 => {
                        // Read Status Register
                        self.mode = PFlashMode::StatusRegister;
                    }
                    0x50 => {
                        // Clear Status Register
                        self.status = Self::STATUS_READY;
                    }
                    0x98 | 0x90 => {
                        // Read Query (CFI) / Read ID
                        self.mode = PFlashMode::CfiQuery;
                    }
                    0x20 => {
                        // Block Erase Setup -> espera confirmación 0xD0
                        self.mode = PFlashMode::BlockEraseWaitingConfirm;
                    }
                    0x40 | 0x10 => {
                        // Program Word / Byte
                        self.mode = PFlashMode::ProgramWord;
                    }
                    _ => {}
                }
            }
            PFlashMode::BlockEraseWaitingConfirm => {
                if cmd == 0xD0 {
                    // Confirmación de borrado de bloque: llenar el bloque con 0xFF
                    let block_start = (offset / self.block_size) * self.block_size;
                    let block_end = (block_start + self.block_size).min(self.data.len());
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
                // Escribir bytes respetando la regla física de la flash (AND de bits)
                let end = (offset + data.len()).min(self.data.len());
                for (i, &b) in data.iter().take(end - offset).enumerate() {
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

    /// Sincroniza las modificaciones al archivo en el disco host (fsync).
    pub fn flush_to_disk(&mut self) -> std::io::Result<()> {
        if !self.dirty || self.readonly {
            return Ok(());
        }

        if let Some(ref path) = self.file_path {
            let mut file = File::create(path)?;
            file.write_all(&self.data)?;
            file.sync_all()?;
            self.dirty = false;
            eprintln!("[PFLASH] VarStore guardado en disco: {} ({} bytes)", path, self.data.len());
        }
        Ok(())
    }
}
