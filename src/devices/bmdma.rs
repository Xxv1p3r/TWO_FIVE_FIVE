//! Emulación de Bus Master IDE (BMDMA) del chipset PIIX3 (Intel 82371FB/SB/AB).
//! Traducido y adaptado de `DevATA.cpp` de Oracle VirtualBox (BMDMAState, BMDMADesc).
//!
//! Permite que el sistema operativo invitado (Linux Mint, etc.) realice transferencias de
//! disco duro y CD-ROM mediante DMA en RAM física en lugar de lecturas PIO palabra a palabra.
//!
//! BAR4 del PCI IDE Controller (00:01.1):
//!   Canal Primario (Hard Disk):
//!     +0x00: Command (Bit 0 = Start/Stop, Bit 3 = Read/Write)
//!     +0x02: Status (Bit 0 = Active, Bit 1 = Error, Bit 2 = Interrupt [R/WC], Bit 5/6 = DMA Capable)
//!     +0x04: PRD Table Base (u32 LE, dirección física de la tabla PRD)
//!   Canal Secundario (CD-ROM ATAPI):
//!     +0x08: Command
//!     +0x0A: Status
//!     +0x0C: PRD Table Base

use super::IoDevice;
use super::cdrom::{AtapiPhase, CdRom, PrimaryIde, ST_DRDY};
use crate::guest_mem::GuestMemory;

pub const BM_CMD_START: u8 = 0x01; // Bit 0: Start transfer
#[allow(dead_code)]
pub const BM_CMD_WRITE: u8 = 0x08; // Bit 3: 1 = Device to RAM (READ), 0 = RAM to Device (WRITE)

pub const BM_STATUS_ACTIVE: u8 = 0x01; // Bit 0: DMA in progress
#[allow(dead_code)]
pub const BM_STATUS_ERROR: u8 = 0x02;  // Bit 1: DMA error
pub const BM_STATUS_INT: u8 = 0x04;    // Bit 2: Interrupt generated (Write-1-to-Clear)
pub const BM_STATUS_DRV0: u8 = 0x20;   // Bit 5: Drive 0 DMA capable
pub const BM_STATUS_DRV1: u8 = 0x40;   // Bit 6: Drive 1 DMA capable

#[derive(Debug, Clone)]
pub struct BmdmaChannel {
    pub cmd: u8,
    pub status: u8,
    pub prd_addr: u32,
}

impl Default for BmdmaChannel {
    fn default() -> Self {
        Self {
            cmd: 0,
            status: BM_STATUS_DRV0 | BM_STATUS_DRV1,
            prd_addr: 0,
        }
    }
}

impl BmdmaChannel {
    pub fn reset(&mut self) {
        self.cmd = 0;
        self.status = BM_STATUS_DRV0 | BM_STATUS_DRV1;
        self.prd_addr = 0;
    }
}

/// Controlador PIIX3 Bus Master IDE (2 canales: Primario y Secundario)
#[derive(Debug, Clone)]
pub struct BmdmaController {
    pub iobase: u16,
    pub primary: BmdmaChannel,
    pub secondary: BmdmaChannel,
}

impl BmdmaController {
    pub fn new() -> Self {
        Self {
            iobase: 0xC000,
            primary: BmdmaChannel::default(),
            secondary: BmdmaChannel::default(),
        }
    }

    pub fn set_iobase(&mut self, base: u16) {
        self.iobase = base;
        eprintln!("[BMDMA] I/O base configurado en 0x{:04X} (dev 1:1 BAR4)", base);
    }

    pub fn reset(&mut self) {
        self.primary.reset();
        self.secondary.reset();
    }

    /// Ejecuta la transferencia DMA del canal primario (Hard Disk) si está activo.
    pub fn execute_primary_dma(
        &mut self,
        primary_ide: &mut PrimaryIde,
        mem: &GuestMemory,
    ) -> bool {
        if self.primary.cmd & BM_CMD_START == 0 || !primary_ide.dma_active {
            return false;
        }

        let mut prd_ptr = (self.primary.prd_addr & !3) as usize;
        let is_write = primary_ide.dma_is_write;
        let lba = primary_ide.get_lba();
        let total_sectors_requested = primary_ide.get_sector_count();
        let total_bytes_requested = (total_sectors_requested as usize) * 512;

        // Recolectar tabla PRD completa para soportar scatter-gather no alineado a 512 B
        let mut prds: Vec<(usize, usize)> = Vec::new();
        let mut prd_total_bytes = 0usize;
        for _ in 0..1024 {
            let phys_buf = mem.read_u32(prd_ptr) as usize;
            let count_and_eot = mem.read_u32(prd_ptr + 4);
            let mut byte_count = (count_and_eot & 0xFFFF) as usize;
            if byte_count == 0 {
                byte_count = 65536; // Especificación ATA: 0 significa 64 KiB
            }
            let eot = (count_and_eot & 0x8000_0000) != 0;
            prds.push((phys_buf, byte_count));
            prd_total_bytes = prd_total_bytes.saturating_add(byte_count);
            prd_ptr += 8;
            if eot || (total_bytes_requested > 0 && prd_total_bytes >= total_bytes_requested) {
                break;
            }
        }

        let transfer_bytes = if total_bytes_requested > 0 {
            total_bytes_requested.min(prd_total_bytes)
        } else {
            prd_total_bytes
        };
        let sectors = ((transfer_bytes + 511) / 512) as u32;

        if sectors > 0 {
            if is_write {
                let mut gathered = vec![0u8; (sectors as usize) * 512];
                let mut off = 0usize;
                for (phys_buf, byte_count) in prds {
                    if off >= transfer_bytes {
                        break;
                    }
                    let take = byte_count.min(transfer_bytes - off);
                    mem.copy_from(phys_buf, &mut gathered[off..off + take]);
                    off += take;
                }
                primary_ide.write_sectors(lba, sectors, &gathered);
            } else {
                let data = primary_ide.read_sectors(lba, sectors);
                let mut off = 0usize;
                for (phys_buf, byte_count) in prds {
                    if off >= transfer_bytes || off >= data.len() {
                        break;
                    }
                    let take = byte_count.min(transfer_bytes - off).min(data.len() - off);
                    mem.copy_to(phys_buf, &data[off..off + take]);
                    off += take;
                }
            }
        }

        primary_ide.dma_active = false;
        primary_ide.set_status(ST_DRDY);
        primary_ide.raise_irq();

        self.primary.status &= !BM_STATUS_ACTIVE;
        self.primary.status |= BM_STATUS_INT;

        true
    }

    /// Ejecuta la transferencia DMA del canal secundario (CD-ROM ATAPI) si está activo.
    pub fn execute_secondary_dma(
        &mut self,
        cdrom: &mut CdRom,
        mem: &GuestMemory,
    ) -> bool {
        if self.secondary.cmd & BM_CMD_START == 0 || !cdrom.is_dma_active() {
            return false;
        }

        let mut prd_ptr = (self.secondary.prd_addr & !3) as usize;
        let mut data_offset = cdrom.get_data_offset();
        let total_bytes = cdrom.get_data_len();
        let mut hit_eot = false;

        for _ in 0..1024 {
            let phys_buf = mem.read_u32(prd_ptr) as usize;
            let count_and_eot = mem.read_u32(prd_ptr + 4);
            let mut byte_count = (count_and_eot & 0xFFFF) as usize;
            if byte_count == 0 {
                byte_count = 65536;
            }
            let eot = (count_and_eot & 0x8000_0000) != 0;

            let remaining = total_bytes.saturating_sub(data_offset);
            let to_transfer = byte_count.min(remaining);

            if to_transfer > 0 {
                let slice = cdrom.get_data_slice(data_offset, to_transfer);
                mem.copy_to(phys_buf, slice);
                data_offset += to_transfer;
            }

            prd_ptr += 8;
            if eot {
                hit_eot = true;
                break;
            }
            if data_offset >= total_bytes {
                break;
            }
        }

        cdrom.set_data_offset(data_offset);
        if hit_eot || data_offset >= total_bytes {
            cdrom.clear_data_buf();
            cdrom.set_phase(AtapiPhase::StatusIn);
            cdrom.set_dma_active(false);
            cdrom.raise_irq();

            self.secondary.status &= !BM_STATUS_ACTIVE;
            self.secondary.status |= BM_STATUS_INT;
            return true;
        }

        false
    }
}

impl IoDevice for BmdmaController {
    fn matches_port(&self, port: u16) -> bool {
        self.iobase != 0
            && port.checked_sub(self.iobase).is_some_and(|off| off < 16)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let Some(base_off) = port.checked_sub(self.iobase) else { return; };
        for (i, &val) in data.iter().enumerate() {
            let offset = (base_off as usize) + i;
            match offset {
                0x00 => { // Primary Command
                    let old_cmd = self.primary.cmd;
                    self.primary.cmd = val;
                    if (val & BM_CMD_START) != 0 && (old_cmd & BM_CMD_START) == 0 {
                        self.primary.status |= BM_STATUS_ACTIVE;
                    } else if (val & BM_CMD_START) == 0 {
                        self.primary.status &= !BM_STATUS_ACTIVE;
                    }
                }
                0x02 => { // Primary Status (Write-1-to-Clear para INT y ERROR)
                    self.primary.status = (self.primary.status & !0x06)
                        | (self.primary.status & !val & 0x06);
                }
                0x04..=0x07 => { // Primary PRD Table Base (u32 LE)
                    let shift = (offset - 0x04) * 8;
                    self.primary.prd_addr =
                        (self.primary.prd_addr & !(0xFF << shift)) | ((val as u32) << shift);
                }
                0x08 => { // Secondary Command
                    let old_cmd = self.secondary.cmd;
                    self.secondary.cmd = val;
                    if (val & BM_CMD_START) != 0 && (old_cmd & BM_CMD_START) == 0 {
                        self.secondary.status |= BM_STATUS_ACTIVE;
                    } else if (val & BM_CMD_START) == 0 {
                        self.secondary.status &= !BM_STATUS_ACTIVE;
                    }
                }
                0x0A => { // Secondary Status (Write-1-to-Clear)
                    self.secondary.status = (self.secondary.status & !0x06)
                        | (self.secondary.status & !val & 0x06);
                }
                0x0C..=0x0F => { // Secondary PRD Table Base (u32 LE)
                    let shift = (offset - 0x0C) * 8;
                    self.secondary.prd_addr =
                        (self.secondary.prd_addr & !(0xFF << shift)) | ((val as u32) << shift);
                }
                _ => {}
            }
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let Some(base_off) = port.checked_sub(self.iobase) else {
            return vec![0xFF; count];
        };
        let offset = base_off as usize;
        let mut res = Vec::with_capacity(count);

        for i in 0..count {
            let reg = offset + i;
            let byte = match reg {
                0x00 => self.primary.cmd,
                0x01 => 0x00,
                0x02 => self.primary.status,
                0x03 => 0x00,
                0x04 => (self.primary.prd_addr & 0xFF) as u8,
                0x05 => ((self.primary.prd_addr >> 8) & 0xFF) as u8,
                0x06 => ((self.primary.prd_addr >> 16) & 0xFF) as u8,
                0x07 => ((self.primary.prd_addr >> 24) & 0xFF) as u8,
                0x08 => self.secondary.cmd,
                0x09 => 0x00,
                0x0A => self.secondary.status,
                0x0B => 0x00,
                0x0C => (self.secondary.prd_addr & 0xFF) as u8,
                0x0D => ((self.secondary.prd_addr >> 8) & 0xFF) as u8,
                0x0E => ((self.secondary.prd_addr >> 16) & 0xFF) as u8,
                0x0F => ((self.secondary.prd_addr >> 24) & 0xFF) as u8,
                _ => 0xFF,
            };
            res.push(byte);
        }

        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmdma_register_read_write() {
        let mut bmdma = BmdmaController::new();
        bmdma.set_iobase(0xC000);

        // Check default status: drives DMA capable
        let st = bmdma.read(0xC002, 1);
        assert_eq!(st[0], BM_STATUS_DRV0 | BM_STATUS_DRV1);

        // Write PRD address: 0x1234_5678 to primary channel
        bmdma.write(0xC004, &0x1234_5678u32.to_le_bytes());
        let prd = bmdma.read(0xC004, 4);
        assert_eq!(u32::from_le_bytes([prd[0], prd[1], prd[2], prd[3]]), 0x1234_5678);

        // Start command (bit 0 = 1, bit 3 = 1: device to RAM)
        bmdma.write(0xC000, &[BM_CMD_START | BM_CMD_WRITE]);
        assert_eq!(bmdma.read(0xC000, 1)[0], BM_CMD_START | BM_CMD_WRITE);
        assert_ne!(bmdma.read(0xC002, 1)[0] & BM_STATUS_ACTIVE, 0);

        // Stop command
        bmdma.write(0xC000, &[0x00]);
        assert_eq!(bmdma.read(0xC002, 1)[0] & BM_STATUS_ACTIVE, 0);
    }

    #[test]
    fn bmdma_secondary_cdrom_prd_transfer() {
        use std::io::Write;

        // Create temporary ISO
        let iso_path = std::env::temp_dir().join("test_bmdma.iso");
        {
            let mut f = std::fs::File::create(&iso_path).unwrap();
            let mut sector = vec![0xABu8; 2048];
            sector[0] = 0x12;
            sector[1] = 0x34;
            f.write_all(&sector).unwrap();
        }

        let mut cd = CdRom::new(iso_path.to_str().unwrap()).unwrap();
        // Prepare dummy guest memory of 64 KiB
        let mut ram = vec![0u8; 65536];
        let mem = GuestMemory::new(ram.as_mut_ptr(), ram.len());

        // Set up PRD table at offset 0x1000:
        // PRD 0: PhysBuf = 0x2000, Count = 2048 bytes (0x0800), EOT = 0x8000_0000
        mem.write_u32(0x1000, 0x2000);
        mem.write_u32(0x1004, 2048 | 0x8000_0000);

        let mut bmdma = BmdmaController::new();
        bmdma.set_iobase(0xC000);
        // Write PRD address to Secondary PRD Table Base (0xC00C)
        bmdma.write(0xC00C, &0x1000u32.to_le_bytes());
        // Start Secondary DMA (0xC008: BM_CMD_START | BM_CMD_WRITE)
        bmdma.write(0xC008, &[BM_CMD_START | BM_CMD_WRITE]);

        // Put 2048 bytes in CD-ROM data buffer
        let mut dummy_data = vec![0xABu8; 2048];
        dummy_data[0] = 0x12;
        dummy_data[1] = 0x34;
        cd.write(0x171, &[0x01]); // Feature = DMA
        // Simulate reading into cdrom
        cd.set_data_buf(dummy_data);
        cd.set_dma_active(true);

        assert!(bmdma.execute_secondary_dma(&mut cd, &mem));

        // Verify data in guest RAM at 0x2000
        assert_eq!(mem.read_u8(0x2000), 0x12);
        assert_eq!(mem.read_u8(0x2001), 0x34);
        assert_eq!(mem.read_u8(0x2002), 0xAB);

        // Verify BMDMA channel state
        assert_eq!(bmdma.secondary.status & BM_STATUS_ACTIVE, 0, "BM_STATUS_ACTIVE should clear on completion");
        assert_ne!(bmdma.secondary.status & BM_STATUS_INT, 0, "Interrupt status bit should be set");
        assert!(!cd.is_dma_active());

        let _ = std::fs::remove_file(iso_path);
    }
}
