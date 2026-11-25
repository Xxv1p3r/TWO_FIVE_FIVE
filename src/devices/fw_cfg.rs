//! QEMU fw_cfg con soporte PIO y DMA (0x510, 0x511, 0x514..0x51B).
//!
//! SeaBIOS y OVMF sondean 0x510/0x511 buscando la firma "QEMU".
//!
//! Puertos:
//!   0x510 (write u16 LE): selector de item
//!   0x511 (read  u8):     byte siguiente del item seleccionado
//!   0x514 (write BE u32): parte alta de dirección física de FWCfgDmaAccess
//!   0x518 (write BE u32): parte baja + disparo de operación DMA

use super::IoDevice;
use crate::guest_mem::GuestMemory;
use std::collections::HashMap;

const PORT_SELECT: u16 = 0x510;
const PORT_DATA: u16 = 0x511;
const PORT_DMA_HIGH: u16 = 0x514;
const PORT_DMA_LOW: u16 = 0x518;

// Selectores fw_cfg estándar.
const FW_CFG_SIGNATURE: u16 = 0x00;
const FW_CFG_ID: u16 = 0x01;
const FW_CFG_NB_CPUS: u16 = 0x05;
const FW_CFG_MAX_CPUS: u16 = 0x0F;
const FW_CFG_RAM_SIZE: u16 = 0x03;
const FW_CFG_FILE_DIR: u16 = 0x19;
/// Primer selector asignable a archivos custom.
const FILE_BASE: u16 = 0x20;

const FW_CFG_DMA_CTL_ERROR: u32 = 0x01;
const FW_CFG_DMA_CTL_READ: u32 = 0x02;
const FW_CFG_DMA_CTL_SKIP: u32 = 0x04;
const FW_CFG_DMA_CTL_SELECT: u32 = 0x08;
const FW_CFG_DMA_CTL_WRITE: u32 = 0x10;

pub struct FwCfg {
    /// Selector actual.
    select: u16,
    /// Offset de lectura dentro del item actual.
    offset: usize,
    /// Dirección DMA acumulada (big-endian escrita en 0x514/0x518).
    dma_addr: u64,
    /// Contenido por selector.
    items: HashMap<u16, Vec<u8>>,
}

impl FwCfg {
    /// `acpi`: tablas ACPI (tables + rsdp + loader) para exponerlas con el
    /// interface estándar de QEMU; SeaBIOS las carga vía romfile-loader.
    /// `vga_rom`: opcionalmente el binario de VGA Option ROM (vgaroms/vgabios.bin)
    /// para que SeaBIOS lo cargue y ejecute directamente.
    pub fn new(
        ram_size: u64,
        num_cpus: u32,
        acpi: Option<super::acpi::AcpiFiles>,
        vga_rom: Option<Vec<u8>>,
    ) -> Self {
        let mut items: HashMap<u16, Vec<u8>> = HashMap::new();
        let ram_below_4g = ram_size.min(0xE000_0000);
        items.insert(FW_CFG_SIGNATURE, b"QEMU".to_vec());
        // Bit 0 = versión tradicional, Bit 1 = DMA disponible (0x03u32 LE)
        items.insert(FW_CFG_ID, 3u32.to_le_bytes().to_vec());
        items.insert(
            FW_CFG_RAM_SIZE,
            ram_below_4g.to_le_bytes().to_vec(),
        );
        items.insert(
            FW_CFG_NB_CPUS,
            (num_cpus as u16).to_le_bytes().to_vec(),
        );
        items.insert(
            FW_CFG_MAX_CPUS,
            (num_cpus.max(16) as u16).to_le_bytes().to_vec(),
        );

        // Directorio de archivos (selector 0x19):
        // Formato REAL de QEMU (ver struct QemuCfgFile en SeaBIOS):
        let mut e820_data = Vec::new();
        let mut add_e820 = |addr: u64, len: u64, typ: u32| {
            e820_data.extend_from_slice(&addr.to_le_bytes());
            e820_data.extend_from_slice(&len.to_le_bytes());
            e820_data.extend_from_slice(&typ.to_le_bytes());
        };

        // 1. RAM baja: 0..0x9FC00 (639 KiB)
        add_e820(0, 0x9FC00, 1);
        // 2. EBDA: 0x9FC00..0xA0000 (1 KiB)
        add_e820(0x9FC00, 0x400, 2);
        // 3. Video RAM y ROMs de BIOS: 0xA0000..0x100000 (384 KiB)
        add_e820(0xA0000, 0x60000, 2);
        // 4. RAM principal por debajo de 4GB: 0x100000..(ram_size.min(0xE000_0000))
        if ram_below_4g > 0x100000 {
            add_e820(0x100000, ram_below_4g - 0x100000, 1);
        }
        // 5. Si hay RAM por encima de 4GB:
        if ram_size > ram_below_4g {
            add_e820(0x1_0000_0000, ram_size - ram_below_4g, 1);
        }

        let mut files: Vec<(&str, Vec<u8>)> = vec![
            ("etc/ram-size", ram_below_4g.to_le_bytes().to_vec()),
            ("etc/e820", e820_data),
        ];
        if let Some(rom) = &vga_rom {
            files.push(("pci1234,1111.rom", rom.clone()));
        }
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
            dma_addr: 0,
            items,
        }
    }

    fn current_item(&self) -> &[u8] {
        self.items
            .get(&self.select)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Reset del dispositivo: vuelve al selector 0 (los items son hardware
    /// estático y se conservan).
    pub fn reset(&mut self) {
        self.select = 0;
        self.offset = 0;
        self.dma_addr = 0;
    }

    /// Procesa una transferencia DMA de `fw_cfg` (QEMU `FWCfgDmaAccess` en big-endian).
    fn execute_dma(&mut self, mem: &GuestMemory) {
        let dma_gpa = self.dma_addr as usize;
        self.dma_addr = 0;

        let mut hdr = [0u8; 16];
        if mem.copy_from(dma_gpa, &mut hdr) != 16 {
            return;
        }
        let control = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let length = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        let addr = u64::from_be_bytes([
            hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13], hdr[14], hdr[15],
        ]);

        if (control & FW_CFG_DMA_CTL_SELECT) != 0 {
            self.select = (control >> 16) as u16;
            self.offset = 0;
        }

        let mut out_ctrl = 0u32;
        if (control & FW_CFG_DMA_CTL_WRITE) != 0 {
            out_ctrl = FW_CFG_DMA_CTL_ERROR;
        } else if (control & FW_CFG_DMA_CTL_READ) != 0 {
            if length > 0 {
                let mut tmp = vec![0u8; length];
                if let Some(item) = self.items.get(&self.select) {
                    if self.offset < item.len() {
                        let avail = (item.len() - self.offset).min(length);
                        tmp[..avail].copy_from_slice(&item[self.offset..self.offset + avail]);
                    }
                }
                if !mem.copy_to(addr as usize, &tmp) {
                    out_ctrl = FW_CFG_DMA_CTL_ERROR;
                }
                self.offset = self.offset.saturating_add(length);
            }
        } else if (control & FW_CFG_DMA_CTL_SKIP) != 0 {
            self.offset = self.offset.saturating_add(length);
        }

        mem.copy_to(dma_gpa, &out_ctrl.to_be_bytes());
    }

    pub fn write_with_mem(&mut self, port: u16, data: &[u8], mem: Option<&GuestMemory>) {
        if port == PORT_SELECT && data.len() >= 2 {
            self.select = u16::from_le_bytes([data[0], data[1]]);
            self.offset = 0;
        } else if port == PORT_DMA_HIGH && data.len() >= 4 {
            let hi = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as u64;
            self.dma_addr = (self.dma_addr & 0x0000_0000_FFFF_FFFF) | (hi << 32);
            if data.len() >= 8 {
                let lo = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as u64;
                self.dma_addr = (hi << 32) | lo;
                if let Some(m) = mem {
                    self.execute_dma(m);
                }
            }
        } else if port == PORT_DMA_LOW && data.len() >= 4 {
            let lo = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as u64;
            self.dma_addr = (self.dma_addr & 0xFFFF_FFFF_0000_0000) | lo;
            if let Some(m) = mem {
                self.execute_dma(m);
            }
        }
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
        let cfg = FwCfg::new(256 * 1024 * 1024, 2, Some(acpi), None);
        let dir = cfg.items.get(&FW_CFG_FILE_DIR).unwrap();
        // count BE + 5 entradas de 64 bytes (QemuCfgFile)
        assert_eq!(u32::from_be_bytes(dir[0..4].try_into().unwrap()), 5);
        for (i, name) in [
            "etc/ram-size",
            "etc/e820",
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

    #[test]
    fn vga_rom_present_in_directory() {
        let mock_rom = vec![0x55, 0xAA, 0x01, 0x02];
        let cfg = FwCfg::new(256 * 1024 * 1024, 1, None, Some(mock_rom.clone()));
        let dir = cfg.items.get(&FW_CFG_FILE_DIR).unwrap();
        assert_eq!(u32::from_be_bytes(dir[0..4].try_into().unwrap()), 3);
        let off = 4 + 2 * 64;
        let entry = &dir[off..off + 64];
        let entry_name =
            String::from_utf8_lossy(&entry[8..64]).trim_end_matches('\0').to_string();
        assert_eq!(entry_name, "pci1234,1111.rom");
        let sel = u16::from_be_bytes(entry[4..6].try_into().unwrap());
        assert_eq!(cfg.items.get(&sel).unwrap(), &mock_rom);
    }
}

impl IoDevice for FwCfg {
    fn matches_port(&self, port: u16) -> bool {
        matches!(port, PORT_SELECT | PORT_DATA | PORT_DMA_HIGH..=0x51B)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        self.write_with_mem(port, data, None);
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        if port == PORT_DMA_HIGH {
            // Firma QEMU DMA "QEMU CFG" (0x514..0x51B big-endian)
            const DMA_SIG: [u8; 8] = *b"QEMU CFG";
            return (0..count).map(|i| *DMA_SIG.get(i).unwrap_or(&0)).collect();
        }
        if port == PORT_DMA_LOW {
            const DMA_SIG_LO: [u8; 4] = *b" CFG";
            return (0..count).map(|i| *DMA_SIG_LO.get(i).unwrap_or(&0)).collect();
        }
        if port != PORT_DATA {
            return vec![0; count];
        }
        // SeaBIOS lee con IN de 1, 2 o 4 bytes: hay que servir `count` bytes
        // y avanzar el offset por cada uno sin clonar el item en cada lectura.
        let mut out = Vec::with_capacity(count);
        let mut off = self.offset;
        {
            let item = self.current_item();
            for _ in 0..count {
                let b = item.get(off).copied().unwrap_or(0);
                off += 1;
                out.push(b);
            }
        }
        self.offset = off;
        out
    }
}
