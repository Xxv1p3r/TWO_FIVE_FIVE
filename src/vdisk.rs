//! Subsistema de Discos Virtuales (`VD` / `VDI` / `RAW`) basado en la arquitectura de VirtualBox.
//!
//! Implementa fielmente las estructuras y algoritmos de `src/VBox/Storage/VDICore.h`,
//! `src/VBox/Storage/VDI.cpp` y `src/VBox/Storage/VD.cpp` de VirtualBox 7.2:
//! - Pre-Header (`VDIPREHEADER`, 72 bytes) con firma `0xBEDA107F` (`VDI_IMAGE_SIGNATURE`).
//! - Header versión 1.1+ (`VDIHEADER1PLUS`, 400 bytes) con soporte para imágenes:
//!   * Dinámicas (`VDI_IMAGE_TYPE_NORMAL = 1`): crecen bajo demanda en bloques de 1 MiB.
//!   * Fijas (`VDI_IMAGE_TYPE_FIXED = 2`): preasignadas completamente al crearse.
//! - Tabla de punteros de bloques (`paBlocks` de `VDIIMAGEBLOCKPOINTER` = `u32`):
//!   * `VDI_IMAGE_BLOCK_FREE (0xFFFFFFFF)`: bloque no asignado.
//!   * `VDI_IMAGE_BLOCK_ZERO (0xFFFFFFFE)`: bloque de ceros sin almacenamiento físico.
//!   * `< 0xFFFFFFFE`: índice físico del bloque de 1 MiB en el área de datos (`offData`).
//! - Alineación de tabla de bloques y área de datos a 1 MiB (`VDI_DATA_ALIGN = _1M`).
//! - Operaciones de lectura/escritura LBA, compactación (`vdiCompact`) y redimensionado (`vdiResize`).
//! - Detección automática (`vdiProbe`) entre formato nativo `.vdi` y formato plano `.img` (`RAW`).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Comprobación vectorizada en bloques de 64 bits de si un slice contiene únicamente ceros.
#[inline]
fn is_all_zero(buf: &[u8]) -> bool {
    let chunks = buf.chunks_exact(8);
    let rem = chunks.remainder();
    for chunk in chunks {
        if u64::from_ne_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]) != 0
        {
            return false;
        }
    }
    rem.iter().all(|&b| b == 0)
}

/// Texto descriptivo en `VDIPREHEADER::szFileInfo` (64 bytes máx).
pub const VDI_IMAGE_FILE_INFO: &[u8] = b"<<< Oracle VirtualBox Disk Image >>>\n";
/// Firma mágica de imagen VDI (`0xbeda107f` en `VDICore.h`).
pub const VDI_IMAGE_SIGNATURE: u32 = 0xBEDA_107F;
/// Versión 1.1 de formato VDI (`(1 << 16) | 1`).
pub const VDI_IMAGE_VERSION: u32 = 0x0001_0001;
/// Tamaño del encabezado `VDIPREHEADER` en disco (64 + 4 + 4 = 72 bytes).
pub const VDI_PREHEADER_SIZE: usize = 72;
/// Tamaño del encabezado `VDIHEADER1` (sin LCHS, 384 bytes).
pub const VDI_HEADER1_SIZE: u32 = 384;
/// Tamaño del encabezado `VDIHEADER1PLUS` en disco (400 bytes).
pub const VDI_HEADER1PLUS_SIZE: u32 = 400;
/// Tamaño del comentario en el header VDI (256 bytes).
pub const VDI_IMAGE_COMMENT_SIZE: usize = 256;
/// Tamaño de sector estándar (512 bytes).
pub const VDI_SECTOR_SIZE: u32 = 512;
/// Tamaño por defecto de un bloque de asignación VDI (1 MiB).
pub const VDI_DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;
/// Alineación en archivo para la tabla de bloques y datos (`VDI_DATA_ALIGN = _1M`).
pub const VDI_DATA_ALIGN: u32 = 1024 * 1024;

/// Puntero a bloque no asignado (`~0u32`).
pub const VDI_IMAGE_BLOCK_FREE: u32 = 0xFFFF_FFFF;
/// Puntero a bloque lleno de ceros no asignado físicamente (`~1u32`).
pub const VDI_IMAGE_BLOCK_ZERO: u32 = 0xFFFF_FFFE;
/// Umbral a partir del cual el bloque no tiene almacenamiento físico asignado.
pub const VDI_IMAGE_BLOCK_UNALLOCATED: u32 = VDI_IMAGE_BLOCK_ZERO;

/// Tipos de imagen VDI (`VDIIMAGETYPE` en `VDICore.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum VdiImageType {
    /// Imagen base de crecimiento dinámico (`VDI_IMAGE_TYPE_NORMAL = 1`).
    Normal = 1,
    /// Imagen base preasignada de tamaño fijo (`VDI_IMAGE_TYPE_FIXED = 2`).
    Fixed = 2,
    /// Imagen de cambios undo (`VDI_IMAGE_TYPE_UNDO = 3`).
    Undo = 3,
    /// Imagen diferencial / snapshot (`VDI_IMAGE_TYPE_DIFF = 4`).
    Diff = 4,
}

impl VdiImageType {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            1 => Some(Self::Normal),
            2 => Some(Self::Fixed),
            3 => Some(Self::Undo),
            4 => Some(Self::Diff),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Normal => "VDI Dinamico",
            Self::Fixed => "VDI Fijo",
            Self::Undo => "VDI Undo",
            Self::Diff => "VDI Diferencial",
        }
    }
}

/// Formato del contenedor de disco virtual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    Vdi(VdiImageType),
    Raw,
}

impl DiskFormat {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Vdi(t) => t.label(),
            Self::Raw => "RAW (.img)",
        }
    }
}

/// Geometría CHS (`VDIDISKGEOMETRY`).
#[derive(Debug, Clone, Copy, Default)]
pub struct VdiDiskGeometry {
    pub cylinders: u32,
    pub heads: u32,
    pub sectors: u32,
    pub sector_size: u32,
}

impl VdiDiskGeometry {
    /// Calcula la geometría LCHS para un tamaño en bytes siguiendo `vdCalcLCHSGeometry` de `VD.cpp`.
    pub fn calc_lchs(cb_disk: u64) -> Self {
        let total_sectors = cb_disk / 512;
        let sectors = 63u32;
        let heads = if total_sectors <= 1024 * 16 * 63 {
            16u32
        } else if total_sectors <= 1024 * 32 * 63 {
            32u32
        } else if total_sectors <= 1024 * 64 * 63 {
            64u32
        } else if total_sectors <= 1024 * 128 * 63 {
            128u32
        } else {
            255u32
        };
        let cylinders = ((total_sectors / (heads as u64 * sectors as u64)).min(1024) as u32).max(1);
        Self {
            cylinders,
            heads,
            sectors,
            sector_size: VDI_SECTOR_SIZE,
        }
    }
}

/// Información resumida de una imagen de disco virtual (equivalente a `VBoxManage showmediuminfo`).
#[derive(Debug, Clone)]
pub struct DiskInfo {
    pub path: PathBuf,
    pub format: DiskFormat,
    /// Capacidad lógica vista por la máquina virtual (en bytes).
    pub virtual_size: u64,
    /// Tamaño real ocupado actualmente en el host (en bytes).
    pub actual_size: u64,
    /// Tamaño de bloque de asignación (1 MiB en VDI, 512 en RAW).
    pub block_size: u32,
    /// Número total de bloques lógicos.
    pub blocks_total: u32,
    /// Número de bloques físicos asignados actualmente.
    pub blocks_allocated: u32,
    /// UUID de creación formateado como cadena.
    pub uuid_create: String,
    /// Comentario almacenado en el header VDI.
    pub comment: String,
}

/// Estructura en memoria del header VDI 1.1+ (`VDIPREHEADER` + `VDIHEADER1PLUS`).
#[derive(Debug, Clone)]
pub struct VdiHeader {
    pub version: u32,
    pub cb_header: u32,
    pub image_type: VdiImageType,
    pub flags: u32,
    pub comment: String,
    pub off_blocks: u32,
    pub off_data: u32,
    pub legacy_geo: VdiDiskGeometry,
    pub cb_disk: u64,
    pub cb_block: u32,
    pub cb_block_extra: u32,
    pub c_blocks: u32,
    pub c_blocks_allocated: u32,
    pub uuid_create: [u8; 16],
    pub uuid_modify: [u8; 16],
    pub uuid_linkage: [u8; 16],
    pub uuid_parent_modify: [u8; 16],
    pub lchs_geo: VdiDiskGeometry,
}

#[inline]
fn align_up_u32(val: u32, align: u32) -> u32 {
    (val + align - 1) & !(align - 1)
}

/// Genera un UUID v4 pseudoaleatorio para identificar imágenes VDI (`RTUuidCreate`).
fn generate_uuid() -> [u8; 16] {
    let mut uuid = [0u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut uuid);
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0x1234_5678_9ABC_DEF0);
        let bytes = nanos.to_le_bytes();
        uuid.copy_from_slice(&bytes);
    }
    // RFC 4122 versión 4 y variante 1
    uuid[6] = (uuid[6] & 0x0F) | 0x40;
    uuid[8] = (uuid[8] & 0x3F) | 0x80;
    uuid
}

fn format_uuid(u: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u[3], u[2], u[1], u[0],
        u[5], u[4],
        u[7], u[6],
        u[8], u[9],
        u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

impl VdiHeader {
    /// Inicializa un header VDI 1.1+ nuevo siguiendo `vdiInitHeader` de `VDI.cpp`.
    pub fn new(cb_disk: u64, image_type: VdiImageType, comment: &str) -> Self {
        let cb_disk_aligned = (cb_disk + 511) & !511;
        let cb_block = VDI_DEFAULT_BLOCK_SIZE;
        let mut c_blocks = (cb_disk_aligned / cb_block as u64) as u32;
        if cb_disk_aligned % (cb_block as u64) != 0 {
            c_blocks += 1;
        }
        let off_blocks = align_up_u32(
            VDI_PREHEADER_SIZE as u32 + VDI_HEADER1PLUS_SIZE,
            VDI_DATA_ALIGN,
        );
        let off_data = align_up_u32(
            off_blocks + c_blocks.saturating_mul(4),
            VDI_DATA_ALIGN,
        );
        let c_blocks_allocated = if image_type == VdiImageType::Fixed {
            c_blocks
        } else {
            0
        };

        Self {
            version: VDI_IMAGE_VERSION,
            cb_header: VDI_HEADER1PLUS_SIZE,
            image_type,
            flags: 0,
            comment: comment.to_string(),
            off_blocks,
            off_data,
            legacy_geo: VdiDiskGeometry {
                cylinders: 0,
                heads: 0,
                sectors: 0,
                sector_size: VDI_SECTOR_SIZE,
            },
            cb_disk: cb_disk_aligned,
            cb_block,
            cb_block_extra: 0,
            c_blocks,
            c_blocks_allocated,
            uuid_create: generate_uuid(),
            uuid_modify: generate_uuid(),
            uuid_linkage: [0u8; 16],
            uuid_parent_modify: [0u8; 16],
            lchs_geo: VdiDiskGeometry::calc_lchs(cb_disk_aligned),
        }
    }

    /// Serializa `VDIPREHEADER` (72 bytes) + `VDIHEADER1PLUS` (400 bytes) en formato Little-Endian.
    pub fn to_bytes(&self) -> [u8; VDI_PREHEADER_SIZE + VDI_HEADER1PLUS_SIZE as usize] {
        let mut buf = [0u8; VDI_PREHEADER_SIZE + VDI_HEADER1PLUS_SIZE as usize];

        // 1. Pre-header (0..72)
        let info_len = VDI_IMAGE_FILE_INFO.len().min(63);
        buf[0..info_len].copy_from_slice(&VDI_IMAGE_FILE_INFO[..info_len]);
        buf[64..68].copy_from_slice(&VDI_IMAGE_SIGNATURE.to_le_bytes());
        buf[68..72].copy_from_slice(&self.version.to_le_bytes());

        // 2. VDIHEADER1PLUS (72..472)
        let h = &mut buf[72..];
        h[0..4].copy_from_slice(&self.cb_header.to_le_bytes());
        h[4..8].copy_from_slice(&(self.image_type as u32).to_le_bytes());
        h[8..12].copy_from_slice(&self.flags.to_le_bytes());

        let comment_bytes = self.comment.as_bytes();
        let c_len = comment_bytes.len().min(VDI_IMAGE_COMMENT_SIZE - 1);
        h[12..12 + c_len].copy_from_slice(&comment_bytes[..c_len]);

        // offBlocks (268..272) y offData (272..276)
        h[268..272].copy_from_slice(&self.off_blocks.to_le_bytes());
        h[272..276].copy_from_slice(&self.off_data.to_le_bytes());

        // LegacyGeometry (276..292) + u32Dummy (292..296)
        h[276..280].copy_from_slice(&self.legacy_geo.cylinders.to_le_bytes());
        h[280..284].copy_from_slice(&self.legacy_geo.heads.to_le_bytes());
        h[284..288].copy_from_slice(&self.legacy_geo.sectors.to_le_bytes());
        h[288..292].copy_from_slice(&self.legacy_geo.sector_size.to_le_bytes());
        h[292..296].copy_from_slice(&0u32.to_le_bytes());

        // cbDisk (296..304), cbBlock (304..308), cbBlockExtra (308..312), cBlocks (312..316), cBlocksAllocated (316..320)
        h[296..304].copy_from_slice(&self.cb_disk.to_le_bytes());
        h[304..308].copy_from_slice(&self.cb_block.to_le_bytes());
        h[308..312].copy_from_slice(&self.cb_block_extra.to_le_bytes());
        h[312..316].copy_from_slice(&self.c_blocks.to_le_bytes());
        h[316..320].copy_from_slice(&self.c_blocks_allocated.to_le_bytes());

        // UUIDs (320..384)
        h[320..336].copy_from_slice(&self.uuid_create);
        h[336..352].copy_from_slice(&self.uuid_modify);
        h[352..368].copy_from_slice(&self.uuid_linkage);
        h[368..384].copy_from_slice(&self.uuid_parent_modify);

        // LCHSGeometry (384..400)
        h[384..388].copy_from_slice(&self.lchs_geo.cylinders.to_le_bytes());
        h[388..392].copy_from_slice(&self.lchs_geo.heads.to_le_bytes());
        h[392..396].copy_from_slice(&self.lchs_geo.sectors.to_le_bytes());
        h[396..400].copy_from_slice(&self.lchs_geo.sector_size.to_le_bytes());

        buf
    }

    /// Lee y valida `VDIPREHEADER` y `VDIHEADER1` / `VDIHEADER1PLUS` (`vdiValidatePreHeader` + `vdiValidateHeader`).
    pub fn from_reader<R: Read + Seek>(reader: &mut R) -> std::io::Result<Self> {
        reader.seek(SeekFrom::Start(0))?;
        let mut pre = [0u8; VDI_PREHEADER_SIZE];
        reader.read_exact(&mut pre)?;

        let sig = u32::from_le_bytes([pre[64], pre[65], pre[66], pre[67]]);
        if sig != VDI_IMAGE_SIGNATURE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Firma VDI invalida: {:#010x} (esperada {:#010x})", sig, VDI_IMAGE_SIGNATURE),
            ));
        }
        let version = u32::from_le_bytes([pre[68], pre[69], pre[70], pre[71]]);
        let major = version >> 16;
        if major != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Version mayor VDI no soportada: {}", major),
            ));
        }

        let mut h = [0u8; VDI_HEADER1PLUS_SIZE as usize];
        reader.read_exact(&mut h[..VDI_HEADER1_SIZE as usize])?;
        let cb_header = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        if cb_header < VDI_HEADER1_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("cbHeader VDI demasiado pequeno: {}", cb_header),
            ));
        }
        if cb_header >= VDI_HEADER1PLUS_SIZE {
            reader.read_exact(&mut h[VDI_HEADER1_SIZE as usize..VDI_HEADER1PLUS_SIZE as usize])?;
        }

        let raw_type = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        let image_type = VdiImageType::from_u32(raw_type).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Tipo de imagen VDI invalido: {}", raw_type),
            )
        })?;
        let flags = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);

        let comment_end = h[12..12 + VDI_IMAGE_COMMENT_SIZE]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(VDI_IMAGE_COMMENT_SIZE);
        let comment = String::from_utf8_lossy(&h[12..12 + comment_end]).to_string();

        let off_blocks = u32::from_le_bytes([h[268], h[269], h[270], h[271]]);
        let off_data = u32::from_le_bytes([h[272], h[273], h[274], h[275]]);

        let legacy_geo = VdiDiskGeometry {
            cylinders: u32::from_le_bytes([h[276], h[277], h[278], h[279]]),
            heads: u32::from_le_bytes([h[280], h[281], h[282], h[283]]),
            sectors: u32::from_le_bytes([h[284], h[285], h[286], h[287]]),
            sector_size: u32::from_le_bytes([h[288], h[289], h[290], h[291]]),
        };

        let cb_disk = u64::from_le_bytes([
            h[296], h[297], h[298], h[299], h[300], h[301], h[302], h[303],
        ]);
        let cb_block = u32::from_le_bytes([h[304], h[305], h[306], h[307]]);
        let cb_block_extra = u32::from_le_bytes([h[308], h[309], h[310], h[311]]);
        let c_blocks = u32::from_le_bytes([h[312], h[313], h[314], h[315]]);
        let c_blocks_allocated = u32::from_le_bytes([h[316], h[317], h[318], h[319]]);

        if cb_disk == 0 || cb_block == 0 || !cb_block.is_power_of_two() || c_blocks == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Dimensiones de bloque o disco invalidas en header VDI",
            ));
        }

        let mut uuid_create = [0u8; 16];
        let mut uuid_modify = [0u8; 16];
        let mut uuid_linkage = [0u8; 16];
        let mut uuid_parent_modify = [0u8; 16];
        uuid_create.copy_from_slice(&h[320..336]);
        uuid_modify.copy_from_slice(&h[336..352]);
        uuid_linkage.copy_from_slice(&h[352..368]);
        uuid_parent_modify.copy_from_slice(&h[368..384]);

        let lchs_geo = if cb_header >= VDI_HEADER1PLUS_SIZE {
            VdiDiskGeometry {
                cylinders: u32::from_le_bytes([h[384], h[385], h[386], h[387]]),
                heads: u32::from_le_bytes([h[388], h[389], h[390], h[391]]),
                sectors: u32::from_le_bytes([h[392], h[393], h[394], h[395]]),
                sector_size: u32::from_le_bytes([h[396], h[397], h[398], h[399]]),
            }
        } else {
            VdiDiskGeometry::calc_lchs(cb_disk)
        };

        Ok(Self {
            version,
            cb_header: VDI_HEADER1PLUS_SIZE,
            image_type,
            flags,
            comment,
            off_blocks,
            off_data,
            legacy_geo,
            cb_disk,
            cb_block,
            cb_block_extra,
            c_blocks,
            c_blocks_allocated,
            uuid_create,
            uuid_modify,
            uuid_linkage,
            uuid_parent_modify,
            lchs_geo,
        })
    }
}

/// Descriptor de imagen VDI abierta (`VDIIMAGEDESC` en `VDICore.h`).
pub struct VdiDisk {
    pub path: PathBuf,
    pub file: File,
    pub header: VdiHeader,
    /// Tabla de punteros a bloques (`paBlocks` en `VDICore.h`).
    pub pa_blocks: Vec<u32>,
    /// Shift para convertir un offset de byte en índice de bloque (`uShiftOffset2Index`).
    pub shift_offset_to_index: u32,
    /// Máscara de offset dentro de un bloque (`uBlockMask = cbBlock - 1`).
    pub block_mask: u32,
    /// Tamaño total de un bloque en el archivo (`cbBlockExtra + cbBlock`).
    pub cb_total_block_data: u64,
    /// Tamaño físico actual cacheado del archivo en el host para evitar syscalls `lseek(SEEK_END)`.
    pub file_len: u64,
}

impl VdiDisk {
    /// Crea un nuevo archivo `.vdi` en disco (`vdiCreateImage` + `vdiImageCreateFile`).
    pub fn create(
        path: impl AsRef<Path>,
        cb_size: u64,
        fixed: bool,
        comment: &str,
    ) -> std::io::Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let img_type = if fixed {
            VdiImageType::Fixed
        } else {
            VdiImageType::Normal
        };
        let header = VdiHeader::new(cb_size, img_type, comment);

        let mut pa_blocks = vec![VDI_IMAGE_BLOCK_FREE; header.c_blocks as usize];
        if fixed {
            for (i, slot) in pa_blocks.iter_mut().enumerate() {
                *slot = i as u32;
            }
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path_buf)?;

        let cb_total_block_data = (header.cb_block_extra + header.cb_block) as u64;
        let file_len = if fixed {
            let total_file_size =
                header.off_data as u64 + (header.c_blocks as u64) * cb_total_block_data;
            file.set_len(total_file_size)?;
            total_file_size
        } else {
            // Para imágenes dinámicas, el tamaño inicial del archivo cubre hasta offData
            let initial_size = header.off_data as u64;
            file.set_len(initial_size)?;
            initial_size
        };

        // Escribir PreHeader + Header
        let hdr_bytes = header.to_bytes();
        file.write_all_at(&hdr_bytes, 0)?;

        // Escribir tabla de bloques (paBlocks) en offBlocks
        let mut block_table_bytes = Vec::with_capacity(pa_blocks.len() * 4);
        for &bp in &pa_blocks {
            block_table_bytes.extend_from_slice(&bp.to_le_bytes());
        }
        file.write_all_at(&block_table_bytes, header.off_blocks as u64)?;
        file.sync_all()?;

        let shift_offset_to_index = header.cb_block.trailing_zeros();
        let block_mask = header.cb_block - 1;

        Ok(Self {
            path: path_buf,
            file,
            header,
            pa_blocks,
            shift_offset_to_index,
            block_mask,
            cb_total_block_data,
            file_len,
        })
    }

    /// Abre una imagen `.vdi` existente (`vdiOpenImage` en `VDI.cpp`).
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path_buf)
            .or_else(|_| OpenOptions::new().read(true).open(&path_buf))?;

        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let header = VdiHeader::from_reader(&mut file)?;

        // Leer tabla paBlocks desde offBlocks
        let mut raw_table = vec![0u8; (header.c_blocks as usize) * 4];
        file.read_exact_at(&mut raw_table, header.off_blocks as u64)?;

        let mut pa_blocks = Vec::with_capacity(header.c_blocks as usize);
        for chunk in raw_table.chunks_exact(4) {
            pa_blocks.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }

        let shift_offset_to_index = header.cb_block.trailing_zeros();
        let block_mask = header.cb_block - 1;
        let cb_total_block_data = (header.cb_block_extra + header.cb_block) as u64;

        Ok(Self {
            path: path_buf,
            file,
            header,
            pa_blocks,
            shift_offset_to_index,
            block_mask,
            cb_total_block_data,
            file_len,
        })
    }

    #[inline]
    fn is_block_allocated(bp: u32) -> bool {
        bp < VDI_IMAGE_BLOCK_UNALLOCATED
    }

    /// Persiste el header VDI actualizado en el archivo (`vdiUpdateHeader`).
    fn update_header(&mut self) -> std::io::Result<()> {
        let hdr_bytes = self.header.to_bytes();
        self.file.write_all_at(&hdr_bytes, 0)
    }

    /// Persiste una entrada individual de `paBlocks[u_block]` en el archivo (`vdiUpdateBlockInfo`).
    fn update_block_info(&mut self, u_block: usize) -> std::io::Result<()> {
        let file_off = (self.header.off_blocks as u64) + (u_block as u64) * 4;
        let val = self.pa_blocks[u_block].to_le_bytes();
        self.file.write_all_at(&val, file_off)
    }

    /// Lee bytes arbitrarios a partir de `offset` (`vdiRead` en `VDI.cpp`).
    pub fn read_at(&mut self, mut offset: u64, mut dst: &mut [u8]) -> std::io::Result<()> {
        while !dst.is_empty() {
            if offset >= self.header.cb_disk {
                dst.fill(0);
                break;
            }
            let u_block = (offset >> self.shift_offset_to_index) as usize;
            let off_in_block = (offset as u32) & self.block_mask;
            let rem_in_block = (self.header.cb_block - off_in_block) as usize;
            let to_read = dst.len().min(rem_in_block);

            if u_block >= self.pa_blocks.len() {
                dst[..to_read].fill(0);
            } else {
                let bp = self.pa_blocks[u_block];
                if bp == VDI_IMAGE_BLOCK_FREE || bp == VDI_IMAGE_BLOCK_ZERO {
                    dst[..to_read].fill(0);
                } else {
                    let phys_off = (self.header.off_data as u64)
                        + (bp as u64) * self.cb_total_block_data
                        + (self.header.cb_block_extra as u64)
                        + (off_in_block as u64);
                    if self.file.read_exact_at(&mut dst[..to_read], phys_off).is_err() {
                        dst[..to_read].fill(0);
                    }
                }
            }

            offset += to_read as u64;
            dst = &mut dst[to_read..];
        }
        Ok(())
    }

    /// Escribe bytes a partir de `offset` asignando bloques bajo demanda (`vdiWrite` + `vdiBlockAllocUpdate`).
    pub fn write_at(&mut self, mut offset: u64, mut src: &[u8]) -> std::io::Result<()> {
        while !src.is_empty() {
            if offset >= self.header.cb_disk {
                break;
            }
            let u_block = (offset >> self.shift_offset_to_index) as usize;
            let off_in_block = (offset as u32) & self.block_mask;
            let rem_in_block = (self.header.cb_block - off_in_block) as usize;
            let to_write = src.len().min(rem_in_block);

            if u_block >= self.pa_blocks.len() {
                break;
            }

            let bp = self.pa_blocks[u_block];
            if !Self::is_block_allocated(bp) {
                // Optimización VDI.cpp (línea 1732): si el bloque sigue sin asignar y
                // todos los bytes a escribir son cero, marcar VDI_IMAGE_BLOCK_ZERO sin gastar disco.
                let all_zero = is_all_zero(&src[..to_write]);
                if all_zero {
                    if bp != VDI_IMAGE_BLOCK_ZERO {
                        self.pa_blocks[u_block] = VDI_IMAGE_BLOCK_ZERO;
                        self.update_block_info(u_block)?;
                    }
                } else {
                    // Asignar nuevo bloque físico al final del área de datos (cBlocksAllocated)
                    let new_phys_block = self.header.c_blocks_allocated;
                    let block_base_off = (self.header.off_data as u64)
                        + (new_phys_block as u64) * self.cb_total_block_data
                        + (self.header.cb_block_extra as u64);
                    let block_end_off = block_base_off + (self.header.cb_block as u64);

                    if to_write == self.header.cb_block as usize {
                        // Escritura de bloque completo (1 MiB)
                        self.file.write_all_at(&src[..to_write], block_base_off)?;
                        if self.file_len < block_end_off {
                            self.file_len = block_end_off;
                        }
                    } else {
                        // Escritura parcial en bloque nuevo: asegurar que el bloque completo de 1 MiB
                        // exista y tenga ceros en las zonas pre-read y post-read
                        if self.file_len < block_end_off {
                            self.file.set_len(block_end_off)?;
                            self.file_len = block_end_off;
                        }
                        self.file.write_all_at(
                            &src[..to_write],
                            block_base_off + off_in_block as u64,
                        )?;
                    }

                    self.pa_blocks[u_block] = new_phys_block;
                    self.header.c_blocks_allocated += 1;
                    self.update_block_info(u_block)?;
                    self.update_header()?;
                }
            } else {
                // El bloque ya está asignado físicamente: escribir directamente
                let phys_off = (self.header.off_data as u64)
                    + (bp as u64) * self.cb_total_block_data
                    + (self.header.cb_block_extra as u64)
                    + (off_in_block as u64);
                self.file.write_all_at(&src[..to_write], phys_off)?;
            }

            offset += to_write as u64;
            src = &src[to_write..];
        }
        Ok(())
    }

    /// Compacta una imagen VDI dinámica (`vdiCompact` en `VDI.cpp`):
    /// detecta bloques asignados que contienen únicamente ceros, los marca como
    /// `VDI_IMAGE_BLOCK_ZERO`, reubica los últimos bloques físicos para tapar huecos
    /// y trunca el archivo en el host. Devuelve cuántos bytes físicos se liberaron.
    pub fn compact(&mut self) -> std::io::Result<u64> {
        if self.header.image_type == VdiImageType::Fixed {
            return Ok(0);
        }
        let old_file_size = self.file.metadata()?.len();
        let cb_block = self.header.cb_block as usize;
        let mut buf = vec![0u8; cb_block];

        // 1. Mapa inverso: phys_to_logical[phys_idx] = Some(logical_idx)
        let mut allocated_count = self.header.c_blocks_allocated as usize;
        let mut phys_to_logical: Vec<Option<usize>> = vec![None; allocated_count];
        for (log_idx, &bp) in self.pa_blocks.iter().enumerate() {
            if Self::is_block_allocated(bp) && (bp as usize) < allocated_count {
                phys_to_logical[bp as usize] = Some(log_idx);
            }
        }

        // 2. Identificar bloques llenos de ceros y liberarlos
        for log_idx in 0..self.pa_blocks.len() {
            let bp = self.pa_blocks[log_idx];
            if Self::is_block_allocated(bp) && (bp as usize) < allocated_count {
                let phys_off = (self.header.off_data as u64)
                    + (bp as u64) * self.cb_total_block_data
                    + (self.header.cb_block_extra as u64);
                if self.file.read_exact_at(&mut buf, phys_off).is_ok() && is_all_zero(&buf) {
                    self.pa_blocks[log_idx] = VDI_IMAGE_BLOCK_ZERO;
                    phys_to_logical[bp as usize] = None;
                }
            }
        }

        // 3. Compactar huecos moviendo el último bloque físico ocupado al hueco libre
        let mut total_buf = vec![0u8; self.cb_total_block_data as usize];
        let mut free_phys = 0usize;
        while free_phys < allocated_count {
            if phys_to_logical[free_phys].is_some() {
                free_phys += 1;
                continue;
            }
            // Recortar huecos al final
            while allocated_count > free_phys && phys_to_logical[allocated_count - 1].is_none() {
                allocated_count -= 1;
            }
            if allocated_count <= free_phys {
                break;
            }
            let src_phys = allocated_count - 1;
            if let Some(log_idx) = phys_to_logical[src_phys] {
                let src_off = (self.header.off_data as u64)
                    + (src_phys as u64) * self.cb_total_block_data;
                let dst_off = (self.header.off_data as u64)
                    + (free_phys as u64) * self.cb_total_block_data;
                self.file.read_exact_at(&mut total_buf, src_off)?;
                self.file.write_all_at(&total_buf, dst_off)?;

                self.pa_blocks[log_idx] = free_phys as u32;
                phys_to_logical[free_phys] = Some(log_idx);
                phys_to_logical[src_phys] = None;
            }
            allocated_count -= 1;
            free_phys += 1;
        }

        self.header.c_blocks_allocated = allocated_count as u32;
        self.header.uuid_modify = generate_uuid();
        self.update_header()?;

        // Escribir toda la tabla paBlocks actualizada
        let mut block_table_bytes = Vec::with_capacity(self.pa_blocks.len() * 4);
        for &bp in &self.pa_blocks {
            block_table_bytes.extend_from_slice(&bp.to_le_bytes());
        }
        self.file.write_all_at(&block_table_bytes, self.header.off_blocks as u64)?;

        let new_file_size = (self.header.off_data as u64)
            + (self.header.c_blocks_allocated as u64) * self.cb_total_block_data;
        self.file.set_len(new_file_size)?;
        self.file_len = new_file_size;
        self.file.sync_all()?;

        Ok(old_file_size.saturating_sub(new_file_size))
    }
}

/// Contenedor unificado de Disco Virtual (`VDISK` en `VDInternal.h`), que abstrae
/// tanto imágenes nativas `.vdi` de VirtualBox como imágenes planas `.img` (`RAW`).
pub enum VirtualDiskBackend {
    Vdi(VdiDisk),
    Raw { path: PathBuf, file: File, size: u64 },
}

pub struct VirtualDisk {
    pub backend: VirtualDiskBackend,
}

impl VirtualDisk {
    /// Detecta automáticamente si `path` es una imagen VDI (`0xbeda107f`) o RAW y la abre.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let p = path.as_ref();
        if Self::is_vdi_file(p) {
            let vdi = VdiDisk::open(p)?;
            Ok(Self {
                backend: VirtualDiskBackend::Vdi(vdi),
            })
        } else {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(p)
                .or_else(|_| OpenOptions::new().read(true).open(p))?;
            let size = file.seek(SeekFrom::End(0))?;
            let _ = file.seek(SeekFrom::Start(0));
            Ok(Self {
                backend: VirtualDiskBackend::Raw {
                    path: p.to_path_buf(),
                    file,
                    size,
                },
            })
        }
    }

    /// Crea un disco virtual VDI (dinámico o fijo) compatible con VirtualBox 7.2.
    pub fn create_vdi(
        path: impl AsRef<Path>,
        size_bytes: u64,
        fixed: bool,
        comment: &str,
    ) -> std::io::Result<Self> {
        let vdi = VdiDisk::create(path, size_bytes, fixed, comment)?;
        Ok(Self {
            backend: VirtualDiskBackend::Vdi(vdi),
        })
    }

    /// Crea un disco virtual RAW (`.img`) disperso del tamaño indicado.
    pub fn create_raw(path: impl AsRef<Path>, size_bytes: u64) -> std::io::Result<Self> {
        let p = path.as_ref().to_path_buf();
        let size_aligned = (size_bytes + 511) & !511;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&p)?;
        file.set_len(size_aligned)?;
        file.sync_all()?;
        Ok(Self {
            backend: VirtualDiskBackend::Raw {
                path: p,
                file,
                size: size_aligned,
            },
        })
    }

    /// Comprueba si un archivo tiene la firma `VDI_IMAGE_SIGNATURE` (`vdiProbe` en `VDI.cpp`).
    pub fn is_vdi_file(path: impl AsRef<Path>) -> bool {
        if let Ok(mut f) = File::open(path.as_ref()) {
            let mut pre = [0u8; VDI_PREHEADER_SIZE];
            if f.read_exact(&mut pre).is_ok() {
                let sig = u32::from_le_bytes([pre[64], pre[65], pre[66], pre[67]]);
                return sig == VDI_IMAGE_SIGNATURE;
            }
        }
        false
    }

    /// Obtiene los metadatos de cualquier imagen de disco (`.vdi` o `.img`) sin cargar toda la tabla.
    pub fn probe_info(path: impl AsRef<Path>) -> std::io::Result<DiskInfo> {
        let p = path.as_ref().to_path_buf();
        let mut f = File::open(&p)?;
        let actual_size = f.metadata()?.len();

        let mut pre = [0u8; VDI_PREHEADER_SIZE];
        if actual_size >= (VDI_PREHEADER_SIZE as u64 + VDI_HEADER1_SIZE as u64)
            && f.read_exact(&mut pre).is_ok()
        {
            let sig = u32::from_le_bytes([pre[64], pre[65], pre[66], pre[67]]);
            if sig == VDI_IMAGE_SIGNATURE {
                if let Ok(hdr) = VdiHeader::from_reader(&mut f) {
                    return Ok(DiskInfo {
                        path: p,
                        format: DiskFormat::Vdi(hdr.image_type),
                        virtual_size: hdr.cb_disk,
                        actual_size,
                        block_size: hdr.cb_block,
                        blocks_total: hdr.c_blocks,
                        blocks_allocated: hdr.c_blocks_allocated,
                        uuid_create: format_uuid(&hdr.uuid_create),
                        comment: hdr.comment,
                    });
                }
            }
        }

        let sectors = (actual_size / 512) as u32;
        Ok(DiskInfo {
            path: p,
            format: DiskFormat::Raw,
            virtual_size: actual_size,
            actual_size,
            block_size: 512,
            blocks_total: sectors,
            blocks_allocated: sectors,
            uuid_create: String::from("00000000-0000-0000-0000-000000000000"),
            comment: String::from("Imagen de disco RAW"),
        })
    }

    /// Devuelve la capacidad lógica del disco en bytes (`VDGetSize`).
    pub fn logical_size(&self) -> u64 {
        match &self.backend {
            VirtualDiskBackend::Vdi(vdi) => vdi.header.cb_disk,
            VirtualDiskBackend::Raw { size, .. } => *size,
        }
    }

    /// Devuelve el formato del disco abierto.
    pub fn format(&self) -> DiskFormat {
        match &self.backend {
            VirtualDiskBackend::Vdi(vdi) => DiskFormat::Vdi(vdi.header.image_type),
            VirtualDiskBackend::Raw { .. } => DiskFormat::Raw,
        }
    }

    /// Lee `count` sectores de 512 bytes comenzando en `lba`.
    pub fn read_sectors(&mut self, lba: u64, count: u32, dst: &mut [u8]) -> std::io::Result<()> {
        let offset = lba * 512;
        let total = (count as usize) * 512;
        let len = dst.len().min(total);
        match &mut self.backend {
            VirtualDiskBackend::Vdi(vdi) => vdi.read_at(offset, &mut dst[..len]),
            VirtualDiskBackend::Raw { file, size, .. } => {
                if offset >= *size {
                    dst[..len].fill(0);
                    return Ok(());
                }
                let avail = ((*size - offset) as usize).min(len);
                if file.read_exact_at(&mut dst[..avail], offset).is_err() {
                    dst[..avail].fill(0);
                }
                if avail < len {
                    dst[avail..len].fill(0);
                }
                Ok(())
            }
        }
    }

    /// Escribe `count` sectores de 512 bytes comenzando en `lba`.
    pub fn write_sectors(&mut self, lba: u64, count: u32, src: &[u8]) -> std::io::Result<()> {
        let offset = lba * 512;
        let total = (count as usize) * 512;
        let len = src.len().min(total);
        match &mut self.backend {
            VirtualDiskBackend::Vdi(vdi) => vdi.write_at(offset, &src[..len]),
            VirtualDiskBackend::Raw { file, size, .. } => {
                file.write_all_at(&src[..len], offset)?;
                if offset + (len as u64) > *size {
                    *size = offset + (len as u64);
                }
                Ok(())
            }
        }
    }

    /// Sincroniza los cambios pendientes con el almacenamiento del host (`VDFlush`).
    pub fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.backend {
            VirtualDiskBackend::Vdi(vdi) => vdi.file.sync_data(),
            VirtualDiskBackend::Raw { file, .. } => file.sync_data(),
        }
    }

    /// Compacta el disco si es un archivo VDI dinámico (`VDCompact`).
    pub fn compact(&mut self) -> std::io::Result<u64> {
        match &mut self.backend {
            VirtualDiskBackend::Vdi(vdi) => vdi.compact(),
            VirtualDiskBackend::Raw { .. } => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vdi_dynamic_create_read_write_and_compact() {
        let tmp_path = std::env::temp_dir().join(format!("test_vb_disk_{}.vdi", std::process::id()));
        let _ = std::fs::remove_file(&tmp_path);

        // 1. Crear disco VDI dinámico de 16 MiB
        let size = 16 * 1024 * 1024u64;
        {
            let mut disk = VirtualDisk::create_vdi(&tmp_path, size, false, "Test VBox VDI").unwrap();
            assert_eq!(disk.logical_size(), size);
            let info = VirtualDisk::probe_info(&tmp_path).unwrap();
            assert_eq!(info.format, DiskFormat::Vdi(VdiImageType::Normal));
            assert_eq!(info.blocks_total, 16);
            assert_eq!(info.blocks_allocated, 0);
            assert_eq!(info.actual_size, 2 * 1024 * 1024); // offData = 2 MiB

            // 2. Escribir datos reales en LBA 2048 (inicio del bloque 1 = 1 MiB)
            let mut sector = [0xA5u8; 512];
            sector[0..4].copy_from_slice(b"VBOX");
            disk.write_sectors(2048, 1, &sector).unwrap();
            disk.flush().unwrap();

            // 3. Escribir un bloque entero de ceros en el bloque 2 (no debe asignar espacio físico)
            let zero_sector = [0u8; 512];
            disk.write_sectors(4096, 1, &zero_sector).unwrap();
            disk.flush().unwrap();
        }

        // 4. Reabrir el disco y verificar persistencia y bloques asignados
        {
            let info = VirtualDisk::probe_info(&tmp_path).unwrap();
            assert_eq!(info.blocks_allocated, 1);
            assert_eq!(info.actual_size, 3 * 1024 * 1024); // 2 MiB header + 1 MiB bloque asignado

            let mut disk = VirtualDisk::open(&tmp_path).unwrap();
            let mut read_buf = [0u8; 512];
            disk.read_sectors(2048, 1, &mut read_buf).unwrap();
            assert_eq!(&read_buf[0..4], b"VBOX");
            assert_eq!(read_buf[4], 0xA5);

            // Sobrescribir con ceros el sector de LBA 2048 y compactar el VDI
            let zero_sector = [0u8; 512];
            disk.write_sectors(2048, 1, &zero_sector).unwrap();
            let saved = disk.compact().unwrap();
            assert_eq!(saved, 1024 * 1024);
        }

        let info_after = VirtualDisk::probe_info(&tmp_path).unwrap();
        assert_eq!(info_after.blocks_allocated, 0);
        let _ = std::fs::remove_file(&tmp_path);
    }
}
