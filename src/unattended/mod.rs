//! Coordinador del subsistema de instalación desatendida (Unattended) para Two Five Five (255).
//!
//! Este módulo orquesta la detección automática del sistema operativo desde la imagen ISO,
//! la generación de plantillas de configuración (Preseed, Cloud-Init, Autounattend)
//! y la construcción de medios de disco auxiliar virtual en memoria (OEMDRV / CIDATA).

pub mod detect;
pub mod media;
pub mod templates;

pub use detect::{detect_os_from_iso, GuestOsType};
pub use media::{build_oemdrv_image, create_temp_oemdrv_file};
pub use templates::UnattendedConfig;

/// Prepara un medio auxiliar de instalación desatendida según el sistema operativo huésped.
///
/// Detecta el sistema operativo a partir de la imagen ISO indicada y genera el medio
/// de disco con la configuración apropiada:
/// - Debian / Kali Linux: genera `preseed.cfg` en un disco etiquetado `OEMDRV`.
/// - Ubuntu Cloud: genera `user-data` y `meta-data` en un disco NoCloud etiquetado `cidata`.
/// - Windows: genera `autounattend.xml` en un disco etiquetado `OEMDRV`.
/// - Otros / Desconocido: genera `preseed.cfg` como configuración predeterminada.
///
/// Devuelve la ruta [`std::path::PathBuf`] al archivo de imagen de disco auxiliar generado
/// (por ejemplo, en el directorio temporal del sistema) para su montaje en el controlador AHCI.
pub fn prepare_unattended_media(
    iso_path: &std::path::Path,
    config: &UnattendedConfig,
) -> Result<std::path::PathBuf, String> {
    let os_type = detect_os_from_iso(iso_path).unwrap_or(GuestOsType::Unknown);
    eprintln!("[unattended] Sistema operativo detectado: {}", os_type);

    match os_type {
        GuestOsType::UbuntuCloud => {
            let (user_data, meta_data) = templates::generate_cloud_init(config);
            create_temp_oemdrv_file(&[
                ("user-data", user_data.as_bytes()),
                ("meta-data", meta_data.as_bytes()),
            ])
        }
        GuestOsType::Windows => {
            let win_unattend = templates::generate_windows_unattend(config);
            create_temp_oemdrv_file(&[("autounattend.xml", win_unattend.as_bytes())])
        }
        GuestOsType::DebianKali | GuestOsType::RedHatCentOS | GuestOsType::Unknown => {
            let preseed = templates::generate_debian_preseed(config);
            create_temp_oemdrv_file(&[("preseed.cfg", preseed.as_bytes())])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn create_dummy_iso(magic: &[u8]) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("test_iso_{}.iso", std::process::id()));
        let mut f = File::create(&path).expect("No se pudo crear archivo dummy ISO");
        // Sector 16: Primary Volume Descriptor (offset 32768)
        let mut buf = vec![0u8; 32768 + 2048];
        buf[32768] = 1; // PVD
        buf[32769..32774].copy_from_slice(b"CD001");
        // Escribir identificador en el campo Volume ID (offset 32768 + 40 .. + 72)
        let len = magic.len().min(32);
        buf[32768 + 40..32768 + 40 + len].copy_from_slice(&magic[..len]);
        f.write_all(&buf).expect("Error escribiendo dummy ISO");
        path
    }

    #[test]
    fn test_prepare_unattended_debian_preseed() {
        let iso = create_dummy_iso(b"DEBIAN_12_NETINST");
        let config = UnattendedConfig {
            username: "testuser".to_string(),
            password: "testpassword".to_string(),
            hostname: "test-vm".to_string(),
            timezone: "UTC".to_string(),
        };

        let media_path = prepare_unattended_media(&iso, &config).expect("Error preparando OEMDRV");
        assert!(media_path.exists());
        let meta = std::fs::metadata(&media_path).expect("No se pudo leer metadata");
        assert!(meta.len() > 0);

        // Limpiar
        let _ = std::fs::remove_file(&iso);
        let _ = std::fs::remove_file(&media_path);
    }

    #[test]
    fn test_prepare_unattended_ubuntu_cloud_init() {
        let iso = create_dummy_iso(b"UBUNTU_SERVER_24_04");
        let config = UnattendedConfig {
            username: "clouduser".to_string(),
            password: "cloudpass".to_string(),
            hostname: "cloud-box".to_string(),
            timezone: "UTC".to_string(),
        };

        let media_path = prepare_unattended_media(&iso, &config).expect("Error preparando CIDATA");
        assert!(media_path.exists());
        let meta = std::fs::metadata(&media_path).expect("No se pudo leer metadata");
        assert!(meta.len() > 0);

        // Limpiar
        let _ = std::fs::remove_file(&iso);
        let _ = std::fs::remove_file(&media_path);
    }

    #[test]
    fn test_prepare_unattended_fallback_unknown() {
        let temp_dir = std::env::temp_dir();
        let non_existent = temp_dir.join("non_existent_iso_12345.iso");
        let config = UnattendedConfig::default();

        let media_path = prepare_unattended_media(&non_existent, &config).expect("Error preparando OEMDRV");
        assert!(media_path.exists());

        // Limpiar
        let _ = std::fs::remove_file(&media_path);
    }
}
