//! Emulación de CD-ROM ATAPI en el canal secundario del bus IDE,
//! y stub del canal primario (0x1F0-0x1F7, 0x3F6).
//!
//! SeaBIOS usa el protocolo ATAPI (SCSI over ATA) para hablar con
//! el CD-ROM. El guest escribe el comando 0xA0 (PACKET), luego
//! envía un CDB de 12 bytes por el registro de datos.

use super::IoDevice;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

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
pub const ST_DRDY: u8 = 1 << 6;  // 0x40 — Drive Ready
const ST_BSY: u8 = 1 << 7;   // 0x80 — Busy
#[allow(dead_code)]
const ST_DSC: u8 = 1 << 4;   // 0x10 — Seek Complete / Service

// ─── Comandos ATA ──────────────────────────────────────────────────
const CMD_IDENTIFY_PACKET: u8 = 0xA1; // ATAPI IDENTIFY
const CMD_IDENTIFY: u8 = 0xEC;        // ATA IDENTIFY
const CMD_PACKET: u8 = 0xA0;          // ATAPI PACKET

/// Sector CD-ROM = 2048 bytes.
const CD_SECTOR_SIZE: usize = 2048;

const SCSI_TEST_READY: u8 = 0x00;
const SCSI_REQ_SENSE: u8 = 0x03;
const SCSI_READ_6: u8 = 0x08;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1A;
const SCSI_START_STOP: u8 = 0x1B;
const SCSI_PREVENT_ALLOW: u8 = 0x1E;
const SCSI_READ_FORMAT_CAPACITIES: u8 = 0x23;
const SCSI_READ_CAP_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_SEEK_10: u8 = 0x2B;
const SCSI_SYNCHRONIZE_CACHE: u8 = 0x35;
const SCSI_READ_TOC: u8 = 0x43;
const SCSI_GET_CONFIGURATION: u8 = 0x46;
const SCSI_GET_EVENT_STATUS: u8 = 0x4A;
const SCSI_READ_DISC_INFO: u8 = 0x51;
const SCSI_READ_TRACK_INFO: u8 = 0x52;
const SCSI_MODE_SENSE_10: u8 = 0x5A;
const SCSI_READ_12: u8 = 0xA8;
const SCSI_READ_DVD_STRUCTURE: u8 = 0xAD;
const SCSI_MECHANISM_STATUS: u8 = 0xBD;
const SCSI_READ_CD: u8 = 0xBE;

/// Copia una cadena de texto en formato ATA (intercambiando bytes en cada palabra de 16 bits).
fn copy_ata_string(dst: &mut [u8], src: &[u8]) {
    for i in (0..dst.len()).step_by(2) {
        let b0 = src.get(i).copied().unwrap_or(b' ');
        let b1 = src.get(i + 1).copied().unwrap_or(b' ');
        dst[i] = b1;
        if i + 1 < dst.len() {
            dst[i + 1] = b0;
        }
    }
}

// ─── Estado del CD-ROM ATAPI ───────────────────────────────────────
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum AtapiPhase {
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
    /// Cylinder Low (0x174) y High (0x175): firma ATAPI 0x14 y 0xEB.
    /// libata (Linux) exige que tras IDENTIFY (0xEC) o reset estos registros
    /// valgan 0x14 y 0xEB; de lo contrario clasifica el canal como DEV_NONE y
    /// no expone /dev/sr0 al sistema.
    reg_cl: u8,
    reg_ch: u8,
    reg_error: u8,
    irq_pending: bool,
    nien: bool,
    /// Límite de bytes por bloque DRQ establecido por el host en Cilindro Low/High (0x174/0x175).
    byte_count_limit: usize,
    /// Tamaño del bloque DRQ actual que se está transfiriendo.
    current_chunk_len: usize,
    /// Bytes ya transferidos del bloque DRQ actual.
    chunk_offset: usize,
    /// Count of consecutive status reads where DRQ was set but data was not
    /// consumed from the data port. If this exceeds DRQ_STUCK_THRESHOLD,
    /// the DRQ state is cleared to prevent infinite loops in SeaBIOS.
    drq_unread_count: u32,
    ///sense: media_changed flag
    media_changed: bool,
    pub reg_feature: u8,
    pub dma_active: bool,
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
            reg_sc: 0x01,
            reg_sn: 0x01,
            reg_cl: 0x14, // ATAPI signature low
            reg_ch: 0xEB, // ATAPI signature high
            reg_dh: 0,
            reg_error: 0x01, // 0x01 = Device 0 passed diagnostics, Device 1 passed/absent
            irq_pending: false,
            nien: false,
            byte_count_limit: 0xFFFE,
            current_chunk_len: 0,
            chunk_offset: 0,
            drq_unread_count: 0,
            media_changed: false,
            reg_feature: 0,
            dma_active: false,
        }
    }
}
pub struct CdRom {
    iso_file: Option<File>,
    pub iso_size: u64,
    #[allow(dead_code)]
    sector_bytes: usize,
    state: CdromState,
    pub sectors_read_total: u64,
    pub unattended: bool,
    pub unattended_os: crate::unattended::GuestOsType,
    pub unattended_config: crate::unattended::UnattendedConfig,
}

impl CdRom {
    pub fn new(iso_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut resolved_path = PathBuf::from(iso_path);
        if !resolved_path.is_file() {
            for ext in &[".iso", "-current.iso"] {
                let test = PathBuf::from(format!("{}{}", iso_path, ext));
                if test.is_file() {
                    resolved_path = test;
                    break;
                }
            }
            if !resolved_path.is_file() {
                if let Ok(entries) = std::fs::read_dir(".") {
                    let req_lower = iso_path.to_lowercase();
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_file() {
                            if let Some(ext) = path.extension() {
                                if ext.eq_ignore_ascii_case("iso") {
                                    let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                                    if name.contains(&req_lower) {
                                        resolved_path = path;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let iso_file = File::open(&resolved_path)?;
        let iso_size = iso_file.metadata()?.len();

        eprintln!(
            "[CDROM] ISO cargado: {} ({:.1} MB)",
            resolved_path.display(),
            iso_size as f64 / (1024.0 * 1024.0)
        );

        Ok(Self {
            iso_file: Some(iso_file),
            iso_size,
            sector_bytes: CD_SECTOR_SIZE,
            state: CdromState::default(),
            sectors_read_total: 0,
            unattended: false,
            unattended_os: crate::unattended::GuestOsType::Unknown,
            unattended_config: crate::unattended::UnattendedConfig::default(),
        })
    }

    /// Crea un stub sin ISO (responde "no media" a los probes de SeaBIOS)
    pub fn stub() -> Self {
        Self {
            iso_file: None,
            iso_size: 0,
            sector_bytes: CD_SECTOR_SIZE,
            state: CdromState::default(),
            sectors_read_total: 0,
            unattended: false,
            unattended_os: crate::unattended::GuestOsType::Unknown,
            unattended_config: crate::unattended::UnattendedConfig::default(),
        }
    }

    pub fn is_inserted(&self) -> bool {
        self.iso_file.is_some() || self.iso_size > 0
    }

    pub fn eject(&mut self) {
        self.iso_file = None;
        self.iso_size = 0;
        self.state = CdromState::default();
        self.state.sense_key = 0x02; // NOT READY
        self.state.sense_asc = 0x3A; // MEDIUM NOT PRESENT
        self.state.media_changed = true;
    }

    pub fn insert(&mut self, iso_path: &str) -> Result<u64, Box<dyn std::error::Error>> {
        let f = File::open(iso_path)?;
        let size = f.metadata()?.len();
        self.iso_file = Some(f);
        self.iso_size = size;
        self.state = CdromState::default();
        self.state.sense_key = 0x06; // UNIT ATTENTION
        self.state.sense_asc = 0x28; // NOT READY TO READY CHANGE
        self.state.media_changed = true;
        Ok(size)
    }

    /// Reset del dispositivo (reset hardware del canal ATAPI): vuelve a la
    /// fase Idle sin transferencias pendientes. El medio (ISO) y `nien` se conservan.
    pub fn reset(&mut self) {
        let nien = self.state.nien;
        self.state = CdromState::default();
        self.state.nien = nien;
    }

    pub fn raise_irq(&mut self) {
        if !self.state.nien {
            self.state.irq_pending = true;
        }
    }

    pub fn take_irq(&mut self) -> bool {
        let pending = self.state.irq_pending;
        self.state.irq_pending = false;
        pending
    }

    pub fn irq_pending(&self) -> bool {
        self.state.irq_pending && !self.state.nien
    }

    pub fn has_dma_data(&self) -> bool {
        !self.state.data_buf.is_empty()
    }

    pub fn get_data_offset(&self) -> usize {
        self.state.data_offset
    }

    pub fn get_data_len(&self) -> usize {
        self.state.data_buf.len()
    }

    pub fn get_data_byte(&self, idx: usize) -> u8 {
        self.state.data_buf.get(idx).copied().unwrap_or(0)
    }

    pub fn set_data_offset(&mut self, off: usize) {
        self.state.data_offset = off;
    }

    pub fn clear_data_buf(&mut self) {
        self.state.data_buf.clear();
    }

    pub fn set_data_buf(&mut self, buf: Vec<u8>) {
        self.state.data_buf = buf;
    }

    pub fn set_phase(&mut self, phase: AtapiPhase) {
        self.state.phase = phase;
    }

    pub fn is_dma_active(&self) -> bool {
        self.state.dma_active
    }

    pub fn set_dma_active(&mut self, active: bool) {
        self.state.dma_active = active;
    }

    pub fn get_data_slice(&self, offset: usize, len: usize) -> &[u8] {
        let end = (offset + len).min(self.state.data_buf.len());
        if offset < end {
            &self.state.data_buf[offset..end]
        } else {
            &[]
        }
    }

    /// Calcula el byte de status para devolver al guest.
    fn current_status(&mut self) -> u8 {
        let mut status = match self.state.phase {
            AtapiPhase::Idle => ST_DRDY,
            AtapiPhase::CdbIn => ST_DRDY | ST_DRQ,
            AtapiPhase::DataIn => ST_DRDY | ST_DRQ,
            AtapiPhase::StatusIn => ST_DRDY,
        };
        if self.state.sense_key != 0 || (self.state.reg_error != 0 && self.state.reg_error != 0x01) {
            status |= ST_ERR;
        }
        status
    }

    /// ATAPI IDENTIFY (cmd 0xA1): 512 bytes que dicen "soy un CD-ROM ATAPI".
    fn do_identify_packet(&mut self) {
        let mut pkt = [0u8; 512];

        // Word 0: CD-ROM device (0x8580: ATAPI, CD-ROM, 12-byte CDB, removable)
        pkt[0] = 0x80;
        pkt[1] = 0x85;

        // Words 10..19: Serial number (20 bytes)
        copy_ata_string(&mut pkt[20..40], b"VB1234-5678         ");
        // Words 23..26: Firmware revision (8 bytes)
        copy_ata_string(&mut pkt[46..54], b"1.0     ");
        // Words 27..46: Model number (40 bytes)
        copy_ata_string(&mut pkt[54..94], b"VBOX CD-ROM                             ");

        // Word 49: DMA, LBA, IORDY soportados (0x0B00)
        pkt[98] = 0x00;
        pkt[99] = 0x0B;

        // Word 53: Palabras 64-70 y 88 son válidas (0x0006)
        pkt[106] = 0x06;
        pkt[107] = 0x00;

        // Word 63: Multiword DMA modos 0, 1, 2 soportados y activos (0x0007)
        pkt[126] = 0x07;
        pkt[127] = 0x00;

        // Word 64: Modos PIO avanzados 3 y 4 (0x0003)
        pkt[128] = 0x03;
        pkt[129] = 0x00;

        // Words 65..68: Tiempos mínimos de ciclo (120 ns)
        pkt[130] = 120; pkt[132] = 120; pkt[134] = 120; pkt[136] = 120;

        // Word 80: Versión mayor ATA/ATAPI-4..6 (0x007E)
        pkt[160] = 0x7E;
        pkt[161] = 0x00;
        // Word 81: Versión menor (0x0022)
        pkt[162] = 0x22;
        pkt[163] = 0x00;

        // Words 82..87: Command sets
        pkt[164] = 0x10; pkt[165] = 0x02; // Word 82: 0x0210
        pkt[166] = 0x00; pkt[167] = 0x40; // Word 83: 0x4000
        pkt[168] = 0x00; pkt[169] = 0x40; // Word 84: 0x4000
        pkt[170] = 0x10; pkt[171] = 0x02; // Word 85: 0x0210
        pkt[172] = 0x00; pkt[173] = 0x40; // Word 86: 0x4000
        pkt[174] = 0x00; pkt[175] = 0x40; // Word 87: 0x4000

        // Word 88: Ultra DMA modos 0..5 soportados y modo 5 activo (0x207F)
        pkt[176] = 0x7F;
        pkt[177] = 0x20;

        // Word 93 (offset 186): Resultado de reset de hardware (0x4041)
        pkt[186] = 0x40;
        pkt[187] = 0x41;

        // Words 100..103: Total LBA (48-bit)
        let total_lba = (self.iso_size / CD_SECTOR_SIZE as u64).max(1);
        pkt[200..208].copy_from_slice(&total_lba.to_le_bytes());

        // Word 255: Checksum de integridad ATA (incluye 0xA5 en byte 510)
        pkt[510] = 0xA5;
        let sum: u8 = pkt[..511].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        pkt[511] = 0u8.wrapping_sub(sum);

        self.state.reg_sc = 0x02; // Interrupt reason: IO=1, CoD=0
        self.state.byte_count_limit = 512;
        self.start_data_in_mode(pkt.to_vec(), false);

        eprintln!("[CDROM] ATAPI IDENTIFY → CD-ROM ATAPI detectado");
    }

    /// Prepara una transferencia DataIn aplicando el byte_count_limit del host.
    fn start_data_in(&mut self, buf: Vec<u8>) {
        let use_dma = (self.state.reg_feature & 0x01) != 0;
        self.start_data_in_mode(buf, use_dma);
    }

    fn start_data_in_mode(&mut self, buf: Vec<u8>, use_dma: bool) {
        let total = buf.len();
        if total == 0 {
            self.state.data_buf.clear();
            self.state.data_offset = 0;
            self.state.chunk_offset = 0;
            self.state.current_chunk_len = 0;
            self.state.dma_active = false;
            self.state.phase = AtapiPhase::StatusIn;
            self.raise_irq();
            return;
        }
        self.state.data_buf = buf;
        self.state.data_offset = 0;
        self.state.chunk_offset = 0;
        let raw_limit = self.state.byte_count_limit;
        let limit = if raw_limit == 0 || raw_limit == 0xFFFF {
            0xFFFE
        } else {
            (raw_limit & !1).max(2)
        };
        let chunk = total.min(limit);
        self.state.current_chunk_len = chunk;
        self.state.reg_cl = (chunk & 0xFF) as u8;
        self.state.reg_ch = ((chunk >> 8) & 0xFF) as u8;
        self.state.phase = AtapiPhase::DataIn;
        if use_dma {
            self.state.dma_active = true;
        } else {
            self.raise_irq();
        }
    }

    /// ATA IDENTIFY (cmd 0xEC) — en un CD-ROM ATAPI, este comando debe abortar (ABRT=0x04, ST_ERR)
    /// y reportar en Cylinder Low/High la firma mágica ATAPI (0x14, 0xEB).
    fn do_identify(&mut self) {
        self.state.phase = AtapiPhase::Idle;
        self.state.reg_cl = 0x14; // ATAPI signature low
        self.state.reg_ch = 0xEB; // ATAPI signature high
        self.state.reg_error = 0x04; // ABRT
        self.state.reg_sc = 0x01;
        self.state.reg_sn = 0x01;
        self.raise_irq();
        eprintln!("[CDROM] ATA IDENTIFY → CD-ROM no es ATA, reportando firma ATAPI (0x14, 0xEB)");
    }

    fn set_sense_err(&mut self, sk: u8, asc: u8, ascq: u8) {
        self.state.sense_key = sk;
        self.state.sense_asc = asc;
        self.state.sense_ascq = ascq;
        self.state.reg_error = (sk << 4) | 0x04;
        self.state.phase = AtapiPhase::StatusIn;
        self.raise_irq();
    }

    fn clear_sense(&mut self) {
        self.state.sense_key = 0;
        self.state.sense_asc = 0;
        self.state.sense_ascq = 0;
        self.state.reg_error = 0;
    }

    /// Ejecuta un SCSI command recibido vía ATAPI PACKET.
    fn execute_scsi(&mut self) {
        let opcode = self.state.cdb[0];
        let has_media = self.is_inserted();
        match opcode {
            SCSI_TEST_READY => {
                if !has_media {
                    self.set_sense_err(0x02, 0x3A, 0x00);
                } else {
                    self.clear_sense();
                    self.state.phase = AtapiPhase::StatusIn;
                    self.raise_irq();
                }
            }
            SCSI_REQ_SENSE => {
                // REQUEST SENSE: devuelve sense almacenado y lo limpia
                let alloc = self.state.cdb[4] as usize;
                let len = alloc.min(18);
                let mut sense = [0u8; 18];
                sense[0] = 0x70; // current errors, fixed format
                sense[2] = self.state.sense_key;
                sense[7] = 10; // additional sense length
                sense[12] = self.state.sense_asc;
                sense[13] = self.state.sense_ascq;
                self.clear_sense();
                self.start_data_in(sense[..len].to_vec());
            }
            SCSI_INQUIRY => {
                let evpd = (self.state.cdb[1] & 0x01) != 0;
                let page_code = self.state.cdb[2];
                let alloc = u16::from_be_bytes([self.state.cdb[3], self.state.cdb[4]]) as usize;
                let resp = if evpd {
                    match page_code {
                        0x00 => vec![0x05, 0x00, 0x00, 0x03, 0x00, 0x80, 0x83],
                        0x80 => {
                            let serial = b"VB1234-5678";
                            let mut p = vec![0x05, 0x80, 0x00, serial.len() as u8];
                            p.extend_from_slice(serial);
                            p
                        }
                        0x83 => {
                            let mut p = vec![0x05, 0x83, 0x00, 0x14, 0x02, 0x01, 0x00, 0x10];
                            p.extend_from_slice(b"VBOX    VB123456");
                            p
                        }
                        _ => {
                            self.set_sense_err(0x05, 0x24, 0x00);
                            return;
                        }
                    }
                } else {
                    let mut inq = [0u8; 96];
                    inq[0] = 0x05; // CD-ROM
                    inq[1] = 0x80; // RMB=1
                    inq[2] = 0x05; // SPC-3
                    inq[3] = 0x02;
                    inq[4] = 91;
                    inq[8..16].copy_from_slice(b"VBOX    ");
                    inq[16..32].copy_from_slice(b"CD-ROM          ");
                    inq[32..36].copy_from_slice(b"1.0 ");
                    inq.to_vec()
                };
                self.clear_sense();
                let len = alloc.min(resp.len());
                self.start_data_in(resp[..len].to_vec());
                eprintln!("[CDROM] SCSI INQUIRY (evpd={}) → {} bytes", evpd, len);
            }
            SCSI_MODE_SENSE_6 => {
                let alloc = self.state.cdb[4] as usize;
                let page_code = self.state.cdb[2] & 0x3F;
                self.clear_sense();
                if page_code == 0x2A || page_code == 0x3F {
                    let mut ms = [0u8; 36];
                    ms[0] = 35;
                    ms[2] = 0x80;
                    ms[4] = 0x2A;
                    ms[5] = 0x1E; // page length = 30 bytes
                    ms[6] = 0x3F; // read caps
                    ms[8] = 0x71;
                    ms[10] = 0x29;
                    let len = alloc.min(36);
                    self.start_data_in(ms[..len].to_vec());
                } else {
                    let mut ms = [0u8; 4];
                    ms[0] = 3;
                    ms[2] = 0x80;
                    let len = alloc.min(4);
                    self.start_data_in(ms[..len].to_vec());
                }
            }
            SCSI_START_STOP | SCSI_PREVENT_ALLOW => {
                self.clear_sense();
                self.state.phase = AtapiPhase::StatusIn;
                self.raise_irq();
            }
            SCSI_READ_CAP_10 => {
                if !has_media {
                    self.set_sense_err(0x02, 0x3A, 0x00);
                    return;
                }
                self.clear_sense();
                let total_sectors = ((self.iso_size + (CD_SECTOR_SIZE as u64 - 1)) / CD_SECTOR_SIZE as u64).max(1);
                let last_lba = (total_sectors - 1) as u32;
                let block_size = CD_SECTOR_SIZE as u32;
                let mut cap = [0u8; 8];
                cap[0..4].copy_from_slice(&last_lba.to_be_bytes());
                cap[4..8].copy_from_slice(&block_size.to_be_bytes());
                self.start_data_in(cap.to_vec());
                eprintln!(
                    "[CDROM] SCSI READ CAPACITY → last_lba={} block_size={}",
                    last_lba, block_size
                );
            }
            SCSI_READ_10 => {
                let lba = u32::from_be_bytes([
                    self.state.cdb[2],
                    self.state.cdb[3],
                    self.state.cdb[4],
                    self.state.cdb[5],
                ]);
                let transfer_len = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as u32;
                self.handle_scsi_read(has_media, lba, transfer_len, "READ(10)");
            }
            SCSI_READ_TOC => {
                if !has_media {
                    self.set_sense_err(0x02, 0x3A, 0x00);
                    return;
                }
                let msf = (self.state.cdb[1] & 0x02) != 0;
                let format = self.state.cdb[2] & 0x0F;
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;

                let lba_to_msf = |lba: u32| -> [u8; 4] {
                    let total_m = lba / (75 * 60);
                    if total_m > 0xFF {
                        return [0x00, 0xFF, 59, 74];
                    }
                    let f = (lba % 75) as u8;
                    let s = ((lba / 75) % 60) as u8;
                    let m = total_m as u8;
                    [0x00, m, s, f]
                };

                let buf = match format {
                    0 => {
                        let mut toc = vec![0u8; 20];
                        let len: u16 = 18;
                        toc[0..2].copy_from_slice(&len.to_be_bytes());
                        toc[2] = 1;
                        toc[3] = 1;
                        toc[5] = 0x14;
                        toc[6] = 1;
                        if msf {
                            toc[8..12].copy_from_slice(&lba_to_msf(150));
                        } else {
                            toc[8..12].copy_from_slice(&0u32.to_be_bytes());
                        }
                        toc[13] = 0x14;
                        toc[14] = 0xAA;
                        if msf {
                            toc[16..20].copy_from_slice(&lba_to_msf(total_sectors.saturating_add(150)));
                        } else {
                            toc[16..20].copy_from_slice(&total_sectors.to_be_bytes());
                        }
                        toc
                    }
                    1 => {
                        let mut toc = vec![0u8; 12];
                        let len: u16 = 10;
                        toc[0..2].copy_from_slice(&len.to_be_bytes());
                        toc[2] = 1;
                        toc[3] = 1;
                        toc[5] = 0x14;
                        toc[6] = 1;
                        if msf {
                            toc[8..12].copy_from_slice(&lba_to_msf(150));
                        } else {
                            toc[8..12].copy_from_slice(&0u32.to_be_bytes());
                        }
                        toc
                    }
                    _ => {
                        self.set_sense_err(0x05, 0x24, 0x00);
                        return;
                    }
                };
                self.clear_sense();
                let send_len = alloc.min(buf.len());
                self.start_data_in(buf[..send_len].to_vec());
            }
            SCSI_READ_TRACK_INFO => {
                if !has_media {
                    self.set_sense_err(0x02, 0x3A, 0x00);
                    return;
                }
                self.clear_sense();
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;
                let mut info = [0u8; 36];
                let len: u16 = 34;
                info[0..2].copy_from_slice(&len.to_be_bytes());
                info[2] = 1;
                info[3] = 1;
                info[5] = 0x04;
                info[6] = 0x01;
                info[24..28].copy_from_slice(&total_sectors.to_be_bytes());
                let send_len = alloc.min(36);
                self.start_data_in(info[..send_len].to_vec());
            }
            SCSI_MODE_SENSE_10 => {
                self.clear_sense();
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let page_code = self.state.cdb[2] & 0x3F;
                if page_code == 0x2A || page_code == 0x3F {
                    let mut ms = vec![0u8; 44];
                    ms[0..2].copy_from_slice(&42u16.to_be_bytes());
                    ms[3] = 0x80;
                    ms[8] = 0x2A;
                    ms[9] = 0x1E;
                    ms[10] = 0x3F;
                    ms[12] = 0x71;
                    ms[14] = 0x29;
                    ms[16] = 0x02; ms[17] = 0xC0;
                    ms[22] = 0x02; ms[23] = 0xC0;
                    let len = alloc.min(44);
                    self.start_data_in(ms[..len].to_vec());
                } else {
                    let mut ms = vec![0u8; 8];
                    ms[0..2].copy_from_slice(&6u16.to_be_bytes());
                    ms[3] = 0x80;
                    let len = alloc.min(8);
                    self.start_data_in(ms[..len].to_vec());
                }
            }
            SCSI_GET_EVENT_STATUS => {
                if (self.state.cdb[1] & 0x01) == 0 {
                    self.set_sense_err(0x05, 0x24, 0x00);
                    return;
                }
                self.clear_sense();
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                if (self.state.cdb[4] & 0x10) != 0 {
                    let mut ev = [0u8; 8];
                    ev[0..2].copy_from_slice(&6u16.to_be_bytes());
                    ev[2] = 0x04;
                    ev[3] = 0x10;
                    ev[4] = if self.state.media_changed {
                        self.state.media_changed = false;
                        if has_media { 0x02 } else { 0x03 }
                    } else {
                        0x00
                    };
                    ev[5] = if has_media { 0x02 } else { 0x00 };
                    let send_len = alloc.min(8);
                    self.start_data_in(ev[..send_len].to_vec());
                } else {
                    let mut ev = [0u8; 4];
                    ev[0..2].copy_from_slice(&2u16.to_be_bytes());
                    ev[2] = 0x80;
                    ev[3] = 0x10;
                    let send_len = alloc.min(4);
                    self.start_data_in(ev[..send_len].to_vec());
                }
            }
            SCSI_GET_CONFIGURATION => {
                self.clear_sense();
                let rt = self.state.cdb[1] & 0x03;
                let start_feat = u16::from_be_bytes([self.state.cdb[2], self.state.cdb[3]]);
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let is_dvd = self.iso_size > 700 * 1024 * 1024;
                let cur_profile: u16 = if !has_media {
                    0x0000
                } else if is_dvd {
                    0x0010
                } else {
                    0x0008
                };

                let mut features: Vec<(u16, Vec<u8>)> = Vec::with_capacity(9);
                // 0000h: Profile List
                let mut f0 = vec![0u8; 12];
                f0[0..2].copy_from_slice(&0x0000u16.to_be_bytes());
                f0[2] = 0x03;
                f0[3] = 8;
                f0[4..6].copy_from_slice(&0x0010u16.to_be_bytes());
                f0[6] = if has_media && is_dvd { 1 } else { 0 };
                f0[8..10].copy_from_slice(&0x0008u16.to_be_bytes());
                f0[10] = if has_media && !is_dvd { 1 } else { 0 };
                features.push((0x0000, f0));

                // 0001h: Core Feature
                let mut f1 = vec![0u8; 12];
                f1[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
                f1[2] = (2 << 2) | 0x03;
                f1[3] = 8;
                f1[4..8].copy_from_slice(&2u32.to_be_bytes());
                f1[8] = 1;
                features.push((0x0001, f1));

                // 0002h: Morphing
                features.push((0x0002, vec![0x00, 0x02, 0x03, 4, 2, 0, 0, 0]));

                // 0003h: Removable Medium
                features.push((0x0003, vec![0x00, 0x03, 0x03, 4, 0x20, 0, 0, 0]));

                // 0010h: Random Readable
                let mut f10 = vec![0u8; 12];
                f10[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
                f10[2] = 0x03;
                f10[3] = 8;
                f10[4..8].copy_from_slice(&2048u32.to_be_bytes());
                f10[8..10].copy_from_slice(&1u16.to_be_bytes());
                features.push((0x0010, f10));

                // 001Eh: CD Read
                features.push((0x001E, vec![0x00, 0x1E, 0x03, 4, 0x03, 0, 0, 0]));

                // 001Fh: DVD Read
                features.push((0x001F, vec![0x00, 0x1F, 0x03, 4, 0, 0, 0, 0]));

                // 0100h: Power Management
                features.push((0x0100, vec![0x01, 0x00, 0x03, 0]));

                // 0105h: Timeout
                features.push((0x0105, vec![0x01, 0x05, 0x03, 4, 0, 0, 0, 0]));

                let mut conf = vec![0u8; 8];
                conf[6..8].copy_from_slice(&cur_profile.to_be_bytes());
                for (code, body) in &features {
                    let include = match rt {
                        0x02 => *code == start_feat,
                        _ => *code >= start_feat,
                    };
                    if include {
                        conf.extend_from_slice(body);
                    }
                }
                let data_len = (conf.len().saturating_sub(4)) as u32;
                conf[0..4].copy_from_slice(&data_len.to_be_bytes());

                let send_len = alloc.min(conf.len());
                self.start_data_in(conf[..send_len].to_vec());
            }
            SCSI_READ_DISC_INFO => {
                if !has_media {
                    self.set_sense_err(0x02, 0x3A, 0x00);
                    return;
                }
                self.clear_sense();
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let is_dvd = self.iso_size > 700 * 1024 * 1024;
                let mut info = [0u8; 34];
                let len: u16 = 32;
                info[0..2].copy_from_slice(&len.to_be_bytes());
                info[2] = 0x0E;
                info[3] = 1;
                info[4] = 1;
                info[5] = 1;
                info[6] = 1;
                info[7] = 0x20;
                info[8] = if is_dvd { 0x10 } else { 0x00 };
                let send_len = alloc.min(34);
                self.start_data_in(info[..send_len].to_vec());
            }
            SCSI_READ_12 => {
                let lba = u32::from_be_bytes([
                    self.state.cdb[2],
                    self.state.cdb[3],
                    self.state.cdb[4],
                    self.state.cdb[5],
                ]);
                let transfer_len = u32::from_be_bytes([
                    self.state.cdb[6],
                    self.state.cdb[7],
                    self.state.cdb[8],
                    self.state.cdb[9],
                ]);
                self.handle_scsi_read(has_media, lba, transfer_len, "READ(12)");
            }
            SCSI_READ_6 => {
                let lba = (((self.state.cdb[1] & 0x1F) as u32) << 16)
                    | ((self.state.cdb[2] as u32) << 8)
                    | (self.state.cdb[3] as u32);
                let mut transfer_len = self.state.cdb[4] as u32;
                if transfer_len == 0 {
                    transfer_len = 256;
                }
                self.handle_scsi_read(has_media, lba, transfer_len, "READ(6)");
            }
            SCSI_READ_FORMAT_CAPACITIES => {
                self.clear_sense();
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let total_sectors = if has_media {
                    ((self.iso_size + (CD_SECTOR_SIZE as u64 - 1)) / CD_SECTOR_SIZE as u64)
                        .max(1) as u32
                } else {
                    0
                };
                let mut cap = vec![0u8; 12];
                cap[3] = 8; // capacity list length
                cap[4..8].copy_from_slice(&total_sectors.to_be_bytes());
                cap[8] = if has_media { 0x02 } else { 0x03 }; // formatted vs no media
                cap[9..12].copy_from_slice(&[0x00, 0x08, 0x00]); // 2048 bytes block length
                let len = alloc.min(12);
                self.start_data_in(cap[..len].to_vec());
            }
            SCSI_SEEK_10 | SCSI_SYNCHRONIZE_CACHE => {
                self.clear_sense();
                self.state.phase = AtapiPhase::StatusIn;
                self.raise_irq();
            }
            SCSI_READ_CD => {
                let lba = u32::from_be_bytes([
                    self.state.cdb[2],
                    self.state.cdb[3],
                    self.state.cdb[4],
                    self.state.cdb[5],
                ]);
                let transfer_len = ((self.state.cdb[6] as u32) << 16)
                    | ((self.state.cdb[7] as u32) << 8)
                    | (self.state.cdb[8] as u32);
                self.handle_scsi_read(has_media, lba, transfer_len, "READ CD");
            }
            SCSI_MECHANISM_STATUS => {
                self.clear_sense();
                let alloc = u16::from_be_bytes([self.state.cdb[8], self.state.cdb[9]]) as usize;
                let mut resp = vec![0u8; 8];
                resp[5] = 1; // 1 slot
                let len = alloc.min(8);
                self.start_data_in(resp[..len].to_vec());
            }
            SCSI_READ_DVD_STRUCTURE => {
                if !has_media {
                    self.set_sense_err(0x02, 0x3A, 0x00);
                    return;
                }
                self.clear_sense();
                let format = self.state.cdb[7];
                let alloc = u16::from_be_bytes([self.state.cdb[8], self.state.cdb[9]]) as usize;
                let total_sectors = ((self.iso_size + (CD_SECTOR_SIZE as u64 - 1))
                    / CD_SECTOR_SIZE as u64)
                    .max(1) as u32;
                match format {
                    0x00 => {
                        let mut buf = vec![0u8; 2048 + 4];
                        let data_len = 2048u16 + 2u16;
                        buf[0..2].copy_from_slice(&data_len.to_be_bytes());
                        buf[4] = 0x01; // DVD-ROM, part version 1
                        buf[5] = 0x0F; // 120mm disc
                        buf[6] = 0x01; // One layer, read-only
                        buf[7] = 0x00;

                        buf[8..12].copy_from_slice(&0x0003_0000u32.to_be_bytes());
                        let end_psn = 0x0003_0000u32
                            .saturating_add(total_sectors.saturating_sub(1))
                            .min(0x00FF_FFFF);
                        buf[12..16].copy_from_slice(&end_psn.to_be_bytes());
                        buf[16..20].copy_from_slice(&end_psn.to_be_bytes());

                        let send_len = alloc.min(buf.len());
                        self.start_data_in(buf[..send_len].to_vec());
                    }
                    0x01 => {
                        let mut buf = vec![0u8; 8];
                        let data_len = 4u16 + 2u16;
                        buf[0..2].copy_from_slice(&data_len.to_be_bytes());
                        let send_len = alloc.min(8);
                        self.start_data_in(buf[..send_len].to_vec());
                    }
                    _ => {
                        let buf = vec![0u8; 4];
                        let send_len = alloc.min(4);
                        self.start_data_in(buf[..send_len].to_vec());
                    }
                }
            }
            _ => {
                eprintln!(
                    "[CDROM] SCSI cmd desconocido: 0x{:02X} CDB={:02X?}",
                    opcode,
                    &self.state.cdb[..]
                );
                self.set_sense_err(0x05, 0x20, 0x00);
            }
        }
    }

    fn handle_scsi_read(&mut self, has_media: bool, lba: u32, transfer_len: u32, op_name: &str) {
        if !has_media {
            self.set_sense_err(0x02, 0x3A, 0x00);
            return;
        }
        if transfer_len == 0 {
            self.clear_sense();
            self.state.phase = AtapiPhase::StatusIn;
            self.raise_irq();
            return;
        }
        let byte_count = (transfer_len as u64).saturating_mul(CD_SECTOR_SIZE as u64);
        let offset = (lba as u64).saturating_mul(CD_SECTOR_SIZE as u64);
        let padded_iso_size = ((self.iso_size.saturating_add(CD_SECTOR_SIZE as u64 - 1))
            / CD_SECTOR_SIZE as u64)
            * CD_SECTOR_SIZE as u64;

        let Some(end_off) = offset.checked_add(byte_count) else {
            self.set_sense_err(0x05, 0x21, 0x00);
            return;
        };
        if end_off > padded_iso_size {
            eprintln!("[CDROM] {} LBA={} len={} fuera de rango", op_name, lba, transfer_len);
            self.set_sense_err(0x05, 0x21, 0x00);
            return;
        }

        self.clear_sense();
        let mut buf = vec![0u8; byte_count as usize];
        if let Some(ref f) = self.iso_file {
            let to_read = self.iso_size.saturating_sub(offset).min(byte_count) as usize;
            if to_read > 0 {
                let _ = f.read_exact_at(&mut buf[..to_read], offset);
            }
        }
        if self.unattended {
            crate::unattended::patch_unattended_iso_sectors_for_os(
                &mut buf,
                self.unattended_os,
                &self.unattended_config,
            );
        }
        self.sectors_read_total += transfer_len as u64;
        self.start_data_in(buf);
    }
}

// ─── Canal primario IDE (disco duro ATA) ──────────────────────────
/// Estado de un dispositivo ATA en el canal primario.
/// Soporta IDENTIFY DEVICE, READ SECTORS/EXT, WRITE SECTORS/EXT.
pub struct PrimaryIde {
    /// Soporte para discos virtuales VDI y RAW
    pub vdisk: Option<crate::vdisk::VirtualDisk>,
    /// Archivo de disco de respaldo (raw, 512-byte sectors)
    disk_file: Option<File>,
    /// Tamaño en bytes del disco (0 si no hay disco)
    disk_size: u64,
    /// Estado del canal ATA
    status: u8,
    reg_error: u8,
    nien: bool,
    /// Registros LBA28/LBA48 con latches HOB (High Order Byte)
    sc_cur: u8,
    sc_hob: u8,
    lba0_cur: u8,
    lba0_hob: u8,
    lba1_cur: u8,
    lba1_hob: u8,
    lba2_cur: u8,
    lba2_hob: u8,
    /// Buffer de datos para lectura/escritura
    data_buf: Vec<u8>,
    /// Offset actual en data_buf
    data_offset: usize,
    /// Tamaño del bloque DRQ PIO actual (512 bytes por sector en READ/WRITE SECTORS)
    pio_sector_rem: usize,
    /// Registro de dirección del dispositivo (DH/DRIVE)
    drive_select: u8,
    /// Último comando ATA recibido (para diagnóstico)
    last_cmd: u8,
    pub dma_active: bool,
    pub dma_is_write: bool,
    pub irq_pending: bool,
    pub sectors_read_total: u64,
    pub sectors_written_total: u64,
}

impl PrimaryIde {
    pub fn new() -> Self {
        Self {
            vdisk: None,
            disk_file: None,
            disk_size: 0,
            status: 0x00,
            reg_error: 0x01,
            nien: false,
            sc_cur: 1,
            sc_hob: 0,
            lba0_cur: 0,
            lba0_hob: 0,
            lba1_cur: 0,
            lba1_hob: 0,
            lba2_cur: 0,
            lba2_hob: 0,
            data_buf: Vec::new(),
            data_offset: 0,
            pio_sector_rem: 0,
            drive_select: 0,
            last_cmd: 0,
            dma_active: false,
            dma_is_write: false,
            irq_pending: false,
            sectors_read_total: 0,
            sectors_written_total: 0,
        }
    }

    pub fn get_lba(&self) -> u64 {
        if matches!(self.last_cmd, 0x24 | 0x25 | 0x29 | 0x34 | 0x35 | 0x39) {
            (self.lba0_cur as u64)
                | ((self.lba1_cur as u64) << 8)
                | ((self.lba2_cur as u64) << 16)
                | ((self.lba0_hob as u64) << 24)
                | ((self.lba1_hob as u64) << 32)
                | ((self.lba2_hob as u64) << 40)
        } else {
            (self.lba0_cur as u64)
                | ((self.lba1_cur as u64) << 8)
                | ((self.lba2_cur as u64) << 16)
                | (((self.drive_select & 0x0F) as u64) << 24)
        }
    }

    pub fn get_sector_count(&self) -> u32 {
        if matches!(self.last_cmd, 0x24 | 0x25 | 0x29 | 0x34 | 0x35 | 0x39) {
            let cnt = (self.sc_cur as u32) | ((self.sc_hob as u32) << 8);
            if cnt == 0 { 65536 } else { cnt }
        } else {
            let cnt = self.sc_cur as u32;
            if cnt == 0 { 256 } else { cnt }
        }
    }

    pub fn set_status(&mut self, s: u8) {
        self.status = s;
    }

    pub fn raise_irq(&mut self) {
        if !self.nien {
            self.irq_pending = true;
        }
    }

    /// Carga un disco duro (VDI o RAW) para el canal primario.
    pub fn with_disk(disk_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let vdisk;
        let mut disk_file = None;
        let size;

        if let Ok(vd) = crate::vdisk::VirtualDisk::open(disk_path) {
            size = vd.logical_size();
            eprintln!(
                "[ATA] Disco duro cargado (VDisk): {} ({:.1} MB, {})",
                disk_path,
                size as f64 / (1024.0 * 1024.0),
                vd.format().label()
            );
            vdisk = Some(vd);
        } else if let Ok(file) = OpenOptions::new().read(true).write(true).open(disk_path) {
            size = file.metadata().map(|m| m.len()).unwrap_or(0);
            eprintln!(
                "[ATA] Disco duro cargado (RAW): {} ({:.1} MB, rw)",
                disk_path,
                size as f64 / (1024.0 * 1024.0)
            );
            disk_file = Some(file);
            vdisk = None;
        } else {
            return Err(format!("No se pudo abrir el disco '{}'", disk_path).into());
        }

        Ok(Self {
            vdisk,
            disk_file,
            disk_size: size,
            status: ST_DRDY,
            reg_error: 0x01,
            nien: false,
            sc_cur: 1,
            sc_hob: 0,
            lba0_cur: 0,
            lba0_hob: 0,
            lba1_cur: 0,
            lba1_hob: 0,
            lba2_cur: 0,
            lba2_hob: 0,
            data_buf: Vec::new(),
            data_offset: 0,
            pio_sector_rem: 0,
            drive_select: 0,
            last_cmd: 0,
            dma_active: false,
            dma_is_write: false,
            irq_pending: false,
            sectors_read_total: 0,
            sectors_written_total: 0,
        })
    }

    /// Indica si hay un disco conectado
    pub fn has_disk(&self) -> bool {
        self.vdisk.is_some() || self.disk_file.is_some()
    }

    pub fn take_irq(&mut self) -> bool {
        let p = self.irq_pending;
        self.irq_pending = false;
        p
    }

    /// Reset del dispositivo ATA: borra el estado de la transferencia actual.
    pub fn reset(&mut self) {
        self.flush();
        self.status = if self.has_disk() { ST_DRDY } else { 0x00 };
        self.reg_error = 0x01;
        self.sc_cur = 1;
        self.sc_hob = 0;
        self.lba0_cur = 0;
        self.lba0_hob = 0;
        self.lba1_cur = 0;
        self.lba1_hob = 0;
        self.lba2_cur = 0;
        self.lba2_hob = 0;
        self.data_buf.clear();
        self.data_offset = 0;
        self.pio_sector_rem = 0;
        self.drive_select = 0;
        self.last_cmd = 0;
        self.dma_active = false;
        self.dma_is_write = false;
        self.irq_pending = false;
    }

    /// Sincroniza la caché de escritura con el almacenamiento del host (`FLUSH CACHE`).
    pub fn flush(&mut self) {
        if let Some(ref mut vd) = self.vdisk {
            let _ = vd.flush();
        } else if let Some(ref f) = self.disk_file {
            let _ = f.sync_data();
        }
    }

    /// Construye un IDENTIFY DEVICE response (512 bytes) para un disco duro ATA
    fn build_identify(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 512];

        buf[0] = 0x40;
        buf[1] = 0x00;
        buf[2] = 0x3F; buf[3] = 0x3F;
        buf[6] = 16;   buf[7] = 0;
        buf[12] = 63;  buf[13] = 0;

        copy_ata_string(&mut buf[20..40], b"TWO555-HDD0         ");
        copy_ata_string(&mut buf[46..54], b"01.00   ");
        copy_ata_string(&mut buf[54..94], b"Two Five Five Virtual HDD               ");

        // Word 49: DMA (bit 8) + LBA (bit 9) + IORDY (bit 11) -> 0x0B00
        buf[98] = 0x00; buf[99] = 0x0B;
        // Word 53: Words 64-70 y 88 válidas
        buf[106] = 0x06; buf[107] = 0x00;

        let total_sectors = (self.disk_size / 512).max(1);
        let lba28 = (total_sectors.min(0x0FFF_FFFF)) as u32;
        buf[120..124].copy_from_slice(&lba28.to_le_bytes());

        // Word 63 (MWDMA 0-2), Word 64 (PIO 3-4), Word 88 (UDMA 0-5, 5 active)
        buf[126] = 0x07; buf[127] = 0x00;
        buf[128] = 0x03; buf[129] = 0x00;
        buf[176] = 0x7F; buf[177] = 0x20;

        // Word 83: LBA48 supported (0x7400), Word 86: LBA48 enabled (0x3400)
        buf[166] = 0x00; buf[167] = 0x74;
        buf[172] = 0x00; buf[173] = 0x34;
        buf[186] = 0x40; buf[187] = 0x41;

        buf[200..208].copy_from_slice(&total_sectors.to_le_bytes());
        buf[510] = 0xA5;
        let sum: u8 = buf[..511].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[511] = 0u8.wrapping_sub(sum);

        buf
    }

    /// Lee n sectores del disco a partir del LBA
    pub fn read_sectors(&mut self, lba: u64, count: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (count as usize) * 512];
        if let Some(ref mut vd) = self.vdisk {
            if vd.read_sectors(lba, count, &mut buf).is_ok() {
                self.sectors_read_total += count as u64;
            }
        } else if let Some(ref f) = self.disk_file {
            if f.read_exact_at(&mut buf, lba * 512).is_ok() {
                self.sectors_read_total += count as u64;
            }
        }
        buf
    }

    /// Escribe n sectores al disco.
    pub fn write_sectors(&mut self, lba: u64, count: u32, data: &[u8]) -> bool {
        let len = (count as usize) * 512;
        if data.len() < len {
            eprintln!("[ATA] WRITE corto: pidió {} bytes, recibió {}", len, data.len());
            return false;
        }
        if let Some(ref mut vd) = self.vdisk {
            if let Err(e) = vd.write_sectors(lba, count, &data[..len]) {
                eprintln!("[ATA] ERROR escribiendo VDisk LBA={:#x} count={}: {}", lba, count, e);
                return false;
            }
            self.sectors_written_total += count as u64;
            return true;
        }
        let Some(file) = self.disk_file.as_ref() else {
            eprintln!("[ATA] WRITE sin archivo de disco (LBA={:#x})", lba);
            return false;
        };
        if let Err(e) = file.write_all_at(&data[..len], lba * 512) {
            eprintln!("[ATA] ERROR escribiendo LBA={:#x} count={}: {}", lba, count, e);
            return false;
        }
        self.sectors_written_total += count as u64;
        true
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

        if port == PRI_DRIVE {
            self.drive_select = val;
            return;
        }

        if (self.drive_select & 0x10) != 0 && port != PRI_ALT_STATUS {
            return;
        }

        match port {
            PRI_DATA => {
                let total = self.data_buf.len();
                if total == 0 {
                    return;
                }
                let copy_len = data.len().min(total.saturating_sub(self.data_offset));
                if copy_len > 0 {
                    self.data_buf[self.data_offset..self.data_offset + copy_len]
                        .copy_from_slice(&data[..copy_len]);
                    self.data_offset += copy_len;
                    self.pio_sector_rem = self.pio_sector_rem.saturating_sub(copy_len);
                }
                if self.data_offset >= total {
                    let count = (total / 512) as u32;
                    let lba = self.get_lba();
                    let buf = std::mem::take(&mut self.data_buf);
                    self.data_offset = 0;
                    self.pio_sector_rem = 0;
                    if self.write_sectors(lba, count, &buf) {
                        self.status = ST_DRDY;
                        self.reg_error = 0;
                    } else {
                        self.status = ST_DRDY | ST_ERR;
                        self.reg_error = 0x04;
                    }
                    self.raise_irq();
                } else if self.pio_sector_rem == 0 {
                    self.pio_sector_rem = 512;
                    self.raise_irq();
                }
            }
            PRI_ERROR => {}
            PRI_SECTORS => {
                self.sc_hob = self.sc_cur;
                self.sc_cur = val;
            }
            PRI_LBA0 => {
                self.lba0_hob = self.lba0_cur;
                self.lba0_cur = val;
            }
            PRI_LBA1 => {
                self.lba1_hob = self.lba1_cur;
                self.lba1_cur = val;
            }
            PRI_LBA2 => {
                self.lba2_hob = self.lba2_cur;
                self.lba2_cur = val;
            }
            PRI_CMD => {
                self.last_cmd = val;
                if !self.has_disk() {
                    self.status = 0x00;
                    return;
                }
                self.status |= ST_BSY;
                self.reg_error = 0;

                match val {
                    0xEC => { // IDENTIFY DEVICE
                        self.data_buf = self.build_identify();
                        self.data_offset = 0;
                        self.pio_sector_rem = 512;
                        self.status = ST_DRDY | ST_DRQ;
                        self.raise_irq();
                    }
                    0x20 | 0x24 => { // READ SECTORS / READ SECTORS EXT
                        let lba = self.get_lba();
                        let count = self.get_sector_count();
                        self.data_buf = self.read_sectors(lba, count);
                        self.data_offset = 0;
                        self.pio_sector_rem = 512;
                        self.status = ST_DRDY | ST_DRQ;
                        self.raise_irq();
                    }
                    0x30 | 0x34 => { // WRITE SECTORS / WRITE SECTORS EXT
                        let count = self.get_sector_count();
                        self.data_buf = vec![0u8; (count as usize) * 512];
                        self.data_offset = 0;
                        self.pio_sector_rem = 512;
                        self.status = ST_DRDY | ST_DRQ;
                    }
                    0xC8 | 0x25 => { // READ DMA / READ DMA EXT
                        self.dma_active = true;
                        self.dma_is_write = false;
                        self.status = ST_DRDY;
                    }
                    0xCA | 0x35 => { // WRITE DMA / WRITE DMA EXT
                        self.dma_active = true;
                        self.dma_is_write = true;
                        self.status = ST_DRDY;
                    }
                    0xE7 | 0xEA => {
                        self.flush();
                        self.status = ST_DRDY;
                        self.raise_irq();
                    }
                    0x90 => {
                        self.reg_error = 0x01;
                        self.status = ST_DRDY;
                        self.raise_irq();
                    }
                    0xEF | 0x00 => {
                        self.status = ST_DRDY;
                        self.raise_irq();
                    }
                    _ => {
                        self.reg_error = 0x04;
                        self.status = ST_DRDY | ST_ERR;
                        self.raise_irq();
                    }
                }

                self.status &= !ST_BSY;
            }
            PRI_ALT_STATUS => {
                self.nien = (val & 0x02) != 0;
                if self.nien {
                    self.irq_pending = false;
                }
                if val & 0x04 != 0 { // SRST
                    self.reset();
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        if !self.has_disk() || ((self.drive_select & 0x10) != 0 && port != PRI_DRIVE && port != PRI_ALT_STATUS) {
            return vec![0x00; count];
        }

        match port {
            PRI_DATA => {
                let mut result = vec![0u8; count];
                let avail = self.data_buf.len().saturating_sub(self.data_offset);
                let copy_len = count.min(avail);
                if copy_len > 0 {
                    result[..copy_len].copy_from_slice(
                        &self.data_buf[self.data_offset..self.data_offset + copy_len],
                    );
                    self.data_offset += copy_len;
                    self.pio_sector_rem = self.pio_sector_rem.saturating_sub(copy_len);
                }
                if self.data_offset >= self.data_buf.len() && !self.data_buf.is_empty() {
                    self.status = ST_DRDY;
                    self.data_buf.clear();
                    self.pio_sector_rem = 0;
                } else if self.pio_sector_rem == 0 && !self.data_buf.is_empty() {
                    self.pio_sector_rem = 512;
                    self.raise_irq();
                }
                result
            }
            PRI_ERROR => vec![self.reg_error],
            PRI_SECTORS => vec![self.sc_cur],
            PRI_LBA0 => vec![self.lba0_cur],
            PRI_LBA1 => vec![self.lba1_cur],
            PRI_LBA2 => vec![self.lba2_cur],
            PRI_DRIVE => vec![self.drive_select],
            PRI_CMD => {
                self.irq_pending = false;
                vec![self.status]
            }
            PRI_ALT_STATUS => vec![self.status],
            _ => vec![0xFF; count],
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

        if port == SEC_DRIVE {
            self.state.reg_dh = val;
            if self.state.phase == AtapiPhase::StatusIn {
                self.state.phase = AtapiPhase::Idle;
            }
            return;
        }

        // Si se selecciona el esclavo (bit 4 = 1), no hay dispositivo esclavo en el canal secundario.
        if (self.state.reg_dh & 0x10) != 0 && port != SEC_ALT_STATUS {
            return;
        }

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
            SEC_ERROR => {
                // Escritura a 0x171 es Features (solo escritura); nunca modifica reg_error
                self.state.reg_feature = val;
            }
            SEC_SECTORS => {
                self.state.reg_sc = val;
                if self.state.phase == AtapiPhase::StatusIn {
                    self.state.phase = AtapiPhase::Idle;
                }
            }
            SEC_LBA0 => {
                self.state.reg_sn = val;
                if self.state.phase == AtapiPhase::StatusIn {
                    self.state.phase = AtapiPhase::Idle;
                }
            }
            SEC_LBA1 => {
                self.state.reg_cl = val;
                if self.state.phase == AtapiPhase::StatusIn {
                    self.state.phase = AtapiPhase::Idle;
                }
            }
            SEC_LBA2 => {
                self.state.reg_ch = val;
                if self.state.phase == AtapiPhase::StatusIn {
                    self.state.phase = AtapiPhase::Idle;
                }
            }
            SEC_CMD => {
                // Command register (write): NO borrar sense aquí (REQUEST SENSE debe leerlo!)
                self.state.phase = AtapiPhase::Idle;
                if val != CMD_PACKET {
                    self.state.reg_error = 0;
                }

                match val {
                    CMD_IDENTIFY_PACKET => {
                        self.do_identify_packet();
                    }
                    CMD_IDENTIFY => {
                        self.do_identify();
                    }
                    CMD_PACKET => {
                        self.state.phase = AtapiPhase::CdbIn;
                        self.state.cdb = [0u8; 12];
                        self.state.cdb_offset = 0;
                        self.state.drq_unread_count = 0;
                        let limit = ((self.state.reg_ch as usize) << 8) | (self.state.reg_cl as usize);
                        self.state.byte_count_limit = if limit == 0 || limit == 0xFFFF {
                            0xFFFE
                        } else {
                            (limit & !1).max(2)
                        };
                    }
                    0xEF => {
                        // SET FEATURES (e.g. transfer mode 0x03)
                        self.state.phase = AtapiPhase::Idle;
                        self.state.reg_error = 0;
                        self.raise_irq();
                    }
                    0x90 => {
                        // EXECUTE DEVICE DIAGNOSTIC
                        self.state.phase = AtapiPhase::Idle;
                        self.state.reg_error = 0x01;
                        self.raise_irq();
                    }
                    0x08 => {
                        // ATAPI DEVICE RESET
                        self.reset();
                        self.raise_irq();
                    }
                    0xE0..=0xE6 => {
                        // Power management: STANDBY, IDLE, CHECK POWER
                        self.state.phase = AtapiPhase::Idle;
                        self.state.reg_error = 0;
                        self.raise_irq();
                    }
                    0x00 => {
                        // NOP
                        self.state.phase = AtapiPhase::Idle;
                        self.state.reg_error = 0;
                        self.raise_irq();
                    }
                    _ => {
                        // Any other command: abort with ABRT and assert IRQ
                        self.state.phase = AtapiPhase::Idle;
                        self.state.reg_error = 0x04; // ABRT
                        self.raise_irq();
                    }
                }
            }
            SEC_ALT_STATUS => {
                // Device Control register write (SRST, nIEN, etc.)
                let nien = (val & 0x02) != 0;
                self.state.nien = nien;
                if nien {
                    self.state.irq_pending = false;
                }
                if val & 0x04 != 0 {
                    self.reset();
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        // Si el host tiene seleccionado el esclavo (bit 4 de reg_dh = 1),
        // no hay dispositivo esclavo: reportamos 0x00.
        if (self.state.reg_dh & 0x10) != 0 && port != SEC_DRIVE {
            return vec![0x00; count];
        }

        match port {
            SEC_CMD => {
                // Status register (read) - clears IRQ and transitions StatusIn -> Idle
                self.state.irq_pending = false;
                let st = self.current_status();
                if self.state.phase == AtapiPhase::StatusIn {
                    self.state.phase = AtapiPhase::Idle;
                    self.state.reg_cl = 0x14;
                    self.state.reg_ch = 0xEB;
                }
                vec![st]
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
                                self.state.chunk_offset += 1;
                            } else {
                                result.push(0x00);
                            }
                        }
                        // Si se terminó el chunk actual:
                        if self.state.chunk_offset >= self.state.current_chunk_len {
                            if self.state.data_offset >= self.state.data_buf.len() {
                                // Todos los datos transferidos: pasar a StatusIn y avisar con IRQ
                                self.state.phase = AtapiPhase::StatusIn;
                                self.raise_irq();
                            } else {
                                // Quedan más datos: armar siguiente bloque DRQ y avisar con IRQ
                                let remaining = self.state.data_buf.len() - self.state.data_offset;
                                let limit = self.state.byte_count_limit;
                                let next_chunk = remaining.min(limit);
                                self.state.current_chunk_len = next_chunk;
                                self.state.chunk_offset = 0;
                                self.state.reg_cl = (next_chunk & 0xFF) as u8;
                                self.state.reg_ch = ((next_chunk >> 8) & 0xFF) as u8;
                                self.state.phase = AtapiPhase::DataIn;
                                self.raise_irq();
                            }
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
            SEC_ERROR => vec![self.state.reg_error],
            SEC_SECTORS => {
                match self.state.phase {
                    AtapiPhase::Idle => vec![self.state.reg_sc],
                    AtapiPhase::CdbIn => vec![0x01],
                    AtapiPhase::DataIn => vec![0x02],
                    AtapiPhase::StatusIn => vec![0x03],
                }
            }
            SEC_LBA0 => vec![self.state.reg_sn],
            SEC_LBA1 => vec![self.state.reg_cl],
            SEC_LBA2 => vec![self.state.reg_ch],
            SEC_DRIVE => vec![self.state.reg_dh],
            _ => vec![0x00],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

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
        assert!(cd.irq_pending(), "IDENTIFY PACKET must signal IRQ");
        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ must be set for data transfer");
        assert!(!cd.irq_pending(), "Reading status clears IRQ");
        // Read all 512 bytes
        let mut all = Vec::new();
        while cd.read(SEC_CMD, 1)[0] & ST_DRQ != 0 && all.len() < 600 {
            all.extend_from_slice(&cd.read(SEC_DATA, 1));
        }
        assert_eq!(all.len(), 512);
        assert_eq!(all[0], 0x80);
        assert_eq!(all[1], 0x85, "Word 0 bit 15 must be 1 (ATAPI device)");
    }

    #[test]
    fn atapi_packet_inquiry() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();
        // Send PACKET command (0xA0)
        cd.write(SEC_CMD, &[CMD_PACKET]);
        let st1 = cd.read(SEC_CMD, 1)[0];
        assert!(st1 & ST_DRQ != 0, "DRQ should be set in CdbIn phase");
        // Send 12-byte CDB via data port
        let cdb: [u8; 12] = [SCSI_INQUIRY, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0];
        cd.write(SEC_DATA, &cdb);
        // After CDB, should be DataIn with DRQ
        let st2 = cd.read(SEC_CMD, 1)[0];
        assert!(st2 & ST_DRQ != 0, "DRQ should be set after SCSI INQUIRY data ready");
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
        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ should be set in CdbIn phase");
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

    #[test]
    fn atapi_chunked_drq_transfer() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();

        // Host fija límite de byte count a 2048 bytes (0x0800)
        cd.write(SEC_LBA1, &[0x00]);
        cd.write(SEC_LBA2, &[0x08]);

        // Envía CMD_PACKET
        cd.write(SEC_CMD, &[CMD_PACKET]);
        let mut cdb = [0u8; 12];
        cdb[0] = SCSI_READ_10;
        cdb[7] = 0;
        cdb[8] = 2; // 2 sectores = 4096 bytes
        cd.write(SEC_DATA, &cdb);

        // Primer chunk: debe reportar exactamente 2048 bytes en cilindros
        assert!(cd.take_irq(), "IRQ raised for chunk 1");
        let cl1 = cd.read(SEC_LBA1, 1)[0];
        let ch1 = cd.read(SEC_LBA2, 1)[0];
        let chunk1_len = ((ch1 as usize) << 8) | (cl1 as usize);
        assert_eq!(chunk1_len, 2048, "Chunk 1 debe ser 2048 bytes");

        // Leer los 2048 bytes del primer chunk
        let data1 = cd.read(SEC_DATA, 2048);
        assert_eq!(data1.len(), 2048);
        assert_eq!(&data1[..8], b"SECTOR-0");

        // Tras leer chunk 1, debe dispararse el segundo chunk con su IRQ
        assert!(cd.take_irq(), "IRQ raised for chunk 2");
        let cl2 = cd.read(SEC_LBA1, 1)[0];
        let ch2 = cd.read(SEC_LBA2, 1)[0];
        let chunk2_len = ((ch2 as usize) << 8) | (cl2 as usize);
        assert_eq!(chunk2_len, 2048, "Chunk 2 debe ser 2048 bytes");

        // Leer los 2048 bytes del segundo chunk
        let data2 = cd.read(SEC_DATA, 2048);
        assert_eq!(data2.len(), 2048);
        assert_eq!(&data2[..8], b"SECTOR-1");

        // Tras leer todo, debe pasar a StatusIn y levantar IRQ final de completación
        assert!(cd.take_irq(), "IRQ raised for command completion");
        let st = cd.read(SEC_CMD, 1)[0];
        assert_eq!(st & ST_DRQ, 0, "DRQ cleared in StatusIn");
        assert_ne!(st & ST_DRDY, 0, "DRDY set");
    }

    #[test]
    fn reset_returns_to_idle_phase() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();
        // Dejar el dispositivo a mitad de una transferencia INQUIRY
        cd.write(SEC_CMD, &[CMD_PACKET]);
        let cdb: [u8; 12] = [SCSI_INQUIRY, 0, 0, 0, 96, 0, 0, 0, 0, 0, 0, 0];
        cd.write(SEC_DATA, &cdb);
        assert_ne!(cd.read(SEC_CMD, 1)[0] & ST_DRQ, 0, "DRQ set after INQUIRY");

        cd.reset();

        // Reset → fase Idle: sin BSY, sin DRQ, solo DRDY
        let st = cd.read(SEC_CMD, 1)[0];
        assert_eq!(st & (ST_BSY | ST_DRQ), 0, "no BSY/DRQ after reset");
        assert_ne!(st & ST_DRDY, 0, "DRDY after reset");
        // Y el medio sigue presente: IDENTIFY PACKET vuelve a funcionar
        cd.write(SEC_CMD, &[CMD_IDENTIFY_PACKET]);
        let st1 = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st1 & ST_DRQ, 0, "DRQ on fresh IDENTIFY after reset");
    }

    // ─── Canal primario (disco ATA) ───────────────────────────────
    fn make_test_disk(size: usize) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mi-vmm-test-disk-{}.img",
            std::process::id() as u64 + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos() as u64
        ));
        let mut f = File::create(&path).unwrap();
        let mut sector = 0u32;
        while (sector as usize + 1) * 512 <= size {
            let mut buf = vec![0u8; 512];
            let tag = format!("DISK-SECTOR-{}", sector);
            buf[..tag.len()].copy_from_slice(tag.as_bytes());
            buf[100] = sector as u8;
            f.write_all(&buf).unwrap();
            sector += 1;
        }
        path
    }

    #[test]
    fn primary_ide_multisector_pio_read() {
        let path = make_test_disk(2048);
        let mut ide = PrimaryIde::with_disk(path.to_str().unwrap()).unwrap();

        // READ SECTORS (0x20): LBA 0, 2 sectores
        ide.write(PRI_SECTORS, &[2]);
        ide.write(PRI_LBA0, &[0x00]);
        ide.write(PRI_LBA1, &[0x00]);
        ide.write(PRI_LBA2, &[0x00]);
        ide.write(PRI_DRIVE, &[0xE0]); // LBA, master
        ide.write(PRI_CMD, &[0x20]);

        let st = ide.read(PRI_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set after READ SECTORS");

        // REP INSW coalescido: una sola lectura de 1024 bytes (PIO multisector)
        let data = ide.read(PRI_DATA, 1024);
        assert_eq!(data.len(), 1024);
        assert_eq!(&data[..13], b"DISK-SECTOR-0");
        assert_eq!(&data[512..512 + 13], b"DISK-SECTOR-1");
        // Tras consumir los datos, DRQ baja
        let st = ide.read(PRI_CMD, 1)[0];
        assert_eq!(st & ST_DRQ, 0, "DRQ cleared after data consumed");
    }

    #[test]
    fn primary_ide_write_persists_and_fsyncs() {
        let path = make_test_disk(2048);
        let mut ide = PrimaryIde::with_disk(path.to_str().unwrap()).unwrap();

        // WRITE SECTORS (0x30): LBA 1, 1 sector
        ide.write(PRI_SECTORS, &[1]);
        ide.write(PRI_LBA0, &[0x01]);
        ide.write(PRI_LBA1, &[0x00]);
        ide.write(PRI_LBA2, &[0x00]);
        ide.write(PRI_DRIVE, &[0xE0]);
        ide.write(PRI_CMD, &[0x30]);
        let st = ide.read(PRI_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set waiting for write data");

        // REP OUTSW coalescido: 512 bytes de una sola vez
        let mut payload = vec![0u8; 512];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        ide.write(PRI_DATA, &payload);

        // Completado: DRQ baja y status OK
        let st = ide.read(PRI_CMD, 1)[0];
        assert_eq!(st & ST_DRQ, 0, "DRQ cleared after write completes");
        assert_eq!(st & ST_ERR, 0, "no error after write");

        // Verificar el contenido persistido en el archivo (fsync ya hecho)
        let mut f = File::open(&path).unwrap();
        let mut buf = vec![0u8; 512];
        f.seek(SeekFrom::Start(512)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(buf, payload, "sector escrito en el archivo");
    }

    #[test]
    fn atapi_signature_and_identify_fallback() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();

        // Inicialmente (o tras reset), LBA1=0x14 y LBA2=0xEB
        assert_eq!(cd.read(SEC_LBA1, 1)[0], 0x14, "Initial LBA1 must be 0x14");
        assert_eq!(cd.read(SEC_LBA2, 1)[0], 0xEB, "Initial LBA2 must be 0xEB");

        // Enviar ATA IDENTIFY (0xEC): debe fallar con ERR y mantener 0x14/0xEB
        cd.write(SEC_CMD, &[CMD_IDENTIFY]);
        assert!(cd.irq_pending(), "ATA IDENTIFY completion must signal IRQ before status read");
        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_ERR, 0, "ATA IDENTIFY must report error on CD-ROM");
        assert!(!cd.irq_pending(), "Reading SEC_CMD must clear IRQ");
        assert_eq!(cd.read(SEC_ERROR, 1)[0], 0x04, "Error register must be ABRT (0x04)");
        assert_eq!(cd.read(SEC_LBA1, 1)[0], 0x14, "LBA1 must be 0x14 (ATAPI signature)");
        assert_eq!(cd.read(SEC_LBA2, 1)[0], 0xEB, "LBA2 must be 0xEB (ATAPI signature)");
    }

    #[test]
    fn scsi_read_toc_works() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();

        cd.write(SEC_CMD, &[CMD_PACKET]);
        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set for CdbIn");

        let mut cdb = [0u8; 12];
        cdb[0] = SCSI_READ_TOC; // 0x43
        cdb[7] = 0;
        cdb[8] = 20; // alloc 20 bytes
        cd.write(SEC_DATA, &cdb);

        assert!(cd.irq_pending(), "READ TOC completion must signal IRQ before status read");
        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set after READ TOC");
        assert!(!cd.irq_pending(), "Reading SEC_CMD must clear IRQ");

        // Bytes disponibles en DataIn reportados en LBA1/LBA2
        let low = cd.read(SEC_LBA1, 1)[0] as u16;
        let high = cd.read(SEC_LBA2, 1)[0] as u16;
        let count = (high << 8) | low;
        assert_eq!(count, 20, "LBA1/LBA2 report 20 bytes available");

        let toc = cd.read(SEC_DATA, 20);
        assert_eq!(toc.len(), 20);
        assert_eq!(toc[2], 1, "First track = 1");
        assert_eq!(toc[3], 1, "Last track = 1");
        assert_eq!(toc[6], 1, "Track 1 descriptor track number");
        assert_eq!(toc[14], 0xAA, "Lead-out track number 0xAA");
    }

    #[test]
    fn scsi_mode_sense_10_works() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();

        cd.write(SEC_CMD, &[CMD_PACKET]);
        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set for CdbIn");

        let mut cdb = [0u8; 12];
        cdb[0] = SCSI_MODE_SENSE_10; // 0x5A
        cdb[2] = 0x2A; // capabilities page
        cdb[7] = 0;
        cdb[8] = 44; // alloc 44 bytes
        cd.write(SEC_DATA, &cdb);

        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set after MODE SENSE 10");
        let ms = cd.read(SEC_DATA, 44);
        assert_eq!(ms.len(), 44);
        assert_eq!(ms[2], 0x00); // medium type
        assert_eq!(ms[3], 0x80); // write protected
        assert_eq!(ms[8], 0x2A); // page code 0x2A
    }

    #[test]
    fn scsi_read_12_and_read_6_works() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();

        // 1. SCSI_READ_12 (0xA8)
        cd.write(SEC_CMD, &[CMD_PACKET]);
        let mut cdb = [0u8; 12];
        cdb[0] = SCSI_READ_12; // 0xA8
        cdb[2..6].copy_from_slice(&0u32.to_be_bytes()); // LBA 0
        cdb[6..10].copy_from_slice(&1u32.to_be_bytes()); // 1 sector
        cd.write(SEC_DATA, &cdb);

        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set after READ 12");
        let data = cd.read(SEC_DATA, CD_SECTOR_SIZE);
        assert_eq!(data.len(), CD_SECTOR_SIZE);
        assert_eq!(&data[..8], b"SECTOR-0");

        // 2. SCSI_READ_6 (0x08)
        cd.write(SEC_CMD, &[CMD_PACKET]);
        let mut cdb6 = [0u8; 12];
        cdb6[0] = SCSI_READ_6; // 0x08
        cdb6[1] = 0; // LBA high
        cdb6[2] = 0; // LBA mid
        cdb6[3] = 0; // LBA low
        cdb6[4] = 1; // 1 sector
        cd.write(SEC_DATA, &cdb6);

        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set after READ 6");
        let data6 = cd.read(SEC_DATA, CD_SECTOR_SIZE);
        assert_eq!(data6.len(), CD_SECTOR_SIZE);
        assert_eq!(&data6[..8], b"SECTOR-0");
    }

    #[test]
    fn scsi_read_format_capacities_works() {
        let path = make_test_iso();
        let mut cd = CdRom::new(path.to_str().unwrap()).unwrap();

        cd.write(SEC_CMD, &[CMD_PACKET]);
        let mut cdb = [0u8; 12];
        cdb[0] = SCSI_READ_FORMAT_CAPACITIES; // 0x23
        cdb[7] = 0;
        cdb[8] = 12; // 12 bytes
        cd.write(SEC_DATA, &cdb);

        let st = cd.read(SEC_CMD, 1)[0];
        assert_ne!(st & ST_DRQ, 0, "DRQ set after READ FORMAT CAPACITIES");
        let cap = cd.read(SEC_DATA, 12);
        assert_eq!(cap.len(), 12);
        assert_eq!(cap[3], 8, "Capacity list length = 8");
        assert_eq!(cap[8], 0x02, "Formatted media descriptor");
    }

    #[test]
    fn primary_ide_slave_isolation_and_geometry() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vmm_test_slave_{}.img", std::process::id()));
        {
            let f = File::create(&path).unwrap();
            f.set_len(10 * 1024 * 1024).unwrap(); // 10 MB = 20480 sectors
        }
        let mut ide = PrimaryIde::with_disk(path.to_str().unwrap()).unwrap();

        // Verificar IDENTIFY DEVICE con Master seleccionado (drive_select = 0x00 / 0xA0)
        ide.write(PRI_DRIVE, &[0xA0]);
        ide.write(PRI_CMD, &[0xEC]);
        assert_ne!(ide.read(PRI_CMD, 1)[0] & ST_DRQ, 0);
        let id_data = ide.read(PRI_DATA, 512);
        let lba28_sectors = u32::from_le_bytes(id_data[120..124].try_into().unwrap());
        assert_eq!(lba28_sectors, 20480, "LBA28 sectors in Word 60-61 must match disk size");
        assert_ne!(id_data[167] & 0x04, 0, "LBA48 supported bit 10 must be set");
        assert_ne!(id_data[173] & 0x04, 0, "LBA48 enabled bit 10 must be set");

        // Seleccionar Slave (drive_select = 0xB0)
        ide.write(PRI_DRIVE, &[0xB0]);
        // Intentar escribir un comando en el esclavo: debe ignorarse
        ide.write(PRI_CMD, &[0xEC]);
        // Leer status o data del esclavo: debe retornar 0x00 (no hay esclavo)
        assert_eq!(ide.read(PRI_CMD, 1), vec![0x00], "Slave read must return 0x00 (no drive)");
        assert_eq!(ide.read(PRI_DATA, 1), vec![0x00], "Slave data read must return 0x00");

        // Volver a seleccionar Master (0xA0)
        ide.write(PRI_DRIVE, &[0xA0]);
        assert_eq!(ide.read(PRI_DRIVE, 1), vec![0xA0]);
        let _ = std::fs::remove_file(path);
    }
}
