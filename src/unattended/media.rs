//! Constructor de Medios y Sistema de Archivos FAT en Memoria (OEMDRV / CIDATA).
//!
//! Generador en memoria en Rust puro para crear imágenes de disco con sistema
//! de archivos FAT12 o FAT16 estándar sin dependencias externas ni herramientas del sistema.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Atributos de entrada de directorio FAT estándar.
pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;

/// Firma canónica del sector de arranque (Boot Sector) en offset 510..512.
pub const BOOT_SIGNATURE: [u8; 2] = [0x55, 0xAA];

/// Media descriptor estándar para disco fijo / disco duro virtual (0xF8) y floppy (0xF0).
pub const MEDIA_DESCRIPTOR_HARD_DISK: u8 = 0xF8;
pub const MEDIA_DESCRIPTOR_FLOPPY: u8 = 0xF0;

/// Convierte una cadena de texto de etiqueta de volumen en un arreglo de 11 bytes
/// rellenado con espacios (0x20).
pub fn format_volume_label(label: &str) -> [u8; 11] {
    let mut buf = [0x20u8; 11];
    let bytes = label.as_bytes();
    let len = bytes.len().min(11);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// Convierte un nombre de archivo (ruta relativa o simple) al formato 8.3 de FAT.
///
/// Soporta conversiones estándar como:
/// - `preseed.cfg` -> `PRESEED CFG` (leído como `PRESEED.CFG`)
/// - `user-data`   -> `USER-DAT   ` (leído como `USER-DAT`)
/// - `meta-data`   -> `META-DAT   ` (leído como `META-DAT`)
/// - `autounattend.xml` -> `AUTOUNATXML` (leído como `AUTOUNAT.XML`)
/// - `ks.cfg`      -> `KS      CFG` (leído como `KS.CFG`)
pub fn format_8_3_name(name: &str) -> Result<[u8; 11], String> {
    let clean = name.trim_start_matches('/').trim();
    if clean.is_empty() {
        return Err("El nombre de archivo no puede estar vacío".to_string());
    }

    let (raw_base, raw_ext) = match clean.rfind('.') {
        Some(pos) => (&clean[..pos], &clean[pos + 1..]),
        None => (clean, ""),
    };

    let base_upper = raw_base.to_ascii_uppercase();
    let ext_upper = raw_ext.to_ascii_uppercase();

    let mut result = [0x20u8; 11];

    let base_bytes = base_upper.as_bytes();
    let base_len = base_bytes.len().min(8);
    result[..base_len].copy_from_slice(&base_bytes[..base_len]);

    let ext_bytes = ext_upper.as_bytes();
    let ext_len = ext_bytes.len().min(3);
    result[8..8 + ext_len].copy_from_slice(&ext_bytes[..ext_len]);

    Ok(result)
}

/// Parsea un nombre 8.3 de 11 bytes a un `String` estándar (con punto si hay extensión).
pub fn parse_8_3_name(raw: &[u8; 11]) -> String {
    let name_str = std::str::from_utf8(&raw[0..8]).unwrap_or("").trim_end();
    let ext_str = std::str::from_utf8(&raw[8..11]).unwrap_or("").trim_end();
    if ext_str.is_empty() {
        name_str.to_string()
    } else {
        format!("{}.{}", name_str, ext_str)
    }
}

/// Genera una imagen FAT16 estándar en memoria.
///
/// Por defecto produce un disco de 4 MB (8,192 sectores de 512 bytes), que se adapta
/// perfectamente a dispositivos AHCI y discos duros virtuales ATA. Si el contenido
/// excede dicho tamaño, la imagen se expande dinámicamente en múltiplos de clústeres.
pub fn build_fat16_image(volume_label: &str, files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let bytes_per_sector: u16 = 512;
    let sectors_per_cluster: u8 = 1;
    let cluster_size: usize = (bytes_per_sector as usize) * (sectors_per_cluster as usize);
    let reserved_sectors: u16 = 1;
    let num_fats: u8 = 2;
    let root_entries: u16 = 512;
    let root_dir_sectors: u16 = ((root_entries as u32 * 32 + bytes_per_sector as u32 - 1)
        / bytes_per_sector as u32) as u16; // 32 sectores

    // Calcular cuántos clústeres de datos se necesitan para los archivos
    let mut total_clusters_needed: usize = 0;
    for &(_name, data) in files {
        if !data.is_empty() {
            let clus = (data.len() + cluster_size - 1) / cluster_size;
            total_clusters_needed += clus;
        }
    }

    // Para FAT16, el número de clústeres debe ser al menos 4085 y menor que 65525.
    // Usamos 8095 clústeres como tamaño base (~4 MB = 8192 sectores).
    let target_clusters = std::cmp::max(total_clusters_needed + 100, 8095);
    if target_clusters >= 65520 {
        return Err("El contenido es demasiado grande para un sistema FAT16 estándar".to_string());
    }

    // Calcular sectores por FAT necesarios para albergar target_clusters entradas de 2 bytes
    let fat_entries_needed = target_clusters + 2;
    let fat_bytes_needed = fat_entries_needed * 2;
    let sectors_per_fat = ((fat_bytes_needed + (bytes_per_sector as usize) - 1)
        / (bytes_per_sector as usize)) as u16;

    let overhead_sectors = (reserved_sectors as u32)
        + (num_fats as u32) * (sectors_per_fat as u32)
        + (root_dir_sectors as u32);
    let total_data_sectors = target_clusters as u32 * (sectors_per_cluster as u32);
    let mut total_sectors = overhead_sectors + total_data_sectors;

    // Alinear tamaño total a múltiplos de 64 sectores para compatibilidad con geometrías LBA
    if total_sectors % 64 != 0 {
        total_sectors += 64 - (total_sectors % 64);
    }
    // Asegurar tamaño mínimo de 8192 sectores (4 MB)
    if total_sectors < 8192 {
        total_sectors = 8192;
    }

    let actual_data_sectors = total_sectors - overhead_sectors;
    let actual_clusters = (actual_data_sectors / (sectors_per_cluster as u32)) as usize;

    let total_image_bytes = (total_sectors as usize) * (bytes_per_sector as usize);
    let mut img = vec![0u8; total_image_bytes];

    // ==========================================
    // 1. Boot Sector / BIOS Parameter Block (BPB)
    // ==========================================
    // Jump instruction: EB 3C 90 (jmp short 0x3E, nop)
    img[0x00..0x03].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    // OEM Name: "MSWIN4.1"
    img[0x03..0x0B].copy_from_slice(b"MSWIN4.1");
    // Bytes per sector
    img[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
    // Sectors per cluster
    img[0x0D] = sectors_per_cluster;
    // Reserved sectors
    img[0x0E..0x10].copy_from_slice(&reserved_sectors.to_le_bytes());
    // Number of FATs
    img[0x10] = num_fats;
    // Root directory entries
    img[0x11..0x13].copy_from_slice(&root_entries.to_le_bytes());
    // Total sectors 16-bit
    let sectors_16 = if total_sectors < 65536 {
        total_sectors as u16
    } else {
        0
    };
    img[0x13..0x15].copy_from_slice(&sectors_16.to_le_bytes());
    // Media descriptor
    img[0x15] = MEDIA_DESCRIPTOR_HARD_DISK;
    // Sectors per FAT
    img[0x16..0x18].copy_from_slice(&sectors_per_fat.to_le_bytes());
    // Sectors per track
    img[0x18..0x1A].copy_from_slice(&32u16.to_le_bytes());
    // Number of heads
    img[0x1A..0x1C].copy_from_slice(&64u16.to_le_bytes());
    // Hidden sectors
    img[0x1C..0x20].copy_from_slice(&0u32.to_le_bytes());
    // Total sectors 32-bit
    let sectors_32 = if total_sectors >= 65536 {
        total_sectors
    } else {
        0
    };
    img[0x20..0x24].copy_from_slice(&sectors_32.to_le_bytes());

    // Extended BPB (FAT16)
    img[0x24] = 0x80; // Physical drive number (0x80 = Hard disk)
    img[0x25] = 0x00; // Reserved
    img[0x26] = 0x29; // Extended boot signature
    img[0x27..0x2B].copy_from_slice(&0x25500800u32.to_le_bytes()); // Serial Number
    let formatted_label = format_volume_label(volume_label);
    img[0x2B..0x36].copy_from_slice(&formatted_label); // Volume Label (11 bytes)
    img[0x36..0x3E].copy_from_slice(b"FAT16   "); // Filesystem type string

    // Boot sector signature (0x55, 0xAA)
    img[0x1FE] = BOOT_SIGNATURE[0];
    img[0x1FF] = BOOT_SIGNATURE[1];

    // ==========================================
    // 2. Offsets de FAT, Directorio Raíz y Datos
    // ==========================================
    let fat1_start = (reserved_sectors as usize) * (bytes_per_sector as usize);
    let fat_size_bytes = (sectors_per_fat as usize) * (bytes_per_sector as usize);
    let fat2_start = fat1_start + fat_size_bytes;
    let root_dir_start = fat2_start + fat_size_bytes;
    let root_dir_size = (root_entries as usize) * 32;
    let data_start = root_dir_start + root_dir_size;

    // Inicializar FAT1 (entradas 0 y 1 reservadas)
    // Entrada 0: Media descriptor en byte bajo + 0xFF00 -> 0xFFF8
    img[fat1_start..fat1_start + 2]
        .copy_from_slice(&(0xFF00u16 | (MEDIA_DESCRIPTOR_HARD_DISK as u16)).to_le_bytes());
    // Entrada 1: EOF marker / dirty flag -> 0xFFFF
    img[fat1_start + 2..fat1_start + 4].copy_from_slice(&0xFFFFu16.to_le_bytes());

    // ==========================================
    // 3. Directorio Raíz: Entrada de Volume Label
    // ==========================================
    // Entrada 0 en el Root Directory representa el Volume Label (ATTR_VOLUME_ID = 0x08)
    let vol_entry = &mut img[root_dir_start..root_dir_start + 32];
    vol_entry[0x00..0x0B].copy_from_slice(&formatted_label);
    vol_entry[0x0B] = ATTR_VOLUME_ID;
    // Marca de tiempo DOS estándar (2026-01-01 12:00:00)
    let dos_time: u16 = (12 << 11) | (0 << 5) | 0;
    let dos_date: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
    vol_entry[0x0E..0x10].copy_from_slice(&dos_time.to_le_bytes());
    vol_entry[0x10..0x12].copy_from_slice(&dos_date.to_le_bytes());
    vol_entry[0x16..0x18].copy_from_slice(&dos_time.to_le_bytes());
    vol_entry[0x18..0x1A].copy_from_slice(&dos_date.to_le_bytes());
    // Clúster inicial 0, tamaño 0

    // ==========================================
    // 4. Escribir Archivos, Asignar FAT y Directorio
    // ==========================================
    let mut next_free_cluster: usize = 2;
    let mut dir_entry_index: usize = 1; // Entrada 0 es el Volume Label

    for &(filename, data) in files {
        if dir_entry_index >= root_entries as usize {
            return Err("Demasiados archivos para la tabla del directorio raíz".to_string());
        }

        let name_8_3 = format_8_3_name(filename)?;
        let entry_offset = root_dir_start + (dir_entry_index * 32);

        let file_size = data.len();
        let (first_cluster, clusters_allocated) = if file_size == 0 {
            (0u16, 0usize)
        } else {
            let needed = (file_size + cluster_size - 1) / cluster_size;
            if next_free_cluster + needed - 2 > actual_clusters {
                return Err("Espacio de datos insuficiente en la imagen FAT16".to_string());
            }

            let start_clus = next_free_cluster;
            for i in 0..needed {
                let curr = start_clus + i;
                let next_val: u16 = if i + 1 < needed {
                    (curr + 1) as u16
                } else {
                    0xFFFF // Fin de cadena de clústeres (EOF)
                };

                // Asignar en FAT1
                let fat_entry_pos = fat1_start + (curr * 2);
                img[fat_entry_pos..fat_entry_pos + 2].copy_from_slice(&next_val.to_le_bytes());

                // Copiar datos del archivo en el clúster correspondiente
                let cluster_data_offset = data_start + (curr - 2) * cluster_size;
                let chunk_start = i * cluster_size;
                let chunk_end = std::cmp::min(chunk_start + cluster_size, file_size);
                let chunk_len = chunk_end - chunk_start;

                img[cluster_data_offset..cluster_data_offset + chunk_len]
                    .copy_from_slice(&data[chunk_start..chunk_end]);
            }

            next_free_cluster += needed;
            (start_clus as u16, needed)
        };

        let _ = clusters_allocated;

        let has_lower_base = filename
            .split('.')
            .next()
            .map_or(false, |b| b.chars().any(|c| c.is_ascii_lowercase()));
        let has_lower_ext = filename
            .split('.')
            .nth(1)
            .map_or(false, |e| e.chars().any(|c| c.is_ascii_lowercase()));
        let mut nt_flags = 0u8;
        if has_lower_base {
            nt_flags |= 0x08; // Base en minúsculas
        }
        if has_lower_ext {
            nt_flags |= 0x10; // Extensión en minúsculas
        }

        // Llenar entrada de directorio primaria (con nt_flags para soportar nombres en minúsculas en Linux)
        let dir_entry = &mut img[entry_offset..entry_offset + 32];
        dir_entry[0x00..0x0B].copy_from_slice(&name_8_3);
        dir_entry[0x0B] = ATTR_ARCHIVE;
        dir_entry[0x0C] = nt_flags; // Reserved Windows NT (0x18 si es minúscula)
        dir_entry[0x0D] = 0x00; // Creation time ms
        dir_entry[0x0E..0x10].copy_from_slice(&dos_time.to_le_bytes());
        dir_entry[0x10..0x12].copy_from_slice(&dos_date.to_le_bytes());
        dir_entry[0x12..0x14].copy_from_slice(&dos_date.to_le_bytes()); // Last access date
        dir_entry[0x14..0x16].copy_from_slice(&0u16.to_le_bytes()); // Clúster alto (0 en FAT16)
        dir_entry[0x16..0x18].copy_from_slice(&dos_time.to_le_bytes()); // Write time
        dir_entry[0x18..0x1A].copy_from_slice(&dos_date.to_le_bytes()); // Write date
        dir_entry[0x1A..0x1C].copy_from_slice(&first_cluster.to_le_bytes()); // Clúster bajo
        dir_entry[0x1C..0x20].copy_from_slice(&(file_size as u32).to_le_bytes());

        dir_entry_index += 1;

        // Si el archivo original contenía minúsculas, crear también una entrada duplicada estrictamente
        // mayúscula (nt_flags = 0x00) apuntando al mismo clúster para máxima compatibilidad con DOS/instaladores
        if nt_flags != 0 && dir_entry_index < root_entries as usize {
            let dup_offset = root_dir_start + (dir_entry_index * 32);
            let dup_entry = &mut img[dup_offset..dup_offset + 32];
            dup_entry[0x00..0x0B].copy_from_slice(&name_8_3);
            dup_entry[0x0B] = ATTR_ARCHIVE;
            dup_entry[0x0C] = 0x00; // Strict uppercase
            dup_entry[0x0D] = 0x00;
            dup_entry[0x0E..0x10].copy_from_slice(&dos_time.to_le_bytes());
            dup_entry[0x10..0x12].copy_from_slice(&dos_date.to_le_bytes());
            dup_entry[0x12..0x14].copy_from_slice(&dos_date.to_le_bytes());
            dup_entry[0x14..0x16].copy_from_slice(&0u16.to_le_bytes());
            dup_entry[0x16..0x18].copy_from_slice(&dos_time.to_le_bytes());
            dup_entry[0x18..0x1A].copy_from_slice(&dos_date.to_le_bytes());
            dup_entry[0x1A..0x1C].copy_from_slice(&first_cluster.to_le_bytes());
            dup_entry[0x1C..0x20].copy_from_slice(&(file_size as u32).to_le_bytes());

            dir_entry_index += 1;
        }
    }

    // ==========================================
    // 5. Duplicar FAT1 en FAT2
    // ==========================================
    img.copy_within(fat1_start..fat1_start + fat_size_bytes, fat2_start);

    Ok(img)
}

/// Helper para manipular entradas de 12 bits en la tabla FAT12.
fn set_fat12_entry(fat: &mut [u8], k: usize, v: u16) {
    let offset = (k * 3) / 2;
    if k % 2 == 0 {
        fat[offset] = (v & 0xFF) as u8;
        fat[offset + 1] = (fat[offset + 1] & 0xF0) | (((v >> 8) & 0x0F) as u8);
    } else {
        fat[offset] = (fat[offset] & 0x0F) | (((v << 4) & 0xF0) as u8);
        fat[offset + 1] = ((v >> 4) & 0xFF) as u8;
    }
}

/// Helper para leer entradas de 12 bits en la tabla FAT12.
fn get_fat12_entry(fat: &[u8], k: usize) -> u16 {
    let offset = (k * 3) / 2;
    if k % 2 == 0 {
        (fat[offset] as u16) | (((fat[offset + 1] & 0x0F) as u16) << 8)
    } else {
        (((fat[offset] & 0xF0) as u16) >> 4) | ((fat[offset + 1] as u16) << 4)
    }
}

/// Genera una imagen FAT12 estándar de 1.44 MB (2880 sectores de 512 bytes).
pub fn build_fat12_image(volume_label: &str, files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let bytes_per_sector: u16 = 512;
    let sectors_per_cluster: u8 = 1;
    let cluster_size: usize = (bytes_per_sector as usize) * (sectors_per_cluster as usize);
    let reserved_sectors: u16 = 1;
    let num_fats: u8 = 2;
    let root_entries: u16 = 224;
    let sectors_per_fat: u16 = 9;
    let total_sectors: u16 = 2880;

    let root_dir_sectors = ((root_entries as u32 * 32 + bytes_per_sector as u32 - 1)
        / bytes_per_sector as u32) as u16; // 14 sectores
    let overhead_sectors = reserved_sectors + (num_fats as u16) * sectors_per_fat + root_dir_sectors;
    let actual_data_sectors = total_sectors - overhead_sectors; // 2847 clústeres
    let actual_clusters = (actual_data_sectors / sectors_per_cluster as u16) as usize;

    let total_image_bytes = (total_sectors as usize) * (bytes_per_sector as usize);
    let mut img = vec![0u8; total_image_bytes];

    // BPB FAT12
    img[0x00..0x03].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    img[0x03..0x0B].copy_from_slice(b"MSWIN4.1");
    img[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
    img[0x0D] = sectors_per_cluster;
    img[0x0E..0x10].copy_from_slice(&reserved_sectors.to_le_bytes());
    img[0x10] = num_fats;
    img[0x11..0x13].copy_from_slice(&root_entries.to_le_bytes());
    img[0x13..0x15].copy_from_slice(&total_sectors.to_le_bytes());
    img[0x15] = MEDIA_DESCRIPTOR_FLOPPY;
    img[0x16..0x18].copy_from_slice(&sectors_per_fat.to_le_bytes());
    img[0x18..0x1A].copy_from_slice(&18u16.to_le_bytes()); // Sectors per track
    img[0x1A..0x1C].copy_from_slice(&2u16.to_le_bytes()); // Number of heads
    img[0x1C..0x20].copy_from_slice(&0u32.to_le_bytes()); // Hidden sectors
    img[0x20..0x24].copy_from_slice(&0u32.to_le_bytes()); // 32-bit total sectors (0)

    // Extended BPB (FAT12)
    img[0x24] = 0x00; // Floppy drive number
    img[0x25] = 0x00; // Reserved
    img[0x26] = 0x29; // Extended boot signature
    img[0x27..0x2B].copy_from_slice(&0x25500801u32.to_le_bytes()); // Serial
    let formatted_label = format_volume_label(volume_label);
    img[0x2B..0x36].copy_from_slice(&formatted_label);
    img[0x36..0x3E].copy_from_slice(b"FAT12   ");

    img[0x1FE] = BOOT_SIGNATURE[0];
    img[0x1FF] = BOOT_SIGNATURE[1];

    let fat1_start = (reserved_sectors as usize) * (bytes_per_sector as usize);
    let fat_size_bytes = (sectors_per_fat as usize) * (bytes_per_sector as usize);
    let fat2_start = fat1_start + fat_size_bytes;
    let root_dir_start = fat2_start + fat_size_bytes;
    let root_dir_size = (root_entries as usize) * 32;
    let data_start = root_dir_start + root_dir_size;

    // Inicializar FAT1 para FAT12
    set_fat12_entry(
        &mut img[fat1_start..fat1_start + fat_size_bytes],
        0,
        0x0F00 | (MEDIA_DESCRIPTOR_FLOPPY as u16),
    );
    set_fat12_entry(&mut img[fat1_start..fat1_start + fat_size_bytes], 1, 0x0FFF);

    // Entrada 0 de Volume Label en Directorio Raíz
    let vol_entry = &mut img[root_dir_start..root_dir_start + 32];
    vol_entry[0x00..0x0B].copy_from_slice(&formatted_label);
    vol_entry[0x0B] = ATTR_VOLUME_ID;
    let dos_time: u16 = (12 << 11) | (0 << 5) | 0;
    let dos_date: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
    vol_entry[0x0E..0x10].copy_from_slice(&dos_time.to_le_bytes());
    vol_entry[0x10..0x12].copy_from_slice(&dos_date.to_le_bytes());
    vol_entry[0x16..0x18].copy_from_slice(&dos_time.to_le_bytes());
    vol_entry[0x18..0x1A].copy_from_slice(&dos_date.to_le_bytes());

    let mut next_free_cluster: usize = 2;
    let mut dir_entry_index: usize = 1;

    for &(filename, data) in files {
        if dir_entry_index >= root_entries as usize {
            return Err("Demasiados archivos para la tabla del directorio raíz FAT12".to_string());
        }

        let name_8_3 = format_8_3_name(filename)?;
        let entry_offset = root_dir_start + (dir_entry_index * 32);

        let file_size = data.len();
        let first_cluster = if file_size == 0 {
            0u16
        } else {
            let needed = (file_size + cluster_size - 1) / cluster_size;
            if next_free_cluster + needed - 2 > actual_clusters {
                return Err("Espacio de datos insuficiente en la imagen FAT12".to_string());
            }

            let start_clus = next_free_cluster;
            for i in 0..needed {
                let curr = start_clus + i;
                let next_val: u16 = if i + 1 < needed {
                    (curr + 1) as u16
                } else {
                    0x0FFF // Fin de cadena FAT12
                };

                set_fat12_entry(
                    &mut img[fat1_start..fat1_start + fat_size_bytes],
                    curr,
                    next_val,
                );

                let cluster_data_offset = data_start + (curr - 2) * cluster_size;
                let chunk_start = i * cluster_size;
                let chunk_end = std::cmp::min(chunk_start + cluster_size, file_size);
                let chunk_len = chunk_end - chunk_start;

                img[cluster_data_offset..cluster_data_offset + chunk_len]
                    .copy_from_slice(&data[chunk_start..chunk_end]);
            }

            next_free_cluster += needed;
            start_clus as u16
        };

        let dir_entry = &mut img[entry_offset..entry_offset + 32];
        dir_entry[0x00..0x0B].copy_from_slice(&name_8_3);
        dir_entry[0x0B] = ATTR_ARCHIVE;
        dir_entry[0x0E..0x10].copy_from_slice(&dos_time.to_le_bytes());
        dir_entry[0x10..0x12].copy_from_slice(&dos_date.to_le_bytes());
        dir_entry[0x12..0x14].copy_from_slice(&dos_date.to_le_bytes());
        dir_entry[0x14..0x16].copy_from_slice(&0u16.to_le_bytes());
        dir_entry[0x16..0x18].copy_from_slice(&dos_time.to_le_bytes());
        dir_entry[0x18..0x1A].copy_from_slice(&dos_date.to_le_bytes());
        dir_entry[0x1A..0x1C].copy_from_slice(&first_cluster.to_le_bytes());
        dir_entry[0x1C..0x20].copy_from_slice(&(file_size as u32).to_le_bytes());

        dir_entry_index += 1;
    }

    // Duplicar FAT1 en FAT2
    img.copy_within(fat1_start..fat1_start + fat_size_bytes, fat2_start);

    Ok(img)
}

/// Envuelve una imagen FAT16 dentro de un disco con tabla de particiones MBR estándar.
///
/// Crea un Sector 0 (MBR) con una partición activa (0x80) de tipo 0x06 (FAT16)
/// que apunta a LBA 64. Además, implementa un esquema Dual-BPB donde tanto el disco entero
/// (LBA 0, superfloppy) como la partición (LBA 64) comparten de manera idéntica
/// la misma tabla FAT y el mismo directorio raíz.
pub fn wrap_mbr_partition(fat16_part: &[u8], start_lba: u32, _volume_label: &str) -> Vec<u8> {
    let part_sectors = (fat16_part.len() / 512) as u32;
    let total_sectors = start_lba + part_sectors;
    let mut disk_img = vec![0u8; (total_sectors as usize) * 512];

    // 1. Copiar la imagen FAT16 en la partición 1 (a partir de start_lba)
    let part_start_byte = (start_lba as usize) * 512;
    disk_img[part_start_byte..part_start_byte + fat16_part.len()].copy_from_slice(fat16_part);

    // Ajustar hidden_sectors en el BPB de la partición 1 (offset 0x1C)
    disk_img[part_start_byte + 0x1C..part_start_byte + 0x20].copy_from_slice(&start_lba.to_le_bytes());

    // 2. Configurar Sector 0: Dual BPB + Tabla MBR
    // Copiar el BPB base de la partición al Sector 0 (hasta antes de la tabla de particiones MBR en 0x1BE)
    disk_img[0x00..0x1BE].copy_from_slice(&fat16_part[0x00..0x1BE]);

    // En Sector 0:
    // - hidden_sectors = 0
    disk_img[0x1C..0x20].copy_from_slice(&0u32.to_le_bytes());
    // - reserved_sectors = (start_lba as u16) + part_reserved_sectors (normalmente 64 + 1 = 65)
    let part_reserved = u16::from_le_bytes([fat16_part[0x0E], fat16_part[0x0F]]);
    let disk_reserved = (start_lba as u16) + part_reserved;
    disk_img[0x0E..0x10].copy_from_slice(&disk_reserved.to_le_bytes());

    // - total_sectors del disco entero
    if total_sectors < 65536 {
        disk_img[0x13..0x15].copy_from_slice(&(total_sectors as u16).to_le_bytes());
        disk_img[0x20..0x24].copy_from_slice(&0u32.to_le_bytes());
    } else {
        disk_img[0x13..0x15].copy_from_slice(&0u16.to_le_bytes());
        disk_img[0x20..0x24].copy_from_slice(&total_sectors.to_le_bytes());
    }

    // 3. Escribir entrada de Partición 1 en la tabla MBR (offset 0x1BE..0x1CE)
    let mbr_entry = &mut disk_img[0x1BE..0x1CE];
    mbr_entry[0x00] = 0x80; // Estado: Activa / Booteable
    mbr_entry[0x01] = 0x01; // CHS Start Head (1)
    mbr_entry[0x02] = 0x01; // CHS Start Sector (1)
    mbr_entry[0x03] = 0x00; // CHS Start Cylinder (0)
    mbr_entry[0x04] = 0x06; // Tipo de partición: FAT16
    mbr_entry[0x05] = 0xFE; // CHS End Head
    mbr_entry[0x06] = 0xFF; // CHS End Sector
    mbr_entry[0x07] = 0xFF; // CHS End Cylinder
    mbr_entry[0x08..0x0C].copy_from_slice(&start_lba.to_le_bytes()); // LBA inicial (64)
    mbr_entry[0x0C..0x10].copy_from_slice(&part_sectors.to_le_bytes()); // Número de sectores

    // Firma de arranque en Sector 0 (0x55, 0xAA)
    disk_img[0x1FE] = BOOT_SIGNATURE[0];
    disk_img[0x1FF] = BOOT_SIGNATURE[1];

    disk_img
}

/// Construye el buffer binario completo (`Vec<u8>`) de la imagen FAT estándar.
///
/// Utiliza el estándar FAT16 con particionado MBR y esquema Dual-BPB (LBA 0 y LBA 64)
/// para garantizar compatibilidad universal tanto si el sistema huésped monta el disco
/// entero (/dev/sdb) como si monta la partición (/dev/sdb1), con soporte nativo para
/// Debian, Kali, RedHat, Ubuntu Cloud-Init y Windows.
pub fn build_fat_image(volume_label: &str, files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let raw = build_fat16_image(volume_label, files)?;
    Ok(wrap_mbr_partition(&raw, 64, volume_label))
}

/// Construye una imagen de disco con la etiqueta `"OEMDRV"`.
///
/// Los instaladores Debian, Kali y RedHat Anaconda buscan automáticamente un disco
/// con label `"OEMDRV"` y cargan `/preseed.cfg` o `/ks.cfg`.
pub fn build_oemdrv_image(files: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    build_fat_image("OEMDRV", files)
}

/// Construye una imagen de disco NoCloud de Cloud-Init con la etiqueta `"cidata"`,
/// conteniendo los archivos `user-data` y `meta-data`.
pub fn build_cidata_image(user_data: &str, meta_data: &str) -> Result<Vec<u8>, String> {
    build_fat_image(
        "cidata",
        &[
            ("user-data", user_data.as_bytes()),
            ("meta-data", meta_data.as_bytes()),
        ],
    )
}

/// Guarda la imagen FAT en un archivo temporal para que pueda ser montado como disco virtual en el hipervisor.
///
/// Si los archivos contienen `user-data` o `meta-data`, asigna la etiqueta `"cidata"`.
/// En cualquier otro caso, asigna `"OEMDRV"`.
pub fn create_temp_oemdrv_file(files: &[(&str, &[u8])]) -> Result<PathBuf, String> {
    let is_cidata = files.iter().any(|(name, _)| {
        let lower = name.to_ascii_lowercase();
        lower == "user-data" || lower == "meta-data"
    });
    let label = if is_cidata { "cidata" } else { "OEMDRV" };

    let img_bytes = build_fat_image(label, files)?;

    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let filename = format!("tff_oemdrv_{}_{}.img", pid, now_nanos);
    let path = std::env::temp_dir().join(filename);

    let mut file = File::create(&path)
        .map_err(|e| format!("Error creando archivo temporal OEMDRV en {:?}: {}", path, e))?;
    file.write_all(&img_bytes)
        .map_err(|e| format!("Error escribiendo imagen en archivo {:?}: {}", path, e))?;
    file.sync_all()
        .map_err(|e| format!("Error sincronizando archivo {:?}: {}", path, e))?;

    Ok(path)
}

// =========================================================================
// Funciones de inspección y lectura de imágenes FAT (para tests y validación)
// =========================================================================

/// Lee y devuelve la etiqueta de volumen registrada en el BPB y en el Root Directory.
pub fn read_volume_label(image: &[u8]) -> Result<String, String> {
    if image.len() < 512 {
        return Err("Imagen demasiado pequeña para tener sector de arranque".to_string());
    }
    if image[0x1FE] != 0x55 || image[0x1FF] != 0xAA {
        return Err("Firma de arranque inválida (no es 0x55 0xAA)".to_string());
    }

    // Leer etiqueta del BPB (offset 0x2B, 11 bytes)
    let bpb_label = std::str::from_utf8(&image[0x2B..0x36])
        .map_err(|e| format!("Etiqueta BPB no es UTF-8 válido: {}", e))?
        .trim_end()
        .to_string();

    Ok(bpb_label)
}

/// Extrae la lista de archivos presentes en el directorio raíz de la imagen FAT con sus tamaños.
pub fn list_files(image: &[u8]) -> Result<Vec<(String, usize)>, String> {
    if image.len() < 512 {
        return Err("Imagen demasiado corta".to_string());
    }

    let bytes_per_sector = u16::from_le_bytes([image[0x0B], image[0x0C]]) as usize;
    if bytes_per_sector == 0 {
        return Err("Bytes por sector es 0".to_string());
    }
    let reserved_sectors = u16::from_le_bytes([image[0x0E], image[0x0F]]) as usize;
    let num_fats = image[0x10] as usize;
    let root_entries = u16::from_le_bytes([image[0x11], image[0x12]]) as usize;
    let sectors_per_fat = u16::from_le_bytes([image[0x16], image[0x17]]) as usize;

    let root_dir_offset = (reserved_sectors + num_fats * sectors_per_fat) * bytes_per_sector;
    let mut files = Vec::new();

    for i in 0..root_entries {
        let entry_offset = root_dir_offset + (i * 32);
        if entry_offset + 32 > image.len() {
            break;
        }

        let first_byte = image[entry_offset];
        if first_byte == 0x00 {
            break; // No más entradas
        }
        if first_byte == 0xE5 {
            continue; // Entrada borrada
        }

        let attr = image[entry_offset + 0x0B];
        if attr & ATTR_VOLUME_ID != 0 {
            continue; // Saltar entrada de volumen
        }

        let mut raw_name = [0u8; 11];
        raw_name.copy_from_slice(&image[entry_offset..entry_offset + 11]);
        let parsed_name = parse_8_3_name(&raw_name);
        let file_size = u32::from_le_bytes([
            image[entry_offset + 0x1C],
            image[entry_offset + 0x1D],
            image[entry_offset + 0x1E],
            image[entry_offset + 0x1F],
        ]) as usize;

        if !files.iter().any(|(n, _)| n == &parsed_name) {
            files.push((parsed_name, file_size));
        }
    }

    Ok(files)
}

/// Lee el contenido exacto de un archivo recorriendo el Root Directory y la tabla FAT.
pub fn read_file_content(image: &[u8], filename: &str) -> Result<Vec<u8>, String> {
    if image.len() < 512 {
        return Err("Imagen demasiado corta".to_string());
    }
    if image[0x1FE] != 0x55 || image[0x1FF] != 0xAA {
        return Err("Firma de arranque inválida".to_string());
    }

    let bytes_per_sector = u16::from_le_bytes([image[0x0B], image[0x0C]]) as usize;
    let sectors_per_cluster = image[0x0D] as usize;
    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let reserved_sectors = u16::from_le_bytes([image[0x0E], image[0x0F]]) as usize;
    let num_fats = image[0x10] as usize;
    let root_entries = u16::from_le_bytes([image[0x11], image[0x12]]) as usize;
    let mut total_sectors = u16::from_le_bytes([image[0x13], image[0x14]]) as usize;
    if total_sectors == 0 {
        total_sectors = u32::from_le_bytes([
            image[0x20],
            image[0x21],
            image[0x22],
            image[0x23],
        ]) as usize;
    }
    let sectors_per_fat = u16::from_le_bytes([image[0x16], image[0x17]]) as usize;

    let fat_offset = reserved_sectors * bytes_per_sector;
    let root_dir_start_sector = reserved_sectors + num_fats * sectors_per_fat;
    let root_dir_offset = root_dir_start_sector * bytes_per_sector;
    let root_dir_sectors = (root_entries * 32 + bytes_per_sector - 1) / bytes_per_sector;
    let data_start_sector = root_dir_start_sector + root_dir_sectors;
    let data_offset = data_start_sector * bytes_per_sector;

    let data_sectors = total_sectors.saturating_sub(data_start_sector);
    let total_clusters = data_sectors / sectors_per_cluster;
    let is_fat12 = total_clusters < 4085;

    // Buscar archivo en Root Directory
    let target_8_3 = format_8_3_name(filename)?;
    let mut file_first_cluster = None;
    let mut file_size = 0usize;

    for i in 0..root_entries {
        let entry_offset = root_dir_offset + (i * 32);
        if entry_offset + 32 > image.len() {
            break;
        }

        let first_byte = image[entry_offset];
        if first_byte == 0x00 {
            break;
        }
        if first_byte == 0xE5 {
            continue;
        }

        let attr = image[entry_offset + 0x0B];
        if attr & ATTR_VOLUME_ID != 0 {
            continue;
        }

        let entry_name = &image[entry_offset..entry_offset + 11];
        if entry_name == target_8_3 {
            let cluster = u16::from_le_bytes([
                image[entry_offset + 0x1A],
                image[entry_offset + 0x1B],
            ]) as usize;
            let size = u32::from_le_bytes([
                image[entry_offset + 0x1C],
                image[entry_offset + 0x1D],
                image[entry_offset + 0x1E],
                image[entry_offset + 0x1F],
            ]) as usize;

            file_first_cluster = Some(cluster);
            file_size = size;
            break;
        }
    }

    let first_cluster = match file_first_cluster {
        Some(c) => c,
        None => return Err(format!("Archivo '{}' no encontrado en el directorio raíz", filename)),
    };

    if file_size == 0 {
        return Ok(Vec::new());
    }

    // Reconstruir contenido siguiendo la cadena de clústeres en la FAT
    let mut result = Vec::with_capacity(file_size);
    let mut curr_cluster = first_cluster;
    let fat_slice = &image[fat_offset..fat_offset + sectors_per_fat * bytes_per_sector];

    while curr_cluster >= 2 && result.len() < file_size {
        let cluster_pos = data_offset + (curr_cluster - 2) * cluster_size;
        if cluster_pos >= image.len() {
            return Err("Acceso fuera de rango al leer clúster de datos".to_string());
        }

        let remaining = file_size - result.len();
        let bytes_to_read = std::cmp::min(remaining, cluster_size);
        result.extend_from_slice(&image[cluster_pos..cluster_pos + bytes_to_read]);

        if result.len() >= file_size {
            break;
        }

        // Obtener siguiente clúster
        if is_fat12 {
            let next_clus = get_fat12_entry(fat_slice, curr_cluster);
            if next_clus >= 0x0FF8 {
                break; // EOF
            }
            curr_cluster = next_clus as usize;
        } else {
            let next_offset = curr_cluster * 2;
            if next_offset + 2 > fat_slice.len() {
                break;
            }
            let next_clus = u16::from_le_bytes([fat_slice[next_offset], fat_slice[next_offset + 1]]);
            if next_clus >= 0xFFF8 {
                break; // EOF
            }
            curr_cluster = next_clus as usize;
        }
    }

    if result.len() != file_size {
        return Err(format!(
            "Lectura incompleta: esperados {} bytes, leídos {}",
            file_size,
            result.len()
        ));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_sector_signature_fat16() {
        let img = build_fat16_image("OEMDRV", &[]).expect("build fat16");
        assert!(img.len() >= 512);
        assert_eq!(img[0x1FE], 0x55);
        assert_eq!(img[0x1FF], 0xAA);
        assert_eq!(&img[0x1FE..0x200], &BOOT_SIGNATURE);

        // Bytes por sector (512)
        assert_eq!(u16::from_le_bytes([img[0x0B], img[0x0C]]), 512);
        // Media descriptor (0xF8)
        assert_eq!(img[0x15], MEDIA_DESCRIPTOR_HARD_DISK);
        // Sectores reservados (1)
        assert_eq!(u16::from_le_bytes([img[0x0E], img[0x0F]]), 1);
        // Número de FATs (2)
        assert_eq!(img[0x10], 2);
        // Filesystem type
        assert_eq!(&img[0x36..0x3E], b"FAT16   ");
    }

    #[test]
    fn test_boot_sector_signature_fat12() {
        let img = build_fat12_image("OEMDRV", &[]).expect("build fat12");
        assert_eq!(img.len(), 2880 * 512);
        assert_eq!(img[0x1FE], 0x55);
        assert_eq!(img[0x1FF], 0xAA);
        assert_eq!(&img[0x1FE..0x200], &BOOT_SIGNATURE);

        // Bytes por sector (512)
        assert_eq!(u16::from_le_bytes([img[0x0B], img[0x0C]]), 512);
        // Media descriptor (0xF0)
        assert_eq!(img[0x15], MEDIA_DESCRIPTOR_FLOPPY);
        // Sectores por FAT (9)
        assert_eq!(u16::from_le_bytes([img[0x16], img[0x17]]), 9);
        // Directorio raíz entradas (224)
        assert_eq!(u16::from_le_bytes([img[0x11], img[0x12]]), 224);
        // Filesystem type
        assert_eq!(&img[0x36..0x3E], b"FAT12   ");
    }

    #[test]
    fn test_volume_label_bpb_and_root_entry_oemdrv() {
        let img = build_oemdrv_image(&[]).expect("build oemdrv");

        // BPB Volume label
        assert_eq!(&img[0x2B..0x36], b"OEMDRV     ");
        assert_eq!(read_volume_label(&img).unwrap(), "OEMDRV");

        // Root directory entry 0 (Volume label entry)
        let reserved = u16::from_le_bytes([img[0x0E], img[0x0F]]) as usize;
        let sectors_per_fat = u16::from_le_bytes([img[0x16], img[0x17]]) as usize;
        let num_fats = img[0x10] as usize;
        let root_dir_start = (reserved + num_fats * sectors_per_fat) * 512;
        assert_eq!(&img[root_dir_start..root_dir_start + 11], b"OEMDRV     ");
        assert_eq!(img[root_dir_start + 0x0B], ATTR_VOLUME_ID);
    }

    #[test]
    fn test_volume_label_bpb_and_root_entry_cidata() {
        let img = build_cidata_image("test-user-data", "test-meta-data").expect("build cidata");

        // BPB Volume label
        assert_eq!(&img[0x2B..0x36], b"cidata     ");
        assert_eq!(read_volume_label(&img).unwrap(), "cidata");

        // Root directory entry 0
        let reserved = u16::from_le_bytes([img[0x0E], img[0x0F]]) as usize;
        let sectors_per_fat = u16::from_le_bytes([img[0x16], img[0x17]]) as usize;
        let num_fats = img[0x10] as usize;
        let root_dir_start = (reserved + num_fats * sectors_per_fat) * 512;
        assert_eq!(&img[root_dir_start..root_dir_start + 11], b"cidata     ");
        assert_eq!(img[root_dir_start + 0x0B], ATTR_VOLUME_ID);
    }

    #[test]
    fn test_8_3_name_formatting_and_parsing() {
        // PRESEED.CFG
        let name1 = format_8_3_name("preseed.cfg").unwrap();
        assert_eq!(&name1, b"PRESEED CFG");
        assert_eq!(parse_8_3_name(&name1), "PRESEED.CFG");

        // USER-DAT (from user-data)
        let name2 = format_8_3_name("user-data").unwrap();
        assert_eq!(&name2, b"USER-DAT   ");
        assert_eq!(parse_8_3_name(&name2), "USER-DAT");

        // META-DAT (from meta-data)
        let name3 = format_8_3_name("meta-data").unwrap();
        assert_eq!(&name3, b"META-DAT   ");
        assert_eq!(parse_8_3_name(&name3), "META-DAT");

        // AUTOUNAT.XML (from autounattend.xml)
        let name4 = format_8_3_name("autounattend.xml").unwrap();
        assert_eq!(&name4, b"AUTOUNATXML");
        assert_eq!(parse_8_3_name(&name4), "AUTOUNAT.XML");

        // Leading slash stripping (/preseed.cfg)
        let name5 = format_8_3_name("/preseed.cfg").unwrap();
        assert_eq!(&name5, b"PRESEED CFG");

        // KS.CFG
        let name6 = format_8_3_name("ks.cfg").unwrap();
        assert_eq!(&name6, b"KS      CFG");
        assert_eq!(parse_8_3_name(&name6), "KS.CFG");
    }

    #[test]
    fn test_read_written_files_exact_bytes_fat16() {
        let preseed_content = b"d-i debian-installer/locale string en_US.UTF-8\n\
d-i console-keymaps-at/keymap select us\n\
d-i netcfg/choose_interface select auto\n\
d-i mirror/country string manual\n\
d-i partman-auto/disk string /dev/sda\n\
d-i partman-auto/method string regular\n\
d-i finish-install/reboot_in_progress note\n";

        let ks_content = b"lang en_US.UTF-8\nkeyboard us\nrootpw --plaintext two55\nreboot\n";
        let unattend_xml = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<unattend><settings/></unattend>\n";

        let files: &[(&str, &[u8])] = &[
            ("preseed.cfg", preseed_content),
            ("ks.cfg", ks_content),
            ("autounattend.xml", unattend_xml),
        ];

        let img = build_fat16_image("OEMDRV", files).expect("build fat16 with files");

        // Listar archivos
        let list = list_files(&img).expect("list files");
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].0, "PRESEED.CFG");
        assert_eq!(list[0].1, preseed_content.len());
        assert_eq!(list[1].0, "KS.CFG");
        assert_eq!(list[1].1, ks_content.len());
        assert_eq!(list[2].0, "AUTOUNAT.XML");
        assert_eq!(list[2].1, unattend_xml.len());

        // Leer cada archivo y comprobar coincidencia exacta
        let read_preseed = read_file_content(&img, "preseed.cfg").expect("read preseed");
        assert_eq!(read_preseed, preseed_content);

        let read_ks = read_file_content(&img, "ks.cfg").expect("read ks");
        assert_eq!(read_ks, ks_content);

        let read_unattend = read_file_content(&img, "autounattend.xml").expect("read unattend");
        assert_eq!(read_unattend, unattend_xml);
    }

    #[test]
    fn test_read_written_files_exact_bytes_fat12() {
        let preseed_content = b"# Preseed configuration for Debian/Kali\n\
d-i debian-installer/locale string es_ES.UTF-8\n\
d-i passwd/user-fullname string Two Five Five User\n\
d-i passwd/username string two55\n";

        let files: &[(&str, &[u8])] = &[("preseed.cfg", preseed_content)];

        let img = build_fat12_image("OEMDRV", files).expect("build fat12 with files");

        let read_preseed = read_file_content(&img, "preseed.cfg").expect("read preseed fat12");
        assert_eq!(read_preseed, preseed_content);
    }

    #[test]
    fn test_large_multi_cluster_file() {
        // Archivo que abarca múltiples clústeres (ej. 16,384 bytes = 32 clústeres de 512 bytes)
        let mut large_data = Vec::with_capacity(16384);
        for i in 0..16384 {
            large_data.push((i % 251) as u8);
        }

        let files: &[(&str, &[u8])] = &[("large.bin", &large_data)];

        // Test en FAT16
        let img16 = build_fat16_image("OEMDRV", files).expect("build fat16 large");
        let read16 = read_file_content(&img16, "large.bin").expect("read large fat16");
        assert_eq!(read16.len(), large_data.len());
        assert_eq!(read16, large_data);

        // Test en FAT12
        let img12 = build_fat12_image("OEMDRV", files).expect("build fat12 large");
        let read12 = read_file_content(&img12, "large.bin").expect("read large fat12");
        assert_eq!(read12.len(), large_data.len());
        assert_eq!(read12, large_data);
    }

    #[test]
    fn test_empty_file() {
        let files: &[(&str, &[u8])] = &[("empty.txt", b"")];

        let img = build_fat16_image("OEMDRV", files).expect("build with empty file");
        let list = list_files(&img).expect("list files");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].0, "EMPTY.TXT");
        assert_eq!(list[0].1, 0);

        let read_empty = read_file_content(&img, "empty.txt").expect("read empty file");
        assert!(read_empty.is_empty());
    }

    #[test]
    fn test_cidata_convenience_function() {
        let user_data = "#cloud-config\nhostname: two55-node\n";
        let meta_data = "instance-id: i-two55-001\nlocal-hostname: two55-node\n";

        let img = build_cidata_image(user_data, meta_data).expect("build cidata");

        assert_eq!(read_volume_label(&img).unwrap(), "cidata");

        let read_user = read_file_content(&img, "user-data").expect("read user-data");
        assert_eq!(read_user, user_data.as_bytes());

        let read_meta = read_file_content(&img, "meta-data").expect("read meta-data");
        assert_eq!(read_meta, meta_data.as_bytes());
    }

    #[test]
    fn test_create_temp_oemdrv_file() {
        let files: &[(&str, &[u8])] = &[("preseed.cfg", b"d-i test/data string 12345\n")];

        let path = create_temp_oemdrv_file(files).expect("create temp oemdrv file");
        assert!(path.exists());

        let bytes = std::fs::read(&path).expect("read temp file from disk");
        assert_eq!(read_volume_label(&bytes).unwrap(), "OEMDRV");

        let read_file = read_file_content(&bytes, "preseed.cfg").expect("read file from disk img");
        assert_eq!(read_file, b"d-i test/data string 12345\n");

        // Limpiar archivo temporal creado
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_fat2_is_identical_to_fat1() {
        let files: &[(&str, &[u8])] = &[("file1.txt", b"hello world"), ("file2.txt", b"second file")];
        let img = build_fat16_image("OEMDRV", files).expect("build fat16");

        let reserved = u16::from_le_bytes([img[0x0E], img[0x0F]]) as usize;
        let sectors_per_fat = u16::from_le_bytes([img[0x16], img[0x17]]) as usize;
        let fat_bytes = sectors_per_fat * 512;

        let fat1_start = reserved * 512;
        let fat2_start = fat1_start + fat_bytes;

        assert_eq!(&img[fat1_start..fat1_start + fat_bytes], &img[fat2_start..fat2_start + fat_bytes]);
    }

    #[test]
    fn test_fat_table_chain_integrity() {
        // Archivo de 3 clústeres (1500 bytes con clúster de 512 bytes)
        let data = vec![0xABu8; 1500];
        let files: &[(&str, &[u8])] = &[("chain.bin", &data)];
        let img = build_fat16_image("OEMDRV", files).expect("build fat16");

        let fat1_start = 512; // sector 1
        // Cluster 2 -> 3
        let clus2 = u16::from_le_bytes([img[fat1_start + 4], img[fat1_start + 5]]);
        assert_eq!(clus2, 3);
        // Cluster 3 -> 4
        let clus3 = u16::from_le_bytes([img[fat1_start + 6], img[fat1_start + 7]]);
        assert_eq!(clus3, 4);
        // Cluster 4 -> 0xFFFF (EOF)
        let clus4 = u16::from_le_bytes([img[fat1_start + 8], img[fat1_start + 9]]);
        assert_eq!(clus4, 0xFFFF);
    }

    #[test]
    fn test_volume_label_spaces_padding() {
        assert_eq!(format_volume_label("OEMDRV"), *b"OEMDRV     ");
        assert_eq!(format_volume_label("cidata"), *b"cidata     ");
        assert_eq!(format_volume_label("123456789012345"), *b"12345678901");
        assert_eq!(format_volume_label(""), *b"           ");
    }

    #[test]
    fn test_invalid_filename_error() {
        let res = format_8_3_name("");
        assert!(res.is_err());
        let res2 = format_8_3_name("   ");
        assert!(res2.is_err());
    }

    #[test]
    fn test_fat16_cluster_count_in_valid_range() {
        let img = build_fat16_image("TEST", &[]).expect("build fat16");
        let total_sectors = u16::from_le_bytes([img[0x13], img[0x14]]) as usize;
        let reserved = u16::from_le_bytes([img[0x0E], img[0x0F]]) as usize;
        let num_fats = img[0x10] as usize;
        let sectors_per_fat = u16::from_le_bytes([img[0x16], img[0x17]]) as usize;
        let root_entries = u16::from_le_bytes([img[0x11], img[0x12]]) as usize;
        let root_dir_sectors = (root_entries * 32 + 511) / 512;

        let overhead = reserved + num_fats * sectors_per_fat + root_dir_sectors;
        let data_sectors = total_sectors - overhead;
        let clusters = data_sectors; // 1 sector per cluster

        // El estándar FAT16 exige que los clústeres estén entre 4085 y 65524
        assert!(clusters >= 4085, "Clusters {} < 4085", clusters);
        assert!(clusters < 65525, "Clusters {} >= 65525", clusters);
    }

    #[test]
    fn test_wrap_mbr_partition_and_dual_bpb() {
        let content = b"d-i test/data string hello\n";
        let files: &[(&str, &[u8])] = &[("preseed.cfg", content)];

        let disk = build_fat_image("OEMDRV", files).expect("build partitioned oemdrv");
        assert!(disk.len() >= (64 + 8192) * 512);

        // 1. Validar Sector 0: MBR
        assert_eq!(disk[0x1FE], 0x55);
        assert_eq!(disk[0x1FF], 0xAA);
        // Partición 1 en 0x1BE
        assert_eq!(disk[0x1BE], 0x80); // Bootable
        assert_eq!(disk[0x1C2], 0x06); // FAT16
        let start_lba = u32::from_le_bytes([disk[0x1C6], disk[0x1C7], disk[0x1C8], disk[0x1C9]]);
        assert_eq!(start_lba, 64);
        let part_sec = u32::from_le_bytes([disk[0x1CA], disk[0x1CB], disk[0x1CC], disk[0x1CD]]);
        assert!(part_sec >= 8192);

        // 2. Validar Sector 0: Dual BPB
        assert_eq!(&disk[0x03..0x0B], b"MSWIN4.1");
        assert_eq!(&disk[0x2B..0x36], b"OEMDRV     ");
        assert_eq!(&disk[0x36..0x3E], b"FAT16   ");
        let disk_reserved = u16::from_le_bytes([disk[0x0E], disk[0x0F]]);
        assert_eq!(disk_reserved, 65); // 64 + 1

        // 3. Validar Sector 64: BPB de Partición 1
        let p_offset = 64 * 512;
        assert_eq!(disk[p_offset + 0x1FE], 0x55);
        assert_eq!(disk[p_offset + 0x1FF], 0xAA);
        assert_eq!(&disk[p_offset + 0x03..p_offset + 0x0B], b"MSWIN4.1");
        assert_eq!(&disk[p_offset + 0x2B..p_offset + 0x36], b"OEMDRV     ");
        let part_reserved = u16::from_le_bytes([disk[p_offset + 0x0E], disk[p_offset + 0x0F]]);
        assert_eq!(part_reserved, 1);
        let hidden_sec = u32::from_le_bytes([
            disk[p_offset + 0x1C],
            disk[p_offset + 0x1D],
            disk[p_offset + 0x1E],
            disk[p_offset + 0x1F],
        ]);
        assert_eq!(hidden_sec, 64);

        // 4. Leer archivos desde la imagen particionada
        let read = read_file_content(&disk, "preseed.cfg").expect("read from dual disk");
        assert_eq!(read, content);
    }

    #[test]
    fn test_case_sensitivity_preseed_lowercase_entry() {
        let content = b"d-i locale string en_US\n";
        let files: &[(&str, &[u8])] = &[("preseed.cfg", content)];

        let raw = build_fat16_image("OEMDRV", files).expect("build raw fat16");
        // El directorio raíz empieza en (1 + 2 * 32) * 512 = 65 * 512 = 33280
        let root_dir_start = (1 + 2 * 32) * 512;
        let e1 = root_dir_start + 32; // Entrada 1 (preseed.cfg)
        assert_eq!(&raw[e1..e1 + 11], b"PRESEED CFG");
        assert_eq!(raw[e1 + 0x0C], 0x18); // NT Flags: minúsculas

        let e2 = root_dir_start + 64; // Entrada 2 (PRESEED.CFG duplicada)
        assert_eq!(&raw[e2..e2 + 11], b"PRESEED CFG");
        assert_eq!(raw[e2 + 0x0C], 0x00); // Strict uppercase
    }
}
