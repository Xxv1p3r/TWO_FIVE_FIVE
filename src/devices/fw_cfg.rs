//! QEMU fw_cfg mínimo (PIO, sin DMA).
//!
//! SeaBIOS compilado para QEMU sondea 0x510/0x511 buscando la firma "QEMU".
//! Sin fw_cfg, el preinit de SeaBIOS muere con triple fault.
//!
//! Puertos:
//!   0x510 (write u16 LE): selector de item
//!   0x511 (read  u8):     byte siguiente del item seleccionado
//!
//! No anunci DMA (id sin bit 0x02... usamos id=0) → SeaBIOS lee por PIO.

use super::IoDevice;
use std::collections::HashMap;

const PORT_SELECT: u16 = 0x510;
const PORT_DATA: u16 = 0x511;

// Selectores fw_cfg estándar.
const FW_CFG_SIGNATURE: u16 = 0x00;
const FW_CFG_ID: u16 = 0x01;
const FW_CFG_NB_CPUS: u16 = 0x05;
const FW_CFG_MAX_CPUS: u16 = 0x0F;
const FW_CFG_RAM_SIZE: u16 = 0x03;
const FW_CFG_FILE_DIR: u16 = 0x19;
/// Primer selector asignable a archivos custom.
const FILE_BASE: u16 = 0x20;

pub struct FwCfg {
    /// Selector actual.
    select: u16,
    /// Offset de lectura dentro del item actual.
    offset: usize,
    /// Contenido por selector.
    items: HashMap<u16, Vec<u8>>,
}

impl FwCfg {
    /// `acpi`: tablas ACPI (tables + rsdp + loader) para exponerlas con el
    /// interface estándar de QEMU; SeaBIOS las carga vía romfile-loader.
    pub fn new(ram_size: u64, num_cpus: u32, acpi: Option<super::acpi::AcpiFiles>) -> Self {
        let mut items: HashMap<u16, Vec<u8>> = HashMap::new();
        items.insert(FW_CFG_SIGNATURE, b"QEMU".to_vec());
        // id: sin DMA (SeaBIOS usará lecturas PIO byte a byte).
        items.insert(FW_CFG_ID, vec![0x00]);
        items.insert(
            FW_CFG_RAM_SIZE,
            ram_size.to_le_bytes().to_vec(),
        );
        items.insert(
            FW_CFG_NB_CPUS,
            (num_cpus as u16).to_le_bytes().to_vec(),
        );
        items.insert(
            FW_CFG_MAX_CPUS,
            (num_cpus as u16).to_le_bytes().to_vec(),
        );

        // Directorio de archivos (selector 0x19):
        // Formato REAL de QEMU (ver struct QemuCfgFile en SeaBIOS):
        //   u32 count BE + entries { size: u32 BE, select: u16 BE,
        //                            reserved: u16, name: [u8;56] } = 64 bytes
        let mut files: Vec<(&str, Vec<u8>)> = vec![("etc/ram-size", ram_size.to_le_bytes().to_vec())];
        if let Some(a) = &acpi {
            // Tablas ACPI: los tres ficheros que SeaBIOS busca (acpi.c:
            // loadQemuAcpiTables) para instalar RSDP/RSDT/FADT/DSDT/MADT.
            files.push((super::acpi::ACPI_TABLES_FILE, a.tables.clone()));
            files.push((super::acpi::ACPI_RSDP_FILE, a.rsdp.clone()));
            files.push((super::acpi::TABLE_LOADER_FILE, a.loader.clone()));
        }
        let mut dir = Vec::new();
        dir.extend_from_slice(&(files.len() as u32).to_be_bytes());
        let mut next_sel = FILE_BASE;
        for (name, content) in &files {
            dir.extend_from_slice(&(content.len() as u32).to_be_bytes()); // size
            dir.extend_from_slice(&(next_sel as u16).to_be_bytes());     // select
            dir.extend_from_slice(&0u16.to_be_bytes());                  // reserved
            let mut entry = [0u8; 56];
            let nb = name.len().min(55);
            entry[..nb].copy_from_slice(&name.as_bytes()[..nb]);
            dir.extend_from_slice(&entry);                               // name
            items.insert(next_sel, content.clone());
            next_sel += 1;
        }
        items.insert(FW_CFG_FILE_DIR, dir);

        eprintln!(
            "[FWCFG] Inicializado (ram={} MiB, cpus={}, {} archivo(s))",
            ram_size / (1024 * 1024),
            num_cpus,
            files.len()
        );

        Self {
            select: 0,
            offset: 0,
            items,
        }
    }

    fn current_item(&self) -> Vec<u8> {
        self.items
            .get(&self.select)
            .cloned()
            .unwrap_or_else(|| vec![0])
    }

    /// Reset del dispositivo: vuelve al selector 0 (los items son hardware
    /// estático y se conservan).
    pub fn reset(&mut self) {
        self.select = 0;
        self.offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// El directorio de archivos (0x19) debe listar los tres ficheros ACPI
    /// con sus tamaños/selectores correctos para que SeaBIOS los encuentre.
    #[test]
    fn acpi_files_present_in_directory() {
        let acpi = super::super::acpi::build_acpi_files(2);
        let cfg = FwCfg::new(256 * 1024 * 1024, 2, Some(acpi));
        let dir = cfg.items.get(&FW_CFG_FILE_DIR).unwrap();
        // count BE + 4 entradas de 64 bytes (QemuCfgFile)
        assert_eq!(u32::from_be_bytes(dir[0..4].try_into().unwrap()), 4);
        for (i, name) in [
            "etc/ram-size",
            super::super::acpi::ACPI_TABLES_FILE,
            super::super::acpi::ACPI_RSDP_FILE,
            super::super::acpi::TABLE_LOADER_FILE,
        ]
        .iter()
        .enumerate()
        {
            let off = 4 + i * 64;
            let entry = &dir[off..off + 64];
            let size = u32::from_be_bytes(entry[0..4].try_into().unwrap());
            let sel = u16::from_be_bytes(entry[4..6].try_into().unwrap());
            let entry_name =
                String::from_utf8_lossy(&entry[8..64]).trim_end_matches('\0').to_string();
            assert_eq!(entry_name, *name, "entrada {}", i);
            assert!(size > 0);
            assert_eq!(sel, (0x20 + i as u16) as u16);
            // El contenido registrado bajo el selector coincide en tamaño.
            assert_eq!(cfg.items.get(&sel).unwrap().len(), size as usize);
        }
    }
}

impl IoDevice for FwCfg {
    fn matches_port(&self, port: u16) -> bool {
        port == PORT_SELECT || port == PORT_DATA
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if port == PORT_SELECT && data.len() >= 2 {
            self.select = u16::from_le_bytes([data[0], data[1]]);
            self.offset = 0;
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        if port != PORT_DATA {
            return vec![0; count.min(1)];
        }
        // SeaBIOS lee con IN de 1, 2 o 4 bytes: hay que servir `count` bytes
        // y avanzar el offset por cada uno.
        let item = self.current_item();
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let b = if self.offset < item.len() {
                item[self.offset]
            } else {
                0
            };
            self.offset += 1;
            out.push(b);
        }
        out
    }
}
