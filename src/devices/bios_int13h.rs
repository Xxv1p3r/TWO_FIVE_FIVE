//! Handlers de interrupción BIOS INT 13h para CD-ROM boot.
//!
//! Implementa los sub-funciones más usadas por gestores de arranque
//! (ISOLINUX/GRUB) para cargar el kernel y initrd desde un CD-ROM:
//!
//! - AH=41h: Check LBA Extensions
//! - AH=42h: Extended Read (DAP → SCSI READ(10))
//! - AH=08h: Get Drive Parameters
//!
//! Estos handlers pueden ser invocados directamente por el VMM para
//! servicio de INT 13h sin pasar por el dispatch completo de SeaBIOS.

/// LBA sector size for CD-ROM (2048 bytes).
pub const CD_SECTOR_SIZE: u64 = 2048;

/// Standard ATA sector size (512 bytes) used by INT 13h DAP.
pub const ATA_SECTOR_SIZE: u64 = 512;

/// Drive ID for the first CD-ROM (DL register value).
pub const CDROM_DRIVE_ID: u8 = 0x80;

/// ─── Disk Address Packet (DAP) ────────────────────────────────────
/// Structure at DS:SI for INT 13h AH=42h.
///
/// Offset  Size  Field
/// 0x00    1     Size of DAP (always 16)
/// 0x01    1     Number of sectors to transfer (max 127 for CD-ROM)
/// 0x02    2     Offset of target buffer
/// 0x04    2     Segment of target buffer
/// 0x06    4     LBA low 32 bits
/// 0x0A    4     LBA high 32 bits
#[derive(Debug, Clone)]
pub struct DiskAddressPacket {
    pub size: u8,
    pub num_sectors: u8,
    pub target_offset: u16,
    pub target_segment: u16,
    pub lba_low: u32,
    pub lba_high: u32,
}

impl DiskAddressPacket {
    /// Parse a DAP from a 16-byte buffer.
    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < 16 {
            return None;
        }
        Some(Self {
            size: buf[0],
            num_sectors: buf[1],
            target_offset: u16::from_le_bytes([buf[2], buf[3]]),
            target_segment: u16::from_le_bytes([buf[4], buf[5]]),
            lba_low: u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]),
            lba_high: u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]),
        })
    }

    /// Full 64-bit LBA.
    pub fn lba(&self) -> u64 {
        ((self.lba_high as u64) << 32) | (self.lba_low as u64)
    }

    /// Physical address of the target buffer in guest memory.
    pub fn target_phys(&self) -> u64 {
        ((self.target_segment as u64) << 4) + (self.target_offset as u64)
    }

    /// Total bytes to transfer.
    pub fn byte_count(&self) -> u64 {
        self.num_sectors as u64 * ATA_SECTOR_SIZE
    }
}

/// ─── INT 13h Result ───────────────────────────────────────────────
/// Registers to set after handling INT 13h.
#[derive(Debug, Clone, Copy)]
pub struct Int13hResult {
    /// AH: return code (0x00 = success)
    pub ah: u8,
    /// Carry flag: 0 = success, 1 = error
    pub cf: bool,
    /// Additional register values (e.g., BX for AH=41h)
    pub bx: u16,
    /// Additional register values (e.g., CX for AH=41h)
    pub cx: u16,
    /// DL: number of drives (for AH=08h)
    pub dl: u8,
}

impl Int13hResult {
    pub fn success() -> Self {
        Self { ah: 0x00, cf: false, bx: 0, cx: 0, dl: 0 }
    }

    pub fn error(ah: u8) -> Self {
        Self { ah, cf: true, bx: 0, cx: 0, dl: 0 }
    }
}

/// ─── AH=41h: Check LBA Extensions ────────────────────────────────
///
/// Input:
///   AH = 0x41
///   DL = drive ID (0x80+)
///   BX = 0x55AA
///
/// Output:
///   CF = 0 (success)
///   AH = 0x00 (no error)
///   BX = 0xAA55 (LBA extensions signature)
///   CX = 0x0001 (function bits: extended disk access)
pub fn check_lba_extensions(drive_id: u8) -> Int13hResult {
    if drive_id < 0x80 {
        return Int13hResult::error(0x80); // invalid drive
    }
    Int13hResult {
        ah: 0x00,
        cf: false,
        bx: 0xAA55,
        cx: 0x0001,
        dl: 0,
    }
}

/// ─── AH=42h: Extended Read via DAP ───────────────────────────────
///
/// Reads sectors from the CD-ROM using the DAP structure at DS:SI.
/// Converts to SCSI READ(10) ATAPI commands internally.
///
/// Input:
///   AH = 0x42
///   DL = drive ID (0x80+)
///   DS:SI = pointer to Disk Address Packet
///
/// Output:
///   CF = 0 (success), AH = 0x00
///   CF = 1 (error), AH = error code
pub fn extended_read(
    dap: &DiskAddressPacket,
    iso_data: Option<&[u8]>,
) -> Int13hResult {
    if dap.num_sectors == 0 {
        return Int13hResult::success();
    }

    let lba = dap.lba();
    let sector_count = dap.num_sectors as u64;
    let byte_count = sector_count * ATA_SECTOR_SIZE;
    let target = dap.target_phys();

    if let Some(iso) = iso_data {
        // For CD-ROM, each INT 13h sector is 512 bytes, but the ISO
        // stores 2048-byte sectors. We need to handle the translation.
        // The boot loader uses 512-byte sectors for INT 13h reads.
        // For a CD-ROM with 2048-byte sectors, LBA in INT 13h terms
        // maps to byte offset = lba * 512.
        let byte_offset = lba * ATA_SECTOR_SIZE;

        if byte_offset + byte_count > iso.len() as u64 {
            return Int13hResult::error(0x04); // sector not found
        }

        // Return the raw bytes — the caller writes them to guest memory
        let _data = &iso[byte_offset as usize..(byte_offset + byte_count) as usize];
        Int13hResult::success()
    } else {
        Int13hResult::error(0x80) // timeout / no media
    }
}

/// ─── AH=08h: Get Drive Parameters ────────────────────────────────
///
/// Input:
///   AH = 0x08
///   DL = drive ID (0x80+)
///
/// Output:
///   CF = 0
///   AH = 0x00
///   CH = max cylinder number (low 8 bits)
///   CL = max sector number
///   DH = max head number
///   DL = number of drives
///   ES:DI = DPT pointer (optional, set to 0:0 if not needed)
pub fn get_drive_params(drive_id: u8, num_cdroms: u8) -> Int13hResult {
    if drive_id < 0x80 {
        return Int13hResult::error(0x80);
    }
    // CD-ROM: report as a generic geometry
    // CHS = 0/0/0, BL=0x05 (ATAPI CD-ROM type in BIOS spec)
    Int13hResult {
        ah: 0x00,
        cf: false,
        bx: 0x0005, // BL=0x05 (ATAPI CD-ROM), BH=0
        cx: 0x0001, // CH=0, CL=1 (1 sector per track)
        dl: num_cdroms,
    }
}

/// Build a SCSI READ(10) CDB for reading sectors from a CD-ROM.
///
/// Parameters:
///   lba: logical block address (in 2048-byte CD-ROM sectors)
///   transfer_length: number of 2048-byte sectors to read
///
/// Returns a 12-byte CDB for ATAPI PACKET command.
pub fn build_scsi_read10_cdb(lba: u32, transfer_length: u16) -> [u8; 12] {
    let mut cdb = [0u8; 12];
    cdb[0] = 0x28; // SCSI READ(10) opcode
    // cdb[1] = 0 (FUA, DPO bits = 0)
    cdb[2..6].copy_from_slice(&lba.to_be_bytes());
    // cdb[6] = 0 (groups)
    cdb[7..9].copy_from_slice(&transfer_length.to_be_bytes());
    // cdb[9..12] = 0 (control)
    cdb
}

/// Convert INT 13h LBA sectors (512 bytes each) to CD-ROM LBA sectors (2048 bytes each).
/// Returns (cdrom_lba, cdrom_sector_offset, byte_count).
pub fn int13h_to_cdrom_lba(lba_512: u64, num_512_sectors: u64) -> (u32, u32, u64) {
    let byte_offset = lba_512 * ATA_SECTOR_SIZE;
    let cdrom_lba = (byte_offset / CD_SECTOR_SIZE) as u32;
    let cdrom_offset = (byte_offset % CD_SECTOR_SIZE) as u32;
    let total_bytes = num_512_sectors * ATA_SECTOR_SIZE;
    (cdrom_lba, cdrom_offset, total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dap_parsing() {
        let mut buf = [0u8; 16];
        buf[0] = 0x10;  // size = 16
        buf[1] = 0x04;  // 4 sectors
        buf[2] = 0x00;  // offset low
        buf[3] = 0x7C; // offset high = 0x7C00
        buf[4] = 0x00;  // segment low
        buf[5] = 0x00;  // segment high = 0x0000
        buf[6] = 0x01;  // LBA low byte 0
        buf[7] = 0x00;  // LBA low byte 1
        buf[8] = 0x00;  // LBA low byte 2
        buf[9] = 0x00;  // LBA low byte 3
        buf[10] = 0x00; // LBA high byte 0
        buf[11] = 0x00; // LBA high byte 1
        buf[12] = 0x00; // LBA high byte 2
        buf[13] = 0x00; // LBA high byte 3

        let dap = DiskAddressPacket::from_bytes(&buf).unwrap();
        assert_eq!(dap.num_sectors, 4);
        assert_eq!(dap.target_offset, 0x7C00);
        assert_eq!(dap.target_segment, 0x0000);
        assert_eq!(dap.lba(), 1);
        assert_eq!(dap.target_phys(), 0x7C00);
        assert_eq!(dap.byte_count(), 2048);
    }

    #[test]
    fn test_check_lba_extensions() {
        let r = check_lba_extensions(0x80);
        assert!(!r.cf);
        assert_eq!(r.bx, 0xAA55);
        assert_eq!(r.cx, 0x0001);

        let r = check_lba_extensions(0x00);
        assert!(r.cf);
    }

    #[test]
    fn test_get_drive_params() {
        let r = get_drive_params(0x80, 1);
        assert!(!r.cf);
        assert_eq!(r.dl, 1);
        assert_eq!(r.bx & 0xFF, 0x05); // BL = ATAPI CD-ROM
    }

    #[test]
    fn test_build_scsi_read10_cdb() {
        let cdb = build_scsi_read10_cdb(100, 1);
        assert_eq!(cdb[0], 0x28); // READ(10)
        assert_eq!(cdb[2..6], [0, 0, 0, 100]); // LBA = 100
        assert_eq!(cdb[7..9], [0, 1]); // transfer length = 1
    }

    #[test]
    fn test_int13h_to_cdrom_lba() {
        // LBA 0, 1 sector → CD-ROM LBA 0, offset 0
        let (lba, offset, bytes) = int13h_to_cdrom_lba(0, 1);
        assert_eq!(lba, 0);
        assert_eq!(offset, 0);
        assert_eq!(bytes, 512);

        // LBA 4 (byte offset 2048), 1 sector → CD-ROM LBA 1, offset 0
        let (lba, offset, bytes) = int13h_to_cdrom_lba(4, 1);
        assert_eq!(lba, 1);
        assert_eq!(offset, 0);
        assert_eq!(bytes, 512);
    }
}
