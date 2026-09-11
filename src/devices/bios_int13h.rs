//! Handlers de interrupción BIOS INT 13h para boot desde CD-ROM y disco.
//!
//! Implementa las sub-funciones más usadas por gestores de arranque
//! (ISOLINUX/GRUB) para cargar el kernel y initrd:
//!
//! - AH=41h: Check LBA Extensions
//! - AH=42h: Extended Read (DAP → sectores 512B del medio)
//! - AH=08h: Get Drive Parameters (disco: geometría CHS; CD: tipo ATAPI)
//! - AH=02h: Read Sectors (CHS)
//!
//! Estos handlers pueden ser invocados directamente por el VMM para
//! servicio de INT 13h sin pasar por el dispatch completo de SeaBIOS
//! (que en el boot normal es quien atiende INT 13h con sus propios
//! drivers ATA/ATAPI sobre los dispositivos emulados).
//!
//! Los sectores lógicos de INT 13h son SIEMPRE de 512 bytes, tanto para
//! disco (sectores físicos de 512B) como para CD-ROM (El Torito los
//! presenta como bloques lógicos de 512B sobre sectores físicos de 2048B).

/// LBA sector size for CD-ROM physical sectors (2048 bytes).
pub const CD_SECTOR_SIZE: u64 = 2048;

/// Standard INT 13h / ATA sector size (512 bytes).
pub const ATA_SECTOR_SIZE: u64 = 512;

/// Drive ID for the first hard disk / CD-ROM (DL register value).
pub const FIRST_DRIVE_ID: u8 = 0x80;

/// ─── Tipo de unidad ────────────────────────────────────────────────
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveType {
    /// Disco duro ATA: sectores físicos de 512 bytes.
    Disk,
    /// CD-ROM ATAPI: sectores físicos de 2048 bytes.
    Cdrom,
}

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
    /// Additional register values (e.g., CX for AH=41h / AH=08h)
    pub cx: u16,
    /// DH: max head number (for AH=08h)
    pub dh: u8,
    /// DL: number of drives (for AH=08h)
    pub dl: u8,
}

impl Int13hResult {
    pub fn success() -> Self {
        Self { ah: 0x00, cf: false, bx: 0, cx: 0, dh: 0, dl: 0 }
    }

    pub fn error(ah: u8) -> Self {
        Self { ah, cf: true, bx: 0, cx: 0, dh: 0, dl: 0 }
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
        dh: 0,
        dl: 0,
    }
}

/// ─── AH=42h: Extended Read via DAP ───────────────────────────────
///
/// Reads sectors from the drive (disco o CD-ROM) using the DAP structure
/// at DS:SI. Los sectores INT 13h son de 512 bytes: para un disco el LBA
/// mapea directo (offset = lba * 512); para un CD-ROM es la vista lógica
/// El Torito sobre el byte-array del medio.
///
/// Devuelve los bytes leídos; el llamador los copia a la memoria del guest
/// en `dap.target_phys()`. `Err(ah)` es un error INT 13h (CF=1).
///
/// Input:
///   AH = 0x42
///   DL = drive ID (0x80+)
///   DS:SI = pointer to Disk Address Packet
///
/// Output:
///   Ok(bytes) con CF = 0 (success), AH = 0x00
///   Err(ah) con CF = 1 (error)
pub fn extended_read(
    dap: &DiskAddressPacket,
    media: &[u8],
) -> Result<Vec<u8>, u8> {
    if dap.num_sectors == 0 {
        return Ok(Vec::new());
    }

    let lba = dap.lba();
    let sector_count = dap.num_sectors as u64;
    let byte_offset = lba * ATA_SECTOR_SIZE;
    let byte_count = sector_count * ATA_SECTOR_SIZE;

    let Some(off) = byte_offset.checked_add(byte_count) else {
        return Err(0x04); // sector not found
    };
    if off > media.len() as u64 {
        return Err(0x04); // sector not found
    }

    Ok(media[byte_offset as usize..off as usize].to_vec())
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
///   CL = max sector number | ((max cylinder >> 8) << 6)
///   DH = max head number
///   DL = number of drives
///   BL = 0x00 (fixed disk) / 0x05 (ATAPI CD-ROM)
pub fn get_drive_params(
    drive_type: DriveType,
    drive_id: u8,
    num_drives: u8,
    disk_size_bytes: u64,
) -> Int13hResult {
    if drive_id < 0x80 {
        return Int13hResult::error(0x80);
    }
    match drive_type {
        DriveType::Disk => {
            let (cylinders, heads, sectors_per_track) = disk_chs_geometry(disk_size_bytes);
            // CL bits 0-5 = max sector; bits 6-7 = high 2 bits of cylinder
            let cl = (sectors_per_track & 0x3F) | ((((cylinders >> 8) & 0x03) as u8) << 6);
            let cx = (((cylinders & 0xFF) as u16) << 8) | cl as u16;
            Int13hResult {
                ah: 0x00,
                cf: false,
                bx: 0x0000, // BL = 0 (fixed disk type)
                cx,
                dh: heads.saturating_sub(1),
                dl: num_drives,
            }
        }
        DriveType::Cdrom => {
            // CD-ROM: geometría genérica, BL=0x05 (ATAPI CD-ROM en la spec BIOS)
            Int13hResult {
                ah: 0x00,
                cf: false,
                bx: 0x0005, // BL=0x05 (ATAPI CD-ROM), BH=0
                cx: 0x0001, // CH=0, CL=1 (1 sector per track)
                dh: 0,
                dl: num_drives,
            }
        }
    }
}

/// ─── Geometría CHS ────────────────────────────────────────────────
/// Geometría INT 13h típica para un disco: 16 cabezas, 63 sectores por
/// pista, cilindros = ceil(sectores / (16*63)), limitado a 1024 (el máximo
/// representable en CHS). Devuelve (cilindros, cabezas, sectores/pista).
pub fn disk_chs_geometry(disk_size_bytes: u64) -> (u16, u8, u8) {
    const HEADS: u64 = 16;
    const SPT: u64 = 63;
    let sectors = disk_size_bytes / ATA_SECTOR_SIZE;
    let cylinders = sectors.div_ceil(HEADS * SPT).clamp(1, 1024) as u16;
    (cylinders, HEADS as u8, SPT as u8)
}

/// ─── CHS → LBA ───────────────────────────────────────────────────
/// sector está en 1..=sectors_per_track (INT 13h los numera desde 1).
pub fn chs_to_lba(
    cylinder: u16,
    head: u8,
    sector: u8,
    heads_per_cyl: u8,
    sectors_per_track: u8,
) -> u64 {
    (cylinder as u64 * heads_per_cyl as u64 + head as u64)
        * sectors_per_track as u64
        + (sector as u64 - 1)
}

/// ─── AH=02h: Read Sectors (CHS) ─────────────────────────────────
/// Lee `count` sectores de 512 bytes usando direccionamiento CHS.
pub fn read_sectors_chs(
    cylinder: u16,
    head: u8,
    sector: u8,
    count: u8,
    heads_per_cyl: u8,
    sectors_per_track: u8,
    media: &[u8],
) -> Result<Vec<u8>, u8> {
    let lba = chs_to_lba(cylinder, head, sector, heads_per_cyl, sectors_per_track);
    let byte_offset = lba * ATA_SECTOR_SIZE;
    let byte_count = count as u64 * ATA_SECTOR_SIZE;
    let Some(end) = byte_offset.checked_add(byte_count) else {
        return Err(0x04);
    };
    if end > media.len() as u64 {
        return Err(0x04);
    }
    Ok(media[byte_offset as usize..end as usize].to_vec())
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

    fn make_media(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

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
    fn test_get_drive_params_disk() {
        // Disco de 1 cilindro (16 cabezas × 63 sectores × 512 B)
        let r = get_drive_params(DriveType::Disk, 0x80, 2, 63 * 16 * 512);
        assert!(!r.cf);
        assert_eq!(r.dl, 2); // nº de unidades
        assert_eq!(r.bx & 0xFF, 0x00); // BL = fixed disk
        // CH=1, CL=63 | 0<<6 → CX = 0x013F; DH = 15
        assert_eq!(r.cx, 0x013F);
        assert_eq!(r.dh, 15);
    }

    #[test]
    fn test_get_drive_params_cdrom() {
        let r = get_drive_params(DriveType::Cdrom, 0x80, 1, 0);
        assert!(!r.cf);
        assert_eq!(r.dl, 1);
        assert_eq!(r.bx & 0xFF, 0x05); // BL = ATAPI CD-ROM
    }

    #[test]
    fn test_extended_read_disk_sectors() {
        // Un disco de 4 sectores de 512 B con contenido conocido
        let mut media = vec![0u8; 4 * 512];
        for (i, b) in media.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        // DAP: LBA=1, 2 sectores (bytes 512..1536)
        let dap = DiskAddressPacket {
            size: 16,
            num_sectors: 2,
            target_offset: 0x7C00,
            target_segment: 0x0000,
            lba_low: 1,
            lba_high: 0,
        };
        let data = extended_read(&dap, &media).unwrap();
        assert_eq!(data.len(), 1024);
        assert_eq!(data[0], media[512]);
        assert_eq!(data[1023], media[1535]);
    }

    #[test]
    fn test_extended_read_cdrom_el_torito_view() {
        // ISO con sectores físicos de 2048 B: la vista INT 13h es de 512 B.
        // LBA lógico 4 (byte offset 2048) cae al inicio del sector físico 1.
        let mut media = vec![0u8; 2 * 2048];
        media[2048..2056].copy_from_slice(b"SECTOR-1");
        let dap = DiskAddressPacket {
            size: 16,
            num_sectors: 1,
            target_offset: 0,
            target_segment: 0x7C0,
            lba_low: 4,
            lba_high: 0,
        };
        let data = extended_read(&dap, &media).unwrap();
        assert_eq!(data.len(), 512);
        assert_eq!(&data[..8], b"SECTOR-1");
    }

    #[test]
    fn test_extended_read_out_of_bounds() {
        let media = make_media(1024);
        let dap = DiskAddressPacket {
            size: 16,
            num_sectors: 2, // 1024 bytes
            target_offset: 0,
            target_segment: 0,
            lba_low: 10, // byte offset 5120 → fuera de rango
            lba_high: 0,
        };
        assert_eq!(extended_read(&dap, &media), Err(0x04));
    }

    #[test]
    fn test_chs_to_lba() {
        assert_eq!(chs_to_lba(0, 0, 1, 16, 63), 0);
        assert_eq!(chs_to_lba(0, 1, 1, 16, 63), 63);
        assert_eq!(chs_to_lba(1, 0, 1, 16, 63), 1008);
        assert_eq!(chs_to_lba(1, 2, 3, 16, 63), 1008 + 2 * 63 + 2);
    }

    #[test]
    fn test_read_sectors_chs() {
        // 64 sectores (32 KB): CHS (0,0,63)=LBA 62 queda dentro del rango
        let media = make_media(64 * 512);
        // CHS (0,0,1) = LBA 0, 1 sector = primeros 512 bytes
        let data = read_sectors_chs(0, 0, 1, 1, 16, 63, &media).unwrap();
        assert_eq!(data, media[..512]);
        // CHS (0,0,63) = LBA 62
        let data = read_sectors_chs(0, 0, 63, 1, 16, 63, &media).unwrap();
        assert_eq!(data, media[62 * 512..63 * 512]);
        // Fuera de rango → error (200 sectores > 64 disponibles)
        let err = read_sectors_chs(0, 0, 1, 200, 16, 63, &media);
        assert_eq!(err, Err(0x04));
    }

    #[test]
    fn test_disk_chs_geometry() {
        // 1 cilindro exacto: 16×63×512 = 516096 bytes
        assert_eq!(disk_chs_geometry(516096), (1, 16, 63));
        // Disco pequeño: mínimo 1 cilindro
        assert_eq!(disk_chs_geometry(512), (1, 16, 63));
        // Disco enorme: se recorta a 1024 cilindros
        assert_eq!(disk_chs_geometry(1 << 40), (1024, 16, 63));
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