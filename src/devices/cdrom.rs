//! Emulación de CD-ROM ATAPI en el canal secundario del bus IDE,
//! y stub del canal primario (0x1F0-0x1F7, 0x3F6).
//!
//! SeaBIOS usa el protocolo ATAPI (SCSI over ATA) para hablar con
//! el CD-ROM. El guest escribe el comando 0xA0 (PACKET), luego
//! envía un CDB de 12 bytes por el registro de datos.

use super::IoDevice;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
const SCSI_MODE_SENSE_10: u8 = 0x5A;
const SCSI_READ_12: u8 = 0xA8;
const SCSI_MECHANISM_STATUS: u8 = 0xBD;
const SCSI_READ_CD: u8 = 0xBE;

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
        }
    }

    pub fn is_inserted(&self) -> bool {
        self.iso_file.is_some()
    }

    pub fn eject(&mut self) {
        self.iso_file = None;
        self.iso_size = 0;
        self.state = CdromState::default();
        self.state.sense_key = 0x02; // NOT READY
        self.state.sense_asc = 0x3A; // MEDIUM NOT PRESENT
    }

    pub fn insert(&mut self, iso_path: &str) -> Result<u64, Box<dyn std::error::Error>> {
        let f = File::open(iso_path)?;
        let size = f.metadata()?.len();
        self.iso_file = Some(f);
        self.iso_size = size;
        self.state = CdromState::default();
        self.state.sense_key = 0x06; // UNIT ATTENTION
        self.state.sense_asc = 0x28; // NOT READY TO READY CHANGE
        Ok(size)
    }

    /// Reset del dispositivo (reset hardware del canal ATAPI): vuelve a la
    /// fase Idle sin transferencias pendientes. El medio (ISO) se conserva.
    pub fn reset(&mut self) {
        self.state = CdromState::default();
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

        // Word 0: ATAPI flag + removable + CD-ROM device (0x8580)
        // Bit 15=1 (ATAPI device: Linux ata_id_is_ata() comprueba bit 15 == 0; si es 0 falla con 'invalid type')
        // Bits 12:8 = 0x05 (CD-ROM: SeaBIOS comprueba ((word0 >> 8) & 0x1f) == 0x05 para iscd=true)
        // Bit 7 = 1 (removable device)
        // Bits 1:0 = 0 (12-byte CDB)
        pkt[0] = 0x80;
        pkt[1] = 0x85;

        // Word 1: cylindros = 0 (ignored for ATAPI)
        // Word 49: capabilities: Bit 9 = LBA (0x0200), Bit 11 = IORDY (0x0800) -> 0x0A00
        pkt[98] = 0x00;
        pkt[99] = 0x0A;

        // Word 53: fields valid
        pkt[106] = 0x06; // words 88 and 70 valid

        // Word 63: multiword DMA
        pkt[126] = 0x07; // mode 0,1,2

        // Word 64: PIO mode
        pkt[128] = 0x03; // mode 3,4

        // Word 76-79: serial
        pkt[152..160].copy_from_slice(b"VMM0001 ");

        // Word 80: Major version (bits 1..6 = ATA/ATAPI-1 a ATA/ATAPI-6)
        pkt[160] = 0x7E;
        pkt[161] = 0x00;

        // Word 82-84: command set (nop, atapi pkt, atapi mgr, generic)
        pkt[164] = 0x00;
        pkt[165] = 0x00;
        pkt[166] = 0x20; // ATAPI PACKET command
        pkt[167] = 0x40; // Word 83 bit 14=1

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
        let model_str = b"Two Five Five CD-ROM ATAPI   ";
        for (i, slot) in pkt[256..296].chunks_mut(2).enumerate() {
            let get = |k: usize| -> u8 { model_str.get(k).copied().unwrap_or(b' ') };
            slot[0] = get(i * 2 + 1);
            slot[1] = get(i * 2);
        }

        self.state.reg_sc = 0x02; // Interrupt reason: IO=1, CoD=0
        self.state.byte_count_limit = 512;
        self.start_data_in(pkt.to_vec());

        eprintln!("[CDROM] ATAPI IDENTIFY → CD-ROM ATAPI detectado");
    }

    /// Prepara una transferencia DataIn aplicando el byte_count_limit del host (fragmentación DRQ si aplica).
    fn start_data_in(&mut self, buf: Vec<u8>) {
        let total = buf.len();
        self.state.data_buf = buf;
        self.state.data_offset = 0;
        self.state.chunk_offset = 0;
        let limit = if self.state.byte_count_limit == 0 {
            0xFFFE
        } else {
            self.state.byte_count_limit
        };
        let chunk = total.min(limit);
        self.state.current_chunk_len = chunk;
        self.state.reg_cl = (chunk & 0xFF) as u8;
        self.state.reg_ch = ((chunk >> 8) & 0xFF) as u8;
        self.state.phase = AtapiPhase::DataIn;
        if (self.state.reg_feature & 0x01) != 0 {
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

    /// Ejecuta un SCSI command recibido vía ATAPI PACKET.
    fn execute_scsi(&mut self) {
        let opcode = self.state.cdb[0];
        match opcode {
            SCSI_TEST_READY => {
                // TEST UNIT READY: always ready
                self.state.phase = AtapiPhase::StatusIn;
                self.state.sense_key = 0;
                self.raise_irq();
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
                // Clear sense after reporting
                self.state.sense_key = 0;
                self.state.sense_asc = 0;
                self.state.sense_ascq = 0;
                self.start_data_in(sense[..len].to_vec());
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
                inq[8..16].copy_from_slice(b"TWO555  ");
                // Product ID (16 bytes, space-padded)
                inq[16..32].copy_from_slice(b"CD-ROM Drive    ");
                // Product revision (4 bytes)
                inq[32..36].copy_from_slice(b"1.0 ");
                let len = alloc.min(96);
                self.start_data_in(inq[..len].to_vec());
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
                let len = alloc.min(36);
                self.start_data_in(ms[..len].to_vec());
            }
            SCSI_START_STOP => {
                // START/STOP UNIT (for media eject/load)
                self.state.phase = AtapiPhase::StatusIn;
                self.raise_irq();
            }
            SCSI_PREVENT_ALLOW => {
                // PREVENT/ALLOW MEDIUM REMOVAL
                self.state.phase = AtapiPhase::StatusIn;
                self.raise_irq();
            }
            SCSI_READ_CAP_10 => {
                // READ CAPACITY(10): returns 8 bytes
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1);
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
                    self.state.reg_error = (self.state.sense_key << 4) | 0x04;
                    self.state.phase = AtapiPhase::StatusIn;
                    self.raise_irq();
                    return;
                }

                let mut buf = vec![0u8; byte_count as usize];
                if let Some(ref mut f) = self.iso_file {
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let _ = f.read_exact(&mut buf);
                    }
                }
                if self.unattended {
                    crate::unattended::patch_unattended_iso_sectors(&mut buf);
                }
                self.sectors_read_total += transfer_len as u64;
                self.start_data_in(buf);
            }
            SCSI_READ_TOC => {
                let msf = (self.state.cdb[1] & 0x02) != 0;
                let format = self.state.cdb[2] & 0x0F;
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;

                let lba_to_msf = |lba: u32| -> [u8; 4] {
                    let f = (lba % 75) as u8;
                    let s = ((lba / 75) % 60) as u8;
                    let m = (lba / (75 * 60)) as u8;
                    [0x00, m, s, f]
                };

                let buf = match format {
                    0 => {
                        // Standard TOC: Header (4 bytes) + Track 1 (8 bytes) + Lead-out (8 bytes) = 20 bytes
                        let mut toc = vec![0u8; 20];
                        let len: u16 = 18;
                        toc[0..2].copy_from_slice(&len.to_be_bytes());
                        toc[2] = 1; // first track
                        toc[3] = 1; // last track

                        // Track 1
                        toc[4] = 0;
                        toc[5] = 0x14; // ADR=1, Control=4 (data track)
                        toc[6] = 1;
                        toc[7] = 0;
                        if msf {
                            toc[8..12].copy_from_slice(&lba_to_msf(150));
                        } else {
                            toc[8..12].copy_from_slice(&0u32.to_be_bytes());
                        }

                        // Lead-out (track 0xAA)
                        toc[12] = 0;
                        toc[13] = 0x14;
                        toc[14] = 0xAA;
                        toc[15] = 0;
                        if msf {
                            toc[16..20].copy_from_slice(&lba_to_msf(total_sectors + 150));
                        } else {
                            toc[16..20].copy_from_slice(&total_sectors.to_be_bytes());
                        }
                        toc
                    }
                    1 => {
                        // Multi-session info: Header (4 bytes) + Track descriptor (8 bytes) = 12 bytes
                        let mut toc = vec![0u8; 12];
                        let len: u16 = 10;
                        toc[0..2].copy_from_slice(&len.to_be_bytes());
                        toc[2] = 1;
                        toc[3] = 1;

                        toc[4] = 0;
                        toc[5] = 0x14;
                        toc[6] = 1;
                        toc[7] = 0;
                        if msf {
                            toc[8..12].copy_from_slice(&lba_to_msf(150));
                        } else {
                            toc[8..12].copy_from_slice(&0u32.to_be_bytes());
                        }
                        toc
                    }
                    _ => vec![0u8; 4],
                };
                let send_len = alloc.min(buf.len());
                self.start_data_in(buf[..send_len].to_vec());
            }
            0x52 => {
                // READ TRACK INFORMATION (MMC)
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;
                let mut info = [0u8; 36];
                let len: u16 = 34;
                info[0..2].copy_from_slice(&len.to_be_bytes());
                info[2] = 1; // track number LSB
                info[3] = 1; // session number LSB
                info[5] = 0x04; // data track
                info[6] = 0x01; // mode 1
                info[8..12].copy_from_slice(&0u32.to_be_bytes());
                info[24..28].copy_from_slice(&total_sectors.to_be_bytes());
                let send_len = alloc.min(36);
                self.start_data_in(info[..send_len].to_vec());
            }
            SCSI_MODE_SENSE_10 => {
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let page_code = self.state.cdb[2] & 0x3F;
                let mut ms = vec![0u8; 44];
                let mode_len: u16 = 42;
                ms[0..2].copy_from_slice(&mode_len.to_be_bytes());
                ms[2] = 0x00; // medium type
                ms[3] = 0x80; // write protected
                ms[4..8].copy_from_slice(&[0u8; 4]);

                // Mode page 0x2A (CD-ROM capabilities)
                ms[8] = 0x2A;
                ms[9] = 0x1E;
                ms[10] = 0x00;
                ms[11] = 0x00;
                ms[16] = 0x02; ms[17] = 0xC0; // max read speed (4x)
                ms[22] = 0x02; ms[23] = 0xC0; // cur read speed

                let len = alloc.min(if page_code == 0x2A || page_code == 0x3F { 44 } else { 8 });
                self.start_data_in(ms[..len].to_vec());
                eprintln!("[CDROM] SCSI MODE SENSE(10) page={:#x} alloc={} -> {} bytes", page_code, alloc, len);
            }
            SCSI_GET_EVENT_STATUS => {
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let mut ev = [0u8; 8];
                let len: u16 = 4;
                ev[0..2].copy_from_slice(&len.to_be_bytes());
                ev[2] = 0x84; // NEA | class 4 (Media)
                ev[3] = 0x00;
                ev[4] = 0x02; // Media present
                let send_len = alloc.min(8);
                self.start_data_in(ev[..send_len].to_vec());
            }
            SCSI_GET_CONFIGURATION => {
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let mut conf = [0u8; 8];
                let len: u32 = 4;
                conf[0..4].copy_from_slice(&len.to_be_bytes());
                conf[6] = 0x00;
                conf[7] = 0x08; // Profile 0x08: CD-ROM
                let send_len = alloc.min(8);
                self.start_data_in(conf[..send_len].to_vec());
            }
            SCSI_READ_DISC_INFO => {
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let mut info = [0u8; 34];
                let len: u16 = 32;
                info[0..2].copy_from_slice(&len.to_be_bytes());
                info[2] = 0x0E;
                info[3] = 1;
                info[4] = 1;
                info[5] = 1;
                info[6] = 1;
                info[7] = 0x20;
                info[8] = 0x00;
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
                let byte_count = transfer_len as u64 * CD_SECTOR_SIZE as u64;
                let offset = lba as u64 * CD_SECTOR_SIZE as u64;

                if offset + byte_count > self.iso_size {
                    eprintln!(
                        "[CDROM] READ(12) LBA={} len={} fuera de rango",
                        lba, transfer_len
                    );
                    self.state.sense_key = 0x05; // ILLEGAL REQUEST
                    self.state.sense_asc = 0x21; // LBA out of range
                    self.state.reg_error = (self.state.sense_key << 4) | 0x04;
                    self.state.phase = AtapiPhase::StatusIn;
                    self.raise_irq();
                    return;
                }

                let mut buf = vec![0u8; byte_count as usize];
                if let Some(ref mut f) = self.iso_file {
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let _ = f.read_exact(&mut buf);
                    }
                }
                if self.unattended {
                    crate::unattended::patch_unattended_iso_sectors(&mut buf);
                }
                self.sectors_read_total += transfer_len as u64;
                self.start_data_in(buf);
            }
            SCSI_READ_6 => {
                let lba = (((self.state.cdb[1] & 0x1F) as u32) << 16)
                    | ((self.state.cdb[2] as u32) << 8)
                    | (self.state.cdb[3] as u32);
                let mut transfer_len = self.state.cdb[4] as u32;
                if transfer_len == 0 {
                    transfer_len = 256;
                }
                let byte_count = transfer_len as u64 * CD_SECTOR_SIZE as u64;
                let offset = lba as u64 * CD_SECTOR_SIZE as u64;

                if offset + byte_count > self.iso_size {
                    self.state.sense_key = 0x05;
                    self.state.sense_asc = 0x21;
                    self.state.reg_error = (self.state.sense_key << 4) | 0x04;
                    self.state.phase = AtapiPhase::StatusIn;
                    self.raise_irq();
                    return;
                }

                let mut buf = vec![0u8; byte_count as usize];
                if let Some(ref mut f) = self.iso_file {
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let _ = f.read_exact(&mut buf);
                    }
                }
                if self.unattended {
                    crate::unattended::patch_unattended_iso_sectors(&mut buf);
                }
                self.sectors_read_total += transfer_len as u64;
                self.start_data_in(buf);
            }
            SCSI_READ_FORMAT_CAPACITIES => {
                let alloc = u16::from_be_bytes([self.state.cdb[7], self.state.cdb[8]]) as usize;
                let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;
                let mut cap = vec![0u8; 12];
                cap[3] = 8; // capacity list length
                cap[4..8].copy_from_slice(&total_sectors.to_be_bytes());
                cap[8] = 0x02; // formatted media
                cap[9..12].copy_from_slice(&[0x00, 0x08, 0x00]); // 2048 bytes block length
                let len = alloc.min(12);
                self.start_data_in(cap[..len].to_vec());
            }
            SCSI_SEEK_10 | SCSI_SYNCHRONIZE_CACHE => {
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
                let byte_count = transfer_len as u64 * CD_SECTOR_SIZE as u64;
                let offset = lba as u64 * CD_SECTOR_SIZE as u64;

                if offset + byte_count > self.iso_size {
                    self.state.sense_key = 0x05;
                    self.state.sense_asc = 0x21;
                    self.state.reg_error = (self.state.sense_key << 4) | 0x04;
                    self.state.phase = AtapiPhase::StatusIn;
                    self.raise_irq();
                    return;
                }

                let mut buf = vec![0u8; byte_count as usize];
                if let Some(ref mut f) = self.iso_file {
                    if f.seek(SeekFrom::Start(offset)).is_ok() {
                        let _ = f.read_exact(&mut buf);
                    }
                }
                if self.unattended {
                    crate::unattended::patch_unattended_iso_sectors(&mut buf);
                }
                self.sectors_read_total += transfer_len as u64;
                self.start_data_in(buf);
            }
            SCSI_MECHANISM_STATUS => {
                let alloc = u16::from_be_bytes([self.state.cdb[8], self.state.cdb[9]]) as usize;
                let mut resp = vec![0u8; 8];
                resp[5] = 1; // 1 slot
                let len = alloc.min(8);
                self.start_data_in(resp[..len].to_vec());
            }
            _ => {
                eprintln!(
                    "[CDROM] SCSI cmd desconocido: 0x{:02X} CDB={:02X?}",
                    opcode,
                    &self.state.cdb[..]
                );
                self.state.sense_key = 0x05; // ILLEGAL REQUEST
                self.state.sense_asc = 0x20; // INVALID COMMAND OPERATION CODE
                self.state.reg_error = (self.state.sense_key << 4) | 0x04;
                self.state.phase = AtapiPhase::StatusIn;
                self.raise_irq();
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
    pub dma_active: bool,
    pub dma_is_write: bool,
    pub irq_pending: bool,
    pub sectors_read_total: u64,
    pub sectors_written_total: u64,
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
            dma_active: false,
            dma_is_write: false,
            irq_pending: false,
            sectors_read_total: 0,
            sectors_written_total: 0,
        }
    }

    pub fn get_lba(&self) -> u64 {
        self.lba
    }

    pub fn get_sector_count(&self) -> u16 {
        if self.sector_count == 0 {
            256
        } else {
            self.sector_count
        }
    }

    pub fn set_status(&mut self, s: u8) {
        self.status = s;
    }

    /// Carga un disco duro raw (.img) para el canal primario.
    ///
    /// Se abre en modo lectura+escritura: un guest (p. ej. Linux) escribe
    /// sectores durante el arranque (journal, etc.). Antes se abría con
    /// `File::open` (solo lectura) y las escrituras fallaban en silencio.
    pub fn with_disk(disk_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();

        eprintln!(
            "[ATA] Disco duro cargado: {} ({:.1} MB, rw)",
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
            dma_active: false,
            dma_is_write: false,
            irq_pending: false,
            sectors_read_total: 0,
            sectors_written_total: 0,
        })
    }

    /// Indica si hay un disco conectado
    pub fn has_disk(&self) -> bool {
        self.disk_file.is_some()
    }

    pub fn take_irq(&mut self) -> bool {
        let p = self.irq_pending;
        self.irq_pending = false;
        p
    }

    /// Reset del dispositivo ATA: borra el estado de la transferencia actual.
    pub fn reset(&mut self) {
        self.status = ST_DRDY;
        self.lba = 0;
        self.sector_count = 1;
        self.data_buf.clear();
        self.data_offset = 0;
        self.drive_select = 0;
        self.last_cmd = 0;
        self.dma_active = false;
        self.dma_is_write = false;
        self.irq_pending = false;
    }

    /// Construye un IDENTIFY DEVICE response (512 bytes) para un disco duro ATA
    fn build_identify(&self) -> Vec<u8> {
        let mut buf = vec![0u8; 512];

        // Word 0: general configuration (0x0040: non-removable ATA device)
        buf[0] = 0x40;
        buf[1] = 0x00;

        // Word 1: cylinders (16383)
        buf[2] = 0x3F; buf[3] = 0x3F;
        // Word 3: heads (16)
        buf[6] = 16;   buf[7] = 0;
        // Word 6: sectors per track (63)
        buf[12] = 63;  buf[13] = 0;

        // Word 10-19: serial number (20 bytes)
        let serial = b"TWO555-HDD0     ";
        for (i, &b) in serial.iter().take(20).enumerate() {
            buf[20 + i] = b;
        }

        // Word 23-26: firmware revision (8 bytes)
        buf[46..54].copy_from_slice(b"01.00   ");

        // Word 27-46: model name (40 chars, byte-swapped per ATA spec)
        let model = b"Two Five Five Virtual HDD               ";
        for (i, slot) in buf[54..94].chunks_mut(2).enumerate() {
            let get = |k: usize| -> u8 { model.get(k).copied().unwrap_or(b' ') };
            slot[0] = get(i * 2 + 1);
            slot[1] = get(i * 2);
        }

        // Word 49: capabilities (LBA supported = 0x0200)
        buf[98] = 0x00; buf[99] = 0x02;

        // Word 53: fields valid
        buf[106] = 0x06; buf[107] = 0x00;

        // Word 59: Ultra DMA modes supported
        buf[118] = 0x70; buf[119] = 0x00;

        let total_sectors = (self.disk_size / 512).max(1);
        let lba28 = (total_sectors.min(0x0FFF_FFFF)) as u32;
        // Word 60-61 (offset 120-123): Total LBA28 sectors
        buf[120..124].copy_from_slice(&lba28.to_le_bytes());

        // Word 83 (offset 166-167): LBA48 feature set supported
        buf[166] = 0x00; buf[167] = 0x44;
        // Word 86 (offset 172-173): LBA48 feature set enabled
        buf[172] = 0x00; buf[173] = 0x04;
        // Word 93 (offset 186-187): Hardware reset result
        buf[186] = 0x40; buf[187] = 0x41;

        // Word 100-103 (offset 200-207): Total LBA48 sectors (64-bit LE)
        buf[200..208].copy_from_slice(&total_sectors.to_le_bytes());

        buf
    }

    /// Lee n sectores del disco a partir del LBA
    pub fn read_sectors(&mut self, lba: u64, count: u16) -> Vec<u8> {
        let mut buf = vec![0u8; (count as usize) * 512];
        if let Some(ref mut f) = self.disk_file {
            if let Ok(_) = f.seek(SeekFrom::Start(lba * 512)) {
                let _ = f.read_exact(&mut buf);
                self.sectors_read_total += count as u64;
            }
        }
        buf
    }

    /// Escribe n sectores al disco y hace fsync (sync_data) para que los
    /// datos queden persistidos en el archivo antes de devolver status OK.
    /// Devuelve false si el archivo no está abierto, los datos no cubren
    /// `count*512` bytes o la escritura/fsync falla (el guest verá ST_ERR).
    pub fn write_sectors(&mut self, lba: u64, count: u16, data: &[u8]) -> bool {
        let Some(file) = self.disk_file.as_mut() else {
            eprintln!("[ATA] WRITE sin archivo de disco (LBA={:#x})", lba);
            return false;
        };
        let len = (count as usize) * 512;
        if data.len() < len {
            eprintln!("[ATA] WRITE corto: pidió {} bytes, recibió {}", len, data.len());
            return false;
        }
        let result = (|| -> std::io::Result<()> {
            file.seek(SeekFrom::Start(lba * 512))?;
            file.write_all(&data[..len])?;
            file.sync_data()
        })();
        if let Err(e) = result {
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
            let lba_ext = (val & 0x0F) as u64;
            self.lba = (self.lba & 0x0000000000FFFFFF) | (lba_ext << 24);
            return;
        }

        // Si se selecciona el esclavo (bit 4 = 1), no hay dispositivo esclavo en el canal primario.
        if (self.drive_select & 0x10) != 0 && port != PRI_ALT_STATUS {
            return;
        }

        match port {
            PRI_DATA => {
                // PIO rápido multisector: KVM coalesce un REP OUTSW/OUTSB en
                // UN solo IoOut con todos los bytes. Antes solo se consumía
                // data[0] y una transferencia WRITE SECTORS de N sectores
                // nunca se completaba (el guest se quedaba esperando DRQ).
                let total = self.data_buf.len();
                if total == 0 {
                    return; // escritura fuera de transferencia: ignorar
                }
                for &b in data.iter() {
                    if self.data_offset < total {
                        self.data_buf[self.data_offset] = b;
                        self.data_offset += 1;
                    }
                }
                if self.data_offset >= total {
                    let count = (total / 512) as u16;
                    let buf = std::mem::take(&mut self.data_buf);
                    self.data_offset = 0;
                    if self.write_sectors(self.lba, count, &buf) {
                        self.status = ST_DRDY;
                        self.irq_pending = true;
                        eprintln!("[ATA] WRITE SECTORS LBA={:#x} count={} (fsync ok)", self.lba, count);
                    } else {
                        self.status = ST_DRDY | ST_ERR;
                        self.irq_pending = true;
                    }
                }
            }
            PRI_ERROR => {}
            PRI_SECTORS => { self.sector_count = val as u16; }
            // Cada registro LBAx reemplaza SOLO su byte (LBA28, bits 0-27):
            // las máscaras conservan el resto. Antes PRI_LBA1/PRI_LBA2
            // borraban los bytes bajos ya escritos y cualquier LBA > 0xFF
            // quedaba truncado (p. ej. LBA 1 se leía como 0).
            PRI_LBA0 => {
                self.lba = (self.lba & 0xFFFF_FFFF_FFFF_FF00) | (val as u64);
            }
            PRI_LBA1 => {
                self.lba = (self.lba & 0xFFFF_FFFF_FFFF_00FF) | ((val as u64) << 8);
            }
            PRI_LBA2 => {
                self.lba = (self.lba & 0xFFFF_FFFF_FF00_FFFF) | ((val as u64) << 16);
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
                            self.irq_pending = true;
                            eprintln!("[ATA] IDENTIFY DEVICE (sector_count={})", self.sector_count);
                        } else {
                            self.status = ST_DRDY | ST_ERR;
                            self.irq_pending = true;
                        }
                    }
                    0x20 => { // READ SECTORS (LBA28)
                        if self.has_disk() {
                            let count = if self.sector_count == 0 { 256 } else { self.sector_count };
                            self.data_buf = self.read_sectors(self.lba, count);
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                            self.irq_pending = true;
                            eprintln!("[ATA] READ SECTORS LBA={:#x} count={}", self.lba, count);
                        } else {
                            self.status = ST_DRDY | ST_ERR;
                            self.irq_pending = true;
                        }
                    }
                    0x24 => { // READ SECTORS EXT (LBA48)
                        if self.has_disk() {
                            let count = if self.sector_count == 0 { 256 } else { self.sector_count };
                            self.data_buf = self.read_sectors(self.lba, count);
                            self.data_offset = 0;
                            self.status = ST_DRDY | ST_DRQ;
                            self.irq_pending = true;
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
                    0xC8 | 0x25 => { // READ DMA (LBA28) / READ DMA EXT (LBA48)
                        if self.has_disk() {
                            self.dma_active = true;
                            self.dma_is_write = false;
                            self.status = ST_DRDY;
                        } else {
                            self.status = ST_DRDY | ST_ERR;
                            self.irq_pending = true;
                        }
                    }
                    0xCA | 0x35 => { // WRITE DMA (LBA28) / WRITE DMA EXT (LBA48)
                        if self.has_disk() {
                            self.dma_active = true;
                            self.dma_is_write = true;
                            self.status = ST_DRDY;
                        } else {
                            self.status = ST_DRDY | ST_ERR;
                            self.irq_pending = true;
                        }
                    }
                    0x90 => {
                        self.status = ST_DRDY;
                        self.irq_pending = true;
                    }
                    0xEF => {
                        // SET FEATURES
                        self.status = ST_DRDY;
                        self.irq_pending = true;
                    }
                    0x00 => {
                        self.status = ST_DRDY;
                        self.irq_pending = true;
                    }
                    _ => {
                        self.status = ST_DRDY | ST_ERR;
                        self.irq_pending = true;
                    }
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

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        // Si el host tiene seleccionado el esclavo (bit 4 de drive_select = 1),
        // no hay dispositivo esclavo en el canal primario: reportamos 0x00.
        if (self.drive_select & 0x10) != 0 && port != PRI_DRIVE && port != PRI_ALT_STATUS {
            return vec![0x00; count];
        }

        match port {
            PRI_DATA => {
                // PIO rápido multisector: KVM entrega un REP INSW coalescido
                // como un solo IoIn de `count` bytes. Antes se devolvían
                // SIEMPRE 2 bytes y las lecturas de >1 sector truncaban.
                let mut result = Vec::with_capacity(count);
                for _ in 0..count {
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
            PRI_CMD => {
                self.irq_pending = false;
                vec![self.status]
            }
            PRI_ALT_STATUS => vec![self.status],
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
                self.state.reg_error = val;
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
                // Command register (write): ATA commands go here
                self.state.phase = AtapiPhase::Idle;
                self.state.sense_key = 0;
                self.state.sense_asc = 0;
                self.state.sense_ascq = 0;
                self.state.reg_error = 0;

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
                        self.state.byte_count_limit = if limit == 0 { 0xFFFE } else { limit };
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
