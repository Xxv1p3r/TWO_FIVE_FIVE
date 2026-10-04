//! Generación de tablas ACPI (RSDP/RSDT/FADT/DSDT/MADT/FACS).
//!
//! Se exponen al firmware con el interface estándar de QEMU:
//!   - `etc/table-loader` : comandos del romfile-loader (ALLOCATE,
//!     ADD_POINTER, ADD_CHECKSUM) que SeaBIOS ejecuta para instalar las
//!     tablas en RAM y parchear las direcciones absolutas.
//!   - `etc/acpi/tables`  : RSDT + FADT + DSDT + MADT + FACS concatenados.
//!     Los punteros internos van como OFFSETS dentro del blob; el loader
//!     les suma la dirección base final.
//!   - `etc/acpi/rsdp`    : Root System Description Pointer (ACPI 1.0).
//!     Se aloja en la zona FSEG (0xF0000-0xFFFFF) que Linux escanea en
//!     0xE0000-0xFFFFF, y SeaBIOS además la registra para el arranque S3.
//!
//! Con esto el guest (Linux) encuentra ACPI sin depender de la generación
//! interna de tablas de SeaBIOS (que solo existe en builds QEMU con
//! CONFIG_ACPI) y puede hacer un apagado limpio: evalúa _S5 y escribe
//! SLP_EN en PM1a_CNT (0x604), que AcpiPm detecta para apagar la VM.
//!
//! Referencias: SeaBIOS src/fw/romfile_loader.{c,h} y src/fw/biostables.c;
//! QEMU hw/acpi/bios-linker-loader.h.

/// Fichero fw_cfg con las tablas concatenadas.
pub const ACPI_TABLES_FILE: &str = "etc/acpi/tables";
/// Fichero fw_cfg con el RSDP.
pub const ACPI_RSDP_FILE: &str = "etc/acpi/rsdp";
/// Fichero fw_cfg con los comandos del romfile-loader.
pub const TABLE_LOADER_FILE: &str = "etc/table-loader";

/// Base de I/O del PM del PIIX (PIIX_PMBASE): debe coincidir con lo que
/// SeaBIOS escribe en el PCI config 0x40 (0x600) y con AcpiPm.
pub const PM_IO_BASE: u16 = 0x600;
/// Bloque GPE0 (coincide con el hardcode de AcpiPm).
pub const GPE0_BLK: u16 = 0xAFE0;

/// Comandos del romfile-loader (bios-linker-loader).
const CMD_ALLOCATE: u32 = 0x1;
const CMD_ADD_POINTER: u32 = 0x2;
const CMD_ADD_CHECKSUM: u32 = 0x3;
/// Zonas de asignación en SeaBIOS.
const ZONE_HIGH: u8 = 0x1;
const ZONE_FSEG: u8 = 0x2;
/// Tamaño de los campos de nombre de fichero en las entradas del loader.
const FILESZ: usize = 56;
/// Tamaño de cada entrada del loader (u32 command + union de 124 bytes).
const ENTRY_LEN: usize = 128;

/// Los tres ficheros fw_cfg que componen las tablas ACPI.
pub struct AcpiFiles {
    pub tables: Vec<u8>,
    pub rsdp: Vec<u8>,
    pub loader: Vec<u8>,
}

// ─── Tablas individuales ───────────────────────────────────────────

/// Cabecera estándar ACPI (36 bytes). El checksum lo calcula el loader.
fn acpi_header(sig: &[u8; 4], length: u32, oem_table: &[u8; 8]) -> Vec<u8> {
    let mut t = vec![0u8; 36];
    t[0..4].copy_from_slice(sig);
    t[4..8].copy_from_slice(&length.to_le_bytes());
    t[8] = 1; // revision
    t[9] = 0; // checksum (lo rellena ADD_CHECKSUM del loader)
    t[10..16].copy_from_slice(b"MI-VMM");
    t[16..24].copy_from_slice(oem_table);
    t[24..28].copy_from_slice(&1u32.to_le_bytes()); // oem revision
    t[28..32].copy_from_slice(b"MI-V");
    t[32..36].copy_from_slice(&1u32.to_le_bytes()); // creator revision
    t
}

/// RSDT con dos entradas (FADT y MADT). Los valores de las entradas son los
/// offsets dentro del blob; el loader les suma la base del blob.
fn build_rsdt(fadt_off: u32, madt_off: u32) -> Vec<u8> {
    let mut t = acpi_header(b"RSDT", 44, b"MI-RSDT ");
    t.extend_from_slice(&fadt_off.to_le_bytes());
    t.extend_from_slice(&madt_off.to_le_bytes());
    t
}

/// FADT rev 1 (116 bytes) con los bloques PM del PIIX.
fn build_fadt(facs_off: u32, dsdt_off: u32) -> Vec<u8> {
    let mut t = acpi_header(b"FACP", 116, b"MI-FADT ");
    t.resize(116, 0);
    t[34..38].copy_from_slice(&facs_off.to_le_bytes()); // firmware_ctrl → FACS
    t[38..42].copy_from_slice(&dsdt_off.to_le_bytes()); // dsdt → DSDT
    t[43] = 9; // SCI interrupt
    t[52..56].copy_from_slice(&(PM_IO_BASE as u32).to_le_bytes());       // PM1a_EVT_BLK
    t[60..64].copy_from_slice(&((PM_IO_BASE + 4) as u32).to_le_bytes()); // PM1a_CNT_BLK
    t[72..76].copy_from_slice(&((PM_IO_BASE + 8) as u32).to_le_bytes()); // PM_TMR_BLK
    t[76..80].copy_from_slice(&(GPE0_BLK as u32).to_le_bytes());         // GPE0_BLK
    t[84] = 4; // pm1_evt_len
    t[85] = 2; // pm1_cnt_len
    t[87] = 4; // pm_tmr_len
    t[88] = 4; // gpe0_blk_len
    t[92..94].copy_from_slice(&0xFFFu16.to_le_bytes()); // plvl2_lat: C2 no soportado
    t[94..96].copy_from_slice(&0xFFFu16.to_le_bytes()); // plvl3_lat: C3 no soportado
    t[105] = 0x02; // iapc_boot_arch: 8042 presente (teclado/ratón PS/2)
    // flags: WBINVD | PROC_C1 | SLP_BUTTON | RTC_S4 | USE_PLATFORM_CLOCK
    t[108..112].copy_from_slice(&0x80A5u32.to_le_bytes());
    t
}

/// DSDT mínimo: solo define `_S5` (estado S5 = apagado) para que Linux
/// pueda pedir un apagado limpio. AML: Name(_S5, Package(4){0,0,0,0}).
fn build_dsdt() -> Vec<u8> {
    let aml: [u8; 11] = [
        0x08, b'_', b'S', b'5', b'_', // NameOp "_S5_"
        0x12, 0x04, // PackageOp, 4 elementos
        0x00, 0x00, 0x00, 0x00, // { 0, 0, 0, 0 } (SLP_TYP S5)
    ];
    let mut t = acpi_header(b"DSDT", 36 + 11, b"MI-DSDT ");
    t.extend_from_slice(&aml);
    t
}

/// MADT (Multiple APIC): un LAPIC por vCPU + IOAPIC. Sin IRQ0 override:
/// el irqchip del kernel enruta la línea 0 al pin 0 del IOAPIC, así que
/// IRQ0 (PIT) debe quedar en GSI 0.
fn build_madt(num_cpus: u32) -> Vec<u8> {
    let len = 44 + 8 * num_cpus as usize + 12;
    let mut t = acpi_header(b"APIC", len as u32, b"MI-APIC ");
    t.resize(len, 0);
    t[36..40].copy_from_slice(&0xFEE0_0000u32.to_le_bytes()); // LAPIC base
    t[40..44].copy_from_slice(&1u32.to_le_bytes()); // flags: PCAT_COMPAT (PIC dual)
    let mut off = 44;
    for i in 0..num_cpus {
        t[off] = 0; // type: processor local APIC
        t[off + 1] = 8; // length
        t[off + 2] = i as u8; // acpi processor id
        t[off + 3] = i as u8; // apic id
        t[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes()); // flags: enabled
        off += 8;
    }
    // IOAPIC
    t[off] = 1; // type: io apic
    t[off + 1] = 12; // length
    t[off + 2] = 0x01; // ioapic id
    t[off + 4..off + 8].copy_from_slice(&0xFEC0_0000u32.to_le_bytes()); // address
    t[off + 8..off + 12].copy_from_slice(&0u32.to_le_bytes()); // gsi base
    t
}

/// FACS de 64 bytes (todo a cero salvo la cabecera).
fn build_facs() -> Vec<u8> {
    let mut t = vec![0u8; 64];
    t[0..4].copy_from_slice(b"FACS");
    t[4..8].copy_from_slice(&64u32.to_le_bytes());
    t
}

/// RSDP ACPI 1.0 (20 bytes). El campo rsdt_physical_address (offset 16)
/// es el offset del RSDT dentro del blob (0); el loader le suma la base.
fn build_rsdp() -> Vec<u8> {
    let mut t = vec![0u8; 20];
    t[0..8].copy_from_slice(b"RSD PTR ");
    t[8] = 0; // checksum (loader)
    t[9..15].copy_from_slice(b"MI-VMM");
    t[15] = 0; // revision 0 (ACPI 1.0)
    // t[16..20] = 0 → RSDT (placeholder: offset 0 del blob)
    t
}

// ─── Entradas del romfile-loader ───────────────────────────────────

fn write_name(dst: &mut [u8], name: &str) {
    let b = name.as_bytes();
    let n = b.len().min(FILESZ - 1);
    dst[..n].copy_from_slice(&b[..n]);
}

fn entry_allocate(file: &str, align: u32, zone: u8) -> Vec<u8> {
    let mut e = vec![0u8; ENTRY_LEN];
    e[0..4].copy_from_slice(&CMD_ALLOCATE.to_le_bytes());
    write_name(&mut e[4..4 + FILESZ], file);
    e[60..64].copy_from_slice(&align.to_le_bytes());
    e[64] = zone;
    e
}

fn entry_add_pointer(dest: &str, src: &str, offset: u32, size: u8) -> Vec<u8> {
    let mut e = vec![0u8; ENTRY_LEN];
    e[0..4].copy_from_slice(&CMD_ADD_POINTER.to_le_bytes());
    write_name(&mut e[4..4 + FILESZ], dest);
    write_name(&mut e[60..60 + FILESZ], src);
    e[116..120].copy_from_slice(&offset.to_le_bytes());
    e[120] = size;
    e
}

fn entry_add_checksum(file: &str, offset: u32, start: u32, length: u32) -> Vec<u8> {
    let mut e = vec![0u8; ENTRY_LEN];
    e[0..4].copy_from_slice(&CMD_ADD_CHECKSUM.to_le_bytes());
    write_name(&mut e[4..4 + FILESZ], file);
    e[60..64].copy_from_slice(&offset.to_le_bytes());
    e[64..68].copy_from_slice(&start.to_le_bytes());
    e[68..72].copy_from_slice(&length.to_le_bytes());
    e
}

// ─── Construcción completa ─────────────────────────────────────────

/// Construye las tablas ACPI y los comandos del loader.
///
/// Orden dentro del blob: RSDT, FADT, DSDT, MADT, FACS. Los punteros
/// entre tablas son offsets del blob; ADD_POINTER les suma la base.
/// Los checksums se calculan DESPUÉS de parchear los punteros.
pub fn build_acpi_files(num_cpus: u32) -> AcpiFiles {
    const RSDT_OFF: u32 = 0x00;
    const FADT_OFF: u32 = 0x40;
    const DSDT_OFF: u32 = 0xC0;
    const MADT_OFF: u32 = 0xF0;
    let madt_len = (44 + 8 * num_cpus as usize + 12) as u32;
    let facs_off = (MADT_OFF + madt_len + 15) & !15; // alinear a 16
    let total = (facs_off + 64) as usize;

    let mut tables = Vec::with_capacity(total);
    tables.extend_from_slice(&build_rsdt(FADT_OFF, MADT_OFF));
    tables.resize(FADT_OFF as usize, 0);
    tables.extend_from_slice(&build_fadt(facs_off, DSDT_OFF));
    tables.resize(DSDT_OFF as usize, 0);
    tables.extend_from_slice(&build_dsdt());
    tables.resize(MADT_OFF as usize, 0);
    tables.extend_from_slice(&build_madt(num_cpus));
    tables.resize(facs_off as usize, 0);
    tables.extend_from_slice(&build_facs());
    debug_assert_eq!(tables.len(), total);

    let rsdp = build_rsdp();

    let mut loader = Vec::new();
    // 1) Alojar los dos ficheros: tablas en la zona alta, RSDP en FSEG
    //    (0xF0000-0xFFFFF, dentro del rango que Linux escanea).
    loader.extend_from_slice(&entry_allocate(ACPI_TABLES_FILE, 0x1000, ZONE_HIGH));
    loader.extend_from_slice(&entry_allocate(ACPI_RSDP_FILE, 0x10, ZONE_FSEG));
    // 2) Parchear punteros (offsets dentro del blob + base del blob).
    loader.extend_from_slice(&entry_add_pointer(
        ACPI_RSDP_FILE, ACPI_TABLES_FILE, 16, 4, // RSDP → RSDT (offset 0)
    ));
    loader.extend_from_slice(&entry_add_pointer(
        ACPI_TABLES_FILE, ACPI_TABLES_FILE, RSDT_OFF + 36, 4, // RSDT[0] → FADT
    ));
    loader.extend_from_slice(&entry_add_pointer(
        ACPI_TABLES_FILE, ACPI_TABLES_FILE, RSDT_OFF + 40, 4, // RSDT[1] → MADT
    ));
    loader.extend_from_slice(&entry_add_pointer(
        ACPI_TABLES_FILE, ACPI_TABLES_FILE, FADT_OFF + 34, 4, // FADT → FACS
    ));
    loader.extend_from_slice(&entry_add_pointer(
        ACPI_TABLES_FILE, ACPI_TABLES_FILE, FADT_OFF + 38, 4, // FADT → DSDT
    ));
    // 3) Checksums de cada tabla y del RSDP (tras los punteros).
    loader.extend_from_slice(&entry_add_checksum(ACPI_TABLES_FILE, RSDT_OFF + 9, RSDT_OFF, 44));
    loader.extend_from_slice(&entry_add_checksum(ACPI_TABLES_FILE, FADT_OFF + 9, FADT_OFF, 116));
    loader.extend_from_slice(&entry_add_checksum(ACPI_TABLES_FILE, DSDT_OFF + 9, DSDT_OFF, 36 + 11));
    loader.extend_from_slice(&entry_add_checksum(ACPI_TABLES_FILE, MADT_OFF + 9, MADT_OFF, madt_len));
    loader.extend_from_slice(&entry_add_checksum(ACPI_RSDP_FILE, 8, 0, 20));

    AcpiFiles { tables, rsdp, loader }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simula el romfile_loader de SeaBIOS con bases falsas: aplica los
    /// ADD_POINTER (suma la base del fichero fuente) y los ADD_CHECKSUM.
    fn simulate_loader(files: &AcpiFiles) -> (Vec<u8>, Vec<u8>) {
        const TABLES_BASE: usize = 0x0010_0000; // zona alta
        const RSDP_BASE: usize = 0x000F_0000; // fseg
        let mut tables = files.tables.clone();
        let mut rsdp = files.rsdp.clone();

        let read_le = |buf: &[u8], off: usize, size: u8| -> u64 {
            let mut v = 0u64;
            for i in 0..size as usize {
                v |= (buf[off + i] as u64) << (8 * i);
            }
            v
        };
        let write_le = |buf: &mut [u8], off: usize, size: u8, v: u64| {
            for i in 0..size as usize {
                buf[off + i] = ((v >> (8 * i)) & 0xFF) as u8;
            }
        };

        let mut off = 0;
        while off < files.loader.len() {
            let cmd = u32::from_le_bytes(files.loader[off..off + 4].try_into().unwrap());
            match cmd {
                CMD_ADD_POINTER => {
                    let dest = name_at(&files.loader, off + 4);
                    let src = name_at(&files.loader, off + 60);
                    let offset =
                        u32::from_le_bytes(files.loader[off + 116..off + 120].try_into().unwrap())
                            as usize;
                    let size = files.loader[off + 120];
                    let src_base = if src == ACPI_TABLES_FILE { TABLES_BASE } else { RSDP_BASE };
                    let (dst, is_rsdp) = if dest == ACPI_RSDP_FILE {
                        (&mut rsdp, true)
                    } else {
                        (&mut tables, false)
                    };
                    let _ = is_rsdp;
                    let cur = read_le(dst, offset, size);
                    write_le(dst, offset, size, cur + src_base as u64);
                }
                CMD_ADD_CHECKSUM => {
                    let file = name_at(&files.loader, off + 4);
                    let offset =
                        u32::from_le_bytes(files.loader[off + 60..off + 64].try_into().unwrap())
                            as usize;
                    let start =
                        u32::from_le_bytes(files.loader[off + 64..off + 68].try_into().unwrap())
                            as usize;
                    let length =
                        u32::from_le_bytes(files.loader[off + 68..off + 72].try_into().unwrap())
                            as usize;
                    let dst = if file == ACPI_RSDP_FILE { &mut rsdp } else { &mut tables };
                    let sum: u8 = dst[start..start + length]
                        .iter()
                        .fold(0u8, |a, &b| a.wrapping_add(b));
                    dst[offset] = dst[offset].wrapping_sub(sum);
                }
                _ => {}
            }
            off += ENTRY_LEN;
        }
        (tables, rsdp)
    }

    fn name_at(loader: &[u8], off: usize) -> String {
        let end = loader[off..off + FILESZ]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(FILESZ);
        String::from_utf8_lossy(&loader[off..off + end]).into_owned()
    }

    fn checksum(range: &[u8]) -> u8 {
        range.iter().fold(0u8, |a, &b| a.wrapping_add(b))
    }

    fn sig(buf: &[u8], off: usize) -> [u8; 4] {
        buf[off..off + 4].try_into().unwrap()
    }

    #[test]
    fn loader_simulation_produces_valid_tables() {
        let files = build_acpi_files(2);
        let (tables, rsdp) = simulate_loader(&files);

        // RSDP: firma + checksum del bloque de 20 bytes + puntero al RSDT.
        assert_eq!(&rsdp[0..8], b"RSD PTR ");
        assert_eq!(checksum(&rsdp[..20]), 0, "checksum RSDP");
        assert_eq!(u32::from_le_bytes(rsdp[16..20].try_into().unwrap()), 0x0010_0000);

        // RSDT: checksum y entrada[0] → FADT (base + 0x40), entrada[1] → MADT (base + 0xF0).
        assert_eq!(sig(&tables, 0x00), *b"RSDT");
        assert_eq!(checksum(&tables[0x00..0x00 + 44]), 0, "checksum RSDT");
        assert_eq!(
            u32::from_le_bytes(tables[0x24..0x28].try_into().unwrap()),
            0x0010_0000 + 0x40,
            "RSDT[0] apunta al FADT"
        );
        assert_eq!(
            u32::from_le_bytes(tables[0x28..0x2C].try_into().unwrap()),
            0x0010_0000 + 0xF0,
            "RSDT[1] apunta al MADT"
        );

        // FADT: checksum + punteros a DSDT y FACS.
        assert_eq!(sig(&tables, 0x40), *b"FACP");
        assert_eq!(checksum(&tables[0x40..0x40 + 116]), 0, "checksum FADT");
        assert_eq!(
            u32::from_le_bytes(tables[0x40 + 38..0x40 + 42].try_into().unwrap()),
            0x0010_0000 + 0xC0,
            "FADT.dsdt"
        );
        assert_eq!(
            u32::from_le_bytes(tables[0x40 + 34..0x40 + 38].try_into().unwrap()),
            0x0010_0000 + 0x140,
            "FADT.firmware_ctrl (FACS)"
        );
        // Bloques PM del PIIX.
        assert_eq!(u32::from_le_bytes(tables[0x40 + 60..0x40 + 64].try_into().unwrap()), 0x604);
        assert_eq!(u32::from_le_bytes(tables[0x40 + 72..0x40 + 76].try_into().unwrap()), 0x608);

        // DSDT: checksum + AML de _S5.
        assert_eq!(sig(&tables, 0xC0), *b"DSDT");
        assert_eq!(checksum(&tables[0xC0..0xC0 + 47]), 0, "checksum DSDT");
        assert_eq!(&tables[0xC0 + 36..0xC0 + 36 + 11], &[0x08, b'_', b'S', b'5', b'_', 0x12, 0x04, 0, 0, 0, 0]);

        // MADT: checksum, LAPIC base y flags PCAT.
        let madt_len = 44 + 16 + 12;
        assert_eq!(sig(&tables, 0xF0), *b"APIC");
        assert_eq!(checksum(&tables[0xF0..0xF0 + madt_len]), 0, "checksum MADT");
        assert_eq!(u32::from_le_bytes(tables[0xF0 + 36..0xF0 + 40].try_into().unwrap()), 0xFEE0_0000);
        assert_eq!(u32::from_le_bytes(tables[0xF0 + 40..0xF0 + 44].try_into().unwrap()), 1);

        // FACS presente con longitud correcta.
        assert_eq!(sig(&tables, 0x140), *b"FACS");
    }

    #[test]
    fn madt_scales_with_cpu_count() {
        let files = build_acpi_files(8);
        // 8 LAPIC + IOAPIC: MADT = 44 + 64 + 12 = 120 bytes en 0xF0.
        let (tables, _) = simulate_loader(&files);
        let madt_len = 44 + 64 + 12;
        assert_eq!(checksum(&tables[0xF0..0xF0 + madt_len]), 0, "checksum MADT 8 cpus");
        // El FACS debe estar tras el MADT (offset 0xF0+120=0x168 → 0x170).
        assert_eq!(sig(&tables, 0x170), *b"FACS");
    }

    #[test]
    fn loader_entries_have_expected_layout() {
        let files = build_acpi_files(2);
        // 2 ALLOCATE + 5 ADD_POINTER + 5 ADD_CHECKSUM = 12 entradas de 128.
        assert_eq!(files.loader.len(), 12 * ENTRY_LEN);
        // Primera: ALLOCATE de las tablas en zona alta con alineación 0x1000.
        assert_eq!(u32::from_le_bytes(files.loader[0..4].try_into().unwrap()), CMD_ALLOCATE);
        assert_eq!(
            String::from_utf8_lossy(&files.loader[4..4 + FILESZ])
                .trim_end_matches('\0'),
            ACPI_TABLES_FILE
        );
        assert_eq!(u32::from_le_bytes(files.loader[60..64].try_into().unwrap()), 0x1000);
        assert_eq!(files.loader[64], ZONE_HIGH);
        // Segunda: ALLOCATE del RSDP en FSEG.
        assert_eq!(files.loader[128 + 64], ZONE_FSEG);
    }
}