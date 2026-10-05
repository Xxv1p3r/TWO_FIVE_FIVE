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

/// Aplica parches al vuelo en los sectores de arranque leídos desde la ISO para activar
/// el modo 100% desatendido (Debian, Kali, Ubuntu) sin modificar el archivo ISO en disco.
pub fn patch_unattended_iso_sectors(buf: &mut [u8]) {
    // 1. ISOLINUX: Reducir timeout a 1 (0.1s) para arranque automático sin esperar confirmación
    patch_slice(buf, b"timeout 0\n", b"timeout 1\n");
    patch_slice(buf, b"timeout 0\r\n", b"timeout 1\r\n");

    // 2. ISOLINUX: Cambiar la opción por defecto a la etiqueta de instalación automatizada
    patch_slice(buf, b"default installgui", b"default autogui   ");
    patch_slice(buf, b"default install\n", b"default auto   \n");
    patch_slice(buf, b"default install\r\n", b"default auto   \r\n");

    // 3. Forzar auto=true y priority=critical en la línea append
    patch_slice(
        buf,
        b"simple-cdd/profiles=kali,offline desktop=xfce vga=788",
        b"desktop=xfce auto=true priority=critical vga=788     ",
    );

    // 4. Parchear descriptor de directorio ISO 9660 y Joliet para ampliar default.preseed a 2048 bytes
    // LBA 2343049 (0x0023C089) con longitud original 211 bytes (0x000000D3) -> longitud 2048 bytes (0x00000800)
    let dir_orig: [u8; 16] = [
        0x89, 0xC0, 0x23, 0x00, 0x00, 0x23, 0xC0, 0x89, 0xD3, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xD3,
    ];
    let dir_patch: [u8; 16] = [
        0x89, 0xC0, 0x23, 0x00, 0x00, 0x23, 0xC0, 0x89, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
        0x08, 0x00,
    ];
    patch_slice(buf, &dir_orig, &dir_patch);

    // 5. Parchear simple-cdd/default.preseed en el CD-ROM:
    // Inyecta el comando include condicional para OEMDRV y el preseed base autónomo
    let preseed_header_orig = b"# loads the simple-cdd-profiles udeb to which asks for which profiles to use,\n# load the debconf preseeding and queue packages for installation.\n";
    let preseed_header_patch = b"d-i preseed/include_command string mountmedia >/dev/null 2>&1 && [ -f /media/preseed.cfg ] && echo file:///media/preseed.cfg                    \n";

    if let Some(pos) = find_subsequence(buf, preseed_header_orig) {
        patch_slice(buf, preseed_header_orig, preseed_header_patch);

        // Si el buffer contiene espacio tras el encabezado original de 211 bytes y está a ceros,
        // inyectamos las directivas preseed base autónomas de respaldo
        let tail_start = pos + 211;
        if buf.len() >= pos + 1024 && tail_start < buf.len() && buf[tail_start] == 0 {
            let base_preseed = b"\
d-i simple-cdd/profiles multiselect kali, offline\n\
d-i debian-installer/locale string en_US.UTF-8\n\
d-i keyboard-configuration/xkb-keymap select us\n\
d-i netcfg/choose_interface select auto\n\
d-i netcfg/get_hostname string kali\n\
d-i netcfg/get_domain string local\n\
d-i apt-setup/use_mirror boolean false\n\
d-i apt-setup/cdrom/set-first boolean false\n\
d-i apt-setup/cdrom/set-next boolean false\n\
d-i apt-setup/cdrom/set-failed boolean false\n\
d-i passwd/root-login boolean true\n\
d-i passwd/root-password password kali\n\
d-i passwd/root-password-again password kali\n\
d-i passwd/make-user boolean true\n\
d-i passwd/user-fullname string kali\n\
d-i passwd/username string kali\n\
d-i passwd/user-password password kali\n\
d-i passwd/user-password-again password kali\n\
d-i passwd/user-default-groups string audio cdrom video sudo adm\n\
d-i partman/early_command string for d in $(list-devices disk); do size=$(cat /sys/block/$(basename $d)/size 2>/dev/null || echo 0); if [ \"$size\" -gt 2097152 ]; then debconf-set partman-auto/disk \"$d\"; debconf-set grub-installer/bootdev \"$d\"; break; fi; done\n\
d-i partman-auto/disk string /dev/sda\n\
d-i partman-auto/method string regular\n\
d-i partman-auto/choose_recipe select atomic\n\
d-i partman-auto/purge_lvm_from_device boolean true\n\
d-i partman-partitioning/confirm_write_new_label boolean true\n\
d-i partman-partitioning/confirm_new_label boolean true\n\
d-i partman/choose_partition select finish\n\
d-i partman/confirm boolean true\n\
d-i partman/confirm_nooverwrite boolean true\n\
d-i partman-basicfilesystems/no_swap boolean false\n\
d-i partman-lvm/device_remove_lvm boolean true\n\
d-i partman-lvm/confirm boolean true\n\
d-i partman-lvm/confirm_nochanges boolean true\n\
d-i partman-md/device_remove_md boolean true\n\
d-i partman-md/confirm boolean true\n\
d-i partman-md/confirm_nochanges boolean true\n\
d-i grub-installer/only_debian boolean true\n\
d-i grub-installer/bootdev string default\n\
d-i finish-install/reboot_in_progress note\n";
            let copy_len = base_preseed.len().min(buf.len() - tail_start);
            buf[tail_start..tail_start + copy_len].copy_from_slice(&base_preseed[..copy_len]);
        }
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn patch_slice(buf: &mut [u8], from: &[u8], to: &[u8]) {
    if buf.len() < from.len() || from.len() != to.len() {
        return;
    }
    let mut i = 0;
    while i + from.len() <= buf.len() {
        if &buf[i..i + from.len()] == from {
            buf[i..i + to.len()].copy_from_slice(to);
            i += from.len();
        } else {
            i += 1;
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

    #[test]
    fn test_patch_unattended_iso_sectors() {
        let mut sample = b"timeout 0\ndefault installgui\nappend net.ifnames=0 preseed/file=/cdrom/simple-cdd/default.preseed simple-cdd/profiles=kali,offline desktop=xfce vga=788 initrd=/install.amd/gtk/initrd.gz\n# loads the simple-cdd-profiles udeb to which asks for which profiles to use,\n# load the debconf preseeding and queue packages for installation.\nd-i preseed/early_command string anna-install simple-cdd-profiles\n".to_vec();
        patch_unattended_iso_sectors(&mut sample);
        let s = String::from_utf8_lossy(&sample);
        assert!(s.contains("timeout 1\n"));
        assert!(s.contains("default autogui   "));
        assert!(s.contains("auto=true priority=critical vga=788"));
        assert!(s.contains("include_command string mountmedia"));
    }
}
