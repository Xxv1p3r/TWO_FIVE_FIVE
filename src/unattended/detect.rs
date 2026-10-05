//! Módulo de detección de sistema operativo para imágenes ISO en Two Five Five (255).
//!
//! Inspecciona los primeros sectores del archivo ISO (incluyendo el Primary Volume Descriptor
//! en el sector 16 / offset 0x8000 según ISO 9660) y busca firmas y etiquetas de volumen
//! para clasificar el sistema operativo huésped.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Tipo de sistema operativo huésped detectado a partir de la imagen ISO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOsType {
    /// Distribuciones Debian, Kali Linux o derivadas que utilizan `debian-installer` / preseed.
    DebianKali,
    /// Distribuciones Ubuntu Server o imágenes cloud que emplean `cloud-init` / Subiquity.
    UbuntuCloud,
    /// Distribuciones Red Hat, CentOS, Fedora, AlmaLinux, Rocky Linux (Anaconda / Kickstart).
    RedHatCentOS,
    /// Instaladores de Microsoft Windows (autounattend.xml).
    Windows,
    /// Sistema operativo no reconocido o ISO genérica.
    Unknown,
}

impl std::fmt::Display for GuestOsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestOsType::DebianKali => write!(f, "Debian / Kali Linux"),
            GuestOsType::UbuntuCloud => write!(f, "Ubuntu / Cloud-Init"),
            GuestOsType::RedHatCentOS => write!(f, "Red Hat / CentOS / Fedora"),
            GuestOsType::Windows => write!(f, "Microsoft Windows"),
            GuestOsType::Unknown => write!(f, "Desconocido (Unknown)"),
        }
    }
}

/// Clasifica una cadena de texto (Volume ID, System ID, etc.) en un tipo de SO huésped.
fn classify_string(text: &str) -> Option<GuestOsType> {
    let lower = text.to_lowercase();

    // 1. Ubuntu (revisar antes de Debian por si la ISO contiene referencias compartidas)
    if lower.contains("ubuntu") {
        return Some(GuestOsType::UbuntuCloud);
    }

    // 2. Kali Linux y Debian
    if lower.contains("kali") || lower.contains("debian") {
        return Some(GuestOsType::DebianKali);
    }

    // 3. Familia Red Hat / CentOS / Fedora / Rocky / Alma
    if lower.contains("centos")
        || lower.contains("fedora")
        || lower.contains("redhat")
        || lower.contains("red hat")
        || lower.contains("rhel")
        || lower.contains("almalinux")
        || lower.contains("rocky")
    {
        return Some(GuestOsType::RedHatCentOS);
    }

    // 4. Windows (etiquetas estándar de ISOs de Windows como CCCOMA_*, CPBA_*, GSP1RM*, etc.)
    if lower.contains("windows")
        || lower.contains("win10")
        || lower.contains("win11")
        || lower.contains("winserver")
        || lower.contains("cccoma")
        || lower.contains("cpba_")
        || lower.contains("gsp1rm")
        || lower.contains("ir5_sss")
        || lower.contains("sss_x64")
        || lower.contains("bootmgr")
    {
        return Some(GuestOsType::Windows);
    }

    None
}

/// Detecta el sistema operativo a partir de un búfer en memoria que contiene los primeros
/// sectores o descriptores de la ISO.
pub fn detect_os_from_bytes(data: &[u8]) -> GuestOsType {
    const SECTOR_SIZE: usize = 2048;
    const PVD_OFFSET: usize = 16 * SECTOR_SIZE; // 0x8000 = 32768

    // Paso 1: Inspeccionar descriptores de volumen ISO 9660 desde el sector 16
    if data.len() >= PVD_OFFSET + SECTOR_SIZE {
        let mut offset = PVD_OFFSET;
        while offset + SECTOR_SIZE <= data.len() {
            let sector = &data[offset..offset + SECTOR_SIZE];
            let desc_type = sector[0];
            let standard_id = &sector[1..6];

            if standard_id != b"CD001" {
                break;
            }

            // Volume ID: offset 40..72 (32 bytes)
            if sector.len() >= 72 {
                let vol_id = String::from_utf8_lossy(&sector[40..72]);
                if let Some(os) = classify_string(&vol_id) {
                    return os;
                }
            }

            // System ID: offset 8..40 (32 bytes)
            if sector.len() >= 40 {
                let sys_id = String::from_utf8_lossy(&sector[8..40]);
                if let Some(os) = classify_string(&sys_id) {
                    return os;
                }
            }

            // Publisher ID: offset 318..446 (128 bytes)
            if sector.len() >= 446 {
                let pub_id = String::from_utf8_lossy(&sector[318..446]);
                if let Some(os) = classify_string(&pub_id) {
                    return os;
                }
            }

            // Application ID: offset 574..702 (128 bytes)
            if sector.len() >= 702 {
                let app_id = String::from_utf8_lossy(&sector[574..702]);
                if let Some(os) = classify_string(&app_id) {
                    return os;
                }
            }

            // Tipo 255 (0xFF) marca el fin del conjunto de descriptores de volumen
            if desc_type == 255 {
                break;
            }

            offset += SECTOR_SIZE;
            // No buscar más allá del sector 32 para evitar búsquedas excesivas
            if offset > 32 * SECTOR_SIZE {
                break;
            }
        }
    }

    // Paso 2: Búsqueda heurística en el texto sin formato de los primeros sectores
    // (cubre ISOHybrid, El Torito, MBR inicial y cadenas incrustadas).
    let raw_text = String::from_utf8_lossy(data);
    if let Some(os) = classify_string(&raw_text) {
        return os;
    }

    GuestOsType::Unknown
}

/// Detecta el sistema operativo huésped inspeccionando una imagen ISO en el sistema de archivos.
///
/// Abre el archivo ISO y lee hasta 256 KiB iniciales para examinar el Primary Volume Descriptor
/// (offset 0x8000), el catálogo El Torito y las firmas de distribución.
pub fn detect_os_from_iso<P: AsRef<Path>>(iso_path: P) -> Result<GuestOsType, String> {
    let path = iso_path.as_ref();
    let mut file = File::open(path)
        .map_err(|e| format!("No se pudo abrir la imagen ISO '{}': {}", path.display(), e))?;

    // Leer hasta 256 KiB para capturar el MBR, descriptores ISO 9660 y metadatos
    let mut buf = vec![0u8; 256 * 1024];
    let mut total_read = 0;
    while total_read < buf.len() {
        let n = file
            .read(&mut buf[total_read..])
            .map_err(|e| format!("Error al leer la imagen ISO '{}': {}", path.display(), e))?;
        if n == 0 {
            break;
        }
        total_read += n;
    }
    buf.truncate(total_read);

    if buf.is_empty() {
        return Ok(GuestOsType::Unknown);
    }

    Ok(detect_os_from_bytes(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Crea un búfer simulado de ISO 9660 con un Primary Volume Descriptor en el offset 0x8000.
    fn create_mock_iso_pvd(volume_id: &str, system_id: &str) -> Vec<u8> {
        const SECTOR_SIZE: usize = 2048;
        const PVD_OFFSET: usize = 16 * SECTOR_SIZE; // 0x8000 = 32768
        let total_size = PVD_OFFSET + 2 * SECTOR_SIZE; // PVD + Terminator
        let mut buf = vec![0u8; total_size];

        // Sector 16: Primary Volume Descriptor (Type 1)
        buf[PVD_OFFSET] = 1; // PVD Type
        buf[PVD_OFFSET + 1..PVD_OFFSET + 6].copy_from_slice(b"CD001");
        buf[PVD_OFFSET + 6] = 1; // Version

        // System ID (32 bytes at offset 8)
        let sys_bytes = system_id.as_bytes();
        let sys_len = sys_bytes.len().min(32);
        buf[PVD_OFFSET + 8..PVD_OFFSET + 8 + sys_len].copy_from_slice(&sys_bytes[..sys_len]);

        // Volume ID (32 bytes at offset 40)
        let vol_bytes = volume_id.as_bytes();
        let vol_len = vol_bytes.len().min(32);
        buf[PVD_OFFSET + 40..PVD_OFFSET + 40 + vol_len].copy_from_slice(&vol_bytes[..vol_len]);

        // Sector 17: Terminator (Type 255)
        let term_offset = PVD_OFFSET + SECTOR_SIZE;
        buf[term_offset] = 255;
        buf[term_offset + 1..term_offset + 6].copy_from_slice(b"CD001");
        buf[term_offset + 6] = 1;

        buf
    }

    #[test]
    fn test_detect_debian_kali_pvd() {
        let debian_buf = create_mock_iso_pvd("Debian 12.5.0 amd64 1", "LINUX");
        assert_eq!(detect_os_from_bytes(&debian_buf), GuestOsType::DebianKali);

        let kali_buf = create_mock_iso_pvd("Kali-Linux-2024.1-installer", "LINUX");
        assert_eq!(detect_os_from_bytes(&kali_buf), GuestOsType::DebianKali);
    }

    #[test]
    fn test_detect_ubuntu_cloud_pvd() {
        let ubuntu_buf = create_mock_iso_pvd("Ubuntu-Server 24.04 LTS amd64", "LINUX");
        assert_eq!(detect_os_from_bytes(&ubuntu_buf), GuestOsType::UbuntuCloud);
    }

    #[test]
    fn test_detect_redhat_centos_pvd() {
        let centos_buf = create_mock_iso_pvd("CentOS-7-x86_64-DVD-2009", "LINUX");
        assert_eq!(detect_os_from_bytes(&centos_buf), GuestOsType::RedHatCentOS);

        let fedora_buf = create_mock_iso_pvd("Fedora-Server-dvd-x86_64-39", "LINUX");
        assert_eq!(detect_os_from_bytes(&fedora_buf), GuestOsType::RedHatCentOS);
    }

    #[test]
    fn test_detect_windows_pvd() {
        let win_buf1 = create_mock_iso_pvd("CCCOMA_X64FRE_EN-US_DV9", "");
        assert_eq!(detect_os_from_bytes(&win_buf1), GuestOsType::Windows);

        let win_buf2 = create_mock_iso_pvd("WINDOWS_11_ENTERPRISE", "");
        assert_eq!(detect_os_from_bytes(&win_buf2), GuestOsType::Windows);
    }

    #[test]
    fn test_detect_unknown_pvd() {
        let unknown_buf = create_mock_iso_pvd("CUSTOM_MINIMAL_OS", "");
        assert_eq!(detect_os_from_bytes(&unknown_buf), GuestOsType::Unknown);
    }

    #[test]
    fn test_detect_fallback_raw_buffer() {
        let raw_kali = b"Some initial boot loader data containing Kali Linux live image";
        assert_eq!(detect_os_from_bytes(raw_kali), GuestOsType::DebianKali);

        let raw_ubuntu = b"Ubuntu cloud-init cloudimg amd64";
        assert_eq!(detect_os_from_bytes(raw_ubuntu), GuestOsType::UbuntuCloud);

        let raw_rhel = b"Red Hat Enterprise Linux installer";
        assert_eq!(detect_os_from_bytes(raw_rhel), GuestOsType::RedHatCentOS);

        let raw_windows = b"BOOTMGR is missing or corrupt. Press Ctrl+Alt+Del";
        assert_eq!(detect_os_from_bytes(raw_windows), GuestOsType::Windows);

        let raw_garbage = vec![0x90; 1024];
        assert_eq!(detect_os_from_bytes(&raw_garbage), GuestOsType::Unknown);
    }

    #[test]
    fn test_detect_os_from_iso_file() {
        let debian_buf = create_mock_iso_pvd("Debian 12.0.0", "LINUX");
        let temp_dir = std::env::temp_dir();
        let iso_file = temp_dir.join(format!("test_iso_{}.iso", std::process::id()));

        {
            let mut f = File::create(&iso_file).expect("Failed to create temporary ISO file");
            f.write_all(&debian_buf).expect("Failed to write mock ISO");
        }

        let result = detect_os_from_iso(&iso_file);
        let _ = std::fs::remove_file(&iso_file);

        assert_eq!(result.unwrap(), GuestOsType::DebianKali);
    }

    #[test]
    fn test_detect_os_from_iso_nonexistent() {
        let result = detect_os_from_iso("/path/to/nonexistent/iso_file_12345.iso");
        assert!(result.is_err());
    }

    #[test]
    fn test_guest_os_type_display() {
        assert_eq!(format!("{}", GuestOsType::DebianKali), "Debian / Kali Linux");
        assert_eq!(format!("{}", GuestOsType::UbuntuCloud), "Ubuntu / Cloud-Init");
        assert_eq!(format!("{}", GuestOsType::RedHatCentOS), "Red Hat / CentOS / Fedora");
        assert_eq!(format!("{}", GuestOsType::Windows), "Microsoft Windows");
        assert_eq!(format!("{}", GuestOsType::Unknown), "Desconocido (Unknown)");
    }
}
