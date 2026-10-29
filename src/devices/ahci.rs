//! Controlador SATA / AHCI 1.3 (Intel ICH8M - 8086:2829).
//!
//! Basado fielmente en la implementación de VirtualBox (`DevAHCI.cpp`).
//! Cumple la especificación Serial ATA Advanced Host Controller Interface (AHCI) 1.3.
//!
//! Características:
//! 1. Identificación PCI estándar: Vendor 0x8086, Device 0x2829, Clase 01/06/01.
//! 2. BAR5 MMIO de 4 KiB (0x1000): registros HBA globales y registros por puerto.
//! 3. 2 puertos activos:
//!    - Puerto 0: Disco duro SATA (ATA IDENTIFY, READ/WRITE DMA, FPDMA QUEUED/NCQ, FLUSH).
//!    - Puerto 1: Unidad óptica SATA ATAPI (IDENTIFY PACKET, SCSI PACKET con CD-ROM).
//! 4. Listas de comandos en memoria del guest, tablas de comandos y descriptores PRD
//!    con acceso directo DMA vía `GuestMemory`.
//! 5. Generación de FIS D2H Register y señalización de interrupciones PCI con
//!    desafirmación limpia al vaciar `PxIS` / `GHC.IS`.

use crate::guest_mem::GuestMemory;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;

// ─── Offsets HBA Globales (0x00 .. 0x2C) ──────────────────────────
pub const HOST_CAP: u32 = 0x00;        // Host Capabilities
pub const HOST_CTL: u32 = 0x04;        // Global Host Control (GHC)
pub const HOST_IRQ_STAT: u32 = 0x08;   // Interrupt Status (IS)
pub const HOST_PORTS_IMPL: u32 = 0x0C; // Ports Implemented (PI)
pub const HOST_VERSION: u32 = 0x10;    // AHCI Version (VS)
#[allow(dead_code)]
pub const HOST_CCC_CTL: u32 = 0x14;    // Command Completion Coalescing Control
#[allow(dead_code)]
pub const HOST_CCC_PORTS: u32 = 0x18;  // CCC Ports
pub const HOST_CAP2: u32 = 0x24;       // Host Capabilities Extended
pub const HOST_BOHC: u32 = 0x28;       // BIOS/OS Handoff Control and Status

// ─── Offsets por Puerto (Base = 0x100 + Port * 0x80) ───────────────
pub const PORT_CLB: u32 = 0x00;   // Command List Base Address (32-bit low)
pub const PORT_CLBU: u32 = 0x04;  // Command List Base Address (32-bit high)
pub const PORT_FB: u32 = 0x08;    // FIS Base Address (32-bit low)
pub const PORT_FBU: u32 = 0x0C;   // FIS Base Address (32-bit high)
pub const PORT_IS: u32 = 0x10;    // Interrupt Status
pub const PORT_IE: u32 = 0x14;    // Interrupt Enable
pub const PORT_CMD: u32 = 0x18;   // Command and Status
pub const PORT_TFD: u32 = 0x20;   // Task File Data (Status & Error)
pub const PORT_SIG: u32 = 0x24;   // Signature
pub const PORT_SSTS: u32 = 0x28;  // SATA Status (SCR0: SStatus)
pub const PORT_SCTL: u32 = 0x2C;  // SATA Control (SCR2: SControl)
pub const PORT_SERR: u32 = 0x30;  // SATA Error (SCR1: SError)
pub const PORT_SACT: u32 = 0x34;  // SATA Active (SCR3: SActive / NCQ)
pub const PORT_CI: u32 = 0x38;    // Command Issue

// ─── Banderas de Capacidades y Registros Globales ─────────────────
pub const CAP_S64A: u32 = 1 << 31;     // 64-bit DMA addressing
pub const CAP_SNCQ: u32 = 1 << 30;     // Supports Native Command Queuing
#[allow(dead_code)]
pub const CAP_SSNTF: u32 = 1 << 29;    // Supports SNotification
pub const CAP_SCLO: u32 = 1 << 24;     // Supports Command List Override
pub const CAP_ISS_GEN3: u32 = 3 << 20; // 6.0 Gbps (Gen 3)
pub const CAP_SAM: u32 = 1 << 18;      // Supports AHCI mode only
pub const CAP_SSC: u32 = 1 << 14;      // Slumber State Capable
pub const CAP_PSC: u32 = 1 << 13;      // Partial State Capable
pub const CAP_NCS_32: u32 = 31 << 8;   // 32 command slots (0-based)
pub const CAP_NP_6: u32 = 5;           // 6 ports (0-based: 5, Intel ICH8M standard)

pub const AHCI_CAP_DEFAULT: u32 = CAP_S64A
    | CAP_SNCQ
    | CAP_SCLO
    | CAP_ISS_GEN3
    | CAP_SAM
    | CAP_SSC
    | CAP_PSC
    | CAP_NCS_32
    | CAP_NP_6;

pub const GHC_HR: u32 = 1 << 0;  // HBA Reset
pub const GHC_IE: u32 = 1 << 1;  // Global Interrupt Enable
pub const GHC_AE: u32 = 1 << 31; // AHCI Enable

// ─── Banderas por Puerto ───────────────────────────────────────────
pub const PORT_CMD_ST: u32 = 1 << 0;     // Start processing command list
pub const PORT_CMD_SUD: u32 = 1 << 1;    // Spin-up Device
pub const PORT_CMD_POD: u32 = 1 << 2;    // Power On Device
pub const PORT_CMD_FRE: u32 = 1 << 4;    // FIS Receive Enable
pub const PORT_CMD_FR: u32 = 1 << 14;    // FIS Receive Running
pub const PORT_CMD_CR: u32 = 1 << 15;    // Command List Running
pub const PORT_CMD_ATAPI: u32 = 1 << 24; // Device is ATAPI

pub const PORT_IS_DHRS: u32 = 1 << 0; // Device to Host Register FIS
pub const PORT_IS_PSS: u32 = 1 << 1;  // PIO Setup FIS
#[allow(dead_code)]
pub const PORT_IS_DSS: u32 = 1 << 2;  // DMA Setup FIS
pub const PORT_IS_SDBS: u32 = 1 << 3; // Set Device Bits FIS
#[allow(dead_code)]
pub const PORT_IS_DPS: u32 = 1 << 5;  // Descriptor Processed
pub const PORT_IS_PCS: u32 = 1 << 6;  // Port Connect Status Change
#[allow(dead_code)]
pub const PORT_IS_TFES: u32 = 1 << 30; // Task File Error Status

pub const PORT_SIG_ATA: u32 = 0x0000_0101;   // Firma ATA Hard Disk
pub const PORT_SIG_ATAPI: u32 = 0xEB14_0101; // Firma ATAPI CD-ROM (0xEB14 en cilindros)

pub const PORT_SSTS_PRESENT_GEN3: u32 = 0x0000_0133; // DET=3 (Ready), SPD=3 (Gen3), IPM=1 (Active)
#[allow(dead_code)]
pub const PORT_SSTS_PRESENT_GEN2: u32 = 0x0000_0123; // DET=3 (Ready), SPD=2 (Gen2), IPM=1 (Active)

#[allow(dead_code)]
pub const ATA_STATUS_BUSY: u8 = 0x80;
pub const ATA_STATUS_DRDY: u8 = 0x40;
#[allow(dead_code)]
pub const ATA_STATUS_DRQ: u8 = 0x08;
pub const ATA_STATUS_ERR: u8 = 0x01;

pub const CD_SECTOR_SIZE: usize = 2048;

/// Representación del estado de un puerto SATA individual (0 a 1).
#[derive(Debug)]
pub struct AhciPort {
    #[allow(dead_code)]
    pub port_num: usize,
    pub is_atapi: bool,
    pub present: bool,
    pub clb: u32,
    pub clbu: u32,
    pub fb: u32,
    pub fbu: u32,
    pub is: u32,
    pub ie: u32,
    pub cmd: u32,
    pub tfd: u32,
    pub sig: u32,
    pub ssts: u32,
    pub sctl: u32,
    pub serr: u32,
    pub sact: u32,
    pub ci: u32,
}

impl AhciPort {
    pub fn new(port_num: usize, is_atapi: bool, present: bool) -> Self {
        let mut port = Self {
            port_num,
            is_atapi,
            present,
            clb: 0,
            clbu: 0,
            fb: 0,
            fbu: 0,
            is: 0,
            ie: 0,
            cmd: PORT_CMD_SUD | PORT_CMD_POD,
            tfd: 0x7F, // Estado inicial reset/busy
            sig: if is_atapi { PORT_SIG_ATAPI } else { PORT_SIG_ATA },
            ssts: if present { PORT_SSTS_PRESENT_GEN3 } else { 0 },
            sctl: 0,
            serr: 0,
            sact: 0,
            ci: 0,
        };
        if is_atapi {
            port.cmd |= PORT_CMD_ATAPI;
        }
        if present {
            port.tfd = 0x150; // Unidad lista: Status = 0x50 (DRDY | DSC), Error = 0x01
        }
        port
    }

    pub fn reset(&mut self) {
        self.clb = 0;
        self.clbu = 0;
        self.fb = 0;
        self.fbu = 0;
        self.is = 0;
        self.ie = 0;
        self.cmd = PORT_CMD_SUD | PORT_CMD_POD;
        if self.is_atapi {
            self.cmd |= PORT_CMD_ATAPI;
        }
        self.tfd = 0x7F; // Estado inicial reset/busy
        if self.present {
            self.tfd = 0x150; // Unidad lista: Status = 0x50 (DRDY | DSC), Error = 0x01
        }
        self.sig = if self.is_atapi { PORT_SIG_ATAPI } else { PORT_SIG_ATA };
        self.ssts = if self.present { PORT_SSTS_PRESENT_GEN3 } else { 0 };
        self.sctl = 0;
        self.serr = 0;
        self.sact = 0;
        self.ci = 0;
    }

    #[inline]
    pub fn cmd_list_address(&self) -> u64 {
        ((self.clbu as u64) << 32) | (self.clb as u64)
    }

    #[inline]
    pub fn fis_base_address(&self) -> u64 {
        ((self.fbu as u64) << 32) | (self.fb as u64)
    }
}

/// Controlador AHCI completo con soporte PCI y MMIO.
pub struct AhciController {
    /// Dirección base BAR5 asignada en memoria física (MMIO 4 KiB)
    pub bar5: u32,
    pub cap: u32,
    pub ghc: u32,
    pub is: u32,
    pub pi: u32,
    pub vs: u32,
    pub cap2: u32,
    pub bohc: u32,
    pub ports: [AhciPort; 3],

    // Archivos y almacenamiento de respaldo
    pub vdisk: Option<crate::vdisk::VirtualDisk>,
    pub disk_file: Option<File>,
    pub disk_size: u64,
    pub iso_file: Option<File>,
    pub iso_size: u64,
    pub aux_disk_file: Option<File>,
    pub aux_disk_size: u64,

    // Buffer de disco en RAM simulado para tests o ejecución sin imagen en disco
    pub ram_disk: Option<Vec<u8>>,
    /// Modo de instalación desatendida activado
    pub unattended: bool,
    pub sectors_read_total: u64,
    pub sectors_written_total: u64,
    /// Buffers reutilizables para evitar asignaciones de heap por cada comando DMA/NCQ
    io_buf: Vec<u8>,
    prd_buf: Vec<(u64, usize)>,
}

/// Copia una cadena de texto en formato ATA (intercambiando bytes en cada palabra de 16 bits).
fn copy_ata_string(dst: &mut [u8], src: &[u8]) {
    for (i, slot) in dst.chunks_mut(2).enumerate() {
        let get = |k: usize| -> u8 { src.get(k).copied().unwrap_or(b' ') };
        slot[0] = get(i * 2 + 1);
        slot[1] = get(i * 2);
    }
}

impl AhciController {
    pub fn new() -> Self {
        Self {
            bar5: 0,
            cap: AHCI_CAP_DEFAULT,
            ghc: GHC_AE, // AHCI Enable activado por defecto
            is: 0,
            pi: 0x03, // Puertos 0 y 1 implementados inicialmente (0x07 con aux_disk)
            vs: 0x0001_0300, // AHCI 1.3.0
            cap2: 1, // BOHC soportado
            bohc: 0,
            ports: [
                AhciPort::new(0, false, true),  // Port 0: Disco duro
                AhciPort::new(1, true, true),   // Port 1: CD-ROM ATAPI
                AhciPort::new(2, false, false), // Port 2: HDD Auxiliar OEMDRV (presente si hay aux_disk)
            ],
            vdisk: None,
            disk_file: None,
            disk_size: 0,
            iso_file: None,
            iso_size: 0,
            aux_disk_file: None,
            aux_disk_size: 0,
            ram_disk: None,
            unattended: false,
            sectors_read_total: 0,
            sectors_written_total: 0,
            io_buf: Vec::with_capacity(64 * 1024),
            prd_buf: Vec::with_capacity(32),
        }
    }

    pub fn with_files(disk_path: Option<&str>, iso_path: Option<&str>) -> Self {
        let mut ctrl = Self::new();
        let mut disk_mounted = false;
        if let Some(p) = disk_path {
            if let Ok(vd) = crate::vdisk::VirtualDisk::open(p) {
                let size = vd.logical_size();
                let fmt = vd.format().label();
                ctrl.vdisk = Some(vd);
                ctrl.disk_size = size;
                disk_mounted = true;
                eprintln!(
                    "[AHCI] Puerto 0: Disco SATA montado ('{}', formato {}, {} MiB)",
                    p,
                    fmt,
                    size / (1024 * 1024)
                );
            } else {
                eprintln!("[AHCI] Advertencia: No se pudo abrir el disco '{}'", p);
            }
        }
        if !disk_mounted {
            ctrl.ports[0].present = false;
            ctrl.ports[0].ssts = 0;
            ctrl.ports[0].tfd = 0x7F;
        }

        let mut iso_mounted = false;
        if let Some(p) = iso_path {
            if let Ok(mut f) = OpenOptions::new().read(true).open(p) {
                let size = f.seek(SeekFrom::End(0)).unwrap_or(0);
                let _ = f.seek(SeekFrom::Start(0));
                ctrl.iso_file = Some(f);
                ctrl.iso_size = size;
                iso_mounted = true;
                eprintln!(
                    "[AHCI] Puerto 1: ATAPI CD-ROM SATA montado ('{}', {} MiB)",
                    p,
                    size / (1024 * 1024)
                );
            } else {
                eprintln!("[AHCI] Advertencia: No se pudo abrir la ISO '{}'", p);
            }
        }
        if !iso_mounted {
            ctrl.ports[1].present = false;
            ctrl.ports[1].ssts = 0;
            ctrl.ports[1].tfd = 0x7F;
        }
        ctrl
    }

    /// Adjunta un disco auxiliar (ej. imagen OEMDRV para instalación desatendida) en el Puerto 2.
    pub fn attach_aux_disk(&mut self, path: &str) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .or_else(|_| OpenOptions::new().read(true).open(path));

        match file {
            Ok(mut f) => {
                let size = f.seek(SeekFrom::End(0)).unwrap_or(0);
                let _ = f.seek(SeekFrom::Start(0));
                self.aux_disk_file = Some(f);
                self.aux_disk_size = size;
                self.pi |= 0x04; // 0x07 con puertos 0, 1 y 2
                self.ports[2].present = true;
                self.ports[2].ssts = PORT_SSTS_PRESENT_GEN3;
                self.ports[2].tfd = 0x150; // DRDY | DSC
                self.ports[2].sig = PORT_SIG_ATA;
                self.ports[2].is |= PORT_IS_PCS;
                self.update_irq();
                eprintln!("[AHCI] Puerto 2: Disco auxiliar OEMDRV montado ('{}', {} KiB)", path, size / 1024);
            }
            Err(e) => {
                eprintln!("[AHCI] Error al abrir disco auxiliar '{}': {}", path, e);
            }
        }
    }

    pub fn reset(&mut self) {
        self.flush_sectors(0);
        self.cap = AHCI_CAP_DEFAULT;
        self.ghc = GHC_AE;
        self.is = 0;
        self.pi = if self.aux_disk_file.is_some() { 0x07 } else { 0x03 };
        self.vs = 0x0001_0300;
        self.cap2 = 1;
        self.bohc = 0;
        for p in &mut self.ports {
            p.reset();
        }
        if self.aux_disk_file.is_some() {
            self.ports[2].present = true;
            self.ports[2].ssts = PORT_SSTS_PRESENT_GEN3;
            self.ports[2].tfd = 0x150;
            self.ports[2].sig = PORT_SIG_ATA;
        }
    }

    /// Comprueba si hay una línea IRQ de PCI levantada por el controlador AHCI.
    pub fn is_irq_asserted(&self) -> bool {
        if (self.ghc & GHC_IE) == 0 {
            return false;
        }
        self.is != 0 || self.ports.iter().any(|p| (p.is & p.ie) != 0)
    }

    /// Actualiza el registro global `IS` y sincroniza las líneas de interrupción.
    pub fn update_irq(&mut self) {
        let mut is_val = 0u32;
        for (i, p) in self.ports.iter().enumerate() {
            if (p.is & p.ie) != 0 {
                is_val |= 1 << i;
            }
        }
        self.is = is_val;
    }

    /// Despacha una lectura MMIO dentro del rango BAR5 del AHCI (0x0000 .. 0x0FFF).
    pub fn read(&mut self, offset: u64, size: usize) -> Vec<u8> {
        let reg_offset = (offset & 0x0FFF) as u32;
        let dword_val = self.read_reg_u32(reg_offset);
        let shift = ((reg_offset & 3) * 8) as usize;
        let val = dword_val >> shift;

        let mut res = vec![0u8; size];
        for (i, item) in res.iter_mut().enumerate() {
            *item = ((val >> (i * 8)) & 0xFF) as u8;
        }
        res
    }

    fn read_reg_u32(&self, reg: u32) -> u32 {
        if reg < 0x100 {
            // Registros HBA globales
            match reg & !3 {
                HOST_CAP => self.cap,
                HOST_CTL => self.ghc,
                HOST_IRQ_STAT => self.is,
                HOST_PORTS_IMPL => self.pi,
                HOST_VERSION => self.vs,
                HOST_CCC_CTL => 0,
                HOST_CCC_PORTS => 0,
                HOST_CAP2 => self.cap2,
                HOST_BOHC => self.bohc,
                _ => 0,
            }
        } else {
            // Registros por puerto
            let port_idx = ((reg - 0x100) / 0x80) as usize;
            if port_idx >= self.ports.len() {
                return 0;
            }
            let port = &self.ports[port_idx];
            let port_reg = (reg - 0x100) % 0x80;
            match port_reg & !3 {
                PORT_CLB => port.clb,
                PORT_CLBU => port.clbu,
                PORT_FB => port.fb,
                PORT_FBU => port.fbu,
                PORT_IS => port.is,
                PORT_IE => port.ie,
                PORT_CMD => {
                    let mut cmd = port.cmd;
                    if (cmd & PORT_CMD_ST) != 0 {
                        cmd |= PORT_CMD_CR; // Command list running
                    }
                    if (cmd & PORT_CMD_FRE) != 0 {
                        cmd |= PORT_CMD_FR; // FIS receive running
                    }
                    cmd
                }
                PORT_TFD => port.tfd,
                PORT_SIG => port.sig,
                PORT_SSTS => port.ssts,
                PORT_SCTL => port.sctl,
                PORT_SERR => port.serr,
                PORT_SACT => port.sact,
                PORT_CI => port.ci,
                _ => 0,
            }
        }
    }

    /// Despacha una escritura MMIO dentro del rango BAR5 del AHCI.
    pub fn write(&mut self, offset: u64, data: &[u8], mem: Option<&GuestMemory>) {
        if data.is_empty() {
            return;
        }
        let reg_offset = (offset & 0x0FFF) as u32;
        let mut dword = self.read_reg_u32(reg_offset & !3);
        let shift = (reg_offset & 3) as usize;
        for (i, &b) in data.iter().enumerate() {
            let byte_pos = shift + i;
            if byte_pos < 4 {
                let mask = !(0xFF << (byte_pos * 8));
                dword = (dword & mask) | ((b as u32) << (byte_pos * 8));
            }
        }

        self.write_reg_u32(reg_offset & !3, dword, mem);
    }

    fn write_reg_u32(&mut self, reg: u32, val: u32, mem: Option<&GuestMemory>) {
        if reg < 0x100 {
            // Registros HBA globales
            match reg {
                HOST_CTL => {
                    if (val & GHC_HR) != 0 {
                        self.reset();
                    } else {
                        self.ghc = (self.ghc & !0x03) | (val & 0x03) | (val & GHC_AE);
                    }
                    self.update_irq();
                }
                HOST_IRQ_STAT => {
                    // R/WC (Write 1 to clear)
                    self.is &= !val;
                }
                HOST_BOHC => {
                    self.bohc = val;
                }
                _ => {}
            }
        } else {
            // Registros por puerto
            let port_idx = ((reg - 0x100) / 0x80) as usize;
            if port_idx >= self.ports.len() {
                return;
            }
            let port_reg = (reg - 0x100) % 0x80;
            let mut issued = false;

            {
                let port = &mut self.ports[port_idx];
                match port_reg {
                    PORT_CLB => port.clb = val & !0x3FF, // Alineado a 1 KiB
                    PORT_CLBU => port.clbu = val,
                    PORT_FB => port.fb = val & !0xFF,    // Alineado a 256 bytes
                    PORT_FBU => port.fbu = val,
                    PORT_IS => {
                        // R/WC: escribir 1 limpia el bit
                        port.is &= !val;
                    }
                    PORT_IE => port.ie = val,
                    PORT_CMD => {
                        let old_fre = (port.cmd & PORT_CMD_FRE) != 0;
                        let new_fre = (val & PORT_CMD_FRE) != 0;

                        let mut new_cmd = (port.cmd & (PORT_CMD_CR | PORT_CMD_FR | PORT_CMD_ATAPI))
                            | (val & !(PORT_CMD_CR | PORT_CMD_FR));
                        if (val & PORT_CMD_ST) != 0 {
                            new_cmd |= PORT_CMD_CR;
                        } else {
                            new_cmd &= !PORT_CMD_CR;
                        }
                        if new_fre {
                            new_cmd |= PORT_CMD_FR;
                        } else {
                            new_cmd &= !PORT_CMD_FR;
                        }
                        port.cmd = new_cmd;

                        // Emitir primer FIS D2H inmediatamente al activar FRE por primera vez (DevAHCI.cpp:2651)
                        if !old_fre && new_fre && port.present {
                            let fb_addr = port.fis_base_address();
                            if fb_addr != 0 {
                                if let Some(guest_memory) = mem {
                                    let mut d2h = [0u8; 20];
                                    d2h[0] = 0x34; // FIS Type: Register D2H
                                    d2h[1] = 0x40; // Interrupt bit (I)
                                    d2h[2] = 0x50; // Status (DRDY | DSC)
                                    d2h[3] = 0x01; // Error
                                    d2h[4] = 0x01; // LBA low
                                    d2h[5] = if port.is_atapi { 0x14 } else { 0x00 };
                                    d2h[6] = if port.is_atapi { 0xEB } else { 0x00 };
                                    d2h[7] = 0x00;
                                    d2h[12] = 0x01; // Sector count
                                    let _ = guest_memory.copy_to((fb_addr + 0x40) as usize, &d2h);
                                    port.sig = if port.is_atapi { PORT_SIG_ATAPI } else { PORT_SIG_ATA };
                                    port.tfd = 0x150;
                                    port.is |= PORT_IS_DHRS;
                                }
                            }
                        }
                    }
                    PORT_SCTL => {
                        let old_det = port.sctl & 0x0F;
                        let new_det = val & 0x0F;
                        port.sctl = val;

                        // Secuencia de reset DET = 1 seguido de DET = 0, o cuando se escriba reset (DET = 1)
                        if (old_det == 1 && new_det == 0) || (new_det == 1) {
                            if port.present {
                                port.ssts = 0x123;
                                port.serr = 0;
                                port.sig = if port.is_atapi { PORT_SIG_ATAPI } else { PORT_SIG_ATA };
                                port.tfd = 0x150;

                                if (port.cmd & PORT_CMD_FRE) != 0 {
                                    let fb_addr = port.fis_base_address();
                                    if fb_addr != 0 {
                                        if let Some(guest_memory) = mem {
                                            let mut d2h = [0u8; 20];
                                            d2h[0] = 0x34; // FIS Type: Register D2H
                                            d2h[1] = 0x00; // Interrupt bit (I)
                                            d2h[2] = 0x50; // Status (DRDY | DSC, bit 5 ATA_DF apagado)
                                            d2h[3] = 0x01; // Error
                                            d2h[4] = 0x01; // LBA low
                                            d2h[5] = if port.is_atapi { 0x14 } else { 0x00 };
                                            d2h[6] = if port.is_atapi { 0xEB } else { 0x00 };
                                            d2h[7] = 0x00;
                                            d2h[12] = 0x01; // Sector count
                                            let _ = guest_memory.copy_to((fb_addr + 0x40) as usize, &d2h);
                                        }
                                    }
                                }

                                port.is |= PORT_IS_PCS;
                            } else {
                                port.ssts = 0;
                                port.serr = 0;
                                port.tfd = 0x7F;
                            }
                        } else if new_det == 4 {
                            port.ssts = 0;
                        }
                    }
                    PORT_SERR => port.serr &= !val, // R/WC
                    PORT_SACT => port.sact |= val,
                    PORT_CI => {
                        port.ci |= val;
                        issued = true;
                    }
                    _ => {}
                }
            }

            self.update_irq();

            if issued {
                if let Some(guest_memory) = mem {
                    self.process_commands(port_idx, guest_memory);
                }
            }
        }
    }

    /// Procesa los comandos pendientes marcados en `PxCI` del puerto especificado.
    pub fn process_commands(&mut self, port_idx: usize, mem: &GuestMemory) {
        let ci_mask = self.ports[port_idx].ci;
        if ci_mask == 0 {
            return;
        }

        let is_atapi = self.ports[port_idx].is_atapi;
        let clb_addr = self.ports[port_idx].cmd_list_address();
        let fb_addr = self.ports[port_idx].fis_base_address();

        let mut io_buf = std::mem::take(&mut self.io_buf);
        let mut prd_entries = std::mem::take(&mut self.prd_buf);

        for slot in 0..32 {
            if (ci_mask & (1 << slot)) == 0 {
                continue;
            }

            let cmd_hdr_addr = clb_addr + (slot as u64) * 32;
            let dw0 = mem.read_u32(cmd_hdr_addr as usize);
            let prdtl = ((dw0 >> 16) & 0xFFFF) as usize;
            let cmd_is_atapi = (dw0 & (1 << 5)) != 0 || is_atapi;
            let _is_write = (dw0 & (1 << 6)) != 0;

            let ctba_low = mem.read_u32((cmd_hdr_addr + 8) as usize) as u64;
            let ctba_high = mem.read_u32((cmd_hdr_addr + 12) as usize) as u64;
            let ctba = ((ctba_high << 32) | ctba_low) & !0x7F;

            // Leer Command FIS (20 bytes estándar)
            let mut cfis = [0u8; 64];
            let _ = mem.copy_from(ctba as usize, &mut cfis);

            // Leer ACMD (16 bytes CDB SCSI si es ATAPI)
            let mut acmd = [0u8; 16];
            if cmd_is_atapi {
                let _ = mem.copy_from((ctba + 0x40) as usize, &mut acmd);
            }

            // Parsear PRDT (Physical Region Descriptor Table) reutilizando prd_entries
            prd_entries.clear();
            if prd_entries.capacity() < prdtl {
                prd_entries.reserve(prdtl - prd_entries.capacity());
            }
            for p_idx in 0..prdtl {
                let prd_addr = ctba + 0x80 + (p_idx as u64) * 16;
                let dba_low = mem.read_u32(prd_addr as usize) as u64;
                let dba_high = mem.read_u32((prd_addr + 4) as usize) as u64;
                let dba = (dba_high << 32) | dba_low;
                let dw3 = mem.read_u32((prd_addr + 12) as usize);
                let byte_count = ((dw3 & 0x003F_FFFF) + 1) as usize;
                prd_entries.push((dba, byte_count));
            }

            // Ejecución del comando
            let fis_type = cfis[0];
            let mut bytes_transferred: usize = 0;
            let mut status: u8 = ATA_STATUS_DRDY;
            let mut error: u8 = 0;
            let mut is_ncq = false;
            let mut is_pio = false;

            if fis_type == 0x27 { // Register FIS - Host to Device
                let command = cfis[2];
                if !is_atapi {
                    // ─── Comandos ATA para Disco Duro (Puerto 0 y Puerto 2) ───
                    match command {
                        0xEC => {
                            // IDENTIFY DEVICE (PIO)
                            is_pio = true;
                            let id_data = self.identify_device(port_idx);
                            bytes_transferred = copy_to_prd(mem, &prd_entries, &id_data);
                        }
                        0xC8 | 0x25 => {
                            // READ DMA (LBA28) / READ DMA EXT (LBA48)
                            let lba = parse_lba(&cfis, command == 0x25);
                            let count = parse_sector_count(&cfis, command == 0x25);
                            let total_bytes = (count as usize) * 512;
                            if io_buf.len() < total_bytes {
                                io_buf.resize(total_bytes, 0);
                            }
                            self.read_sectors(port_idx, lba, count, &mut io_buf[..total_bytes]);
                            bytes_transferred = copy_to_prd(mem, &prd_entries, &io_buf[..total_bytes]);
                        }
                        0x20 | 0x24 | 0xC4 | 0x29 => {
                            // READ SECTORS / READ MULTIPLE (PIO, LBA28 / LBA48)
                            is_pio = true;
                            let lba = parse_lba(&cfis, command == 0x24 || command == 0x29);
                            let count = parse_sector_count(&cfis, command == 0x24 || command == 0x29);
                            let total_bytes = (count as usize) * 512;
                            if io_buf.len() < total_bytes {
                                io_buf.resize(total_bytes, 0);
                            }
                            self.read_sectors(port_idx, lba, count, &mut io_buf[..total_bytes]);
                            bytes_transferred = copy_to_prd(mem, &prd_entries, &io_buf[..total_bytes]);
                        }
                        0xCA | 0x35 => {
                            // WRITE DMA (LBA28) / WRITE DMA EXT (LBA48)
                            let lba = parse_lba(&cfis, command == 0x35);
                            let count = parse_sector_count(&cfis, command == 0x35);
                            let total_bytes = (count as usize) * 512;
                            if io_buf.len() < total_bytes {
                                io_buf.resize(total_bytes, 0);
                            }
                            bytes_transferred = copy_from_prd(mem, &prd_entries, &mut io_buf[..total_bytes]);
                            self.write_sectors(port_idx, lba, count, &io_buf[..bytes_transferred]);
                        }
                        0x30 | 0x34 | 0xC5 | 0x39 => {
                            // WRITE SECTORS / WRITE MULTIPLE (PIO, LBA28 / LBA48)
                            is_pio = true;
                            let lba = parse_lba(&cfis, command == 0x34 || command == 0x39);
                            let count = parse_sector_count(&cfis, command == 0x34 || command == 0x39);
                            let total_bytes = (count as usize) * 512;
                            if io_buf.len() < total_bytes {
                                io_buf.resize(total_bytes, 0);
                            }
                            bytes_transferred = copy_from_prd(mem, &prd_entries, &mut io_buf[..total_bytes]);
                            self.write_sectors(port_idx, lba, count, &io_buf[..bytes_transferred]);
                        }
                        0x60 => {
                            // READ FPDMA QUEUED (NCQ Read)
                            is_ncq = true;
                            let lba = parse_ncq_lba(&cfis);
                            let count = parse_ncq_sector_count(&cfis);
                            let total_bytes = (count as usize) * 512;
                            if io_buf.len() < total_bytes {
                                io_buf.resize(total_bytes, 0);
                            }
                            self.read_sectors(port_idx, lba, count, &mut io_buf[..total_bytes]);
                            bytes_transferred = copy_to_prd(mem, &prd_entries, &io_buf[..total_bytes]);
                        }
                        0x61 => {
                            // WRITE FPDMA QUEUED (NCQ Write)
                            is_ncq = true;
                            let lba = parse_ncq_lba(&cfis);
                            let count = parse_ncq_sector_count(&cfis);
                            let total_bytes = (count as usize) * 512;
                            if io_buf.len() < total_bytes {
                                io_buf.resize(total_bytes, 0);
                            }
                            bytes_transferred = copy_from_prd(mem, &prd_entries, &mut io_buf[..total_bytes]);
                            self.write_sectors(port_idx, lba, count, &io_buf[..bytes_transferred]);
                        }
                        0xE7 | 0xEA => {
                            // FLUSH CACHE / FLUSH CACHE EXT
                            self.flush_sectors(port_idx);
                        }
                        0x90 => {
                            // EXECUTE DEVICE DIAGNOSTIC (0x01 = Passed, no error)
                            status = ATA_STATUS_DRDY;
                            error = 0x01;
                        }
                        0xEF | 0x00 | 0x06 | 0x08 | 0x10..=0x1F | 0x40..=0x42 | 0xB0 | 0x27 | 0xF5 | 0xF8
                        | 0x91 | 0xC6 | 0xDA | 0xDE | 0xDF | 0xE0..=0xE6 | 0xE8 => {
                            // 0x06: DATA SET MANAGEMENT (TRIM / Discard en NTFS)
                            // 0x08: DEVICE RESET
                            // 0x40..=0x42: READ VERIFY SECTORS (Requerido por particionadores MBR/GPT)
                            // 0xB0: SMART (Chequeos de salud de disco de Windows)
                            // 0x27/0xF8: READ NATIVE MAX ADDRESS / EXT
                            // 0xEF: SET FEATURES, 0x00: NOP, 0x10..=0x1F: RECALIBRATE
                            // 0x91: INITIALIZE DEVICE PARAMETERS
                            // 0xC6: SET MULTIPLE MODE
                            // 0xDA/0xDE/0xDF: Media status y lock/unlock
                            // 0xE0..=0xE3: Standby/Idle power management
                            // 0xE4/0xE8: Read/Write Buffer
                            // 0xE5: CHECK POWER MODE
                            // 0xE6: SLEEP
                            // 0xF5: SECURITY FREEZE LOCK
                            status = ATA_STATUS_DRDY;
                            error = 0;
                        }
                        0x2F => {
                            // READ LOG EXT (Página de diagnóstico y directorio de logs ATA/NCQ)
                            let log_buf = [0u8; 512];
                            bytes_transferred = copy_to_prd(mem, &prd_entries, &log_buf);
                            status = ATA_STATUS_DRDY;
                            error = 0;
                        }
                        _ => {
                            eprintln!("[AHCI] Puerto {}: comando ATA no implementado: 0x{:02X}", port_idx, command);
                            status = ATA_STATUS_DRDY | ATA_STATUS_ERR;
                            error = 0x04; // ABRT
                        }
                    }
                } else {
                    // ─── Comandos ATAPI para CD-ROM (Puerto 1) ───
                    match command {
                        0xA1 => {
                            // IDENTIFY PACKET DEVICE (PIO)
                            is_pio = true;
                            let id_data = self.generate_atapi_identify();
                            bytes_transferred = copy_to_prd(mem, &prd_entries, &id_data);
                        }
                        0xA0 => {
                            // PACKET (SCSI Command)
                            let scsi_res = self.execute_scsi(&acmd);
                            if let Some(buf) = scsi_res {
                                bytes_transferred = copy_to_prd(mem, &prd_entries, &buf);
                            }
                        }
                        _ => {
                            eprintln!("[AHCI] Puerto 1: comando ATAPI no implementado: 0x{:02X}", command);
                            status = ATA_STATUS_DRDY | ATA_STATUS_ERR;
                            error = 0x04; // ABRT
                        }
                    }
                }
            }

            // Actualizar PRD Byte Count (PRDBC) en la cabecera de comando
            mem.write_u32((cmd_hdr_addr + 4) as usize, bytes_transferred as u32);

            // Generar FIS correspondiente en el área de FIS recibidos del puerto
            if fb_addr != 0 {
                if is_ncq {
                    // Set Device Bits FIS (0xA1) en FB + 0x58 (8 bytes)
                    let mut sdb_fis = [0u8; 8];
                    sdb_fis[0] = 0xA1; // FIS Type: Set Device Bits
                    sdb_fis[1] = 0x40; // Bit 6 = Interrupt Bit (I)
                    sdb_fis[2] = status;
                    sdb_fis[3] = error;
                    sdb_fis[4..8].copy_from_slice(&(1u32 << slot).to_le_bytes());
                    let _ = mem.copy_to((fb_addr + 0x58) as usize, &sdb_fis);
                } else {
                    if is_pio {
                        // PIO Setup FIS (0x5F) en FB + 0x20 (20 bytes)
                        let mut psfis = [0u8; 20];
                        psfis[0] = 0x5F; // FIS Type: PIO Setup
                        psfis[1] = 0x40; // Interrupt bit
                        psfis[2] = status;
                        psfis[3] = error;
                        psfis[16] = (bytes_transferred & 0xFF) as u8;
                        psfis[17] = ((bytes_transferred >> 8) & 0xFF) as u8;
                        let _ = mem.copy_to((fb_addr + 0x20) as usize, &psfis);
                    }
                    // Register FIS - Device to Host (0x34) en FB + 0x40 (20 bytes)
                    let mut d2h_fis = [0u8; 20];
                    d2h_fis[0] = 0x34; // FIS Type: Register D2H
                    d2h_fis[1] = 0x40; // Bit 6 = Interrupt Bit (I)
                    d2h_fis[2] = status;
                    d2h_fis[3] = error;
                    if is_atapi {
                        // Razón de interrupción ATAPI en el registro Sector Count (d2h[12]):
                        // Bit 0 = C/D (Command/Data: 1 = fase comando/status, 0 = fase datos)
                        // Bit 1 = I/O (Dirección: 1 = hacia Host (lectura), 0 = desde Host (escritura))
                        d2h_fis[4] = 0;
                        d2h_fis[5] = 0x14; // LBA Mid (Firma mágica ATAPI byte 1)
                        d2h_fis[6] = 0xEB; // LBA High (Firma mágica ATAPI byte 2)
                        d2h_fis[12] = if bytes_transferred > 0 { 0x02 } else { 0x03 };
                        d2h_fis[13] = 0;
                    } else {
                        d2h_fis[4] = cfis[4];
                        d2h_fis[5] = cfis[5];
                        d2h_fis[6] = cfis[6];
                        d2h_fis[7] = cfis[7];
                        d2h_fis[8] = cfis[8];
                        d2h_fis[9] = cfis[9];
                        d2h_fis[10] = cfis[10];
                        if cfis[2] == 0xE5 {
                            // CHECK POWER MODE: 0xFF = Unidad activa / idle
                            d2h_fis[12] = 0xFF;
                            d2h_fis[13] = 0x00;
                        } else {
                            // En transferencias de datos completadas sin error, el conteo de sectores restantes es 0
                            d2h_fis[12] = 0;
                            d2h_fis[13] = 0;
                        }
                    }
                    let _ = mem.copy_to((fb_addr + 0x40) as usize, &d2h_fis);
                }
            }

            // Finalizar comando en el puerto
            let port = &mut self.ports[port_idx];
            port.tfd = status as u32 | ((error as u32) << 8);
            port.ci &= !(1 << slot);
            port.sact &= !(1 << slot);
            if is_ncq {
                port.is |= PORT_IS_SDBS;
            } else if is_pio {
                port.is |= PORT_IS_DHRS | PORT_IS_PSS;
            } else {
                port.is |= PORT_IS_DHRS;
            }
        }

        self.io_buf = io_buf;
        self.prd_buf = prd_entries;
        self.update_irq();
    }

    // ─── Helpers de Almacenamiento ──────────────────────────────────

    /// Lee sectores de 512 bytes del puerto indicado (Puerto 0: disco principal, Puerto 2: aux_disk).
    pub fn read_sectors(&mut self, port: usize, lba: u64, count: u32, dst: &mut [u8]) {
        let total = (count as usize) * 512;
        let len = dst.len().min(total);
        if port == 2 {
            if let Some(ref f) = self.aux_disk_file {
                if f.read_exact_at(&mut dst[..len], lba * 512).is_err() {
                    dst[..len].fill(0);
                }
            } else {
                dst[..len].fill(0);
            }
            return;
        }

        // Puerto 0 (disco principal VDI/RAW o RAM)
        if let Some(ref mut vd) = self.vdisk {
            if vd.read_sectors(lba, count, &mut dst[..len]).is_err() {
                dst[..len].fill(0);
            }
            self.sectors_read_total += count as u64;
            return;
        }
        if let Some(ref f) = self.disk_file {
            if f.read_exact_at(&mut dst[..len], lba * 512).is_err() {
                dst[..len].fill(0);
            }
            self.sectors_read_total += count as u64;
            return;
        }
        if let Some(ref ram) = self.ram_disk {
            let offset = (lba * 512) as usize;
            if offset < ram.len() {
                let avail = (ram.len() - offset).min(len);
                dst[..avail].copy_from_slice(&ram[offset..offset + avail]);
                if avail < len {
                    dst[avail..len].fill(0);
                }
            } else {
                dst[..len].fill(0);
            }
            self.sectors_read_total += count as u64;
            return;
        }
        dst[..len].fill(0);
    }

    /// Escribe sectores de 512 bytes en el puerto indicado (Puerto 0: disco principal, Puerto 2: aux_disk).
    pub fn write_sectors(&mut self, port: usize, lba: u64, count: u32, src: &[u8]) {
        let total = (count as usize) * 512;
        let len = src.len().min(total);
        if port == 2 {
            if let Some(ref f) = self.aux_disk_file {
                let _ = f.write_all_at(&src[..len], lba * 512);
            }
            return;
        }

        // Puerto 0 (disco principal VDI/RAW o RAM)
        if let Some(ref mut vd) = self.vdisk {
            let _ = vd.write_sectors(lba, count, &src[..len]);
            self.sectors_written_total += count as u64;
            return;
        }
        if let Some(ref f) = self.disk_file {
            let _ = f.write_all_at(&src[..len], lba * 512);
            self.sectors_written_total += count as u64;
            return;
        }
        if let Some(ref mut ram) = self.ram_disk {
            let offset = (lba * 512) as usize;
            if offset + len > ram.len() {
                ram.resize(offset + len, 0);
            }
            ram[offset..offset + len].copy_from_slice(&src[..len]);
            self.sectors_written_total += count as u64;
        }
    }

    /// Sincroniza escrituras en disco para el puerto especificado.
    pub fn flush_sectors(&mut self, port: usize) {
        if port == 2 {
            if let Some(ref mut f) = self.aux_disk_file {
                let _ = f.sync_data();
            }
            return;
        }
        if let Some(ref mut vd) = self.vdisk {
            let _ = vd.flush();
            return;
        }
        if let Some(ref mut f) = self.disk_file {
            let _ = f.sync_data();
        }
    }

    pub fn read_disk_sectors(&mut self, lba: u64, count: u32, dst: &mut [u8]) {
        self.read_sectors(0, lba, count, dst);
    }

    pub fn write_disk_sectors(&mut self, lba: u64, count: u32, src: &[u8]) {
        self.write_sectors(0, lba, count, src);
    }

    pub fn flush_disk(&mut self) {
        self.flush_sectors(0);
    }

    fn read_iso_sectors(&mut self, lba: u32, count: u16, dst: &mut [u8]) {
        let offset = (lba as u64) * CD_SECTOR_SIZE as u64;
        let total = (count as usize) * CD_SECTOR_SIZE;
        let len = dst.len().min(total);
        if let Some(ref f) = self.iso_file {
            if f.read_exact_at(&mut dst[..len], offset).is_err() {
                dst[..len].fill(0);
            } else if self.unattended || self.aux_disk_file.is_some() {
                crate::unattended::patch_unattended_iso_sectors(&mut dst[..len]);
            }
        } else {
            dst[..len].fill(0);
        }
    }

    // ─── Generación de Datos IDENTIFY ──────────────────────────────

    /// Genera la respuesta ATA IDENTIFY para el puerto especificado (Puerto 0: disco principal, Puerto 2: aux_disk).
    pub fn identify_device(&self, port: usize) -> [u8; 512] {
        if port == 2 {
            let mut buf = [0u8; 512];
            buf[0] = 0x40; // Non-removable, ATA device
            buf[1] = 0x00;
            buf[2] = 0x3F; buf[3] = 0x3F; // Cylinders (16383)
            buf[6] = 16;   buf[7] = 0;    // Heads
            buf[8] = 0x03; buf[9] = 0x00;
            buf[12] = 63;  buf[13] = 0;   // Sectors per track

            copy_ata_string(&mut buf[20..40], b"TWO555-SATA2    ");
            copy_ata_string(&mut buf[46..54], b"01.00   ");
            copy_ata_string(&mut buf[54..94], b"Two Five Five Virtual OEMDRV Disk       ");

            // Word 49: DMA (bit 8), LBA (bit 9), IORDY (bit 11) -> 0x0F00
            buf[98] = 0x00; buf[99] = 0x0F;
            // Word 53: Words 54-58, 64-70, 88 son válidos
            buf[106] = 0x07; buf[107] = 0x00;
            // Word 63: Multiword DMA modos 0, 1, 2 soportados y activos
            buf[126] = 0x07; buf[127] = 0x07;

            let total_sectors = (self.aux_disk_size / 512).max(1);
            let sectors_per_track = 63u16;
            let heads = if total_sectors <= 1024 * 16 * 63 { 16u16 } else { 255u16 };
            let cylinders = ((total_sectors / (heads as u64 * sectors_per_track as u64)).min(16383) as u16).max(1);

            buf[2..4].copy_from_slice(&cylinders.to_le_bytes());
            buf[6..8].copy_from_slice(&heads.to_le_bytes());
            buf[12..14].copy_from_slice(&sectors_per_track.to_le_bytes());

            copy_ata_string(&mut buf[20..40], b"TWO555-SATA2    ");
            copy_ata_string(&mut buf[46..54], b"01.00   ");
            copy_ata_string(&mut buf[54..94], b"Two Five Five Virtual OEMDRV Disk       ");

            buf[94] = 16; buf[95] = 0x80;
            buf[96] = 0x01; buf[97] = 0x00;
            buf[98] = 0x00; buf[99] = 0x0B;
            buf[100] = 0x00; buf[101] = 0x40;
            buf[102] = 240; buf[103] = 0;
            buf[104] = 240; buf[105] = 0;

            buf[106] = 0x07; buf[107] = 0x00;

            buf[108..110].copy_from_slice(&cylinders.to_le_bytes());
            buf[110..112].copy_from_slice(&heads.to_le_bytes());
            buf[112..114].copy_from_slice(&sectors_per_track.to_le_bytes());
            let chs_capacity = (cylinders as u32) * (heads as u32) * (sectors_per_track as u32);
            buf[114..118].copy_from_slice(&chs_capacity.to_le_bytes());
            buf[118] = 16; buf[119] = 0x01;

            let lba28 = (total_sectors.min(0x0FFF_FFFF)) as u32;
            buf[120..124].copy_from_slice(&lba28.to_le_bytes());

            buf[126] = 0x07; buf[127] = 0x07;
            buf[128] = 0x03; buf[129] = 0x00;
            buf[130] = 120; buf[132] = 120; buf[134] = 120; buf[136] = 120;

            buf[150] = 31; buf[151] = 0;
            buf[152] = 0x04; buf[153] = 0x01;

            buf[160] = 0xF0; buf[161] = 0x01;
            buf[162] = 0x28; buf[163] = 0x00;
            buf[164] = 0x78; buf[165] = 0x00;
            buf[166] = 0x00; buf[167] = 0x74;
            buf[168] = 0x00; buf[169] = 0x40;
            buf[170] = 0x78; buf[171] = 0x00;
            buf[172] = 0x00; buf[173] = 0x34;
            buf[174] = 0x00; buf[175] = 0x40;
            buf[176] = 0x7F; buf[177] = 0x7F;
            buf[186] = 0x01; buf[187] = 0x60;

            buf[200..208].copy_from_slice(&total_sectors.to_le_bytes());

            let sum: u8 = buf[..510].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
            let csum = 0u8.wrapping_sub(sum);
            buf[510] = 0xA5;
            buf[511] = csum;

            buf
        } else {
            self.generate_ata_identify()
        }
    }

    pub fn generate_ata_identify(&self) -> [u8; 512] {
        let mut buf = [0u8; 512];
        buf[0] = 0x40; // Non-removable, ATA device
        buf[1] = 0x00;

        let total_sectors = (self.disk_size / 512).max(1);
        let sectors_per_track = 63u16;
        let heads = if total_sectors <= 1024 * 16 * 63 { 16u16 } else { 255u16 };
        let cylinders = ((total_sectors / (heads as u64 * sectors_per_track as u64)).min(16383) as u16).max(1);

        // Word 1, 3, 6: Geometría LCHS
        buf[2..4].copy_from_slice(&cylinders.to_le_bytes());
        buf[6..8].copy_from_slice(&heads.to_le_bytes());
        buf[12..14].copy_from_slice(&sectors_per_track.to_le_bytes());

        copy_ata_string(&mut buf[20..40], b"TWO555-SATA0    ");
        copy_ata_string(&mut buf[46..54], b"01.00   ");
        copy_ata_string(&mut buf[54..94], b"Two Five Five Virtual SATA HDD          ");

        // Word 47: Sectores múltiples por interrupción (16)
        buf[94] = 16; buf[95] = 0x80;
        // Word 48: dword I/O soportado
        buf[96] = 0x01; buf[97] = 0x00;
        // Word 49: DMA, LBA, IORDY (0x0B00)
        buf[98] = 0x00; buf[99] = 0x0B;
        // Word 50: Standby timer standard (0x4000)
        buf[100] = 0x00; buf[101] = 0x40;
        // Word 51 & 52: PIO y DMA transfer cycle
        buf[102] = 240; buf[103] = 0;
        buf[104] = 240; buf[105] = 0;

        // Word 53: Palabras 54-58, 64-70, 88 son válidas
        buf[106] = 0x07; buf[107] = 0x00;

        // Words 54-58: Geometría CHS Actual y Capacidad en Sectores (Crucial para Windows 7)
        buf[108..110].copy_from_slice(&cylinders.to_le_bytes());
        buf[110..112].copy_from_slice(&heads.to_le_bytes());
        buf[112..114].copy_from_slice(&sectors_per_track.to_le_bytes());
        let chs_capacity = (cylinders as u32) * (heads as u32) * (sectors_per_track as u32);
        buf[114..118].copy_from_slice(&chs_capacity.to_le_bytes());

        // Word 59: Múltiples sectores habilitados (0x0110)
        buf[118] = 16; buf[119] = 0x01;

        // Words 60-61: LBA28 Sectores Totales (máximo 0x0FFFFFFF)
        let lba28 = (total_sectors.min(0x0FFF_FFFF)) as u32;
        buf[120..124].copy_from_slice(&lba28.to_le_bytes());

        // Word 63: Multiword DMA modos 0, 1, 2
        buf[126] = 0x07; buf[127] = 0x07;
        // Word 64: Modos PIO avanzados 3 y 4
        buf[128] = 0x03; buf[129] = 0x00;
        // Words 65-68: Tiempos de ciclo mínimos (120 ns)
        buf[130] = 120; buf[132] = 120; buf[134] = 120; buf[136] = 120;

        // Words 75 & 76: Específico de Serial ATA (DevAHCI.cpp:2875-2876)
        buf[150] = 31; buf[151] = 0; // 32 slots de comando soportados
        buf[152] = 0x04; buf[153] = 0x01; // Gen2 3.0 Gbps + NCQ soportado

        // Word 80: Versión mayor ATA/ATAPI-4..8
        buf[160] = 0xF0; buf[161] = 0x01;
        // Word 81: Cumplimiento ATA/ATAPI-8 ACS
        buf[162] = 0x28; buf[163] = 0x00;

        // Word 82: Conjunto de Comandos Soportados (Power Mgmt, Write Cache, Look-Ahead)
        buf[164] = 0x78; buf[165] = 0x00;

        // Word 83: Soporte LBA48 y FLUSH CACHE (Bit 14=1, 13=FlushExt, 12=Flush, 10=LBA48 -> 0x7400)
        buf[166] = 0x00; buf[167] = 0x74;

        // Word 84: Bit 14 en 1
        buf[168] = 0x00; buf[169] = 0x40;

        // Word 85: Comandos habilitados por defecto
        buf[170] = 0x78; buf[171] = 0x00;

        // Word 86: LBA48 y FLUSH CACHE habilitados (0x3400)
        buf[172] = 0x00; buf[173] = 0x34;

        // Word 87: Bit 14 en 1
        buf[174] = 0x00; buf[175] = 0x40;

        // Word 88: Ultra DMA modos 0..6 soportados y activos
        buf[176] = 0x7F; buf[177] = 0x7F;

        // Word 93: Resultado de hardware reset (0x6001)
        buf[186] = 0x01; buf[187] = 0x60;

        // Words 100-103: Sectores Totales LBA48 (64-bit LE)
        buf[200..208].copy_from_slice(&total_sectors.to_le_bytes());

        // Word 255: Checksum de Integridad ATA (DevAHCI.cpp:2878-2879)
        let sum: u8 = buf[..510].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        let csum = 0u8.wrapping_sub(sum);
        buf[510] = 0xA5;
        buf[511] = csum;

        buf
    }

    pub fn generate_atapi_identify(&self) -> [u8; 512] {
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
        pkt[98] = 0x00; pkt[99] = 0x0B;
        // Word 53: Palabras 64-70 y 88 son válidas (0x0006)
        pkt[106] = 0x06; pkt[107] = 0x00;
        // Word 63: Multiword DMA modos 0, 1, 2 soportados y activos (0x0007)
        pkt[126] = 0x07; pkt[127] = 0x00;
        // Word 64: Modos PIO avanzados 3 y 4 (0x0003)
        pkt[128] = 0x03; pkt[129] = 0x00;
        // Words 65..68: Tiempos mínimos de ciclo (120 ns)
        pkt[130] = 120; pkt[132] = 120; pkt[134] = 120; pkt[136] = 120;

        // Words 75 & 76: Específico de Serial ATA (32 slots y SATA Gen2 3.0 Gbps + NCQ)
        pkt[150] = 31; pkt[151] = 0;
        pkt[152] = 0x04; pkt[153] = 0x01;

        // Word 80: Versión mayor ATA/ATAPI-4..6 (0x007E)
        pkt[160] = 0x7E; pkt[161] = 0x00;
        // Word 81: Versión menor (0x0022)
        pkt[162] = 0x22; pkt[163] = 0x00;

        // Words 82..87: Command sets (Device reset, Power mgmt)
        pkt[164] = 0x10; pkt[165] = 0x02; // Word 82: 0x0210
        pkt[166] = 0x00; pkt[167] = 0x40; // Word 83: 0x4000
        pkt[168] = 0x00; pkt[169] = 0x40; // Word 84: 0x4000
        pkt[170] = 0x10; pkt[171] = 0x02; // Word 85: 0x0210
        pkt[172] = 0x00; pkt[173] = 0x40; // Word 86: 0x4000
        pkt[174] = 0x00; pkt[175] = 0x40; // Word 87: 0x4000

        // Word 88: Ultra DMA modos 0..5 soportados y modo 5 activo (0x207F)
        pkt[176] = 0x7F; pkt[177] = 0x20;

        // Words 100..103: Total LBA (64-bit)
        let total_lba = (self.iso_size / CD_SECTOR_SIZE as u64).max(1);
        pkt[200..208].copy_from_slice(&total_lba.to_le_bytes());

        // Word 255: Checksum de integridad ATA (DevAHCI.cpp)
        let sum: u8 = pkt[..510].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        let csum = 0u8.wrapping_sub(sum);
        pkt[510] = 0xA5;
        pkt[511] = csum;

        pkt
    }

    // ─── Ejecución de Comandos SCSI para ATAPI CD-ROM ──────────────

    pub fn execute_scsi(&mut self, cdb: &[u8; 16]) -> Option<Vec<u8>> {
        let opcode = cdb[0];
        let total_sectors = (self.iso_size / CD_SECTOR_SIZE as u64).max(1) as u32;
        let is_dvd = self.iso_size > 700 * 1024 * 1024;

        match opcode {
            0x00 => {
                // TEST UNIT READY
                Some(Vec::new())
            }
            0x03 => {
                // REQUEST SENSE
                let alloc = cdb[4] as usize;
                let mut sense = vec![0u8; 18];
                sense[0] = 0x70; // Current errors
                sense[7] = 10;
                let len = alloc.min(18);
                Some(sense[..len].to_vec())
            }
            0x08 => {
                // READ (6)
                let lba = (((cdb[1] & 0x1F) as u32) << 16) | ((cdb[2] as u32) << 8) | (cdb[3] as u32);
                let mut count = cdb[4] as u16;
                if count == 0 { count = 256; }
                let total_bytes = (count as usize) * CD_SECTOR_SIZE;
                let mut buf = vec![0u8; total_bytes];
                self.read_iso_sectors(lba, count, &mut buf);
                Some(buf)
            }
            0x12 => {
                // INQUIRY (Standard & EVPD)
                let evpd = (cdb[1] & 0x01) != 0;
                let page_code = cdb[2];
                let alloc = if cdb[4] == 0 && cdb[3] != 0 {
                    cdb[3] as usize
                } else {
                    cdb[4] as usize
                };
                let resp = if evpd {
                    match page_code {
                        0x00 => {
                            // Supported VPD Pages
                            vec![0x05, 0x00, 0x00, 0x03, 0x00, 0x80, 0x83]
                        }
                        0x80 => {
                            // Unit Serial Number
                            let serial = b"VB1234-5678";
                            let mut p = vec![0x05, 0x80, 0x00, serial.len() as u8];
                            p.extend_from_slice(serial);
                            p
                        }
                        0x83 => {
                            // Device Identification
                            let mut p = vec![0x05, 0x83, 0x00, 0x14, 0x02, 0x01, 0x00, 0x10];
                            p.extend_from_slice(b"VBOX    VB123456");
                            p
                        }
                        _ => {
                            vec![0x05, page_code, 0x00, 0x00]
                        }
                    }
                } else {
                    let mut inq = vec![0u8; 96];
                    inq[0] = 0x05; // CD-ROM
                    inq[1] = 0x80; // Removable
                    inq[2] = 0x02; // SPC-2
                    inq[3] = 0x02;
                    inq[4] = 91;   // Additional length
                    inq[8..16].copy_from_slice(b"VBOX    ");
                    inq[16..32].copy_from_slice(b"CD-ROM          ");
                    inq[32..36].copy_from_slice(b"1.0 ");
                    inq
                };
                let send_len = alloc.min(resp.len());
                Some(resp[..send_len].to_vec())
            }
            0x1A => {
                // MODE SENSE (6)
                let alloc = cdb[4] as usize;
                let mut ms = vec![0u8; 36];
                ms[0] = 35;
                ms[2] = 0x80; // Write protected
                ms[4] = 0x2A; // Page 0x2A (CD capabilities)
                ms[5] = 0x1E;
                let len = alloc.min(36);
                Some(ms[..len].to_vec())
            }
            0x1B | 0x1E | 0x2B | 0x35 => {
                // START/STOP UNIT, PREVENT/ALLOW, SEEK (10), SYNCHRONIZE CACHE
                Some(Vec::new())
            }
            0x23 => {
                // READ FORMAT CAPACITIES
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let mut cap = vec![0u8; 12];
                cap[3] = 8; // Capacity list length
                cap[4..8].copy_from_slice(&total_sectors.to_be_bytes());
                cap[8] = 0x02; // Formatted media
                cap[9..12].copy_from_slice(&[0x00, 0x08, 0x00]); // 2048 sector size
                let len = alloc.min(12);
                Some(cap[..len].to_vec())
            }
            0x25 => {
                // READ CAPACITY (10)
                let last_lba = total_sectors.saturating_sub(1);
                let block_size = CD_SECTOR_SIZE as u32;
                let mut cap = vec![0u8; 8];
                cap[0..4].copy_from_slice(&last_lba.to_be_bytes());
                cap[4..8].copy_from_slice(&block_size.to_be_bytes());
                Some(cap)
            }
            0x28 | 0xA8 => {
                // READ (10) / READ (12)
                let (lba, count) = if opcode == 0x28 {
                    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
                    let count = u16::from_be_bytes([cdb[7], cdb[8]]);
                    (lba, count)
                } else {
                    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
                    let count = (u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]) & 0xFFFF) as u16;
                    (lba, count)
                };
                let total_bytes = (count as usize) * CD_SECTOR_SIZE;
                let mut buf = vec![0u8; total_bytes];
                self.read_iso_sectors(lba, count, &mut buf);
                Some(buf)
            }
            0x43 => {
                // READ TOC (con soporte MSF y formato multi-sesión)
                let msf = (cdb[1] & 0x02) != 0;
                let format = cdb[2] & 0x0F;
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
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
                        toc[2] = 1; // First track
                        toc[3] = 1; // Last track
                        // Track 1
                        toc[4] = 0;
                        toc[5] = 0x14; // ADR=1, Data track
                        toc[6] = 1;
                        toc[7] = 0;
                        if msf {
                            toc[8..12].copy_from_slice(&lba_to_msf(150));
                        } else {
                            toc[8..12].copy_from_slice(&0u32.to_be_bytes());
                        }
                        // Lead-out track 0xAA
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
                        // Multi-session info: Header (4 bytes) + Track info (8 bytes) = 12 bytes
                        let mut toc = vec![0u8; 12];
                        let len: u16 = 10;
                        toc[0..2].copy_from_slice(&len.to_be_bytes());
                        toc[2] = 1; // First session
                        toc[3] = 1; // Last session
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
                Some(buf[..send_len].to_vec())
            }
            0x46 => {
                // GET CONFIGURATION (Soporte completo MMC Feature Descriptors)
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let cur_profile: u16 = if is_dvd { 0x0010 } else { 0x0008 };
                let mut conf = vec![0u8; 88];
                let data_len: u32 = 84; // 88 - 4
                conf[0..4].copy_from_slice(&data_len.to_be_bytes());
                conf[6..8].copy_from_slice(&cur_profile.to_be_bytes());

                let mut off = 8;
                // Feature 0000h: Profile List
                conf[off..off+2].copy_from_slice(&0x0000u16.to_be_bytes());
                conf[off+2] = 0x03; // Version 0, persistent, current
                conf[off+3] = 8;    // 8 additional bytes
                conf[off+4..off+6].copy_from_slice(&0x0010u16.to_be_bytes()); // DVD-ROM
                conf[off+6] = if is_dvd { 1 } else { 0 }; // Current
                conf[off+8..off+10].copy_from_slice(&0x0008u16.to_be_bytes()); // CD-ROM
                conf[off+10] = if is_dvd { 0 } else { 1 }; // Current
                off += 12;

                // Feature 0001h: Core Feature
                conf[off..off+2].copy_from_slice(&0x0001u16.to_be_bytes());
                conf[off+2] = (2 << 2) | 0x03; // Version 2, persistent, current
                conf[off+3] = 8;
                conf[off+4..off+8].copy_from_slice(&2u32.to_be_bytes()); // Physical ATAPI
                conf[off+8] = 1; // DBE
                off += 12;

                // Feature 0002h: Morphing
                conf[off..off+2].copy_from_slice(&0x0002u16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 4;
                conf[off+4] = 2; // ASYNCH
                off += 8;

                // Feature 0003h: Removable Medium
                conf[off..off+2].copy_from_slice(&0x0003u16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 4;
                conf[off+4] = 0x20; // Lockable
                off += 8;

                // Feature 0010h: Random Readable
                conf[off..off+2].copy_from_slice(&0x0010u16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 8;
                conf[off+4..off+8].copy_from_slice(&2048u32.to_be_bytes()); // Block size 2048
                conf[off+8..off+10].copy_from_slice(&1u16.to_be_bytes());   // Blocking 1
                off += 12;

                // Feature 001Eh: CD Read
                conf[off..off+2].copy_from_slice(&0x001Eu16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 4;
                conf[off+4] = 0x03; // CD-Text, DAP
                off += 8;

                // Feature 001Fh: DVD Read
                conf[off..off+2].copy_from_slice(&0x001Fu16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 4;
                off += 8;

                // Feature 0100h: Power Management
                conf[off..off+2].copy_from_slice(&0x0100u16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 0;
                off += 4;

                // Feature 0105h: Timeout
                conf[off..off+2].copy_from_slice(&0x0105u16.to_be_bytes());
                conf[off+2] = 0x03;
                conf[off+3] = 4;
                off += 8;

                let actual_len = off.min(conf.len());
                let send_len = alloc.min(actual_len);
                Some(conf[..send_len].to_vec())
            }
            0x4A => {
                // GET EVENT STATUS NOTIFICATION
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let mut ev = vec![0u8; 8];
                let len: u16 = 4;
                ev[0..2].copy_from_slice(&len.to_be_bytes());
                ev[2] = 0x84; // NEA | class 4 (Media)
                ev[4] = 0x02; // Media present
                let send_len = alloc.min(8);
                Some(ev[..send_len].to_vec())
            }
            0x51 => {
                // READ DISC INFORMATION
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let mut info = vec![0u8; 34];
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
                Some(info[..send_len].to_vec())
            }
            0x52 => {
                // READ TRACK INFORMATION
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let mut info = vec![0u8; 36];
                let len: u16 = 34;
                info[0..2].copy_from_slice(&len.to_be_bytes());
                info[2] = 1;
                info[3] = 1;
                info[5] = 0x04;
                info[6] = 0x01;
                info[8..12].copy_from_slice(&0u32.to_be_bytes());
                info[24..28].copy_from_slice(&total_sectors.to_be_bytes());
                let send_len = alloc.min(36);
                Some(info[..send_len].to_vec())
            }
            0x5A => {
                // MODE SENSE (10)
                let alloc = u16::from_be_bytes([cdb[7], cdb[8]]) as usize;
                let page_code = cdb[2] & 0x3F;
                let mut ms = vec![0u8; 44];
                let mode_len: u16 = 42;
                ms[0..2].copy_from_slice(&mode_len.to_be_bytes());
                ms[3] = 0x80; // Write protected
                ms[8] = 0x2A; // Page 0x2A
                ms[9] = 0x1E;
                ms[16] = 0x02; ms[17] = 0xC0; // max read speed (4x)
                ms[22] = 0x02; ms[23] = 0xC0;
                let len = alloc.min(if page_code == 0x2A || page_code == 0x3F { 44 } else { 8 });
                Some(ms[..len].to_vec())
            }
            0xAD => {
                // READ DVD STRUCTURE (Crucial para Windows 7 / WinPE DVD-ROM detection)
                let format = cdb[7];
                let alloc = u16::from_be_bytes([cdb[8], cdb[9]]) as usize;
                match format {
                    0x00 => {
                        // Physical format information (MMC-2 / VirtualBox vscsiLunMmcReadDvdStructure)
                        let mut buf = vec![0u8; 2048 + 4];
                        let data_len = 2048u16 + 2u16;
                        buf[0..2].copy_from_slice(&data_len.to_be_bytes());
                        buf[2] = 0;
                        buf[3] = 0;
                        buf[4] = 0x01; // DVD-ROM, part version 1
                        buf[5] = 0x0F; // 120mm disc, minimum rate unspecified
                        buf[6] = 0x01; // One layer, read-only
                        buf[7] = 0x00; // Default densities

                        // Starting PSN: 0x00030000 per MMC spec
                        buf[8..12].copy_from_slice(&0x0003_0000u32.to_be_bytes());
                        let end_psn = 0x0003_0000u32.saturating_add(total_sectors.saturating_sub(1));
                        buf[12..16].copy_from_slice(&end_psn.to_be_bytes());
                        buf[16..20].copy_from_slice(&end_psn.to_be_bytes());

                        let send_len = alloc.min(buf.len());
                        Some(buf[..send_len].to_vec())
                    }
                    0x01 => {
                        // DVD copyright information
                        let mut buf = vec![0u8; 8];
                        let data_len = 4u16 + 2u16;
                        buf[0..2].copy_from_slice(&data_len.to_be_bytes());
                        buf[2] = 0;
                        buf[3] = 0;
                        buf[4] = 0; // No copyright data
                        buf[5] = 0; // No region restrictions
                        let send_len = alloc.min(8);
                        Some(buf[..send_len].to_vec())
                    }
                    _ => {
                        let buf = vec![0u8; 4];
                        let send_len = alloc.min(4);
                        Some(buf[..send_len].to_vec())
                    }
                }
            }
            0xBD => {
                // MECHANISM STATUS
                let alloc = u16::from_be_bytes([cdb[8], cdb[9]]) as usize;
                let mut resp = vec![0u8; 8];
                resp[5] = 1; // 1 slot
                let send_len = alloc.min(8);
                Some(resp[..send_len].to_vec())
            }
            0xBE => {
                // READ CD
                let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
                let count = (((cdb[6] as u32) << 16) | ((cdb[7] as u32) << 8) | (cdb[8] as u32)) as u16;
                let total_bytes = (count as usize) * CD_SECTOR_SIZE;
                let mut buf = vec![0u8; total_bytes];
                self.read_iso_sectors(lba, count, &mut buf);
                Some(buf)
            }
            _ => {
                Some(Vec::new())
            }
        }
    }
}

// ─── Helpers de Copia DMA hacia/desde PRD Tables ──────────────────

fn copy_to_prd(mem: &GuestMemory, prds: &[(u64, usize)], src: &[u8]) -> usize {
    let mut src_off = 0;
    for &(dba, len) in prds {
        if src_off >= src.len() {
            break;
        }
        let chunk = (src.len() - src_off).min(len);
        let _ = mem.copy_to(dba as usize, &src[src_off..src_off + chunk]);
        src_off += chunk;
    }
    src_off
}

fn copy_from_prd(mem: &GuestMemory, prds: &[(u64, usize)], dst: &mut [u8]) -> usize {
    let mut dst_off = 0;
    for &(dba, len) in prds {
        if dst_off >= dst.len() {
            break;
        }
        let chunk = (dst.len() - dst_off).min(len);
        let read_bytes = mem.copy_from(dba as usize, &mut dst[dst_off..dst_off + chunk]);
        dst_off += read_bytes;
        if read_bytes < chunk {
            break;
        }
    }
    dst_off
}

// ─── Helpers de Decodificación FIS H2D ─────────────────────────────

fn parse_lba(cfis: &[u8], is_lba48: bool) -> u64 {
    if is_lba48 {
        let l0 = cfis[4] as u64;
        let l1 = cfis[5] as u64;
        let l2 = cfis[6] as u64;
        let l3 = cfis[8] as u64;
        let l4 = cfis[9] as u64;
        let l5 = cfis[10] as u64;
        l0 | (l1 << 8) | (l2 << 16) | (l3 << 24) | (l4 << 32) | (l5 << 40)
    } else {
        let l0 = cfis[4] as u64;
        let l1 = cfis[5] as u64;
        let l2 = cfis[6] as u64;
        let l3 = (cfis[7] & 0x0F) as u64;
        l0 | (l1 << 8) | (l2 << 16) | (l3 << 24)
    }
}

fn parse_sector_count(cfis: &[u8], is_lba48: bool) -> u32 {
    if is_lba48 {
        let low = cfis[12] as u32;
        let high = cfis[13] as u32;
        let cnt = low | (high << 8);
        if cnt == 0 { 65536 } else { cnt }
    } else {
        let cnt = cfis[12] as u32;
        if cnt == 0 { 256 } else { cnt }
    }
}

fn parse_ncq_lba(cfis: &[u8]) -> u64 {
    let l0 = cfis[4] as u64;
    let l1 = cfis[5] as u64;
    let l2 = cfis[6] as u64;
    let l3 = cfis[8] as u64;
    let l4 = cfis[9] as u64;
    let l5 = cfis[10] as u64;
    l0 | (l1 << 8) | (l2 << 16) | (l3 << 24) | (l4 << 32) | (l5 << 40)
}

fn parse_ncq_sector_count(cfis: &[u8]) -> u32 {
    let low = cfis[3] as u32;
    let high = cfis[11] as u32;
    let cnt = low | (high << 8);
    if cnt == 0 { 65536 } else { cnt }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_ahci_initial_registers() {
        let ctrl = AhciController::new();
        // Global
        assert_eq!(ctrl.read_reg_u32(HOST_CAP), AHCI_CAP_DEFAULT);
        assert_eq!(ctrl.read_reg_u32(HOST_CTL), GHC_AE);
        assert_eq!(ctrl.read_reg_u32(HOST_PORTS_IMPL), 0x03);
        assert_eq!(ctrl.read_reg_u32(HOST_VERSION), 0x0001_0300);

        // Port 0 (Disk)
        let p0_sig = ctrl.read_reg_u32(0x100 + PORT_SIG);
        assert_eq!(p0_sig, PORT_SIG_ATA);
        let p0_ssts = ctrl.read_reg_u32(0x100 + PORT_SSTS);
        assert_eq!(p0_ssts, PORT_SSTS_PRESENT_GEN3);

        // Port 1 (ATAPI CD-ROM)
        let p1_sig = ctrl.read_reg_u32(0x180 + PORT_SIG);
        assert_eq!(p1_sig, PORT_SIG_ATAPI);
        let p1_cmd = ctrl.read_reg_u32(0x180 + PORT_CMD);
        assert_ne!(p1_cmd & PORT_CMD_ATAPI, 0);
    }

    #[test]
    fn test_ahci_port_command_issue_identify() {
        let mut ctrl = AhciController::new();
        ctrl.disk_size = 10 * 1024 * 1024; // 10 MiB

        // Crear una región de memoria física de prueba
        let mut raw_mem = vec![0u8; 64 * 1024]; // 64 KiB
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let clb = 0x1000u64; // 4 KiB
        let ctba = 0x2000u64; // 8 KiB
        let prd_data_buf = 0x3000u64; // 12 KiB
        let fb = 0x4000u64;

        // Configurar puerto 0
        ctrl.write_reg_u32(0x100 + PORT_CLB, clb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_FB, fb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_IE, PORT_IS_DHRS, Some(&guest_mem));
        ctrl.write_reg_u32(HOST_CTL, GHC_AE | GHC_IE, Some(&guest_mem));

        // Preparar cabecera de comando en slot 0 (32 bytes en `clb`)
        let dw0: u32 = 5 | (1 << 16); // CFL=5 DWORDs, PRDTL=1
        guest_mem.write_u32(clb as usize, dw0);
        guest_mem.write_u32((clb + 4) as usize, 0); // PRDBC
        guest_mem.write_u32((clb + 8) as usize, ctba as u32);
        guest_mem.write_u32((clb + 12) as usize, (ctba >> 32) as u32);

        // Preparar Command FIS (H2D, Identify 0xEC) en `ctba`
        guest_mem.write_u8(ctba as usize, 0x27);
        guest_mem.write_u8((ctba + 1) as usize, 0x80); // Command
        guest_mem.write_u8((ctba + 2) as usize, 0xEC); // ATA IDENTIFY DEVICE

        // Preparar PRD table en `ctba + 0x80`
        guest_mem.write_u32((ctba + 0x80) as usize, prd_data_buf as u32);
        guest_mem.write_u32((ctba + 0x84) as usize, (prd_data_buf >> 32) as u32);
        guest_mem.write_u32((ctba + 0x8C) as usize, 511); // Byte count - 1 = 511 (512 bytes)

        // Activar comando escribiendo CI = 1
        ctrl.write_reg_u32(0x100 + PORT_CI, 1, Some(&guest_mem));

        // Verificar que el comando finalizó: CI limpio, IS marcado
        let ci = ctrl.read_reg_u32(0x100 + PORT_CI);
        assert_eq!(ci, 0);

        let p_is = ctrl.read_reg_u32(0x100 + PORT_IS);
        assert_ne!(p_is & PORT_IS_DHRS, 0);

        // Verificar interrupción levantada
        assert!(ctrl.is_irq_asserted());

        // Verificar que los 512 bytes de IDENTIFY se copiaron a `prd_data_buf`
        let id_byte0 = guest_mem.read_u8(prd_data_buf as usize);
        assert_eq!(id_byte0, 0x40);

        // Verificar que el FIS D2H se escribió en `fb + 0x40`
        let fis_type = guest_mem.read_u8((fb + 0x40) as usize);
        assert_eq!(fis_type, 0x34);
    }

    #[test]
    fn test_ahci_port_command_issue_read_write_dma() {
        let mut ctrl = AhciController::new();
        ctrl.ram_disk = Some(vec![0u8; 1024 * 1024]); // 1 MiB de disco RAM

        let mut raw_mem = vec![0u8; 64 * 1024];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let clb = 0x1000u64;
        let ctba = 0x2000u64;
        let prd_data_buf = 0x3000u64;
        let fb = 0x4000u64;

        ctrl.write_reg_u32(0x100 + PORT_CLB, clb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_FB, fb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(HOST_CTL, GHC_AE | GHC_IE, Some(&guest_mem));

        // 1. WRITE DMA EXT (0x35) en LBA 5, 1 sector
        let pattern = [0x5Au8; 512];
        guest_mem.copy_to(prd_data_buf as usize, &pattern);

        let dw0: u32 = 5 | (1 << 6) | (1 << 16); // CFL=5, W=1, PRDTL=1
        guest_mem.write_u32(clb as usize, dw0);
        guest_mem.write_u32((clb + 4) as usize, 0);
        guest_mem.write_u32((clb + 8) as usize, ctba as u32);
        guest_mem.write_u32((clb + 12) as usize, (ctba >> 32) as u32);

        // FIS H2D: 0x27, cmd 0x35, LBA=5, count=1
        guest_mem.write_u8(ctba as usize, 0x27);
        guest_mem.write_u8((ctba + 1) as usize, 0x80);
        guest_mem.write_u8((ctba + 2) as usize, 0x35); // WRITE DMA EXT
        guest_mem.write_u8((ctba + 4) as usize, 5);    // LBA low = 5
        guest_mem.write_u8((ctba + 12) as usize, 1);   // Sector count = 1

        guest_mem.write_u32((ctba + 0x80) as usize, prd_data_buf as u32);
        guest_mem.write_u32((ctba + 0x84) as usize, (prd_data_buf >> 32) as u32);
        guest_mem.write_u32((ctba + 0x8C) as usize, 511);

        ctrl.write_reg_u32(0x100 + PORT_CI, 1, Some(&guest_mem));

        // Verificar que los datos se escribieron al disco
        let ram = ctrl.ram_disk.as_ref().unwrap();
        assert_eq!(&ram[5 * 512..6 * 512], &pattern[..]);

        // 2. READ DMA EXT (0x25) desde LBA 5 a otro buffer
        let read_buf = 0x5000u64;
        let dw0_read: u32 = 5 | (1 << 16); // CFL=5, W=0, PRDTL=1
        guest_mem.write_u32((clb + 32) as usize, dw0_read);
        guest_mem.write_u32((clb + 36) as usize, 0);
        guest_mem.write_u32((clb + 40) as usize, (ctba + 0x100) as u32);
        guest_mem.write_u32((clb + 44) as usize, 0);

        let ctba_r = ctba + 0x100;
        guest_mem.write_u8(ctba_r as usize, 0x27);
        guest_mem.write_u8((ctba_r + 1) as usize, 0x80);
        guest_mem.write_u8((ctba_r + 2) as usize, 0x25); // READ DMA EXT
        guest_mem.write_u8((ctba_r + 4) as usize, 5);    // LBA low = 5
        guest_mem.write_u8((ctba_r + 12) as usize, 1);   // Sector count = 1

        guest_mem.write_u32((ctba_r + 0x80) as usize, read_buf as u32);
        guest_mem.write_u32((ctba_r + 0x84) as usize, 0);
        guest_mem.write_u32((ctba_r + 0x8C) as usize, 511);

        ctrl.write_reg_u32(0x100 + PORT_CI, 2, Some(&guest_mem));

        let mut readback = [0u8; 512];
        guest_mem.copy_from(read_buf as usize, &mut readback);
        assert_eq!(&readback[..], &pattern[..]);
    }

    #[test]
    fn test_ahci_port1_atapi_scsi_inquiry_and_read_capacity() {
        let mut ctrl = AhciController::new();
        ctrl.iso_size = 50 * 1024 * 1024; // 50 MiB

        let mut raw_mem = vec![0u8; 64 * 1024];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let clb = 0x1000u64;
        let ctba = 0x2000u64;
        let prd_data_buf = 0x3000u64;
        let fb = 0x4000u64;

        // Configurar puerto 1 (ATAPI CD-ROM)
        ctrl.write_reg_u32(0x180 + PORT_CLB, clb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x180 + PORT_FB, fb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(HOST_CTL, GHC_AE | GHC_IE, Some(&guest_mem));

        // 1. SCSI INQUIRY (0x12)
        let dw0: u32 = 5 | (1 << 5) | (1 << 16); // CFL=5, ATAPI=1, PRDTL=1
        guest_mem.write_u32(clb as usize, dw0);
        guest_mem.write_u32((clb + 4) as usize, 0);
        guest_mem.write_u32((clb + 8) as usize, ctba as u32);
        guest_mem.write_u32((clb + 12) as usize, 0);

        // Command FIS: 0x27, command 0xA0 (PACKET)
        guest_mem.write_u8(ctba as usize, 0x27);
        guest_mem.write_u8((ctba + 1) as usize, 0x80);
        guest_mem.write_u8((ctba + 2) as usize, 0xA0); // ATA PACKET

        // ACMD en ctba + 0x40: SCSI INQUIRY
        guest_mem.write_u8((ctba + 0x40) as usize, 0x12); // INQUIRY
        guest_mem.write_u8((ctba + 0x44) as usize, 96);   // Alloc len = 96

        guest_mem.write_u32((ctba + 0x80) as usize, prd_data_buf as u32);
        guest_mem.write_u32((ctba + 0x84) as usize, 0);
        guest_mem.write_u32((ctba + 0x8C) as usize, 95); // 96 bytes

        ctrl.write_reg_u32(0x180 + PORT_CI, 1, Some(&guest_mem));

        let mut inq_resp = [0u8; 96];
        guest_mem.copy_from(prd_data_buf as usize, &mut inq_resp);
        assert_eq!(inq_resp[0], 0x05); // CD-ROM
        assert_eq!(&inq_resp[8..16], b"VBOX    ");
        assert_eq!(&inq_resp[16..24], b"CD-ROM  ");

        // 2. SCSI READ CAPACITY 10 (0x25)
        guest_mem.write_u8((ctba + 0x40) as usize, 0x25); // READ CAPACITY 10
        guest_mem.write_u32((ctba + 0x8C) as usize, 7);   // 8 bytes

        ctrl.write_reg_u32(0x180 + PORT_CI, 1, Some(&guest_mem));

        let mut cap_resp = [0u8; 8];
        guest_mem.copy_from(prd_data_buf as usize, &mut cap_resp);
        let block_size = u32::from_be_bytes([cap_resp[4], cap_resp[5], cap_resp[6], cap_resp[7]]);
        assert_eq!(block_size, CD_SECTOR_SIZE as u32);
    }

    #[test]
    fn test_ahci_comreset_sequence() {
        let mut ctrl = AhciController::new();
        let mut raw_mem = vec![0u8; 64 * 1024];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let fb0 = 0x4000u64;
        let fb1 = 0x8000u64;

        // Configurar puerto 0 (Disco ATA)
        ctrl.write_reg_u32(0x100 + PORT_FB, fb0 as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_CMD, PORT_CMD_FRE, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_IE, PORT_IS_PCS, Some(&guest_mem));
        ctrl.write_reg_u32(HOST_CTL, GHC_AE | GHC_IE, Some(&guest_mem));

        // Ejecutar secuencia COMRESET: DET = 1 luego DET = 0
        ctrl.write_reg_u32(0x100 + PORT_SCTL, 0x301, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_SCTL, 0x300, Some(&guest_mem));

        // Verificar registros tras COMRESET en Puerto 0
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_SSTS), 0x123);
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_SERR), 0);
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_SIG), PORT_SIG_ATA);
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_TFD), 0x150);

        // Verificar FIS D2H inicial posteado en fb + 0x40
        let fis_type = guest_mem.read_u8((fb0 + 0x40) as usize);
        let fis_status = guest_mem.read_u8((fb0 + 0x42) as usize);
        let fis_error = guest_mem.read_u8((fb0 + 0x43) as usize);
        let fis_lba_low = guest_mem.read_u8((fb0 + 0x44) as usize);
        assert_eq!(fis_type, 0x34);
        assert_eq!(fis_status, 0x50);
        assert_eq!(fis_error, 0x01);
        assert_eq!(fis_lba_low, 0x01);

        // Verificar interrupción PCS
        let p0_is = ctrl.read_reg_u32(0x100 + PORT_IS);
        assert_ne!(p0_is & PORT_IS_PCS, 0);
        assert!(ctrl.is_irq_asserted());
        assert_ne!(ctrl.read_reg_u32(HOST_IRQ_STAT) & 1, 0);

        // Configurar puerto 1 (CD-ROM ATAPI)
        ctrl.write_reg_u32(0x180 + PORT_FB, fb1 as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x180 + PORT_CMD, PORT_CMD_FRE | PORT_CMD_ATAPI, Some(&guest_mem));
        ctrl.write_reg_u32(0x180 + PORT_IE, PORT_IS_PCS, Some(&guest_mem));

        // COMRESET en Puerto 1
        ctrl.write_reg_u32(0x180 + PORT_SCTL, 1, Some(&guest_mem));
        ctrl.write_reg_u32(0x180 + PORT_SCTL, 0, Some(&guest_mem));

        assert_eq!(ctrl.read_reg_u32(0x180 + PORT_SSTS), 0x123);
        assert_eq!(ctrl.read_reg_u32(0x180 + PORT_SIG), PORT_SIG_ATAPI);
        assert_eq!(ctrl.read_reg_u32(0x180 + PORT_TFD), 0x150);

        let p1_fis_lba_mid = guest_mem.read_u8((fb1 + 0x45) as usize);
        let p1_fis_lba_high = guest_mem.read_u8((fb1 + 0x46) as usize);
        assert_eq!(p1_fis_lba_mid, 0x14);
        assert_eq!(p1_fis_lba_high, 0xEB);

        // Probar COMRESET en puerto sin unidad
        ctrl.ports[0].present = false;
        ctrl.write_reg_u32(0x100 + PORT_SCTL, 1, Some(&guest_mem));
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_SSTS), 0);
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_TFD), 0x7F);
    }

    #[test]
    fn test_ahci_initial_and_reset_tfd_values() {
        let p_present = AhciPort::new(0, false, true);
        assert_eq!(p_present.tfd, 0x150);

        let p_not_present = AhciPort::new(0, false, false);
        assert_eq!(p_not_present.tfd, 0x7F);

        let mut p = AhciPort::new(0, false, true);
        p.tfd = 0x50;
        p.reset();
        assert_eq!(p.tfd, 0x150);

        let mut p_none = AhciPort::new(0, false, false);
        p_none.reset();
        assert_eq!(p_none.tfd, 0x7F);

        let mut ctrl = AhciController::new();
        ctrl.ports[0].tfd = 0x50;
        ctrl.reset();
        assert_eq!(ctrl.ports[0].tfd, 0x150);
    }

    #[test]
    fn test_ahci_lba48_65536_sector_count() {
        let mut cfis = [0u8; 64];

        // LBA48: count = 0 -> 65536 sectores
        cfis[12] = 0;
        cfis[13] = 0;
        assert_eq!(parse_sector_count(&cfis, true), 65536u32);
        assert_eq!(parse_ncq_sector_count(&cfis), 65536u32);

        // LBA48: count = 10 -> 10 sectores
        cfis[12] = 10;
        cfis[13] = 0;
        assert_eq!(parse_sector_count(&cfis, true), 10u32);

        // NCQ: Sector Count se ubica en Features (cfis[3] low, cfis[11] high)
        cfis[3] = 10;
        cfis[11] = 0;
        assert_eq!(parse_ncq_sector_count(&cfis), 10u32);

        // LBA28: count = 0 -> 256 sectores
        cfis[12] = 0;
        assert_eq!(parse_sector_count(&cfis, false), 256u32);

        // LBA28: count = 10 -> 10 sectores
        cfis[12] = 10;
        assert_eq!(parse_sector_count(&cfis, false), 10u32);

        // Probar helpers de disco con count = 65536 sin truncamiento
        let mut ctrl = AhciController::new();
        ctrl.ram_disk = Some(vec![0xAAu8; 1024]);
        let mut buf = vec![0u8; 1024];
        ctrl.read_disk_sectors(0, 65536, &mut buf);
        assert_eq!(buf[0], 0xAA);
        assert_eq!(buf[1023], 0xAA);
    }

    #[test]
    fn test_ahci_ncq_fpdma_queued() {
        let mut ctrl = AhciController::new();
        ctrl.ram_disk = Some(vec![0u8; 1024 * 1024]); // 1 MiB

        let mut raw_mem = vec![0u8; 64 * 1024];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let clb = 0x1000u64;
        let ctba = 0x2000u64;
        let prd_data_buf = 0x3000u64;
        let fb = 0x4000u64;

        ctrl.write_reg_u32(0x100 + PORT_CLB, clb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_FB, fb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_IE, PORT_IS_SDBS, Some(&guest_mem));
        ctrl.write_reg_u32(HOST_CTL, GHC_AE | GHC_IE, Some(&guest_mem));

        // 1. READ FPDMA QUEUED (0x60) en slot 2
        let slot = 2usize;
        let slot_offset = (slot as u64) * 32;
        let dw0: u32 = 5 | (1 << 16); // CFL=5, PRDTL=1
        guest_mem.write_u32((clb + slot_offset) as usize, dw0);
        guest_mem.write_u32((clb + slot_offset + 4) as usize, 0);
        guest_mem.write_u32((clb + slot_offset + 8) as usize, ctba as u32);
        guest_mem.write_u32((clb + slot_offset + 12) as usize, 0);

        // CFIS H2D: cmd 0x60, lba 0, count 1
        guest_mem.write_u8(ctba as usize, 0x27);
        guest_mem.write_u8((ctba + 1) as usize, 0x80);
        guest_mem.write_u8((ctba + 2) as usize, 0x60); // READ FPDMA QUEUED
        guest_mem.write_u8((ctba + 4) as usize, 0);    // LBA
        guest_mem.write_u8((ctba + 12) as usize, 1);   // Sector count = 1

        guest_mem.write_u32((ctba + 0x80) as usize, prd_data_buf as u32);
        guest_mem.write_u32((ctba + 0x84) as usize, 0);
        guest_mem.write_u32((ctba + 0x8C) as usize, 511);

        // Activar SACT y CI para slot 2
        ctrl.write_reg_u32(0x100 + PORT_SACT, 1 << slot, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_CI, 1 << slot, Some(&guest_mem));

        // Verificar finalización en SACT y CI
        let sact = ctrl.read_reg_u32(0x100 + PORT_SACT);
        let ci = ctrl.read_reg_u32(0x100 + PORT_CI);
        assert_eq!(sact & (1 << slot), 0, "SACT slot bit debe ser limpiado");
        assert_eq!(ci & (1 << slot), 0, "CI slot bit debe ser limpiado");

        // Verificar interrupciones: SDBS activo, pero NO PSS ni DHRS
        let p_is = ctrl.read_reg_u32(0x100 + PORT_IS);
        assert_ne!(p_is & PORT_IS_SDBS, 0, "PORT_IS_SDBS debe estar activo");
        assert_eq!(p_is & PORT_IS_PSS, 0, "PORT_IS_PSS NO debe estar activo para NCQ");
        assert_eq!(p_is & PORT_IS_DHRS, 0, "PORT_IS_DHRS NO debe estar activo para NCQ");

        // Verificar Set Device Bits FIS en FB + 0x58
        let fis_type = guest_mem.read_u8((fb + 0x58) as usize);
        let fis_flags = guest_mem.read_u8((fb + 0x59) as usize);
        let sactive_done = guest_mem.read_u32((fb + 0x5C) as usize);
        assert_eq!(fis_type, 0xA1, "SDB FIS Type debe ser 0xA1");
        assert_eq!(fis_flags & 0x40, 0x40, "Interrupt bit I debe estar activo");
        assert_eq!(sactive_done, 1 << slot, "SActive mask debe reflejar el tag completado");
    }

    #[test]
    fn test_ahci_pio_vs_dma_is_flags() {
        let mut ctrl = AhciController::new();
        ctrl.ram_disk = Some(vec![0u8; 1024 * 1024]);

        let mut raw_mem = vec![0u8; 64 * 1024];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let clb = 0x1000u64;
        let ctba = 0x2000u64;
        let prd_data_buf = 0x3000u64;
        let fb = 0x4000u64;

        ctrl.write_reg_u32(0x100 + PORT_CLB, clb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(0x100 + PORT_FB, fb as u32, Some(&guest_mem));

        // 1. Comando PIO: IDENTIFY DEVICE (0xEC)
        let dw0: u32 = 5 | (1 << 16);
        guest_mem.write_u32(clb as usize, dw0);
        guest_mem.write_u32((clb + 4) as usize, 0);
        guest_mem.write_u32((clb + 8) as usize, ctba as u32);
        guest_mem.write_u32((clb + 12) as usize, 0);

        guest_mem.write_u8(ctba as usize, 0x27);
        guest_mem.write_u8((ctba + 1) as usize, 0x80);
        guest_mem.write_u8((ctba + 2) as usize, 0xEC);

        guest_mem.write_u32((ctba + 0x80) as usize, prd_data_buf as u32);
        guest_mem.write_u32((ctba + 0x84) as usize, 0);
        guest_mem.write_u32((ctba + 0x8C) as usize, 511);

        ctrl.write_reg_u32(0x100 + PORT_CI, 1, Some(&guest_mem));

        let p_is_pio = ctrl.read_reg_u32(0x100 + PORT_IS);
        assert_ne!(p_is_pio & PORT_IS_PSS, 0, "PORT_IS_PSS DEBE activarse para comando PIO (0xEC)");
        assert_ne!(p_is_pio & PORT_IS_DHRS, 0, "PORT_IS_DHRS DEBE activarse para comando PIO (0xEC)");

        // Verificar PIO Setup FIS en FB + 0x20
        let ps_type = guest_mem.read_u8((fb + 0x20) as usize);
        assert_eq!(ps_type, 0x5F, "PIO Setup FIS type debe ser 0x5F");

        // Limpiar PxIS
        ctrl.write_reg_u32(0x100 + PORT_IS, 0xFFFF_FFFF, Some(&guest_mem));
        assert_eq!(ctrl.read_reg_u32(0x100 + PORT_IS), 0);

        // 2. Transferencia DMA pura: READ DMA EXT (0x25)
        guest_mem.write_u8((ctba + 2) as usize, 0x25);
        ctrl.write_reg_u32(0x100 + PORT_CI, 1, Some(&guest_mem));

        let p_is_dma = ctrl.read_reg_u32(0x100 + PORT_IS);
        assert_ne!(p_is_dma & PORT_IS_DHRS, 0, "PORT_IS_DHRS DEBE activarse para READ DMA EXT");
        assert_eq!(p_is_dma & PORT_IS_PSS, 0, "PORT_IS_PSS NO DEBE activarse para transferencia DMA pura");
    }

    #[test]
    fn test_ahci_aux_disk_attach_and_pi() {
        let mut ctrl = AhciController::new();
        assert_eq!(ctrl.pi, 0x03);
        assert!(!ctrl.ports[2].present);

        let temp_dir = std::env::temp_dir();
        let aux_path = temp_dir.join(format!("test_aux_disk_{}.img", std::process::id()));
        {
            let mut f = File::create(&aux_path).expect("Error creando archivo temporal");
            f.write_all(&vec![0xAAu8; 1024 * 1024]).expect("Error escribiendo aux disk");
        }

        ctrl.attach_aux_disk(&aux_path.to_string_lossy());
        assert_eq!(ctrl.pi, 0x07, "PI debe ser 0x07 al montar disco auxiliar");
        assert!(ctrl.ports[2].present, "Puerto 2 debe estar presente");
        assert_eq!(ctrl.ports[2].sig, PORT_SIG_ATA);
        assert_eq!(ctrl.ports[2].ssts, PORT_SSTS_PRESENT_GEN3);

        // Reset debe conservar PI=0x07 si hay aux_disk
        ctrl.reset();
        assert_eq!(ctrl.pi, 0x07);
        assert!(ctrl.ports[2].present);

        let _ = std::fs::remove_file(&aux_path);
    }

    #[test]
    fn test_ahci_aux_disk_read_write_and_identify() {
        let mut ctrl = AhciController::new();
        let temp_dir = std::env::temp_dir();
        let aux_path = temp_dir.join(format!("test_aux_rw_{}.img", std::process::id()));
        {
            let mut f = File::create(&aux_path).expect("Error creando archivo temporal");
            f.write_all(&vec![0x00u8; 64 * 1024]).expect("Error inicializando");
        }

        ctrl.attach_aux_disk(&aux_path.to_string_lossy());

        // Probar IDENTIFY en puerto 2 (con word-swapping estándar de ATA)
        let id = ctrl.identify_device(2);
        let serial = &id[20..36];
        assert_eq!(serial, b"WT5O55S-TA2A    ");

        // Probar escritura en puerto 2
        let write_data = vec![0x77u8; 512];
        ctrl.write_sectors(2, 0, 1, &write_data);

        // Probar lectura en puerto 2
        let mut read_data = vec![0u8; 512];
        ctrl.read_sectors(2, 0, 1, &mut read_data);
        assert_eq!(read_data, write_data);

        // Verificar que puerto 0 no se vio afectado
        let id0 = ctrl.identify_device(0);
        assert_eq!(&id0[20..36], b"WT5O55S-TA0A    ");

        let _ = std::fs::remove_file(&aux_path);
    }

    #[test]
    fn test_ahci_port2_command_issue() {
        let mut ctrl = AhciController::new();
        let temp_dir = std::env::temp_dir();
        let aux_path = temp_dir.join(format!("test_aux_ci_{}.img", std::process::id()));
        {
            let mut f = File::create(&aux_path).expect("Error creando archivo temporal");
            let mut initial = vec![0x00u8; 128 * 1024];
            initial[..512].fill(0x55);
            f.write_all(&initial).expect("Error escribiendo archivo");
        }

        ctrl.attach_aux_disk(&aux_path.to_string_lossy());

        let mut raw_mem = vec![0u8; 64 * 1024];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let clb = 0x1000u64;
        let ctba = 0x2000u64;
        let prd_data_buf = 0x3000u64;
        let fb = 0x4000u64;

        // Base del Puerto 2 = 0x100 + 2 * 0x80 = 0x200
        let p2_base = 0x200;
        ctrl.write_reg_u32(p2_base + PORT_CLB, clb as u32, Some(&guest_mem));
        ctrl.write_reg_u32(p2_base + PORT_FB, fb as u32, Some(&guest_mem));

        // 1. IDENTIFY DEVICE (0xEC) en Puerto 2
        let dw0: u32 = 5 | (1 << 16);
        guest_mem.write_u32(clb as usize, dw0);
        guest_mem.write_u32((clb + 4) as usize, 0);
        guest_mem.write_u32((clb + 8) as usize, ctba as u32);
        guest_mem.write_u32((clb + 12) as usize, 0);

        guest_mem.write_u8(ctba as usize, 0x27);
        guest_mem.write_u8((ctba + 1) as usize, 0x80);
        guest_mem.write_u8((ctba + 2) as usize, 0xEC);

        guest_mem.write_u32((ctba + 0x80) as usize, prd_data_buf as u32);
        guest_mem.write_u32((ctba + 0x84) as usize, 0);
        guest_mem.write_u32((ctba + 0x8C) as usize, 511);

        ctrl.write_reg_u32(p2_base + PORT_CI, 1, Some(&guest_mem));

        let mut id_buf = [0u8; 512];
        guest_mem.copy_from(prd_data_buf as usize, &mut id_buf);
        assert_eq!(&id_buf[20..36], b"WT5O55S-TA2A    ");

        // 2. READ DMA (0xC8) en Puerto 2
        guest_mem.write_u8((ctba + 2) as usize, 0xC8);
        guest_mem.write_u8((ctba + 4) as usize, 0);  // LBA 0
        guest_mem.write_u8((ctba + 12) as usize, 1); // 1 sector

        ctrl.write_reg_u32(p2_base + PORT_CI, 1, Some(&guest_mem));

        let mut read_buf = [0u8; 512];
        guest_mem.copy_from(prd_data_buf as usize, &mut read_buf);
        assert_eq!(read_buf[0], 0x55);
        assert_eq!(read_buf[511], 0x55);

        let _ = std::fs::remove_file(&aux_path);
    }

    #[test]
    fn test_ahci_scsi_read_dvd_structure_and_get_configuration() {
        let mut ctrl = AhciController::new();
        // Simular ISO de Windows 7 DVD (3.5 GB)
        ctrl.iso_size = 3500 * 1024 * 1024;

        // 1. Probar GET CONFIGURATION (0x46)
        let mut cdb_cfg = [0u8; 16];
        cdb_cfg[0] = 0x46; // GET CONFIGURATION
        cdb_cfg[7] = 0;
        cdb_cfg[8] = 88; // Alloc len = 88
        let cfg_resp = ctrl.execute_scsi(&cdb_cfg).expect("GET CONFIGURATION response");
        assert_eq!(cfg_resp.len(), 88);
        // Current profile must be DVD-ROM (0x0010)
        let cur_profile = u16::from_be_bytes([cfg_resp[6], cfg_resp[7]]);
        assert_eq!(cur_profile, 0x0010, "DVD-ROM profile must be 0x0010 for >700MB ISO");

        // 2. Probar READ DVD STRUCTURE (0xAD) Format 0 (Physical Format Information)
        let mut cdb_dvd = [0u8; 16];
        cdb_dvd[0] = 0xAD; // READ DVD STRUCTURE
        cdb_dvd[7] = 0x00; // Format 0: Physical format information
        cdb_dvd[8] = 0x08;
        cdb_dvd[9] = 0x04; // Alloc len = 2052 (2048 + 4)
        let dvd_resp = ctrl.execute_scsi(&cdb_dvd).expect("READ DVD STRUCTURE response");
        assert_eq!(dvd_resp.len(), 2052);
        assert_eq!(dvd_resp[4], 0x01, "DVD-ROM Book type v1");
        assert_eq!(dvd_resp[5], 0x0F, "120mm size");
        assert_eq!(dvd_resp[6], 0x01, "Single layer read-only");
        let start_psn = u32::from_be_bytes([dvd_resp[8], dvd_resp[9], dvd_resp[10], dvd_resp[11]]);
        assert_eq!(start_psn, 0x0003_0000, "Starting PSN must be 0x30000 per MMC spec");

        // 3. Probar EVPD INQUIRY Page 0x80 (Unit Serial Number)
        let mut cdb_evpd = [0u8; 16];
        cdb_evpd[0] = 0x12; // INQUIRY
        cdb_evpd[1] = 0x01; // EVPD = 1
        cdb_evpd[2] = 0x80; // Page 0x80
        cdb_evpd[4] = 32;
        let evpd_resp = ctrl.execute_scsi(&cdb_evpd).expect("EVPD INQUIRY response");
        assert_eq!(evpd_resp[0], 0x05); // CD-ROM
        assert_eq!(evpd_resp[1], 0x80); // Page 0x80
        assert_eq!(&evpd_resp[4..15], b"VB1234-5678");
    }
}
