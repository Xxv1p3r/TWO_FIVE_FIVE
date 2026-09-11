//! Emulación de CD-ROM ATAPI en el canal secundario del bus IDE,
//! y stub del canal primario (0x1F0-0x1F7, 0x3F6).
//!
//! SeaBIOS usa el protocolo ATAPI (SCSI over ATA) para hablar con
//! el CD-ROM. El guest escribe el comando 0xA0 (PACKET), luego
//! envía un CDB de 12 bytes por el registro de datos.

use super::IoDevice;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

// ─── Puertos canal secundario ATA (CD-ROM) ─────────────────────────
const SEC_DATA: u16 = 0x170;
const SEC_ERROR: u16 = 0x171;
const SEC_SECTORS: u16 = 0x172;
const SEC_LBA0: u16 = 0x173;
const SEC_LBA1: u16 = 0x174;
const SEC_LBA2: u16 = 0x175;
const SEC_DRIVE: u16 = 0x176;
const SEC_CMD: u16 = 0x177;        // Command (write) / Status (read)
const SEC_ALT_STATUS: u16 = 0x376; // Alternate Status (read) / Device Control (write)

// ─── Puertos canal primario ATA (stub) ─────────────────────────────
const PRI_DATA: u16 = 0x1F0;
const PRI_ERROR: u16 = 0x1F1;
const PRI_SECTORS: u16 = 0x1F2;
const PRI_LBA0: u16 = 0x1F3;
const PRI_LBA1: u16 = 0x1F4;
const PRI_LBA2: u16 = 0x1F5;
const PRI_DRIVE: u16 = 0x1F6;
const PRI_CMD: u16 = 0x1F7;        // Command (write) / Status (read)
const PRI_ALT_STATUS: u16 = 0x3F6; // Alternate Status (read) / Device Control (write)

// ─── Flags de status ATA (must match SeaBIOS ata.h exactly) ────
#[allow(dead_code)]
const ST_ERR: u8 = 1 << 0;   // 0x01 — Error/Check
#[allow(dead_code)]
const ST_DRQ: u8 = 1 << 3;   // 0x08 — Data Request
const ST_DRDY: u8 = 1 << 6;  // 0x40 — Drive Ready
const ST_BSY: u8 = 1 << 7;   // 0x80 — Busy
#[allow(dead_code)]
const ST_DSC: u8 = 1 << 4;   // 0x10 — Seek Complete / Service

// ─── Comandos ATA ──────────────────────────────────────────────────
const CMD_IDENTIFY_PACKET: u8 = 0xA1; // ATAPI IDENTIFY
const CMD_IDENTIFY: u8 = 0xEC;        // ATA IDENTIFY
const CMD_PACKET: u8 = 0xA0;          // ATAPI PACKET

/// Sector CD-ROM = 2048 bytes.
const CD_SECTOR_SIZE: usize = 2048;

/// If DRQ is set and SeaBIOS hasn't read the data port after this many
/// consecutive status reads, we consider it stuck and auto-clear.
const DRQ_STUCK_THRESHOLD: u32 = 200;

// ─── SCSI commands (inside ATAPI PACKET) ───────────────────────────
const SCSI_TEST_READY: u8 = 0x00;
const SCSI_REQ_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1A;
const SCSI_START_STOP: u8 = 0x1B;
const SCSI_PREVENT_ALLOW: u8 = 0x1E;
const SCSI_READ_CAP_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;


// ─── Estado del CD-ROM ATAPI ───────────────────────────────────────
#[derive(PartialEq, Clone, Copy, Debug)]
enum AtapiPhase {
    Idle,
    CdbIn,      // Esperando CDB (12 bytes) del guest
    DataIn,     // Leyendo datos hacia el guest
    StatusIn,   // Leyendo status
}

struct CdromState {
    phase: AtapiPhase,
    /// Buffer para CDB de 12 bytes.
    cdb: [u8; 12],
    cdb_offset: usize,
    /// Buffer de datos para transferir al guest.
    data_buf: Vec<u8>,
    data_offset: usize,
    /// Sense key para SCSI errors.
    sense_key: u8,
    sense_asc: u8,
    sense_ascq: u8,
    /// Registros del canal usados por SeaBIOS en el "bus valid check" de
    /// ata_detect(): escribe 0x55 a SC(0x172), 0xAA a SN(0x173) y el selector
    /// (0xA0/0xB0) a DH(0x176), y exige releerlos idénticos. Sin esto, SeaBIOS
    /// asume que el canal está vacío y NUNCA detecta el CD-ROM.
    reg_sc: u8,
    reg_sn: u8,
    reg_dh: u8,
    /// When true, next status read returns BSY, then ERR.
    /// Used for ATA IDENTIFY (0xEC) on CD-ROM: tells SeaBIOS to try ATAPI.
    pending_ata_error: bool,
    /// When true, next status read returns BSY (device processing).
    /// After BSY is seen, transitions to DataIn (DRQ). Simulates the
    /// normal ATA command flow: BSY → clear → DRQ.
    pending_identify: bool,
    /// When true, next status read returns BSY (device processing PACKET command).
    /// After BSY is seen, transitions to CdbIn (DRQ) so SeaBIOS can send the CDB.
    pending_packet: bool,
    /// Count of consecutive status reads where DRQ was set but data was not
    /// consumed from the data port. If this exceeds DRQ_STUCK_THRESHOLD,
    /// the DRQ state is cleared to prevent infinite loops in SeaBIOS.
    drq_unread_count: u32,
}

impl Default for CdromState {
    fn default() -> Self {
        Self {
            phase: AtapiPhase::Idle,
            cdb: [0u8; 12],
            cdb_offset: 0,
            data_buf: Vec::new(),
            data_offset: 0,
            sense_key: 0,
            sense_asc: 0,
            sense_ascq: 0,
            reg_sc: 0,
            reg_sn: 0,
            reg_dh: 0,
            pending_ata_error: false,
            pending_identify: false,
            pending_packet: false,
            drq_unread_count: 0,
        }
    }
}
pub struct CdRom {
    iso_file: Option<File>,
    iso_size: u64,
    #[allow(dead_code)]
    sector_bytes: usize,
    state: CdromState,
}

impl CdRom {
    pub fn new(iso_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let iso_file = File::open(iso_path)?;
        let iso_size = iso_file.metadata()?.len();

        eprintln!(
            "[CDROM] ISO cargado: {} ({:.1} MB)",
            iso_path,
            iso_size as f64 / (1024.0 * 1024.0)
        );

        Ok(Self {
            iso_file: Some(iso_file),
            iso_size,
            sector_bytes: CD_SECTOR_SIZE,
            state: CdromState::default(),
        })
    }

    /// Crea un stub sin ISO (responde "no media" a los probes de SeaBIOS)
    pub fn stub() -> Self {
        Self {
            iso_file: None,
            iso_size: 0,
            sector_bytes: CD_SECTOR_SIZE,
            state: CdromState::default(),
        }
    }

    /// Calcula el byte de status para devolver al guest.
    fn current_status(&mut self) -> u8 {
        // ATA IDENTIFY on CD-ROM: first read returns ERR, then clears the flag
        if self.state.pending_ata_error {
            self.state.pending_ata_error = false;
            return ST_DRDY | ST_ERR; // ERR tells SeaBIOS to try ATAPI IDENTIFY
        }
        // ATAPI IDENTIFY: first read returns BSY (device processing),
        // then transitions to DataIn (DRQ). Simulates real ATA flow.
        if self.state.pending_identify {
            self.state.pending_identify = false;
            self.state.phase = AtapiPhase::DataIn;
            self.state.drq_unread_count = 0; // Reset stuck counter
            return ST_DRDY | ST_BSY; // BSY = device is processing
        }
        // ATAPI PACKET (0xA0): first read returns BSY (device processing),
        // then transitions to CdbIn (DRQ) so SeaBIOS can send the 12-byte CDB.
        if self.state.pending_packet {
            self.state.pending_packet = false;
            self.state.phase = AtapiPhase::CdbIn;
            self.state.cdb = [0u8; 12];
            self.state.cdb_offset = 0;
            self.state.drq_unread_count = 0;
            return ST_DRDY | ST_BSY; // First read: BSY
        }
        let status = match self.state.phase {
            AtapiPhase::Idle => ST_DRDY,
            AtapiPhase::CdbIn => ST_DRDY | ST_DRQ,
            AtapiPhase::DataIn => ST_DRDY | ST_DRQ,
            AtapiPhase::StatusIn => ST_DRDY,
        };
        // DRQ stuck detection: if DRQ is set, increment counter.
        // If it exceeds threshold without data being consumed, clear the state.
        if status & ST_DRQ != 0 && self.state.data_offset < self.state.data_buf.len() {
            self.state.drq_unread_count += 1;
            if self.state.drq_unread_count >= DRQ_STUCK_THRESHOLD {
                eprintln!("[CDROM] DRQ stuck after {} status reads — clearing", self.state.drq_unread_count);
                self.state.data_buf.clear();
                self.state.data_offset = 0;
                self.state.phase = AtapiPhase::StatusIn;
                self.state.drq_unread_count = 0;
                return ST_DRDY | ST_ERR; // Signal error so SeaBIOS retries
            }
        } else {
            self.state.drq_unread_count = 0;
        }
        status
    }

    /// ATAPI IDENTIFY (cmd 0xA1): 512 bytes que dicen "soy un CD-ROM ATAPI".
    fn do_identify_packet(&mut self) {
        let mut pkt = [0u8; 512];

        // Word 0: ATAPI flag + removable
        pkt[0] = 0x80; // bit 7 = removable device
        pkt[1] = 0x05; // ATAPI device type = CD-ROM
        //      SeaBIOS exige ((word0 >> 8) & 0x1f) == 0x05 para iscd=true,
        //      de lo contrario no lo registra como CD bootable.

        // Word 1: cylindros = 0 (ignored for ATAPI)
        // Word 49-50: capabilities
        pkt[98] = 0x00; // LBA supported
        pkt[99] = 0x02; // IORDY supported

        // Word 53: fields valid
        pkt[106] = 0x06; // words 88 and 70 valid

        // Word 63: multiword DMA
        pkt[126] = 0x07; // mode 0,1,2

        // Word 64: PIO mode
        pkt[128] = 0x03; // mode 3,4

        // Word 76-79: serial
        pkt[152..160].copy_from_slice(b"VMM0001 ");

        // Word 82-84: command set (nop, atapi pkt, atapi mgr, generic)
        pkt[164] = 0x00;
        pkt[165] = 0x00;
        pkt[166] = 0x20; // ATAPI PACKET command

        // Word 100-103: total LBA (48-bit, we use 32-bit)
        let total_lba = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;
        pkt[200..204].copy_from_slice(&total_lba.to_le_bytes());

        // Word 93 (offset 186): hardware reset result. SeaBIOS usa esto en
        // ata_detect(): (resetresult & 0xdf61) == 0x4041 significa que device 0
        // responde a device 1 → no hay device 1 en el canal → no lo vuelve a probe.
        pkt[186] = 0x40;
        pkt[187] = 0x41;

        // Word 126: vendor specific
        // Word 127: removable status

        // Word 128-159: model (40 chars, space-padded, byte-swapped)
        let model_str = b"MI-VMM CD-ROM ATAPI    ";
        for (i, slot) in pkt[256..296].chunks_mut(2).enumerate() {
            let get = |k: usize| -> u8 { model_str.get(k).copied().unwrap_or(b' ') };
            slot[0] = get(i * 2 + 1);
            slot[1] = get(i * 2);
        }

        self.state.data_buf = pkt.to_vec();
        self.state.data_offset = 0;
        // Don't set DataIn yet — first status read returns BSY,
        // then transitions to DataIn (DRQ) on second read.
        self.state.pending_identify = true;

        eprintln!("[CDROM] ATAPI IDENTIFY → CD-ROM ATAPI detectado");
    }

    /// ATA IDENTIFY (cmd 0xEC) — on a CD-ROM, this should return BSY
    /// so SeaBIOS knows to try ATAPI IDENTIFY (0xA1) instead.
    /// We simulate: first read returns BSY, second read returns ERR.
    fn do_identify(&mut self) {
        self.state.pending_ata_error = true;
        self.state.phase = AtapiPhase::Idle;
        eprintln!("[CDROM] ATA IDENTIFY → CD-ROM no es ATA, reportando error para ATAPI fallback");
    }

    /// Ejecuta un SCSI command recibido vía ATAPI PACKET.
    fn execute_scsi(&mut self) {
        let opcode = self.state.cdb[0];
        match opcode {
            SCSI_TEST_READY => {
                // TEST UNIT READY: always ready
                self.state.phase = AtapiPhase::StatusIn;
                self.state.sense_key = 0;
            }
            SCSI_REQ_SENSE => {
                // REQUEST SENSE: return sense data
                let alloc = self.state.cdb[4] as usize;
                let len = alloc.min(18);
                let mut sense = [0u8; 18];
                sense[0] = 0x70; // current errors, fixed format
                sense[2] = self.state.sense_key;
                sense[7] = 10; // additional sense length
                sense[12] = self.state.sense_asc;
                sense[13] = self.state.sense_ascq;
                self.state.data_buf = sense[..len].to_vec();
                self.state.data_offset = 0;
                self.state.phase = AtapiPhase::DataIn;
                // Clear sense after reporting
                self.state.sense_key = 0;
                self.state.sense_asc = 0;
                self.state.sense_ascq = 0;
            }
            SCSI_INQUIRY => {
                let alloc = self.state.cdb[4] as usize;
                let mut inq = [0u8; 96];
                // Peripheral qualifier=0, device type=5 (CD-ROM)
                inq[0] = 0x05;
                // RMB=1 (removable)
                inq[1] = 0x80;
                // Version: SPC-2
                inq[2] = 0x02;
                // Response data format=2
                inq[3] = 0x02;
                // Additional length
                inq[4] = 91;
                // Vendor ID (8 bytes, space-padded)
                inq[8..16].copy_from_slice(b"MI-VMM  ");
                // Product ID (16 bytes, space-padded)
                inq[16..32].copy_from_slice(b"CD-ROM Drive    ");
                // Product revision (4 bytes)
                inq[32..36].copy_from_slice(b"1.0 ");
                let len = alloc.min(96);
                self.state.data_buf = inq[..len].to_vec();
                self.state.data_offset = 0;
                self.state.phase = AtapiPhase::DataIn;
                eprintln!("[CDROM] SCSI INQUIRY → CD-ROM, 96 bytes");
            }
            SCSI_MODE_SENSE_6 => {
                let alloc = self.state.cdb[4] as usize;
                let mut ms = [0u8; 36];
                ms[0] = 35; // mode data length
                ms[1] = 0x00; // medium type
                ms[2] = 0x80; // write-protected
                ms[3] = 0; // block descriptor length
                // Mode page 0x2A (CD-ROM capabilities)
                ms[4] = 0x2A;
                ms[5] = 0x00; // page length (high)
                ms[6] = 0x1E; // page length = 30 bytes
                // Multiple session: 1, Audio play: 1, CD-RW: 0
                ms[7] = 0x00;
                // Read speeds supported
                ms[8] = 0x00;
                ms[9] = 0x00;
                // Number of volume levels (0 = audio not supported)
                ms[10] = 0x00;
                ms[11] = 0x00;
                // Buffer size (in 512-byte units): 0
                ms[12] = 0x00;
                ms[13] = 0x00;
                // Current read speed
                ms[14] = 0x00;
                ms[15] = 0x00;

                let len = alloc.min(36);
                self.state.data_buf = ms[..len].to_vec();
                self.state.data_offset = 0;
                self.state.phase = AtapiPhase::DataIn;
            }
            SCSI_START_STOP => {
                // START/STOP UNIT (for media eject/load)
                self.state.phase = AtapiPhase::StatusIn;
            }
            SCSI_PREVENT_ALLOW => {
                // PREVENT/ALLOW MEDIUM REMOVAL
                self.state.phase = AtapiPhase::StatusIn;
            }
            SCSI_READ_CAP_10 => {
                // READ CAPACITY(10): returns 8 bytes
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1);
                let last_lba = (total_sectors - 1) as u32;
                let block_size = CD_SECTOR_SIZE as u32;
                let mut cap = [0u8; 8];
                cap[0..4].copy_from_slice(&last_lba.to_be_bytes());
                cap[4..8].copy_from_slice(&block_size.to_be_bytes());
                self.state.data_buf = cap.to_vec();
                self.state.data_offset = 0;
                self.state.phase = AtapiPhase::DataIn;
                eprintln!(
                    "[CDROM] SCSI READ CAPACITY → last_lba={} block_size={}",
                    last_lba, block_size
                );
            }
            SCSI_READ_10 => {
                // READ(10): LBA and transfer length from CDB
                let lba = u32::from_be_bytes([
                    self.state.cdb[2],
                    self.state.cdb[3],
                    self.state.cdb[4],
                    self.state.cdb[5],
                ]);
                let transfer_len = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]);
                let byte_count = transfer_len as u64 * CD_SECTOR_SIZE as u64;
                let offset = lba as u64 * CD_SECTOR_SIZE as u64;

                if offset + byte_count > self.iso_size {
                    eprintln!(
                        "[CDROM] READ(10) LBA={} len={} fuera de rango",
                        lba, transfer_len
                    );
                    self.state.sense_key = 0x05; // ILLEGAL REQUEST
                    self.state.sense_asc = 0x21; // LBA out of range
                    self.state.phase = AtapiPhase::StatusIn;
                    return;
                }

                let mut buf = vec![0u8; byte_count as usize];
                if let Some(ref mut f) = self.iso_file {
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let _ = f.read_exact(&mut buf);
                    }
                }
                self.state.data_buf = buf;
                self.state.data_offset = 0;
                self.state.phase = AtapiPhase::DataIn;
                eprintln!(
                    "[CDROM] SCSI READ(10) LBA={} len={} ({} bytes)",
                    lba, transfer_len, byte_count
                );
            }
            _ => {
                eprintln!(
                    "[CDROM] SCSI cmd desconocido: 0x{:02X} CDB={:02X?}",
                    opcode,
                    &self.state.cdb[..]
                );
                self.state.sense_key = 0x05; // ILLEGAL REQUEST
                self.state.sense_asc = 0x20; // INVALID COMMAND OPERATION CODE
                self.state.phase = AtapiPhase::StatusIn;
            }
        }
    }
}

// ─── Canal primario IDE (disco duro ATA) ──────────────────────────
/// Estado de un dispositivo ATA en el canal primario.
/// Soporta IDENTIFY DEVICE, READ SECTORS/EXT, WRITE SECTORS/EXT.
pub struct PrimaryIde {
    /// Archivo de disco (raw, 512-byte sectors)
    disk_file: Option<File>,
    /// Tamaño en bytes del disco (0 si no hay disco)
    disk_size: u64,
    /// Estado del canal ATA
    status: u8,
    /// LBA acumulado (28-bit) desde registros LBA0-2 + bits 6-7 de DH
    lba: u64,
    /// Sector count
    sector_count: u16,
    /// Buffer de datos para lectura/escritura
    data_buf: Vec<u8>,
    /// Offset actual en data_buf
    data_offset: usize,
    /// Registro de dirección del dispositivo (DH/DRIVE)
    drive_select: u8,
    /// Último comando ATA recibido (para diagnóstico)
    last_cmd: u8,
}

impl PrimaryIde {
    pub fn new() -> Self {
        Self {
            disk_file: None,
            disk_size: 0,
            status: ST_DRDY,
            lba: 0,
            sector_count: 1,
            data_buf: Vec::new(),
            data_offset: 0,
            drive_select: 0,
            last_cmd: 0,
        }
        }

    /// Carga un disco duro raw (.img) para el canal primario
    pub fn with_disk(disk_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let file = File::open(disk_path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();

        eprintln!(
            "[ATA] Disco duro cargado: {} ({:.1} MB)",
            disk_path,
            size as f64 / (1024.0 * 1024.0)
        );

        Ok(Self {
            disk_file: Some(file),
            disk_size: size,
            status: ST_DRDY,
            lba: 0,
            sector_count: 1,
            data_buf: Vec::new(),
            data_offset: 0,
            drive_select: 0,
            last_cmd: 0,
        })
    }

    /// Indica si hay un disco conectado
    pub fn has_disk(&self) -> bool {
        self.disk_file.is_some()
    }

    /// Construye un IDENTIFY DEVICE response (512 bytes) para un disco duro ATA
    fn build_identify(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 512];

        // Word 0: general configuration
        let mut word0: u16 = 0;
        if self.drive_select & 0x40 != 0 { word0 |= 0x01; } // LBA
        buf[0] = (word0 & 0xFF) as u8;
        buf[1] = ((word0 >> 8) & 0xFF) as u8;

        // Word 1: cylinders (16383)
        buf[2] = 0x3F; buf[3] = 0x3F;
        buf[4] = 0x00; buf[5] = 0x00;
        buf[8] = 0x03; buf[9] = 0x00;
        buf[14] = 0; buf[15] = 0;
        // Word 8-10: serial number (20 bytes)
        let serial = b"MI-VMM-HDD      ";
        for (i, &b) in serial.iter().take(20).enumerate() {
            buf[16 + i] = b;
        }
        buf[24] = 3; buf[25] = 0;
        buf[26] = 0; buf[27] = 0;
        buf[28] = 0x3F; buf[29] = 0x3F;
        buf[30] = 16; buf[31] = 0;
        buf[32] = 63; buf[33] = 0;
        buf[34] = 63; buf[35] = 0;
        buf[36] = 63; buf[37] = 0;
        let total_sectors = (self.disk_size / 512).max(1) as u32;
        buf[40..44].copy_from_slice(&total_sectors.to_le_bytes());
        // Firmware revision at word 23 (offset 46-53)
        buf[46..54].copy_from_slice(b"01.00   ");
        // Model name at word 27 (offset 54-93), 40 chars, byte-swapped per ATA spec
        let model = b"MI-VMM Virtual HDD ";
        for (i, slot) in buf[54..94].chunks_mut(2).enumerate() {
            let get = |k: usize| -> u8 { model.get(k).copied().unwrap_or(b' ') };
            slot[0] = get(i * 2 + 1);
            slot[1] = get(i * 2);
        }
        buf[98] = 0x00; buf[99] = 0x02; // LBA
        buf[106] = 0x06; buf[107] = 0x00;
        buf[108] = 0x3F; buf[109] = 0x3F;
        buf[112] = 0x10; buf[113] = 0x00;
        buf[116] = 0x3F; buf[117] = 0x3F;
        buf[126] = 0x07; buf[127] = 0x00;
        buf[128] = 0x03; buf[129] = 0x00;
        buf[172] = 0x00; buf[173] = 0x00;
        buf[174] = 0x00; buf[175] = 0x00;
        let lba_total = self.disk_size / 512;
        buf[200..204].copy_from_slice(&lba_total.to_le_bytes());

        buf
    }

    /// Lee n sectores del disco a partir del LBA
    fn read_sectors(&mut self, lba: u64, count: u16) -> Vec<u8> {
        let mut buf = vec![0u8; (count as usize) * 512];
        if let Some(ref mut f) = self.disk_file {
            if let Ok(_) = f.seek(SeekFrom::Start(lba * 512)) {
                let _ = f.read_exact(&mut buf);
            }
        }
        buf
    }

    /// Escribe n sectores al disco
    fn write_sectors(&mut self, lba: u64, count: u16, data: &[u8]) {
        if let Some(ref mut f) = self.disk_file {
            if let Ok(_) = f.seek(SeekFrom::Start(lba * 512)) {
                let _ = f.write_all(&data[..((count as u64 * 512) as usize).min(data.len())]);
            }
        }
    }
}

impl IoDevice for PrimaryIde {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port, PRI_DATA | PRI_ERROR | PRI_SECTORS | PRI_LBA0
            | PRI_LBA1 | PRI_LBA2 | PRI_DRIVE | PRI_CMD | PRI_ALT_STATUS)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let val = data[0];

        match port {
            PRI_DATA => {
                if !self.data_buf.is_empty() && self.data_offset < self.data_buf.len() {
                    self.data_buf[self.data_offset] = val;
                    self.data_offset += 1;
                    // When all bytes written, persist to disk
                    if self.data_offset >= self.data_buf.len() {
                        let count = (self.data_buf.len() / 512) as u16;
                        self.write_sectors(self.lba, count, &self.data_buf.clone());
                        self.data_buf.clear();
                        self.data_offset = 0;
                        self.status = ST_DRDY;
                        eprintln!("[ATA] WRITE SECTORS LBA={:#x} count={}", self.lba, count);
                    }
                }
            }
            PRI_ERROR => {}
            PRI_SECTORS => { self.sector_count = val as u16; }
            PRI_LBA0 => {
                self.lba = (self.lba & 0xFFFFFFFFFFFF00FF) | (val as u64);
            }
            PRI_LBA1 => {
                self.lba = (self.lba & 0xFFFFFFFFFFFF0000) | ((val as u64) << 8);
            }
            PRI_LBA2 => {
                self.lba = (self.lba & 0xFFFFFFFFFF000000) | ((val as u64) << 16);
            }
            PRI_DRIVE => {
                self.drive_select = val;
                let lba_ext = (val & 0x0F) as u64;
                self.lba = (self.lba & 0x0000000000FFFFFF) | (lba_ext << 24);
            }
            PRI_CMD => {
                self.last_cmd = val;
                self.status |= ST_BSY;

                match val {
                    0xEC => { // IDENTIFY DEVICE
                        if self.has_disk() {
                            self.data_buf = self.build_identify();
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                            eprintln!("[ATA] IDENTIFY DEVICE (sector_count={})", self.sector_count);
                        } else {
                            self.status = ST_DRDY | ST_ERR;
                        }
                    }
                    0x20 => { // READ SECTORS (LBA28)
                        if self.has_disk() {
                            let count = if self.sector_count == 0 { 256 } else { self.sector_count };
                            self.data_buf = self.read_sectors(self.lba, count);
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                            eprintln!("[ATA] READ SECTORS LBA={:#x} count={}", self.lba, count);
                        } else {
                            self.status = ST_DRDY | ST_ERR;
                        }
                    }
                    0x24 => { // READ SECTORS EXT (LBA48)
                        if self.has_disk() {
                            let count = if self.sector_count == 0 { 256 } else { self.sector_count };
                            self.data_buf = self.read_sectors(self.lba, count);
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                        }
                    }
                    0x30 => { // WRITE SECTORS
                        if self.has_disk() {
                            self.data_buf = vec![0u8; (self.sector_count as usize) * 512];
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                        }
                    }
                    0x34 => { // WRITE SECTORS EXT
                        if self.has_disk() {
                            let count = if self.sector_count == 0 { 256 } else { self.sector_count };
                            self.data_buf = vec![0u8; (count as usize) * 512];
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                        }
                    }
                    0x90 => { self.status = ST_DRDY; }
                    _ => { self.status = ST_DRDY | ST_ERR; }
                }

                self.status &= !ST_BSY;
            }
            PRI_ALT_STATUS => {
                if val & 0x04 != 0 { // SRST
                    self.status = ST_DRDY;
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, _count: usize) -> Vec<u8> {
        match port {
            PRI_DATA => {
                let mut result = Vec::new();
                for _ in 0..2 {
                    if self.data_offset < self.data_buf.len() {
                        result.push(self.data_buf[self.data_offset]);
                        self.data_offset += 1;
                    } else {
                        result.push(0x00);
                    }
                }
                if self.data_offset >= self.data_buf.len() && !self.data_buf.is_empty() {
                    self.status = ST_DRDY;
                    self.data_buf.clear();
                }
                result
            }
            PRI_ERROR => vec![0x00],
            PRI_SECTORS => vec![(self.sector_count & 0xFF) as u8],
            PRI_LBA0 => vec![(self.lba & 0xFF) as u8],
            PRI_LBA1 => vec![((self.lba >> 8) & 0xFF) as u8],
            PRI_LBA2 => vec![((self.lba >> 16) & 0xFF) as u8],
            PRI_DRIVE => vec![self.drive_select],
            PRI_ALT_STATUS | PRI_CMD => vec![self.status],
            _ => vec![0xFF],
        }
    }
}

// ─── Canal secundario IDE (CD-ROM ATAPI) ──────────────────────────
impl IoDevice for CdRom {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port, 0x170..=0x177 | 0x376)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let val = data[0];

        match port {
            SEC_DATA => {
                // En fase CdbIn, cada byte escrito es parte del CDB.
                if self.state.phase == AtapiPhase::CdbIn {
                    // KVM sends all bytes in a single IoOut exit (from outsw rep).
                    // We must consume ALL bytes in the buffer, not just 2.
                    for &b in data.iter() {
                        if self.state.cdb_offset < 12 {
                            self.state.cdb[self.state.cdb_offset] = b;
                            self.state.cdb_offset += 1;
                        }
                    }
                    // Cuando recibimos los 12 bytes, ejecutamos el SCSI command.
                    if self.state.cdb_offset >= 12 {
                        self.execute_scsi();
                    }
                }
            }
            SEC_LBA1 | SEC_LBA2 | SEC_ERROR => {
                // Registers used during PACKET: irrelevant
            }
            SEC_SECTORS => { self.state.reg_sc = val; }
            SEC_LBA0 => { self.state.reg_sn = val; }
            SEC_DRIVE => { self.state.reg_dh = val; }
            SEC_CMD => {
                // Command register (write): ATA commands go here
                self.state.sense_key = 0;
                self.state.sense_asc = 0;
                self.state.sense_ascq = 0;

                match val {
                    CMD_IDENTIFY_PACKET => {
                        self.do_identify_packet();
                    }
                    CMD_IDENTIFY => {
                        self.do_identify();
                    }
                    CMD_PACKET => {
                        // Don't set CdbIn yet — first status read returns BSY,
                        // then transitions to CdbIn (DRQ) on second read.
                        self.state.pending_packet = true;
                    }
                    0x08 => {} // READ SECTORS (legacy)
                    0x20 => {} // READ SECTORS
                    0x00 => {} // NOP
                    _ => {}
                }
            }
            SEC_ALT_STATUS => {
                // Device Control register write (SRST, nIEN, etc.)
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        match port {
            SEC_CMD => {
                // Status register (read)
                vec![self.current_status()]
            }
            SEC_DATA => {
                match self.state.phase {
                    AtapiPhase::CdbIn => {
                        vec![0x00; count]
                    }
                    AtapiPhase::DataIn => {
                        // Data is being consumed — reset stuck counter
                        self.state.drq_unread_count = 0;
                        let mut result = Vec::with_capacity(count);
                        for _ in 0..count {
                            if self.state.data_offset < self.state.data_buf.len() {
                                result.push(self.state.data_buf[self.state.data_offset]);
                                self.state.data_offset += 1;
                            } else {
                                result.push(0x00);
                            }
                        }
                        // Si se terminaron los datos, cambiar a fase StatusIn
                        if self.state.data_offset >= self.state.data_buf.len() {
                            self.state.phase = AtapiPhase::StatusIn;
                        }
                        result
                    }
                    _ => vec![0x00; count],
                }
            }
            SEC_ALT_STATUS => {
                // Alternate Status register — same status bits as SEC_CMD
                // but does NOT clear IRQ. In our emulation, both return
                // the same status so SeaBIOS can probe via either port.
                vec![self.current_status()]
            }
            SEC_ERROR => vec![0x00],
            SEC_SECTORS => vec![self.state.reg_sc],
            SEC_LBA0 => vec![self.state.reg_sn],
            SEC_DRIVE => vec![self.state.reg_dh],
            _ => vec![0x00],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_test_iso() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mi-vmm-test-{}.iso",
            std::process::id() as u64 + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
        ));
        let mut f = File::create(&path).unwrap();
        for sector in 0u32..4 {
            let mut buf = [0u8; 2048];
            let tag = format!("SECTOR-{}", sector);
            buf[..tag.len()].copy_from_slice(tag.as_bytes());
            buf[100] = sector as u8;
            f.write_all(&buf).unwrap();
        }
        path
    }

    #[test]
    fn atapi_identify_packet_works() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();
        // Send IDENTIFY PACKET DEVICE (0xA1)
        cd.write(SEC_CMD, &[CMD_IDENTIFY_PACKET]);
        // First status read: BSY (device processing)
        let st1 = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st1 & ST_BSY, 0, "BSY must be set first (device processing)");
        // Second status read: DRQ (data ready)
        let st2 = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st2 & ST_DRQ, 0, "DRQ must be set after BSY clears");
        // Read all 512 bytes
        let mut all = Vec::new();
        while cd.read(SEC_CMD, 1)[0] & ST_DRQ != 0 && all.len() < 600 {
            all.extend_from_slice(&cd.read(SEC_DATA, 1));
        }
        assert_eq!(all.len(), 512);
    }

    #[test]
    fn atapi_packet_inquiry() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();
        // Send PACKET command (0xA0)
        cd.write(SEC_CMD, &[CMD_PACKET]);
        // First status read: BSY (pending_packet)
        let st1 = cd.read(SEC_CMD, 1)[0];
        assert!(st1 & ST_BSY != 0, "BSY should be set after PACKET command");
        // Second status read: DRQ (CdbIn phase)
        let st2 = cd.read(SEC_CMD, 1)[0];
        assert!(st2 & ST_DRQ != 0, "DRQ should be set in CdbIn phase");
        // Send 12-byte CDB via data port
        let cdb: [u8; 12] = [SCSI_INQUIRY, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0];
        cd.write(SEC_DATA, &cdb);
        // After CDB, should be DataIn with DRQ
        let st3 = cd.read(SEC_CMD, 1)[0];
        assert!(st3 & ST_DRQ != 0, "DRQ should be set after SCSI INQUIRY data ready");
        // Read inquiry data
        let mut all = Vec::new();
        while cd.read(SEC_CMD, 1)[0] & ST_DRQ != 0 && all.len() < 200 {
            all.extend_from_slice(&cd.read(SEC_DATA, 2));
        }
        assert_eq!(all.len(), 96);
        assert_eq!(all[0] & 0x1F, 0x05); // device type = CD-ROM
        assert_eq!(all[1], 0x80); // RMB = removable
    }

    #[test]
    fn multi_sector_read() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();
        // Send PACKET + READ(10) for 2 sectors starting at LBA 0
        cd.write(SEC_CMD, &[CMD_PACKET]);
        let _ = cd.read(SEC_CMD, 1); // BSY
        let _ = cd.read(SEC_CMD, 1); // DRQ → CdbIn
        let mut cdb = [0u8; 12];
        cdb[0] = SCSI_READ_10; // READ(10)
        // LBA = 0 in bytes 2-5 (big-endian)
        cdb[7] = 0; cdb[8] = 2; // transfer length = 2 sectors
        cd.write(SEC_DATA, &cdb);
        // Status should show DRQ with 4096 bytes ready (2 × 2048)
        let st = cd.read(SEC_CMD, 1)[0];
        assert!(st & ST_DRQ != 0, "DRQ should be set for multi-sector read");
        // Read all 4096 bytes
        let mut all = Vec::new();
        while cd.read(SEC_CMD, 1)[0] & ST_DRQ != 0 && all.len() < 5000 {
            all.extend_from_slice(&cd.read(SEC_DATA, 2));
        }
        assert_eq!(all.len(), 4096, "Should read 2 sectors = 4096 bytes");
        // Verify sector 0 content
        assert_eq!(&all[..8], b"SECTOR-0");
        // Verify sector 1 content (starts at offset 2048)
        assert_eq!(&all[2048..2048+8], b"SECTOR-1");
    }
}
