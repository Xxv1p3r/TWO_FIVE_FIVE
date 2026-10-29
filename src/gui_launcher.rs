//! Administrador Gráfico Oficial para Two Five Five (255) KVM Hypervisor.
//!
//! Interfaz gráfica de gestión de máquinas virtuales para Two Five Five (255):
//! - Paleta y estética profesional estilo escritorio (Barra de menú, Toolbar superior con iconos 32x32).
//! - Panel izquierdo de selección de máquinas (`UIChooser` + `UIChooserItemMachine`):
//!   * Iconos de SO (`os_win7.png`, `os_win10.png`, `os_ubuntu.png`, etc.).
//!   * Estado de la máquina (`state_powered_off_16px.png`, `state_saved_16px.png`).
//!   * Cabecera global "Herramientas" (`tools_global_32px.png`).
//! - Panel derecho de detalles (`UIDetails` + `UIDetailsSet`):
//!   * Vista Previa con monitor (`preview_full_16to9_242x155px.png` - `UIMachinePreview`).
//!   * Tarjetas con iconos de 16px: `General`, `Sistema`, `Pantalla`,
//!     `Almacenamiento`, `Audio`, `Red`, `USB`.
//!   * Barras deslizadoras (sliders) interactivas en tiempo real para CPU y RAM.
//!   * Enlaces hipertexto azules (`#0055B5`) para editar o abrir configuración directamente.
//! - Administrador de Medios Virtuales integrado (`UIMediumManager`):
//!   * Pestañas para Discos Duros (`.vdi` / `.img`) y Discos Ópticos (`.iso`).
//!   * Acciones: Crear VDI, Compactar VDI (`compact`), Asignar a VM.
//! - Asistente de Creación de Máquina Virtual (`UIWizardNewVM`) con sliders de CPU, RAM y Disco.
//! - Asistente de Creación de Disco Duro Virtual (`UIWizardNewVD`) con slider de capacidad.
//! - Diálogo de Configuración Avanzada (`UIAdvancedSettingsDialogMachine`) con categorías e
//!   indicadores interactivos de memoria RAM, CPUs y VRAM con zonas verde/naranja/roja.

use crate::devices::font::FONT_8X16;
use crate::vbox_assets::VBoxAssets;
use crate::vdisk::{DiskInfo, VirtualDisk};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const WIN_W: usize = 1140;
const WIN_H: usize = 720;
const VM_CONFIG_FILE: &str = "vms.conf";
const RAM_MIN_MB: usize = 512;
const RAM_MAX_MB: usize = 16384;
const CPU_MIN: u32 = 1;
const CPU_MAX: u32 = 16;

// Paleta de interfaz gráfica Two Five Five (255) (0x00RRGGBB)
const COL_WIN_BG: u32 = 0xF0F2F5;       // Fondo de ventana
const COL_MENU_BG: u32 = 0xFAFAFA;      // Barra de menú superior
const COL_MENU_BORDER: u32 = 0xD8DCE2;  // Borde de menú
const COL_TOOLBAR_TOP: u32 = 0xFAFBFD;  // Gradiente barra de herramientas
const COL_TOOLBAR_BOT: u32 = 0xEBEEF3;
const COL_TOOLBAR_BORDER: u32 = 0xC8CCD4;
const COL_SIDEBAR_BG: u32 = 0xFFFFFF;   // Chooser izquierdo
const COL_SIDEBAR_BORDER: u32 = 0xCCD1D9;
const COL_CARD_BG: u32 = 0xFFFFFF;      // Tarjetas de detalles
const COL_CARD_HEADER: u32 = 0xF4F6F9;  // Cabecera de tarjeta
const COL_CARD_BORDER: u32 = 0xD4D8E0;  // Borde de tarjeta
const COL_TEXT_MAIN: u32 = 0x1A1E24;    // Texto principal
const COL_TEXT_MUTED: u32 = 0x5E6977;   // Texto secundario / etiquetas
const COL_LINK_BLUE: u32 = 0x0055B5;    // Enlaces interactivos
const COL_LINK_HOVER: u32 = 0x0A74E8;
const COL_BRAND_BLUE: u32 = 0x2A73C7;   // Azul corporativo Two Five Five (255)
const COL_SELECTION_BG: u32 = 0x367BC9; // Elemento seleccionado
const COL_SELECTION_HOVER: u32 = 0xE1ECF8;
const COL_INPUT_BG: u32 = 0xFFFFFF;
const COL_INPUT_BORDER: u32 = 0x3A86FF;
const COL_BTN_GRAY: u32 = 0xE9EDF3;
const COL_BTN_GRAY_BORDER: u32 = 0xBDC4D0;
const COL_BTN_GRAY_HOVER: u32 = 0xDCE2EB;
const COL_BTN_START: u32 = 0x1F883D;
const COL_BTN_START_HOVER: u32 = 0x28A745;
const COL_MONITOR_BG: u32 = 0x12151B;

/// Convierte un carácter Unicode al índice correspondiente de la fuente IBM VGA 8x16 (CP437).
#[inline]
fn unicode_to_cp437(ch: char) -> usize {
    match ch {
        '\x00'..='\x7F' => ch as usize,
        'á' | 'à' | 'â' => 0xA0,
        'Á' | 'À' | 'Â' => b'A' as usize,
        'é' | 'è' | 'ê' | 'ë' => 0x82,
        'É' | 'È' | 'Ê' | 'Ë' => 0x90,
        'í' | 'ì' | 'î' | 'ï' => 0xA1,
        'Í' | 'Ì' | 'Î' | 'Ï' => b'I' as usize,
        'ó' | 'ò' | 'ô' => 0xA2,
        'Ó' | 'Ò' | 'Ô' => b'O' as usize,
        'ú' | 'ù' | 'û' => 0xA3,
        'Ú' | 'Ù' | 'Û' => b'U' as usize,
        'ü' => 0x81,
        'Ü' => 0x9A,
        'ñ' => 0xA4,
        'Ñ' => 0xA5,
        'ç' => 0x87,
        'Ç' => 0x80,
        '¿' => 0xA8,
        '¡' => 0xAD,
        '—' | '–' => 0xC4,
        '•' | '·' => 0xFA,
        '°' | 'º' => 0xF8,
        'ª' => 0xA6,
        _ => b'?' as usize,
    }
}

/// Normaliza rutas relativas eliminando prefijos redundantestipos `./`.
pub fn normalize_rel_path(p: &str) -> String {
    let trimmed = p.trim();
    if let Some(stripped) = trimmed.strip_prefix("./") {
        stripped.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Genera un nombre amigable para la VM a partir del nombre de la ISO.
pub fn friendly_vm_name_from_iso(iso: &str, idx: usize) -> String {
    let low = iso.to_lowercase();
    if low.contains("win11") || low.contains("windows_11") {
        return "Windows 11 x64".to_string();
    }
    if low.contains("win10") || low.contains("windows_10") {
        return "Windows 10 x64".to_string();
    }
    if low.contains("win7") || low.contains("windows_7") {
        return "Windows 7 Pro x64".to_string();
    }
    if low.contains("winxp") || low.contains("windows_xp") {
        return "Windows XP SP3".to_string();
    }
    if low.contains("kali") {
        return "Kali Linux x64".to_string();
    }
    if low.contains("mint") {
        return "Linux Mint x64".to_string();
    }
    if low.contains("debian") {
        return "Debian GNU/Linux x64".to_string();
    }
    if low.contains("ubuntu") {
        return "Ubuntu Linux x64".to_string();
    }
    if low.contains("arch") {
        return "Arch Linux x64".to_string();
    }
    if low.contains("coreplus") || low.contains("tinycore") || low.contains("core-current") {
        return "TinyCore Linux".to_string();
    }
    let stem = Path::new(iso)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| format!("VM {}", idx + 1));
    let clean: String = stem
        .chars()
        .take(22)
        .map(|c| if c == '_' || c == '-' { ' ' } else { c })
        .collect();
    clean.trim().to_string()
}

/// Detecta el tipo de sistema operativo a partir del nombre o ruta de la ISO.
pub fn detect_os_type_from_iso(iso: &str) -> String {
    let low = iso.to_lowercase();
    if low.contains("win11") {
        "Windows 11 (64-bit)".to_string()
    } else if low.contains("win10") {
        "Windows 10 (64-bit)".to_string()
    } else if low.contains("win7") {
        "Windows 7 (64-bit)".to_string()
    } else if low.contains("win") {
        "Windows (64-bit)".to_string()
    } else if low.contains("ubuntu") || low.contains("mint") {
        "Ubuntu (64-bit)".to_string()
    } else if low.contains("debian") || low.contains("kali") {
        "Debian (64-bit)".to_string()
    } else if low.contains("arch") {
        "Arch Linux (64-bit)".to_string()
    } else {
        "Linux 2.6 / 3.x / 4.x / 5.x / 6.x (64-bit)".to_string()
    }
}

/// Genera un nombre de archivo sugerido para un disco virtual.
pub fn suggest_disk_filename(vm_name: &str, ext: &str) -> String {
    let mut out = String::new();
    let mut last_underscore = false;
    for c in vm_name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore && !out.is_empty() {
            out.push('_');
            last_underscore = true;
        }
    }
    let clean = out.trim_matches('_');
    let base = if clean.is_empty() { "vm_disk" } else { clean };
    let base_short = if base.len() > 18 { &base[..18] } else { base };
    format!("{}{}", base_short.trim_matches('_'), ext)
}

/// Perfil de configuración de una Máquina Virtual.
#[derive(Debug, Clone)]
pub struct VmProfile {
    pub name: String,
    pub os_type: String,
    pub cpus: u32,
    pub ram_mb: usize,
    pub iso_path: Option<String>,
    pub disk_path: Option<String>,
    pub unattended: bool,
    pub use_efi: bool,
}

impl VmProfile {
    pub fn new_default(name: &str, iso: Option<String>, disk: Option<String>) -> Self {
        let iso_norm = iso.map(|s| normalize_rel_path(&s));
        let disk_norm = disk.map(|s| normalize_rel_path(&s));
        let os_type = if let Some(ref i) = iso_norm {
            let low = i.to_lowercase();
            if low.contains("win11") {
                "Windows 11 (64-bit)".to_string()
            } else if low.contains("win10") {
                "Windows 10 (64-bit)".to_string()
            } else if low.contains("win7") {
                "Windows 7 (64-bit)".to_string()
            } else if low.contains("win") {
                "Windows (64-bit)".to_string()
            } else if low.contains("ubuntu") {
                "Ubuntu (64-bit)".to_string()
            } else if low.contains("debian") {
                "Debian (64-bit)".to_string()
            } else if low.contains("arch") {
                "Arch Linux (64-bit)".to_string()
            } else {
                "Linux 2.6 / 3.x / 4.x / 5.x / 6.x (64-bit)".to_string()
            }
        } else {
            "Linux (64-bit)".to_string()
        };
        Self {
            name: name.to_string(),
            os_type,
            cpus: 4,
            ram_mb: 4096,
            iso_path: iso_norm,
            disk_path: disk_norm,
            unattended: false,
            use_efi: false,
        }
    }
}

/// Selección confirmada por el usuario para arrancar la VM.
#[derive(Debug, Clone)]
pub struct LauncherSelection {
    pub vm_name: String,
    pub cpus: u32,
    pub ram_mb: usize,
    pub iso_path: Option<PathBuf>,
    pub disk_path: Option<PathBuf>,
    pub unattended: bool,
}

/// Resultado de la ejecución del GUI de Two Five Five (255).
pub enum GuiOutcome {
    Selected(LauncherSelection),
    Cancelled,
    Headless,
}

/// Carga los perfiles de VM desde `vms.conf` o los inicializa automáticamente.
pub fn load_vm_profiles(available_isos: &[String], available_disks: &[String]) -> Vec<VmProfile> {
    if let Ok(content) = fs::read_to_string(VM_CONFIG_FILE) {
        let mut list: Vec<VmProfile> = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() >= 6 {
                let mut name = parts[0].trim().to_string();
                let os_type = parts[1].trim().to_string();
                let cpus: u32 = parts[2].trim().parse().unwrap_or(4).clamp(1, 16);
                let ram_mb: usize = parts[3].trim().parse().unwrap_or(4096).max(256);
                let iso_path = if parts[4].trim().is_empty() {
                    None
                } else {
                    let raw = normalize_rel_path(parts[4]);
                    if Path::new(&raw).exists() {
                        Some(raw)
                    } else if let Some(fname) = Path::new(&raw).file_name() {
                        let fname_str = fname.to_string_lossy();
                        if let Some(matched) = available_isos.iter().find(|iso| {
                            Path::new(iso).file_name().map(|f| f.to_string_lossy() == fname_str).unwrap_or(false)
                        }) {
                            Some(matched.clone())
                        } else if Path::new(&fname_str.as_ref()).exists() {
                            Some(fname_str.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                let disk_path = if parts[5].trim().is_empty() {
                    None
                } else {
                    let raw = normalize_rel_path(parts[5]);
                    if Path::new(&raw).exists() {
                        Some(raw)
                    } else if let Some(fname) = Path::new(&raw).file_name() {
                        let fname_str = fname.to_string_lossy();
                        if let Some(matched) = available_disks.iter().find(|d| {
                            Path::new(d).file_name().map(|f| f.to_string_lossy() == fname_str).unwrap_or(false)
                        }) {
                            Some(matched.clone())
                        } else if Path::new(&fname_str.as_ref()).exists() {
                            Some(fname_str.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                let unattended = parts
                    .get(6)
                    .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let use_efi = parts
                    .get(7)
                    .map(|v| v.trim() == "1" || v.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);

                if name.ends_with("...") {
                    if let Some(ref iso) = iso_path {
                        name = friendly_vm_name_from_iso(iso, list.len());
                    } else {
                        name = name.trim_end_matches('.').to_string();
                    }
                }

                let is_dup = list.iter().any(|p| p.name == name);
                if !is_dup {
                    list.push(VmProfile {
                        name,
                        os_type,
                        cpus,
                        ram_mb,
                        iso_path,
                        disk_path,
                        unattended,
                        use_efi,
                    });
                }
            }
        }
        if !list.is_empty() {
            let _ = save_vm_profiles(&list);
            return list;
        }
    }

    let mut profiles = Vec::new();
    let default_disk = available_disks.first().cloned();
    if available_isos.is_empty() {
        profiles.push(VmProfile::new_default(
            "Windows 7 Pro",
            None,
            default_disk,
        ));
    } else {
        for (idx, iso) in available_isos.iter().enumerate() {
            let clean_name = friendly_vm_name_from_iso(iso, idx);
            profiles.push(VmProfile::new_default(
                &clean_name,
                Some(iso.clone()),
                default_disk.clone(),
            ));
        }
    }
    let _ = save_vm_profiles(&profiles);
    profiles
}

/// Guarda los perfiles de VM en `vms.conf`.
pub fn save_vm_profiles(profiles: &[VmProfile]) -> std::io::Result<()> {
    let mut out = String::from("# Two Five Five (255) - Perfiles de Maquinas Virtuales\n");
    out.push_str("# Formato: nombre|os_type|cpus|ram_mb|iso_path|disk_path|unattended|use_efi\n");
    for p in profiles {
        out.push_str(&format!(
            "{}|{}|{}|{}|{}|{}|{}|{}\n",
            p.name.replace('|', "/"),
            p.os_type.replace('|', "/"),
            p.cpus,
            p.ram_mb,
            p.iso_path.as_deref().unwrap_or(""),
            p.disk_path.as_deref().unwrap_or(""),
            if p.unattended { 1 } else { 0 },
            if p.use_efi { 1 } else { 0 }
        ));
    }
    fs::write(VM_CONFIG_FILE, out)
}

/// Determina el directorio inicial predeterminado para el selector de archivos ISO.
pub fn default_iso_browse_dir(current_iso: Option<&str>) -> String {
    if let Some(cur) = current_iso {
        let p = Path::new(cur);
        if p.exists() {
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() && parent.is_dir() {
                    if let Ok(canon) = fs::canonicalize(parent) {
                        return format!("{}/", canon.to_string_lossy());
                    }
                }
            }
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let h = Path::new(&home);
        candidates.push(h.join("Escritorio").join("isos"));
        candidates.push(h.join("Desktop").join("isos"));
        candidates.push(h.join("isos"));
        candidates.push(h.join("Descargas"));
        candidates.push(h.join("Downloads"));
        candidates.push(h.join("Escritorio"));
        candidates.push(h.join("Desktop"));
        candidates.push(h.to_path_buf());
    }
    candidates.push(PathBuf::from("./isos"));
    candidates.push(PathBuf::from("../isos"));
    candidates.push(PathBuf::from("."));

    for p in candidates {
        if p.is_dir() {
            if let Ok(canon) = fs::canonicalize(&p) {
                return format!("{}/", canon.to_string_lossy());
            }
        }
    }
    String::from("./")
}

/// Abre el gestor de archivos nativo del sistema (`zenity`, `kdialog` o `tkinter`)
/// para seleccionar cualquier imagen `.iso` del equipo.
pub fn open_iso_file_dialog(current_iso: Option<&str>) -> Option<String> {
    let start_dir = default_iso_browse_dir(current_iso);

    // 1. Intentar con Zenity (GTK / Cinnamon / GNOME)
    if let Ok(out) = std::process::Command::new("zenity")
        .arg("--file-selection")
        .arg("--title=Seleccionar imagen ISO — Two Five Five (255)")
        .arg(format!("--filename={}", start_dir))
        .arg("--file-filter=Imagenes ISO (*.iso) | *.iso *.ISO")
        .arg("--file-filter=Todos los archivos | *")
        .output()
    {
        if out.status.success() {
            let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !chosen.is_empty() && Path::new(&chosen).is_file() {
                return Some(normalize_selected_file_path(&chosen));
            }
        }
        return None;
    }

    // 2. Fallback a KDialog (KDE Plasma)
    if let Ok(out) = std::process::Command::new("kdialog")
        .arg("--getopenfilename")
        .arg(&start_dir)
        .arg("*.iso *.ISO|Imagenes ISO (*.iso)")
        .arg("--title")
        .arg("Seleccionar imagen ISO — Two Five Five (255)")
        .output()
    {
        if out.status.success() {
            let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !chosen.is_empty() && Path::new(&chosen).is_file() {
                return Some(normalize_selected_file_path(&chosen));
            }
        }
        return None;
    }

    // 3. Fallback a Python3 Tkinter FileDialog
    let py_script = format!(
        "import tkinter as tk; from tkinter import filedialog; root = tk.Tk(); root.withdraw(); f = filedialog.askopenfilename(title='Seleccionar imagen ISO — Two Five Five (255)', initialdir={:?}, filetypes=[('Imagenes ISO', '*.iso *.ISO'), ('Todos', '*.*')]); print(f if f else '')",
        start_dir
    );
    if let Ok(out) = std::process::Command::new("python3").arg("-c").arg(&py_script).output() {
        if out.status.success() {
            let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !chosen.is_empty() && Path::new(&chosen).is_file() {
                return Some(normalize_selected_file_path(&chosen));
            }
        }
    }

    None
}

/// Abre el gestor de archivos nativo del sistema para seleccionar un disco duro virtual (`.vdi`, `.img`, `.raw`).
pub fn open_disk_file_dialog(current_disk: Option<&str>) -> Option<String> {
    let start_dir = if let Some(cur) = current_disk {
        let p = Path::new(cur);
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() && parent.is_dir() {
                fs::canonicalize(parent)
                    .map(|c| format!("{}/", c.to_string_lossy()))
                    .unwrap_or_else(|_| String::from("./"))
            } else {
                String::from("./")
            }
        } else {
            String::from("./")
        }
    } else {
        fs::canonicalize(".")
            .map(|c| format!("{}/", c.to_string_lossy()))
            .unwrap_or_else(|_| String::from("./"))
    };

    if let Ok(out) = std::process::Command::new("zenity")
        .arg("--file-selection")
        .arg("--title=Seleccionar Disco Duro Virtual — Two Five Five (255)")
        .arg(format!("--filename={}", start_dir))
        .arg("--file-filter=Discos Virtuales (*.vdi, *.img, *.raw) | *.vdi *.VDI *.img *.IMG *.raw *.RAW *.vhd")
        .arg("--file-filter=Todos los archivos | *")
        .output()
    {
        if out.status.success() {
            let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !chosen.is_empty() && Path::new(&chosen).is_file() {
                return Some(normalize_selected_file_path(&chosen));
            }
        }
        return None;
    }

    if let Ok(out) = std::process::Command::new("kdialog")
        .arg("--getopenfilename")
        .arg(&start_dir)
        .arg("*.vdi *.img *.raw *.vhd|Discos Virtuales")
        .output()
    {
        if out.status.success() {
            let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !chosen.is_empty() && Path::new(&chosen).is_file() {
                return Some(normalize_selected_file_path(&chosen));
            }
        }
        return None;
    }

    None
}

/// Si el archivo seleccionado reside dentro del directorio de trabajo actual, devuelve su nombre relativo;
/// si está en otra carpeta, conserva su ruta completa.
fn normalize_selected_file_path(chosen: &str) -> String {
    if let (Ok(cwd), Ok(full)) = (std::env::current_dir().and_then(fs::canonicalize), fs::canonicalize(chosen)) {
        if let Ok(rel) = full.strip_prefix(&cwd) {
            return normalize_rel_path(&rel.to_string_lossy());
        }
    }
    normalize_rel_path(chosen)
}

/// Escanea archivos `.iso` tanto en el directorio del proyecto como en carpetas de ISOs del usuario.
pub fn scan_iso_files() -> Vec<String> {
    let mut isos = Vec::new();
    let mut search_dirs: Vec<PathBuf> = vec![PathBuf::from(".")];
    if let Ok(home) = std::env::var("HOME") {
        let h = Path::new(&home);
        search_dirs.push(h.join("Escritorio").join("isos"));
        search_dirs.push(h.join("Desktop").join("isos"));
        search_dirs.push(h.join("isos"));
    }
    search_dirs.push(PathBuf::from("./isos"));
    search_dirs.push(PathBuf::from("../isos"));

    for dir_path in &search_dirs {
        if !dir_path.is_dir() {
            continue;
        }
        let is_current = dir_path.as_os_str() == ".";
        if let Ok(entries) = fs::read_dir(dir_path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(ext) = p.extension() {
                        if ext.eq_ignore_ascii_case("iso") {
                            let formatted = if is_current {
                                p.file_name()
                                    .map(|n| normalize_rel_path(&n.to_string_lossy()))
                                    .unwrap_or_default()
                            } else {
                                fs::canonicalize(&p)
                                    .map(|c| c.to_string_lossy().to_string())
                                    .unwrap_or_else(|_| p.to_string_lossy().to_string())
                            };
                            if !formatted.is_empty() && !isos.contains(&formatted) {
                                isos.push(formatted);
                            }
                        }
                    }
                }
            }
        }
    }
    isos.sort();
    isos
}

/// Escanea archivos de discos virtuales locales.
pub fn scan_disk_files() -> Vec<String> {
    let mut disks = Vec::new();
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension() {
                    let ext_s = ext.to_string_lossy().to_lowercase();
                    if matches!(ext_s.as_str(), "vdi" | "img" | "raw" | "vhd") {
                        if let Some(name) = p.file_name() {
                            let n = normalize_rel_path(&name.to_string_lossy());
                            if !n.contains("oemdrv") && !disks.contains(&n) {
                                disks.push(n);
                            }
                        }
                    }
                }
            }
        }
    }
    disks.sort_by(|a, b| {
        let a_vdi = a.to_lowercase().ends_with(".vdi");
        let b_vdi = b.to_lowercase().ends_with(".vdi");
        b_vdi.cmp(&a_vdi).then_with(|| a.cmp(b))
    });
    disks
}

// ─────────────────────────────────────────────────────────────────────────────
// Estructuras de Estado y Vista de VirtualBox
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainViewTab {
    Details, // Vista clásica de Detalles de la máquina (UIDetails)
    Media,   // Administrador de Medios Virtuales (UIMediumManager)
    Logs,    // Registros / Diagnóstico
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaTab {
    HardDisks,
    OpticalDisks,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalDialog {
    None,
    NewVmWizard,        // UIWizardNewVM
    CreateDiskWizard,   // UIWizardNewVD
    SettingsDialog,     // UIAdvancedSettingsDialogMachine
    AboutDialog,        // UIAboutApplicationDialog
    LogsDialog,         // UIVMActivityLogDialog
}

/// Estado de una ventana flotante con soporte de arrastre e interacción.
#[derive(Debug, Clone)]
pub struct FloatingWindowState {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub is_dragging: bool,
    pub drag_off_x: usize,
    pub drag_off_y: usize,
}

impl FloatingWindowState {
    pub fn new_centered(w: usize, h: usize, screen_w: usize, screen_h: usize) -> Self {
        let x = (screen_w.saturating_sub(w)) / 2;
        let y = (screen_h.saturating_sub(h)) / 2;
        Self {
            x,
            y,
            w,
            h,
            is_dragging: false,
            drag_off_x: 0,
            drag_off_y: 0,
        }
    }

    pub fn titlebar_rect(&self) -> (usize, usize, usize, usize) {
        (self.x, self.y, self.w, 32)
    }

    pub fn close_btn_rect(&self) -> (usize, usize, usize, usize) {
        (self.x + self.w.saturating_sub(36), self.y + 4, 28, 24)
    }

    pub fn contains_point(&self, px: usize, py: usize) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn is_in_titlebar(&self, px: usize, py: usize) -> bool {
        let (tx, ty, tw, th) = self.titlebar_rect();
        let (cx, cy, cw, ch) = self.close_btn_rect();
        let in_close = px >= cx && px < cx + cw && py >= cy && py < cy + ch;
        px >= tx && px < tx + tw && py >= ty && py < ty + th && !in_close
    }

    pub fn is_in_close_btn(&self, px: usize, py: usize) -> bool {
        let (cx, cy, cw, ch) = self.close_btn_rect();
        px >= cx && px < cx + cw && py >= cy && py < cy + ch
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    General,
    System,
    Display,
    Storage,
    Audio,
    Network,
    Usb,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveField {
    None,
    WizardVmName,
    WizardDiskName,
    SettingsVmName,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveSlider {
    None,
    MainRam,
    MainCpu,
    SettingsRam,
    SettingsCpu,
    SettingsVram,
    WizardRam,
    WizardCpu,
    WizardDisk,
    CreateDiskSize,
}

struct SliderRect {
    kind: ActiveSlider,
    track_x: usize,
    track_w: usize,
    hit_x: usize,
    hit_y: usize,
    hit_w: usize,
    hit_h: usize,
}

impl SliderRect {
    fn contains(&self, mx: usize, my: usize) -> bool {
        mx >= self.hit_x && mx < self.hit_x + self.hit_w && my >= self.hit_y && my < self.hit_y + self.hit_h
    }

    fn ratio_at(&self, mx: usize) -> f32 {
        if self.track_w == 0 {
            return 0.0;
        }
        let rel = (mx as isize - self.track_x as isize).clamp(0, self.track_w as isize) as f32;
        (rel / (self.track_w as f32)).clamp(0.0, 1.0)
    }
}

struct ButtonRect {
    id: String,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl ButtonRect {
    fn contains(&self, mx: usize, my: usize) -> bool {
        mx >= self.x && mx < self.x + self.w && my >= self.y && my < self.y + self.h
    }
}

struct Canvas {
    buf: Vec<u32>,
    w: usize,
    h: usize,
    buttons: Vec<ButtonRect>,
    sliders: Vec<SliderRect>,
    mx: usize,
    my: usize,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            buf: vec![COL_WIN_BG; w * h],
            w,
            h,
            buttons: Vec::new(),
            sliders: Vec::new(),
            mx: 0,
            my: 0,
        }
    }

    fn clear(&mut self, color: u32) {
        self.buf.fill(color);
        self.buttons.clear();
        self.sliders.clear();
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x2 = (x + w).min(self.w);
        let y2 = (y + h).min(self.h);
        for py in y..y2 {
            let row = py * self.w;
            for px in x..x2 {
                self.buf[row + px] = color;
            }
        }
    }

    fn draw_border(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.fill_rect(x, y, w, 1, color);
        self.fill_rect(x, (y + h).saturating_sub(1), w, 1, color);
        self.fill_rect(x, y, 1, h, color);
        self.fill_rect((x + w).saturating_sub(1), y, 1, h, color);
    }

    fn draw_char(&mut self, x: usize, y: usize, ch: char, color: u32) {
        let idx = unicode_to_cp437(ch);
        let glyph = &FONT_8X16[idx * 16..(idx + 1) * 16];
        for (row_idx, &bits) in glyph.iter().enumerate() {
            let py = y + row_idx;
            if py >= self.h {
                break;
            }
            for col_idx in 0..8 {
                if (bits & (0x80 >> col_idx)) != 0 {
                    let px = x + col_idx;
                    if px < self.w {
                        self.buf[py * self.w + px] = color;
                    }
                }
            }
        }
    }

    fn draw_text(&mut self, x: usize, y: usize, text: &str, color: u32) {
        let mut cur_x = x;
        for ch in text.chars() {
            if cur_x + 8 > self.w {
                break;
            }
            self.draw_char(cur_x, y, ch, color);
            cur_x += 8;
        }
    }

    fn draw_text_clipped(&mut self, x: usize, y: usize, max_chars: usize, text: &str, color: u32) {
        if max_chars == 0 {
            return;
        }
        let char_count = text.chars().count();
        if char_count <= max_chars {
            self.draw_text(x, y, text, color);
        } else if max_chars > 3 {
            let take_len = max_chars - 3;
            let mut truncated: String = text.chars().take(take_len).collect();
            truncated.push_str("...");
            self.draw_text(x, y, &truncated, color);
        } else {
            let truncated: String = text.chars().take(max_chars).collect();
            self.draw_text(x, y, &truncated, color);
        }
    }

    /// Dibuja un emblema corporativo geométrico de Two Five Five (255).
    fn draw_emblem_255(&mut self, x: usize, y: usize, size: usize) {
        self.fill_rect(x, y, size, size, COL_BRAND_BLUE);
        self.draw_border(x, y, size, size, 0x164684);
        if size >= 32 {
            self.fill_rect(x + 2, y + 2, size - 4, size / 2 - 2, 0x3A86FF);
            self.draw_text(x + (size.saturating_sub(24)) / 2, y + (size.saturating_sub(16)) / 2, "255", 0xFFFFFF);
        } else {
            self.fill_rect(x + 3, y + 3, size.saturating_sub(6), size.saturating_sub(6), 0x3A86FF);
            self.fill_rect(x + 5, y + 5, size.saturating_sub(10), size.saturating_sub(10), 0xFFFFFF);
        }
    }

    /// Dibuja una barra deslizadora (slider) interactiva con zonas de salud (verde/naranja/rojo),
    /// marcas de graduación y cursor arrastrable en tiempo real.
    fn draw_slider(
        &mut self,
        kind: ActiveSlider,
        x: usize,
        y: usize,
        w: usize,
        ratio: f32,
        warn_start: f32,
        danger_start: f32,
        is_dragging: bool,
    ) -> bool {
        let ratio = ratio.clamp(0.0, 1.0);
        let hit_x = x.saturating_sub(8);
        let hit_y = y.saturating_sub(7);
        let hit_w = w + 16;
        let hit_h = 24;
        let hov = self.mx >= hit_x && self.mx < hit_x + hit_w && self.my >= hit_y && self.my < hit_y + hit_h;

        self.sliders.push(SliderRect {
            kind,
            track_x: x,
            track_w: w,
            hit_x,
            hit_y,
            hit_w,
            hit_h,
        });

        // Pista base y zonas de color (verde = óptimo, naranja = precaución, rojo = crítico)
        let track_h = 8usize;
        self.fill_rect(x, y, w, track_h, 0xE2E6EC);
        let w_green = ((w as f32) * warn_start.clamp(0.0, 1.0)).round() as usize;
        let w_orange = ((w as f32) * (danger_start.clamp(warn_start, 1.0) - warn_start.clamp(0.0, 1.0))).round() as usize;
        let w_red = w.saturating_sub(w_green + w_orange);

        // Franja inferior de zonas de salud
        let zone_y = y + track_h + 2;
        if w_green > 0 {
            self.fill_rect(x, zone_y, w_green, 4, 0x4CAF50);
        }
        if w_orange > 0 {
            self.fill_rect(x + w_green, zone_y, w_orange, 4, 0xFF9800);
        }
        if w_red > 0 {
            self.fill_rect(x + w_green + w_orange, zone_y, w_red, 4, 0xF44336);
        }

        // Progreso activo sobre la pista principal
        let fill_w = ((w as f32) * ratio).round() as usize;
        let active_col = if ratio >= danger_start {
            0xE53935
        } else if ratio >= warn_start {
            0xFB8C00
        } else {
            COL_BRAND_BLUE
        };
        if fill_w > 0 {
            self.fill_rect(x, y, fill_w.min(w), track_h, active_col);
        }
        self.draw_border(x, y, w, track_h, 0xB0B8C4);

        // Marcas de graduación (ticks)
        for tick in 0..=8 {
            let tx = x + (w * tick) / 8;
            self.fill_rect(tx.min(self.w.saturating_sub(1)), y + track_h + 6, 1, 3, 0x9AA5B4);
        }

        // Cursor / Thumb arrastrable (12x18 px)
        let thumb_cx = x + fill_w.min(w);
        let thumb_w = 12usize;
        let thumb_h = 18usize;
        let thumb_x = thumb_cx.saturating_sub(thumb_w / 2);
        let thumb_y = y.saturating_sub(5);
        let thumb_bg = if is_dragging {
            COL_BRAND_BLUE
        } else if hov {
            0xE8F1FC
        } else {
            0xFFFFFF
        };
        let thumb_border = if is_dragging || hov {
            COL_LINK_BLUE
        } else {
            0x6C7A89
        };
        self.fill_rect(thumb_x, thumb_y, thumb_w, thumb_h, thumb_bg);
        self.draw_border(thumb_x, thumb_y, thumb_w, thumb_h, thumb_border);
        // Línea central de agarre del thumb
        let grip_col = if is_dragging { 0xFFFFFF } else { COL_BRAND_BLUE };
        self.fill_rect(thumb_x + thumb_w / 2, thumb_y + 4, 1, thumb_h - 8, grip_col);

        hov
    }

    fn register_btn(&mut self, id: &str, x: usize, y: usize, w: usize, h: usize) -> bool {
        let hovered = self.mx >= x && self.mx < x + w && self.my >= y && self.my < y + h;
        self.buttons.push(ButtonRect {
            id: id.to_string(),
            x,
            y,
            w,
            h,
        });
        hovered
    }

    fn draw_button_gray(
        &mut self,
        id: &str,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        label: &str,
    ) -> bool {
        let hov = self.register_btn(id, x, y, w, h);
        let bg = if hov { COL_BTN_GRAY_HOVER } else { COL_BTN_GRAY };
        self.fill_rect(x, y, w, h, bg);
        self.draw_border(x, y, w, h, COL_BTN_GRAY_BORDER);
        let text_w = label.chars().count() * 8;
        let tx = x + (w.saturating_sub(text_w)) / 2;
        let ty = y + (h.saturating_sub(16)) / 2;
        self.draw_text_clipped(tx, ty, w / 8, label, COL_TEXT_MAIN);
        hov
    }

    fn draw_button_primary(
        &mut self,
        id: &str,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        label: &str,
    ) -> bool {
        let hov = self.register_btn(id, x, y, w, h);
        let bg = if hov { COL_BTN_START_HOVER } else { COL_BTN_START };
        self.fill_rect(x, y, w, h, bg);
        self.draw_border(x, y, w, h, 0x186A3B);
        let text_w = label.chars().count() * 8;
        let tx = x + (w.saturating_sub(text_w)) / 2;
        let ty = y + (h.saturating_sub(16)) / 2;
        self.draw_text(tx, ty, label, 0xFFFFFF);
        hov
    }

    fn draw_toolbar_action(
        &mut self,
        assets: &VBoxAssets,
        id: &str,
        icon_name: &str,
        label: &str,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        is_primary: bool,
    ) -> bool {
        let hov = self.register_btn(id, x, y, w, h);
        if is_primary {
            let bg = if hov { 0xD6E8FC } else { 0xE3EFFD };
            self.fill_rect(x, y, w, h, bg);
            self.draw_border(x, y, w, h, COL_BRAND_BLUE);
        } else if hov {
            self.fill_rect(x, y, w, h, 0xE5EBF2);
            self.draw_border(x, y, w, h, 0xB6BFCB);
        }

        // Icono centrado (32x32)
        let icon_x = x + (w.saturating_sub(32)) / 2;
        let icon_y = y + 4;
        if !assets.draw(icon_name, &mut self.buf, self.w, self.h, icon_x as isize, icon_y as isize) {
            // Fallback geométrico si el asset no estuviese disponible
            self.fill_rect(icon_x, icon_y, 32, 32, if is_primary { COL_BRAND_BLUE } else { 0x6C7A89 });
        }

        // Texto centrado debajo
        let text_w = label.chars().count() * 8;
        let tx = x + (w.saturating_sub(text_w)) / 2;
        let ty = y + 38;
        let col = if is_primary { COL_LINK_BLUE } else { COL_TEXT_MAIN };
        self.draw_text_clipped(tx, ty, w / 8, label, col);

        hov
    }

    fn draw_details_card_header(
        &mut self,
        assets: &VBoxAssets,
        icon_name: &str,
        title: &str,
        x: usize,
        y: usize,
        w: usize,
    ) {
        self.fill_rect(x, y, w, 26, COL_CARD_HEADER);
        self.draw_border(x, y, w, 26, COL_CARD_BORDER);

        // Icono de 16x16
        assets.draw(icon_name, &mut self.buf, self.w, self.h, (x + 8) as isize, (y + 5) as isize);

        // Título de la categoría
        self.draw_text(x + 30, y + 5, title, COL_TEXT_MAIN);
    }

    /// Dibuja una sombra proyectada suave y realista alrededor de una ventana flotante
    fn draw_drop_shadow(&mut self, x: usize, y: usize, w: usize, h: usize, radius: usize) {
        let shadow_alpha_base = 90u32;
        let x_end = (x + w + radius).min(self.w);
        let y_end = (y + h + radius).min(self.h);

        for py in y..y_end {
            let row = py * self.w;
            for px in x..x_end {
                let in_window = px >= x && px < x + w && py >= y && py < y + h;
                if in_window {
                    continue;
                }
                let dx = if px < x { x - px } else if px >= x + w { px - (x + w) + 1 } else { 0 };
                let dy = if py < y { y - py } else if py >= y + h { py - (y + h) + 1 } else { 0 };
                let dist = dx.max(dy);
                if dist <= radius {
                    let factor = (radius + 1 - dist) as u32;
                    let alpha = (shadow_alpha_base * factor) / (radius as u32 + 1);
                    let orig = self.buf[row + px];
                    let r = ((orig >> 16) & 0xFF) * (255 - alpha) / 255;
                    let g = ((orig >> 8) & 0xFF) * (255 - alpha) / 255;
                    let b = (orig & 0xFF) * (255 - alpha) / 255;
                    self.buf[row + px] = (r << 16) | (g << 8) | b;
                }
            }
        }
    }

    /// Dibuja el marco y barra de título de una ventana flotante con botón [X] de cierre e icono
    fn draw_floating_titlebar(
        &mut self,
        assets: &VBoxAssets,
        fw: &FloatingWindowState,
        title: &str,
        icon_name: Option<&str>,
    ) -> bool {
        // Fondo y marcos de la ventana flotante
        self.fill_rect(fw.x, fw.y, fw.w, fw.h, COL_CARD_BG);
        self.draw_border(fw.x, fw.y, fw.w, fw.h, COL_BRAND_BLUE);
        if fw.w > 2 && fw.h > 2 {
            self.draw_border(fw.x + 1, fw.y + 1, fw.w - 2, fw.h - 2, 0x4B88D6);
        }

        // Barra de título de la ventana flotante
        self.fill_rect(fw.x + 2, fw.y + 2, fw.w - 4, 30, COL_CARD_HEADER);
        self.fill_rect(fw.x + 2, fw.y + 31, fw.w - 4, 1, COL_CARD_BORDER);

        // Icono en barra de título
        let mut tx = fw.x + 10;
        if let Some(icon) = icon_name {
            if icon == "emblem_255" {
                self.draw_emblem_255(tx, fw.y + 8, 16);
                tx += 22;
            } else if assets.draw(icon, &mut self.buf, self.w, self.h, tx as isize, (fw.y + 7) as isize) {
                tx += 22;
            }
        }

        // Título de la ventana flotante
        let max_chars = fw.w.saturating_sub((tx - fw.x) + 40) / 8;
        self.draw_text_clipped(tx, fw.y + 8, max_chars, title, COL_TEXT_MAIN);

        // Botón [X] de cierre en barra de título
        let (cx, cy, cw, ch) = fw.close_btn_rect();
        let is_hov = self.register_btn("modal_floating_close_btn", cx, cy, cw, ch);
        let bg_close = if is_hov { 0xE81123 } else { 0xE6ECF2 };
        let fg_close = if is_hov { 0xFFFFFF } else { 0x333A42 };
        self.fill_rect(cx, cy, cw, ch, bg_close);
        self.draw_border(cx, cy, cw, ch, if is_hov { 0xB20E1B } else { 0xC8D1DC });
        self.draw_text(cx + 10, cy + 3, "x", fg_close);

        is_hov
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Lanzamiento Externo de Máquinas Virtuales (Multi-Ventana Two Five Five)
// ─────────────────────────────────────────────────────────────────────────────

/// Inicia una máquina virtual en su propio proceso independiente con ventana externa de escritorio.
/// La ventana del Administrador GUI de Two Five Five (255) permanece abierta, responsiva y activa.
pub fn launch_vm_external(
    p: &VmProfile,
    running_vms: &mut HashMap<String, std::process::Child>,
    status_msg: &mut String,
    status_is_err: &mut bool,
) {
    if let Some(child) = running_vms.get_mut(&p.name) {
        match child.try_wait() {
            Ok(None) => {
                *status_msg = format!("La máquina virtual '{}' ya está en ejecución en su propia ventana externa.", p.name);
                *status_is_err = false;
                return;
            }
            _ => {
                running_vms.remove(&p.name);
            }
        }
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => PathBuf::from("two-five-five"),
    };

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--no-gui");
    cmd.arg(format!("--vm-name={}", p.name));
    cmd.arg(format!("--cpus={}", p.cpus));
    cmd.arg(format!("--ram={}", p.ram_mb));
    if let Some(ref iso) = p.iso_path {
        cmd.arg(format!("--iso={}", iso));
    }
    if let Some(ref disk) = p.disk_path {
        cmd.arg(format!("--disk={}", disk));
    }
    if p.unattended {
        cmd.arg("--unattended");
    }

    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());

    match cmd.spawn() {
        Ok(child) => {
            *status_msg = format!("Máquina virtual '{}' iniciada en su propia ventana externa.", p.name);
            *status_is_err = false;
            running_vms.insert(p.name.clone(), child);
        }
        Err(e) => {
            *status_msg = format!("Error al iniciar máquina virtual '{}': {}", p.name, e);
            *status_is_err = true;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bucle Principal del GUI de VirtualBox
// ─────────────────────────────────────────────────────────────────────────────

pub fn run_gui_launcher(
    initial_iso: Option<PathBuf>,
    initial_disk: Option<PathBuf>,
    _cli_cpus: u32,
    _cli_ram_mb: usize,
) -> GuiOutcome {
    if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
        return GuiOutcome::Headless;
    }

    let assets = VBoxAssets::load();

    let mut available_isos = scan_iso_files();
    if let Some(ref iso) = initial_iso {
        let n = normalize_rel_path(&iso.to_string_lossy());
        if !available_isos.contains(&n) {
            available_isos.push(n);
        }
    }
    available_isos.sort();

    let mut available_disks = scan_disk_files();
    if let Some(ref d) = initial_disk {
        let n = normalize_rel_path(&d.to_string_lossy());
        if !available_disks.contains(&n) {
            available_disks.push(n);
        }
    }
    available_disks.sort();

    let mut profiles = load_vm_profiles(&available_isos, &available_disks);
    if profiles.is_empty() {
        profiles.push(VmProfile::new_default(
            "Windows 7 Pro",
            available_isos.first().cloned(),
            available_disks.first().cloned(),
        ));
    }

    let mut sel_idx = 0usize.min(profiles.len().saturating_sub(1));
    let mut current_view = MainViewTab::Details;
    let mut media_tab = MediaTab::HardDisks;
    let mut current_modal = ModalDialog::None;
    let mut settings_category = SettingsCategory::General;

    let mut active_field = ActiveField::None;
    let mut active_slider = ActiveSlider::None;
    let mut vram_mb: usize = 128;
    let mut status_msg = String::from("Listo. Presione [Iniciar] o haga doble clic para arrancar la VM seleccionada.");
    let mut status_is_err = false;
    let mut last_selected_disk_info: Option<DiskInfo> = None;

    // Campos del Asistente de Creación de Máquina
    let mut new_vm_name = String::from("Nueva Maquina Virtual");
    let mut new_vm_iso_idx = 0usize;
    let mut new_vm_ram_mb: usize = 4096;
    let mut new_vm_cpus: u32 = 4;
    let mut new_vm_create_disk = true;
    let mut new_vm_disk_gb: u64 = 20;

    // Campos del Asistente de Creación de Disco
    let mut new_disk_name = suggest_disk_filename(&profiles[sel_idx].name, ".vdi");
    let mut new_disk_size_gb: u64 = 20;
    let mut new_disk_fixed = false;

    // Cache del último disco inspeccionado
    if let Some(ref d) = profiles[sel_idx].disk_path {
        last_selected_disk_info = VirtualDisk::probe_info(d).ok();
    }

    let mut window = match Window::new(
        "Two Five Five (255) — Administrador de Maquinas Virtuales KVM",
        WIN_W,
        WIN_H,
        WindowOptions {
            resize: false,
            scale: minifb::Scale::X1,
            ..WindowOptions::default()
        },
    ) {
        Ok(win) => win,
        Err(e) => {
            eprintln!("[GUI] Error al inicializar ventana de Two Five Five (255): {}", e);
            return GuiOutcome::Headless;
        }
    };

    window.set_target_fps(60);
    let mut canvas = Canvas::new(WIN_W, WIN_H);
    let mut last_mouse_btn = false;
    let mut needs_redraw = true;
    let mut last_mx = usize::MAX;
    let mut last_my = usize::MAX;
    let mut profile_backup: Option<VmProfile> = None;
    let mut floating_state: Option<FloatingWindowState> = None;
    let mut running_vms: HashMap<String, std::process::Child> = HashMap::new();
    let mut last_click_sel_idx: Option<usize> = None;
    let mut last_click_time = std::time::Instant::now();

    while window.is_open() {
        // Comprobar estado de máquinas virtuales externas en ejecución (no bloqueante)
        let mut exited_vms = Vec::new();
        for (vm_name, child) in running_vms.iter_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    exited_vms.push((vm_name.clone(), status));
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("[GUI] Error comprobando estado de '{}': {}", vm_name, e);
                }
            }
        }
        for (vm_name, status) in exited_vms {
            running_vms.remove(&vm_name);
            status_msg = format!("VM '{}' finalizada ({}).", vm_name, status);
            status_is_err = false;
            needs_redraw = true;
        }

        let (mx, my) = window
            .get_mouse_pos(MouseMode::Pass)
            .map(|(x, y)| (x as usize, y as usize))
            .unwrap_or((0, 0));
        let mouse_moved = mx != last_mx || my != last_my;
        last_mx = mx;
        last_my = my;

        let mouse_down = window.get_mouse_down(MouseButton::Left);
        let clicked = mouse_down && !last_mouse_btn;
        if !mouse_down && active_slider != ActiveSlider::None {
            active_slider = ActiveSlider::None;
            needs_redraw = true;
        }
        last_mouse_btn = mouse_down;

        let keys_pressed = window.get_keys_pressed(KeyRepeat::No);
        if mouse_moved || clicked || (mouse_down && active_slider != ActiveSlider::None) || !keys_pressed.is_empty() {
            needs_redraw = true;
        }

        // Si la interfaz no ha tenido eventos, dormir 20 ms para eliminar el 15-30% de CPU en reposo
        if !needs_redraw {
            std::thread::sleep(std::time::Duration::from_millis(20));
            window.update();
            continue;
        }

        canvas.mx = mx;
        canvas.my = my;

        // ─── Sincronización y Arrastre de Ventanas Flotantes ─────────────────────
        if current_modal == ModalDialog::None {
            floating_state = None;
        } else if floating_state.is_none() {
            let (fw_w, fw_h) = match current_modal {
                ModalDialog::NewVmWizard => (660, 460),
                ModalDialog::CreateDiskWizard => (640, 410),
                ModalDialog::SettingsDialog => (760, 500),
                ModalDialog::AboutDialog => (560, 330),
                ModalDialog::LogsDialog => (740, 480),
                ModalDialog::None => (0, 0),
            };
            if fw_w > 0 && fw_h > 0 {
                floating_state = Some(FloatingWindowState::new_centered(fw_w, fw_h, WIN_W, WIN_H));
            }
        }

        if let Some(ref mut fw) = floating_state {
            if mouse_down && active_slider == ActiveSlider::None {
                if clicked {
                    if fw.is_in_titlebar(mx, my) {
                        fw.is_dragging = true;
                        fw.drag_off_x = mx.saturating_sub(fw.x);
                        fw.drag_off_y = my.saturating_sub(fw.y);
                    }
                } else if fw.is_dragging {
                    let next_x = mx.saturating_sub(fw.drag_off_x);
                    let next_y = my.saturating_sub(fw.drag_off_y);
                    fw.x = next_x.clamp(0, WIN_W.saturating_sub(fw.w));
                    fw.y = next_y.clamp(26, WIN_H.saturating_sub(fw.h));
                }
            } else {
                fw.is_dragging = false;
            }
        }

        // ─── Atajos de Teclado Globales ─────────────────────────────────────────
        if window.is_key_pressed(Key::Escape, KeyRepeat::No) {
            if current_modal == ModalDialog::SettingsDialog {
                if let Some(snap) = profile_backup.take() {
                    profiles[sel_idx] = snap;
                }
                current_modal = ModalDialog::None;
                floating_state = None;
                active_field = ActiveField::None;
            } else if current_modal != ModalDialog::None {
                current_modal = ModalDialog::None;
                floating_state = None;
                active_field = ActiveField::None;
            } else if active_field != ActiveField::None {
                active_field = ActiveField::None;
            } else {
                return GuiOutcome::Cancelled;
            }
        }

        if current_modal == ModalDialog::None && active_field == ActiveField::None {
            if window.is_key_pressed(Key::Up, KeyRepeat::No) {
                if sel_idx > 0 {
                    sel_idx -= 1;
                    if let Some(ref d) = profiles[sel_idx].disk_path {
                        last_selected_disk_info = VirtualDisk::probe_info(d).ok();
                    }
                }
            } else if window.is_key_pressed(Key::Down, KeyRepeat::No) {
                if sel_idx + 1 < profiles.len() {
                    sel_idx += 1;
                    if let Some(ref d) = profiles[sel_idx].disk_path {
                        last_selected_disk_info = VirtualDisk::probe_info(d).ok();
                    }
                }
            } else if window.is_key_pressed(Key::Enter, KeyRepeat::No) {
                launch_vm_external(
                    &profiles[sel_idx],
                    &mut running_vms,
                    &mut status_msg,
                    &mut status_is_err,
                );
            }
        }

        // ─── Entrada de Texto para Campos Activos ───────────────────────────────
        if active_field != ActiveField::None {
            for key in window.get_keys_pressed(KeyRepeat::Yes) {
                let target_str = match active_field {
                    ActiveField::WizardVmName => &mut new_vm_name,
                    ActiveField::WizardDiskName => &mut new_disk_name,
                    ActiveField::SettingsVmName => &mut profiles[sel_idx].name,
                    ActiveField::None => continue,
                };

                match key {
                    Key::Backspace => {
                        target_str.pop();
                    }
                    Key::Enter | Key::Escape => {
                        active_field = ActiveField::None;
                        let _ = save_vm_profiles(&profiles);
                    }
                    Key::Space => {
                        target_str.push(' ');
                    }
                    Key::Period => {
                        target_str.push('.');
                    }
                    Key::Minus => {
                        target_str.push('-');
                    }
                    _ => {
                        let ch = format!("{:?}", key).to_lowercase();
                        if ch.starts_with("key") && ch.len() == 4 {
                            let c = ch.chars().nth(3).unwrap();
                            target_str.push(c);
                        } else if ch.starts_with('n') && ch.len() == 5 && ch.starts_with("num") {
                            let c = ch.chars().nth(4).unwrap();
                            target_str.push(c);
                        }
                    }
                }
            }
        }

        // ─── Renderizado Base del Canvas ────────────────────────────────────────
        canvas.clear(COL_WIN_BG);

        // 1. Barra de Menús Superior (Archivo | Máquina | Herramientas | Ayuda)
        canvas.fill_rect(0, 0, WIN_W, 26, COL_MENU_BG);
        canvas.fill_rect(0, 25, WIN_W, 1, COL_MENU_BORDER);
        assets.draw("logo_16px.png", &mut canvas.buf, WIN_W, WIN_H, 8, 5);

        let menu_items = [
            ("menu_file", 32, "Archivo"),
            ("menu_machine", 100, "Maquina"),
            ("menu_tools", 170, "Herramientas"),
            ("menu_help", 280, "Ayuda"),
        ];
        for (mid, mx_pos, label) in &menu_items {
            let mw = label.len() * 8 + 12;
            let hov = canvas.register_btn(mid, *mx_pos, 2, mw, 22);
            if hov {
                canvas.fill_rect(*mx_pos, 2, mw, 22, 0xE5EBF2);
                canvas.draw_border(*mx_pos, 2, mw, 22, 0xB6BFCB);
            }
            canvas.draw_text(*mx_pos + 6, 5, label, COL_TEXT_MAIN);
        }

        // 2. Barra de Herramientas Principal de VirtualBox (56px)
        for y in 26..82 {
            let t = (y - 26) as f32 / 56.0;
            let r = ((COL_TOOLBAR_TOP >> 16) & 0xFF) as f32 * (1.0 - t) + ((COL_TOOLBAR_BOT >> 16) & 0xFF) as f32 * t;
            let g = ((COL_TOOLBAR_TOP >> 8) & 0xFF) as f32 * (1.0 - t) + ((COL_TOOLBAR_BOT >> 8) & 0xFF) as f32 * t;
            let b = (COL_TOOLBAR_TOP & 0xFF) as f32 * (1.0 - t) + (COL_TOOLBAR_BOT & 0xFF) as f32 * t;
            let c = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            canvas.fill_rect(0, y, WIN_W, 1, c);
        }
        canvas.fill_rect(0, 81, WIN_W, 1, COL_TOOLBAR_BORDER);

        // Botones de Acción de la Toolbar
        let mut cur_tb_x = 10usize;
        canvas.draw_toolbar_action(&assets, "tb_new", "vm_new_32px.png", "Nueva", cur_tb_x, 28, 62, 50, false);
        cur_tb_x += 66;

        canvas.draw_toolbar_action(&assets, "tb_add", "vm_add_32px.png", "Anadir", cur_tb_x, 28, 62, 50, false);
        cur_tb_x += 66;

        canvas.draw_toolbar_action(&assets, "tb_settings", "vm_settings_32px.png", "Configuracion", cur_tb_x, 28, 100, 50, false);
        cur_tb_x += 104;

        canvas.draw_toolbar_action(&assets, "tb_discard", "vm_discard_32px.png", "Descartar", cur_tb_x, 28, 76, 50, false);
        cur_tb_x += 80;

        canvas.draw_toolbar_action(&assets, "tb_media", "media_manager_32px.png", "Medios", cur_tb_x, 28, 62, 50, current_view == MainViewTab::Media);
        cur_tb_x += 66;

        let is_cur_running = running_vms.contains_key(&profiles[sel_idx].name);
        let start_label = if is_cur_running { "Mostrar" } else { "Iniciar" };
        canvas.draw_toolbar_action(&assets, "tb_start", "vm_start_32px.png", start_label, cur_tb_x, 28, 72, 50, true);

        // Selector de Vistas de Herramientas a la Derecha de la Toolbar
        let mut r_tb_x = WIN_W.saturating_sub(270);
        let tab_det_hov = canvas.register_btn("tab_details", r_tb_x, 36, 85, 34);
        canvas.fill_rect(r_tb_x, 36, 85, 34, if current_view == MainViewTab::Details { COL_CARD_BG } else if tab_det_hov { 0xE2E7EE } else { 0xEBEEF3 });
        canvas.draw_border(r_tb_x, 36, 85, 34, COL_TOOLBAR_BORDER);
        assets.draw("machine_details_manager_16px.png", &mut canvas.buf, WIN_W, WIN_H, (r_tb_x + 6) as isize, 45);
        canvas.draw_text(r_tb_x + 26, 45, "Detalles", COL_TEXT_MAIN);
        r_tb_x += 88;

        let tab_med_hov = canvas.register_btn("tab_media", r_tb_x, 36, 80, 34);
        canvas.fill_rect(r_tb_x, 36, 80, 34, if current_view == MainViewTab::Media { COL_CARD_BG } else if tab_med_hov { 0xE2E7EE } else { 0xEBEEF3 });
        canvas.draw_border(r_tb_x, 36, 80, 34, COL_TOOLBAR_BORDER);
        assets.draw("media_manager_16px.png", &mut canvas.buf, WIN_W, WIN_H, (r_tb_x + 6) as isize, 45);
        canvas.draw_text(r_tb_x + 26, 45, "Medios", COL_TEXT_MAIN);
        r_tb_x += 83;

        let tab_log_hov = canvas.register_btn("tab_logs", r_tb_x, 36, 92, 34);
        canvas.fill_rect(r_tb_x, 36, 92, 34, if current_view == MainViewTab::Logs { COL_CARD_BG } else if tab_log_hov { 0xE2E7EE } else { 0xEBEEF3 });
        canvas.draw_border(r_tb_x, 36, 92, 34, COL_TOOLBAR_BORDER);
        assets.draw("vm_show_logs_16px.png", &mut canvas.buf, WIN_W, WIN_H, (r_tb_x + 6) as isize, 45);
        canvas.draw_text(r_tb_x + 26, 45, "Checklist", COL_TEXT_MAIN);

        // 3. Panel Izquierdo: Chooser de Máquinas Virtuales (`UIChooser`)
        const CHOOSER_W: usize = 300;
        let content_y = 82usize;
        let content_h = WIN_H - content_y - 28;

        canvas.fill_rect(0, content_y, CHOOSER_W, content_h, COL_SIDEBAR_BG);
        canvas.fill_rect(CHOOSER_W - 1, content_y, 1, content_h, COL_SIDEBAR_BORDER);

        // Cabecera "Herramientas" (Global Tools Item)
        let tools_hov = canvas.register_btn("item_tools", 0, content_y, CHOOSER_W - 1, 46);
        let tools_bg = if current_view == MainViewTab::Media { 0xD6E8FC } else if tools_hov { 0xF2F5F9 } else { 0xF9FAFC };
        canvas.fill_rect(0, content_y, CHOOSER_W - 1, 46, tools_bg);
        canvas.fill_rect(0, content_y + 45, CHOOSER_W - 1, 1, 0xE2E6ED);
        assets.draw("tools_global_32px.png", &mut canvas.buf, WIN_W, WIN_H, 10, (content_y + 7) as isize);
        canvas.draw_text(50, content_y + 10, "Herramientas", COL_TEXT_MAIN);
        canvas.draw_text(50, content_y + 26, "Administrador de Medios VDI", COL_TEXT_MUTED);
        assets.draw("tools_menu_24px.png", &mut canvas.buf, WIN_W, WIN_H, (CHOOSER_W - 32) as isize, (content_y + 11) as isize);

        // Lista de Máquinas Virtuales en el Chooser
        let mut item_y = content_y + 48;
        for (i, p) in profiles.iter().enumerate() {
            let is_sel = i == sel_idx && current_view == MainViewTab::Details;
            let item_hov = canvas.register_btn(&format!("vm_sel_{}", i), 0, item_y, CHOOSER_W - 1, 52);

            let bg = if is_sel {
                COL_SELECTION_BG
            } else if item_hov {
                COL_SELECTION_HOVER
            } else {
                COL_SIDEBAR_BG
            };
            canvas.fill_rect(0, item_y, CHOOSER_W - 1, 52, bg);
            canvas.fill_rect(0, item_y + 51, CHOOSER_W - 1, 1, 0xEEF2F6);

            // Icono del Sistema Operativo de VirtualBox (32x32)
            let os_icon = VBoxAssets::os_icon_for(&p.os_type, &p.name, p.iso_path.as_deref());
            assets.draw(os_icon, &mut canvas.buf, WIN_W, WIN_H, 10, (item_y + 10) as isize);

            // Nombre y Estado
            let title_col = if is_sel { 0xFFFFFF } else { COL_TEXT_MAIN };
            let is_vm_running = running_vms.contains_key(&p.name);
            canvas.draw_text_clipped(50, item_y + 10, (CHOOSER_W - 60) / 8, &p.name, title_col);

            // Icono de estado pequeño (16x16)
            if is_vm_running {
                assets.draw("state_running_16px.png", &mut canvas.buf, WIN_W, WIN_H, 50, (item_y + 28) as isize);
                let run_col = if is_sel { 0xA3E4D7 } else { 0x27AE60 };
                canvas.draw_text(70, item_y + 28, "Corriendo", run_col);
            } else {
                assets.draw("state_powered_off_16px.png", &mut canvas.buf, WIN_W, WIN_H, 50, (item_y + 28) as isize);
                let state_col = if is_sel { 0xD6E8FC } else { COL_TEXT_MUTED };
                canvas.draw_text(70, item_y + 28, "Apagada", state_col);
            }

            item_y += 52;
        }

        // 4. Panel Derecho: Vista Seleccionada (`Detalles`, `Medios` o `Logs`)
        let right_x = CHOOSER_W + 8;
        let right_w = WIN_W - right_x - 8;

        match current_view {
            MainViewTab::Details => {
                let cur_p = &profiles[sel_idx];

                // Fila Superior (2 Columnas): General/Sistema (Izquierda) + Monitor Preview (Derecha)
                const COL1_W: usize = 530;
                let col2_x = right_x + COL1_W + 10;
                let col2_w = right_w.saturating_sub(COL1_W + 10);

                // Tarjeta General
                let mut card_y = content_y + 6;
                canvas.draw_details_card_header(&assets, "machine_16px.png", "General", right_x, card_y, COL1_W);
                canvas.fill_rect(right_x, card_y + 26, COL1_W, 70, COL_CARD_BG);
                canvas.draw_border(right_x, card_y + 26, COL1_W, 70, COL_CARD_BORDER);

                canvas.draw_text(right_x + 12, card_y + 34, "Nombre:", COL_TEXT_MUTED);
                let edit_name_hov = canvas.register_btn("act_edit_name", right_x + 130, card_y + 34, 380, 16);
                canvas.draw_text_clipped(right_x + 130, card_y + 34, 45, &cur_p.name, if edit_name_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                canvas.draw_text(right_x + 12, card_y + 54, "Sistema operativo:", COL_TEXT_MUTED);
                let edit_os_hov = canvas.register_btn("act_edit_os", right_x + 170, card_y + 54, 340, 16);
                canvas.draw_text_clipped(right_x + 170, card_y + 54, 40, &cur_p.os_type, if edit_os_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                canvas.draw_text(right_x + 12, card_y + 74, "Desatendida:", COL_TEXT_MUTED);
                let toggle_unatt_hov = canvas.register_btn("act_toggle_unatt", right_x + 130, card_y + 74, 250, 16);
                let unatt_label = if cur_p.unattended { "Activada (OEMDRV / Preseed)" } else { "Desactivada" };
                canvas.draw_text(right_x + 130, card_y + 74, unatt_label, if toggle_unatt_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                card_y += 102;

                // Tarjeta Sistema
                canvas.draw_details_card_header(&assets, "chipset_16px.png", "Sistema", right_x, card_y, COL1_W);
                canvas.fill_rect(right_x, card_y + 26, COL1_W, 90, COL_CARD_BG);
                canvas.draw_border(right_x, card_y + 26, COL1_W, 90, COL_CARD_BORDER);

                let ram_ratio = ((cur_p.ram_mb.clamp(RAM_MIN_MB, RAM_MAX_MB) - RAM_MIN_MB) as f32)
                    / ((RAM_MAX_MB - RAM_MIN_MB) as f32);
                canvas.draw_text(right_x + 12, card_y + 34, "Memoria base:", COL_TEXT_MUTED);
                canvas.draw_text(right_x + 122, card_y + 34, &format!("{} MB", cur_p.ram_mb), COL_TEXT_MAIN);
                canvas.draw_slider(
                    ActiveSlider::MainRam,
                    right_x + 206,
                    card_y + 36,
                    236,
                    ram_ratio,
                    0.60,
                    0.85,
                    active_slider == ActiveSlider::MainRam,
                );
                canvas.draw_button_gray("ram_dec", right_x + 454, card_y + 30, 28, 22, "-");
                canvas.draw_button_gray("ram_inc", right_x + 488, card_y + 30, 28, 22, "+");

                let cpu_ratio = ((cur_p.cpus.clamp(CPU_MIN, CPU_MAX) - CPU_MIN) as f32)
                    / ((CPU_MAX - CPU_MIN) as f32);
                canvas.draw_text(right_x + 12, card_y + 58, "Procesadores:", COL_TEXT_MUTED);
                canvas.draw_text(right_x + 122, card_y + 58, &format!("{} vCPUs", cur_p.cpus), COL_TEXT_MAIN);
                canvas.draw_slider(
                    ActiveSlider::MainCpu,
                    right_x + 206,
                    card_y + 60,
                    236,
                    cpu_ratio,
                    0.50,
                    0.80,
                    active_slider == ActiveSlider::MainCpu,
                );
                canvas.draw_button_gray("cpu_dec", right_x + 454, card_y + 54, 28, 22, "-");
                canvas.draw_button_gray("cpu_inc", right_x + 488, card_y + 54, 28, 22, "+");

                canvas.draw_text(right_x + 12, card_y + 82, "Firmware:", COL_TEXT_MUTED);
                let toggle_efi_hov = canvas.register_btn("act_toggle_efi", right_x + 122, card_y + 82, 220, 16);
                let efi_label = if cur_p.use_efi { "EFI (OVMF x64)" } else { "BIOS (SeaBIOS)" };
                canvas.draw_text(right_x + 122, card_y + 82, efi_label, if toggle_efi_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                canvas.draw_text(right_x + 350, card_y + 82, "Aceleracion: KVM/VT-x", COL_TEXT_MUTED);

                // Columna Derecha: Vista Previa (`UIMachinePreview` - monitor Two Five Five)
                let prev_y = content_y + 6;
                canvas.draw_details_card_header(&assets, "machine_16px.png", "Vista previa", col2_x, prev_y, col2_w);
                canvas.fill_rect(col2_x, prev_y + 26, col2_w, 192, COL_CARD_BG);
                canvas.draw_border(col2_x, prev_y + 26, col2_w, 192, COL_CARD_BORDER);

                let mon_x = col2_x + (col2_w.saturating_sub(242)) / 2;
                let mon_y = prev_y + 36;
                if !assets.draw("preview_full_16to9_242x155px.png", &mut canvas.buf, WIN_W, WIN_H, mon_x as isize, mon_y as isize) {
                    canvas.fill_rect(mon_x, mon_y, 242, 155, COL_MONITOR_BG);
                    canvas.draw_border(mon_x, mon_y, 242, 155, 0x333333);
                }

                // Centro de la pantalla del monitor: Icono de SO + Nombre de VM
                let os_ic = VBoxAssets::os_icon_for(&cur_p.os_type, &cur_p.name, cur_p.iso_path.as_deref());
                assets.draw(os_ic, &mut canvas.buf, WIN_W, WIN_H, (mon_x + 105) as isize, (mon_y + 40) as isize);
                let vm_name_short: String = cur_p.name.chars().take(22).collect();
                let txt_w = vm_name_short.len() * 8;
                let n_x = mon_x + (242usize.saturating_sub(txt_w)) / 2;
                canvas.draw_text(n_x, mon_y + 80, &vm_name_short, 0xFFFFFF);
                if running_vms.contains_key(&cur_p.name) {
                    canvas.draw_text(mon_x + 36, mon_y + 100, "[En ejecucion externa]", 0x2ECC71);
                } else {
                    canvas.draw_text(mon_x + 60, mon_y + 100, "1024x768 (KVM/VBE)", 0xA0B0C0);
                }

                // Fila Inferior: Tarjetas de Pantalla, Almacenamiento, Audio, Red, USB
                let mut lower_y = card_y + 122;

                // Tarjeta Pantalla
                canvas.draw_details_card_header(&assets, "vrdp_16px.png", "Pantalla", right_x, lower_y, right_w);
                canvas.fill_rect(right_x, lower_y + 26, right_w, 36, COL_CARD_BG);
                canvas.draw_border(right_x, lower_y + 26, right_w, 36, COL_CARD_BORDER);
                canvas.draw_text(right_x + 12, lower_y + 36, &format!("Memoria de video: {} MB", vram_mb), COL_TEXT_MUTED);
                canvas.draw_text(right_x + 240, lower_y + 36, "Controlador grafico: 255-VBE (Bochs VBE LFB @ 0xE8000000)", COL_TEXT_MAIN);

                lower_y += 68;

                // Tarjeta Almacenamiento (AHCI SATA Port 0 y Port 1)
                canvas.draw_details_card_header(&assets, "hd_16px.png", "Almacenamiento", right_x, lower_y, right_w);
                canvas.fill_rect(right_x, lower_y + 26, right_w, 94, COL_CARD_BG);
                canvas.draw_border(right_x, lower_y + 26, right_w, 94, COL_CARD_BORDER);

                canvas.draw_text(right_x + 12, lower_y + 34, "Controlador: SATA (AHCI ICH9)", COL_TEXT_MAIN);

                // Puerto SATA 0: Disco Duro
                assets.draw("hd_16px.png", &mut canvas.buf, WIN_W, WIN_H, (right_x + 16) as isize, (lower_y + 54) as isize);
                canvas.draw_text(right_x + 36, lower_y + 54, "Puerto 0:", COL_TEXT_MUTED);
                let disk_label = if let Some(ref info) = last_selected_disk_info {
                    let gb = info.virtual_size as f64 / (1024.0 * 1024.0 * 1024.0);
                    format!("{} ({:.1} GB, {})", cur_p.disk_path.as_deref().unwrap_or(""), gb, info.format.label())
                } else {
                    cur_p.disk_path.as_deref().unwrap_or("(Ninguno)").to_string()
                };
                let disk_pick_hov = canvas.register_btn("act_pick_disk", right_x + 115, lower_y + 54, 320, 16);
                canvas.draw_text_clipped(right_x + 115, lower_y + 54, 40, &disk_label, if disk_pick_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                canvas.draw_button_gray("btn_create_vdi_quick", right_x + 460, lower_y + 50, 120, 22, "Crear VDI...");
                if let Some(ref d) = cur_p.disk_path {
                    if d.to_lowercase().ends_with(".vdi") {
                        canvas.draw_button_gray("btn_compact_quick", right_x + 586, lower_y + 50, 100, 22, "Compactar");
                    }
                }

                // Puerto SATA 1: Unidad Óptica ISO
                assets.draw("cd_16px.png", &mut canvas.buf, WIN_W, WIN_H, (right_x + 16) as isize, (lower_y + 78) as isize);
                canvas.draw_text(right_x + 36, lower_y + 78, "Puerto 1 [Optico]:", COL_TEXT_MUTED);
                let iso_label = cur_p.iso_path.as_deref().unwrap_or("(Vacio — Clic para seleccionar ISO)");
                let iso_pick_hov = canvas.register_btn("act_pick_iso", right_x + 185, lower_y + 78, 265, 16);
                canvas.draw_text_clipped(right_x + 185, lower_y + 78, 33, iso_label, if iso_pick_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                canvas.draw_button_gray("btn_browse_iso_quick", right_x + 460, lower_y + 74, 120, 22, "Buscar ISO...");
                if cur_p.iso_path.is_some() {
                    canvas.draw_button_gray("btn_eject_iso_quick", right_x + 586, lower_y + 74, 100, 22, "Expulsar");
                }

                lower_y += 126;

                // Tarjeta Audio, Red y USB en tres bloques
                let b_w = (right_w.saturating_sub(16)) / 3;

                // Audio
                canvas.draw_details_card_header(&assets, "sound_16px.png", "Audio", right_x, lower_y, b_w);
                canvas.fill_rect(right_x, lower_y + 26, b_w, 40, COL_CARD_BG);
                canvas.draw_border(right_x, lower_y + 26, b_w, 40, COL_CARD_BORDER);
                canvas.draw_text_clipped(right_x + 10, lower_y + 36, b_w / 8, "Controlador: ICH AC'97", COL_TEXT_MAIN);

                // Red
                let net_x = right_x + b_w + 8;
                canvas.draw_details_card_header(&assets, "nw_16px.png", "Red", net_x, lower_y, b_w);
                canvas.fill_rect(net_x, lower_y + 26, b_w, 40, COL_CARD_BG);
                canvas.draw_border(net_x, lower_y + 26, b_w, 40, COL_CARD_BORDER);
                canvas.draw_text_clipped(net_x + 10, lower_y + 36, b_w / 8, "Adaptador 1: VirtIO-Net (NAT)", COL_TEXT_MAIN);

                // USB
                let usb_x = net_x + b_w + 8;
                canvas.draw_details_card_header(&assets, "usb_16px.png", "USB", usb_x, lower_y, b_w);
                canvas.fill_rect(usb_x, lower_y + 26, b_w, 40, COL_CARD_BG);
                canvas.draw_border(usb_x, lower_y + 26, b_w, 40, COL_CARD_BORDER);
                canvas.draw_text_clipped(usb_x + 10, lower_y + 36, b_w / 8, "UHCI 1.1 + Tableta Absoluta", COL_TEXT_MAIN);
            }

            MainViewTab::Media => {
                // Vista de Administrador de Medios Virtuales (`UIMediumManager`)
                canvas.fill_rect(right_x, content_y + 6, right_w, content_h.saturating_sub(12), COL_CARD_BG);
                canvas.draw_border(right_x, content_y + 6, right_w, content_h.saturating_sub(12), COL_CARD_BORDER);

                // Pestañas de Medios
                let tab1_hov = canvas.register_btn("med_tab_hd", right_x + 10, content_y + 12, 140, 28);
                canvas.fill_rect(right_x + 10, content_y + 12, 140, 28, if media_tab == MediaTab::HardDisks { COL_CARD_HEADER } else if tab1_hov { 0xE6ECF4 } else { 0xF2F5F8 });
                canvas.draw_border(right_x + 10, content_y + 12, 140, 28, COL_CARD_BORDER);
                assets.draw("hd_16px.png", &mut canvas.buf, WIN_W, WIN_H, (right_x + 16) as isize, (content_y + 18) as isize);
                canvas.draw_text(right_x + 36, content_y + 18, "Discos duros", COL_TEXT_MAIN);

                let tab2_hov = canvas.register_btn("med_tab_cd", right_x + 155, content_y + 12, 150, 28);
                canvas.fill_rect(right_x + 155, content_y + 12, 150, 28, if media_tab == MediaTab::OpticalDisks { COL_CARD_HEADER } else if tab2_hov { 0xE6ECF4 } else { 0xF2F5F8 });
                canvas.draw_border(right_x + 155, content_y + 12, 150, 28, COL_CARD_BORDER);
                assets.draw("cd_16px.png", &mut canvas.buf, WIN_W, WIN_H, (right_x + 161) as isize, (content_y + 18) as isize);
                canvas.draw_text(right_x + 181, content_y + 18, "Discos opticos", COL_TEXT_MAIN);

                // Acciones de medios
                canvas.draw_button_gray("btn_med_browse_iso", right_x + right_w - 425, content_y + 14, 128, 26, "Buscar ISO...");
                canvas.draw_button_gray("btn_med_create_vdi", right_x + right_w - 290, content_y + 14, 130, 26, "Crear VDI...");
                canvas.draw_button_gray("btn_med_refresh", right_x + right_w - 150, content_y + 14, 140, 26, "Actualizar lista");

                // Tabla de medios
                let table_y = content_y + 46;
                canvas.fill_rect(right_x + 10, table_y, right_w - 20, 24, COL_CARD_HEADER);
                canvas.draw_border(right_x + 10, table_y, right_w - 20, 24, COL_CARD_BORDER);
                canvas.draw_text(right_x + 20, table_y + 5, "Nombre del Archivo", COL_TEXT_MAIN);
                canvas.draw_text(right_x + 340, table_y + 5, "Tamano Virtual", COL_TEXT_MAIN);
                canvas.draw_text(right_x + 490, table_y + 5, "Tamano en Host", COL_TEXT_MAIN);
                canvas.draw_text(right_x + 640, table_y + 5, "Formato", COL_TEXT_MAIN);

                let mut row_y = table_y + 26;
                match media_tab {
                    MediaTab::HardDisks => {
                        for (idx, d) in available_disks.iter().enumerate() {
                            let is_assigned = profiles[sel_idx].disk_path.as_deref() == Some(d);
                            let row_hov = canvas.register_btn(&format!("row_disk_{}", idx), right_x + 10, row_y, right_w - 20, 26);
                            let row_bg = if is_assigned { 0xE6F0FA } else if row_hov { 0xF5F7FA } else { COL_CARD_BG };
                            canvas.fill_rect(right_x + 10, row_y, right_w - 20, 26, row_bg);
                            canvas.fill_rect(right_x + 10, row_y + 25, right_w - 20, 1, 0xEEF2F6);

                            assets.draw("hd_16px.png", &mut canvas.buf, WIN_W, WIN_H, (right_x + 16) as isize, (row_y + 5) as isize);
                            canvas.draw_text_clipped(right_x + 36, row_y + 5, 36, d, COL_TEXT_MAIN);

                            if let Ok(info) = VirtualDisk::probe_info(d) {
                                let v_gb = info.virtual_size as f64 / (1024.0 * 1024.0 * 1024.0);
                                let h_mb = info.actual_size as f64 / (1024.0 * 1024.0);
                                canvas.draw_text(right_x + 340, row_y + 5, &format!("{:.2} GiB", v_gb), COL_TEXT_MUTED);
                                canvas.draw_text(right_x + 490, row_y + 5, &format!("{:.2} MiB", h_mb), COL_TEXT_MUTED);
                                canvas.draw_text(right_x + 640, row_y + 5, info.format.label(), COL_TEXT_MAIN);
                            }

                            // Botón de asignación rápida a la VM actual
                            if !is_assigned {
                                canvas.draw_button_gray(&format!("btn_assign_disk_{}", idx), right_x + right_w - 120, row_y + 2, 95, 22, "Asignar");
                            } else {
                                canvas.draw_text(right_x + right_w - 100, row_y + 5, "[En uso]", COL_BRAND_BLUE);
                            }

                            row_y += 28;
                        }
                    }
                    MediaTab::OpticalDisks => {
                        for (idx, iso) in available_isos.iter().enumerate() {
                            let is_assigned = profiles[sel_idx].iso_path.as_deref() == Some(iso);
                            let row_hov = canvas.register_btn(&format!("row_iso_{}", idx), right_x + 10, row_y, right_w - 20, 26);
                            let row_bg = if is_assigned { 0xE6F0FA } else if row_hov { 0xF5F7FA } else { COL_CARD_BG };
                            canvas.fill_rect(right_x + 10, row_y, right_w - 20, 26, row_bg);
                            canvas.fill_rect(right_x + 10, row_y + 25, right_w - 20, 1, 0xEEF2F6);

                            assets.draw("cd_16px.png", &mut canvas.buf, WIN_W, WIN_H, (right_x + 16) as isize, (row_y + 5) as isize);
                            canvas.draw_text_clipped(right_x + 36, row_y + 5, 36, iso, COL_TEXT_MAIN);

                            if let Ok(meta) = fs::metadata(iso) {
                                let sz_gb = meta.len() as f64 / (1024.0 * 1024.0 * 1024.0);
                                canvas.draw_text(right_x + 340, row_y + 5, &format!("{:.2} GiB", sz_gb), COL_TEXT_MUTED);
                                canvas.draw_text(right_x + 490, row_y + 5, &format!("{:.2} GiB", sz_gb), COL_TEXT_MUTED);
                                canvas.draw_text(right_x + 640, row_y + 5, "ISO 9660 / UDF", COL_TEXT_MAIN);
                            }

                            if !is_assigned {
                                canvas.draw_button_gray(&format!("btn_assign_iso_{}", idx), right_x + right_w - 120, row_y + 2, 95, 22, "Montar");
                            } else {
                                canvas.draw_text(right_x + right_w - 100, row_y + 5, "[Montada]", COL_BRAND_BLUE);
                            }

                            row_y += 28;
                        }
                    }
                }
            }

            MainViewTab::Logs => {
                // Checklist de Estado de Componentes del Hipervisor
                canvas.fill_rect(right_x, content_y + 6, right_w, content_h.saturating_sub(12), COL_CARD_BG);
                canvas.draw_border(right_x, content_y + 6, right_w, content_h.saturating_sub(12), COL_CARD_BORDER);

                canvas.draw_text(right_x + 20, content_y + 16, "Checklist de Funcionamiento e Integracion — Two Five Five (255)", COL_TEXT_MAIN);

                let items = [
                    ("KVM Kernel Module (/dev/kvm): Activo y operativo", true),
                    ("SeaBIOS x86 (bios-256k.bin): Enlazado para modo real / BIOS", true),
                    ("Soporte UEFI / OVMF x64: Integrado (Item 22)", true),
                    ("Motor Discos Two Five Five VDI 1.1+: Header VDI + dynamic/fixed + compact", true),
                    ("Controlador SATA AHCI (Intel ICH9): Puertos 0 (HDD) y 1 (CD-ROM)", true),
                    ("Controlador Grafico Bochs VBE Dispi: LFB 0xE8000000 hasta 1920x1080", true),
                    ("Red Ethernet PCI VirtIO-Net: DHCP, ARP, ICMP, DNS y NAT integrados", true),
                    ("Controlador Audio ICH AC'97 (8086:2415): Codec SigmaTel + DMA", true),
                    ("Dispositivo VMMDev (80EE:CAFE): Integracion de host, tiempo y raton absoluto", true),
                    ("Tableta Grafica USB UHCI: Coordenadas absolutas 0..32767", true),
                    ("Multi-vCPU (SMP): Hasta 16 nucleos concurrentes con INIT-SIPI", true),
                    ("Temporizador Monotonico PIT / BDA: 1 ms en hilo dedicado sin deriva", true),
                ];

                let mut it_y = content_y + 44;
                for (desc, ok) in &items {
                    assets.draw(if *ok { "status_check_16px.png" } else { "status_error_16px.png" }, &mut canvas.buf, WIN_W, WIN_H, (right_x + 20) as isize, it_y as isize);
                    canvas.draw_text(right_x + 44, it_y, desc, if *ok { COL_TEXT_MAIN } else { 0xC0392B });
                    it_y += 24;
                }
            }
        }

        // 5. Barra de Estado Inferior
        let stat_y = WIN_H - 26;
        canvas.fill_rect(0, stat_y, WIN_W, 26, COL_CARD_HEADER);
        canvas.fill_rect(0, stat_y, WIN_W, 1, COL_TOOLBAR_BORDER);
        assets.draw("status_check_16px.png", &mut canvas.buf, WIN_W, WIN_H, 8, (stat_y + 5) as isize);
        let stat_col = if status_is_err { 0xC0392B } else { COL_TEXT_MUTED };
        canvas.draw_text_clipped(30, stat_y + 5, (WIN_W - 40) / 8, &status_msg, stat_col);

        // ─── Modales y Asistentes Flotantes (Floating Windows) ──────────────────
        match current_modal {
            ModalDialog::NewVmWizard => {
                if let Some(ref fw) = floating_state {
                    let m_w = fw.w;
                    let m_h = fw.h;
                    let mx = fw.x;
                    let my = fw.y;

                    // Sombra proyectada y marco flotante
                    canvas.draw_drop_shadow(mx, my, m_w, m_h, 8);
                    canvas.draw_floating_titlebar(&assets, fw, "Two Five Five (255) — Crear Maquina Virtual", Some("vm_new_32px.png"));

                    // Banner lateral personalizado Two Five Five (`wizard_new_welcome.png`: 145x290)
                    assets.draw("wizard_new_welcome.png", &mut canvas.buf, WIN_W, WIN_H, (mx + 10) as isize, (my + 40) as isize);

                    let form_x = mx + 170;
                    canvas.draw_text(form_x, my + 44, "Nombre y sistema operativo:", COL_TEXT_MAIN);

                    // Campo Nombre
                    canvas.draw_text(form_x, my + 68, "Nombre:", COL_TEXT_MUTED);
                    let _edit_hov = canvas.register_btn("wiz_vm_name_box", form_x + 90, my + 64, 370, 24);
                    canvas.fill_rect(form_x + 90, my + 64, 370, 24, COL_INPUT_BG);
                    canvas.draw_border(form_x + 90, my + 64, 370, 24, if active_field == ActiveField::WizardVmName { COL_INPUT_BORDER } else { COL_CARD_BORDER });
                    canvas.draw_text(form_x + 96, my + 68, &new_vm_name, COL_TEXT_MAIN);

                    // Selector de ISO con Botón de Explorador de Archivos
                    canvas.draw_text(form_x, my + 100, "Imagen ISO:", COL_TEXT_MUTED);
                    let cur_iso_name = available_isos.get(new_vm_iso_idx).map(|s| s.as_str()).unwrap_or("(Ninguna)");
                    let iso_hov = canvas.register_btn("wiz_cycle_iso", form_x + 90, my + 96, 266, 24);
                    canvas.fill_rect(form_x + 90, my + 96, 266, 24, if iso_hov { 0xE6F0FA } else { COL_BTN_GRAY });
                    canvas.draw_border(form_x + 90, my + 96, 266, 24, COL_BTN_GRAY_BORDER);
                    canvas.draw_text_clipped(form_x + 96, my + 100, 31, cur_iso_name, COL_TEXT_MAIN);
                    canvas.draw_button_gray("wiz_browse_iso", form_x + 362, my + 96, 98, 24, "Buscar...");

                    // Hardware: Memoria RAM y Procesadores con Sliders Interactivos
                    canvas.draw_text(form_x, my + 132, "Hardware (RAM y CPU):", COL_TEXT_MAIN);
                    let wiz_ram_ratio = ((new_vm_ram_mb.clamp(RAM_MIN_MB, RAM_MAX_MB) - RAM_MIN_MB) as f32)
                        / ((RAM_MAX_MB - RAM_MIN_MB) as f32);
                    canvas.draw_text(form_x, my + 156, "Memoria:", COL_TEXT_MUTED);
                    canvas.draw_text(form_x + 80, my + 156, &format!("{} MB", new_vm_ram_mb), COL_TEXT_MAIN);
                    canvas.draw_slider(
                        ActiveSlider::WizardRam,
                        form_x + 160,
                        my + 158,
                        220,
                        wiz_ram_ratio,
                        0.60,
                        0.85,
                        active_slider == ActiveSlider::WizardRam,
                    );
                    canvas.draw_button_gray("wiz_ram_dec", form_x + 394, my + 152, 28, 22, "-");
                    canvas.draw_button_gray("wiz_ram_inc", form_x + 428, my + 152, 28, 22, "+");

                    let wiz_cpu_ratio = ((new_vm_cpus.clamp(CPU_MIN, CPU_MAX) - CPU_MIN) as f32)
                        / ((CPU_MAX - CPU_MIN) as f32);
                    canvas.draw_text(form_x, my + 188, "CPUs:", COL_TEXT_MUTED);
                    canvas.draw_text(form_x + 80, my + 188, &format!("{} vCPUs", new_vm_cpus), COL_TEXT_MAIN);
                    canvas.draw_slider(
                        ActiveSlider::WizardCpu,
                        form_x + 160,
                        my + 190,
                        220,
                        wiz_cpu_ratio,
                        0.50,
                        0.80,
                        active_slider == ActiveSlider::WizardCpu,
                    );
                    canvas.draw_button_gray("wiz_cpu_dec", form_x + 394, my + 184, 28, 22, "-");
                    canvas.draw_button_gray("wiz_cpu_inc", form_x + 428, my + 184, 28, 22, "+");

                    // Tipo de Disco
                    canvas.draw_text(form_x, my + 224, "Disco duro virtual:", COL_TEXT_MAIN);
                    let _chk_hov = canvas.register_btn("wiz_toggle_create_disk", form_x, my + 246, 420, 20);
                    canvas.draw_text(form_x + 20, my + 246, "Crear un disco duro virtual ahora (VDI dinamico)", COL_TEXT_MAIN);
                    canvas.draw_border(form_x, my + 246, 14, 14, COL_BRAND_BLUE);
                    if new_vm_create_disk {
                        canvas.fill_rect(form_x + 3, my + 249, 8, 8, COL_BRAND_BLUE);
                    }

                    if new_vm_create_disk {
                        let disk_ratio = ((new_vm_disk_gb.clamp(5, 500) - 5) as f32) / 495.0;
                        canvas.draw_text(form_x, my + 276, &format!("Tamano: {} GiB", new_vm_disk_gb), COL_TEXT_MUTED);
                        canvas.draw_slider(
                            ActiveSlider::WizardDisk,
                            form_x + 160,
                            my + 278,
                            220,
                            disk_ratio,
                            0.70,
                            0.90,
                            active_slider == ActiveSlider::WizardDisk,
                        );
                        canvas.draw_button_gray("wiz_disk_dec", form_x + 394, my + 272, 28, 22, "-");
                        canvas.draw_button_gray("wiz_disk_inc", form_x + 428, my + 272, 28, 22, "+");
                    }

                    // Botones Cancelar / Terminar
                    canvas.draw_button_gray("wiz_cancel", mx + m_w - 220, my + m_h - 40, 95, 28, "Cancelar");
                    canvas.draw_button_primary("wiz_finish", mx + m_w - 115, my + m_h - 40, 95, 28, "Terminar");
                }
            }

            ModalDialog::CreateDiskWizard => {
                if let Some(ref fw) = floating_state {
                    let m_w = fw.w;
                    let m_h = fw.h;
                    let mx = fw.x;
                    let my = fw.y;

                    canvas.draw_drop_shadow(mx, my, m_w, m_h, 8);
                    canvas.draw_floating_titlebar(&assets, fw, "Two Five Five (255) — Crear Disco Duro Virtual VDI", Some("hd_32px.png"));

                    // Banner lateral personalizado Two Five Five (`wizard_new_harddisk.png`: 145x290)
                    assets.draw("wizard_new_harddisk.png", &mut canvas.buf, WIN_W, WIN_H, (mx + 10) as isize, (my + 40) as isize);

                    let form_x = mx + 170;
                    canvas.draw_text(form_x, my + 46, "Nombre de archivo del disco:", COL_TEXT_MUTED);
                    let _n_hov = canvas.register_btn("vd_name_box", form_x, my + 66, 430, 24);
                    canvas.fill_rect(form_x, my + 66, 430, 24, COL_INPUT_BG);
                    canvas.draw_border(form_x, my + 66, 430, 24, if active_field == ActiveField::WizardDiskName { COL_INPUT_BORDER } else { COL_CARD_BORDER });
                    canvas.draw_text(form_x + 6, my + 70, &new_disk_name, COL_TEXT_MAIN);

                    canvas.draw_text(form_x, my + 104, "Tipo de archivo y variante:", COL_TEXT_MAIN);
                    let _dyn_hov = canvas.register_btn("vd_set_dyn", form_x, my + 128, 420, 20);
                    canvas.draw_text(form_x + 22, my + 128, "VDI Reservado dinamicamente (crece bajo demanda)", COL_TEXT_MAIN);
                    canvas.draw_border(form_x, my + 128, 14, 14, COL_BRAND_BLUE);
                    if !new_disk_fixed {
                        canvas.fill_rect(form_x + 3, my + 131, 8, 8, COL_BRAND_BLUE);
                    }

                    let _fix_hov = canvas.register_btn("vd_set_fix", form_x, my + 152, 420, 20);
                    canvas.draw_text(form_x + 22, my + 152, "VDI Tamano fijo (preasignado completo en host)", COL_TEXT_MAIN);
                    canvas.draw_border(form_x, my + 152, 14, 14, COL_BRAND_BLUE);
                    if new_disk_fixed {
                        canvas.fill_rect(form_x + 3, my + 155, 8, 8, COL_BRAND_BLUE);
                    }

                    canvas.draw_text(form_x, my + 188, &format!("Capacidad virtual: {} GiB", new_disk_size_gb), COL_TEXT_MAIN);
                    canvas.draw_button_gray("vd_size_dec", form_x + 220, my + 184, 30, 22, "-");
                    canvas.draw_button_gray("vd_size_inc", form_x + 256, my + 184, 30, 22, "+");
                    canvas.draw_button_gray("vd_preset_20", form_x + 296, my + 184, 40, 22, "20G");
                    canvas.draw_button_gray("vd_preset_40", form_x + 342, my + 184, 40, 22, "40G");
                    canvas.draw_button_gray("vd_preset_80", form_x + 388, my + 184, 40, 22, "80G");

                    let vd_ratio = ((new_disk_size_gb.clamp(5, 500) - 5) as f32) / 495.0;
                    canvas.draw_slider(
                        ActiveSlider::CreateDiskSize,
                        form_x,
                        my + 220,
                        428,
                        vd_ratio,
                        0.70,
                        0.90,
                        active_slider == ActiveSlider::CreateDiskSize,
                    );
                    canvas.draw_text(form_x, my + 240, "5 GiB", COL_TEXT_MUTED);
                    canvas.draw_text(form_x + 372, my + 240, "500 GiB", COL_TEXT_MUTED);

                    canvas.draw_button_gray("vd_cancel", mx + m_w - 220, my + m_h - 40, 95, 28, "Cancelar");
                    canvas.draw_button_primary("vd_create_btn", mx + m_w - 115, my + m_h - 40, 95, 28, "Crear VDI");
                }
            }

            ModalDialog::SettingsDialog => {
                if let Some(ref fw) = floating_state {
                    let m_w = fw.w;
                    let m_h = fw.h;
                    let mx = fw.x;
                    let my = fw.y;

                    canvas.draw_drop_shadow(mx, my, m_w, m_h, 8);
                    canvas.draw_floating_titlebar(&assets, fw, &format!("{} - Configuracion (Two Five Five)", profiles[sel_idx].name), Some("vm_settings_32px.png"));

                // Selector lateral de categorías con iconos de 32x32
                const CAT_W: usize = 160;
                canvas.fill_rect(mx, my + 32, CAT_W, m_h - 32, 0xF9FAFB);
                canvas.fill_rect(mx + CAT_W - 1, my + 32, 1, m_h - 32, COL_CARD_BORDER);

                let cats = [
                    (SettingsCategory::General, "General", "machine_32px.png", "set_cat_general"),
                    (SettingsCategory::System, "Sistema", "chipset_32px.png", "set_cat_system"),
                    (SettingsCategory::Display, "Pantalla", "vrdp_32px.png", "set_cat_display"),
                    (SettingsCategory::Storage, "Almacenamiento", "hd_32px.png", "set_cat_storage"),
                    (SettingsCategory::Audio, "Audio", "sound_32px.png", "set_cat_audio"),
                    (SettingsCategory::Network, "Red", "nw_32px.png", "set_cat_network"),
                    (SettingsCategory::Usb, "USB", "usb_32px.png", "set_cat_usb"),
                ];

                let mut cy = my + 36;
                for (cat, label, icon, btn_id) in &cats {
                    let is_sel = settings_category == *cat;
                    let hov = canvas.register_btn(btn_id, mx, cy, CAT_W - 1, 46);
                    let bg = if is_sel { 0xD8EAFB } else if hov { 0xEBF2F9 } else { 0xF9FAFB };
                    canvas.fill_rect(mx, cy, CAT_W - 1, 46, bg);
                    assets.draw(icon, &mut canvas.buf, WIN_W, WIN_H, (mx + 8) as isize, (cy + 7) as isize);
                    canvas.draw_text(mx + 48, cy + 15, label, if is_sel { COL_LINK_BLUE } else { COL_TEXT_MAIN });
                    cy += 48;
                }

                // Panel derecho de la categoría de Configuración
                let set_x = mx + CAT_W + 16;
                let _set_w = m_w.saturating_sub(CAT_W + 32);

                match settings_category {
                    SettingsCategory::General => {
                        let cur_p = &mut profiles[sel_idx];
                        canvas.draw_text(set_x, my + 46, "Propiedades Generales de la Maquina:", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 76, "Nombre:", COL_TEXT_MUTED);
                        let _e_hov = canvas.register_btn("set_vm_name_box", set_x + 90, my + 72, 360, 24);
                        canvas.fill_rect(set_x + 90, my + 72, 360, 24, COL_INPUT_BG);
                        canvas.draw_border(set_x + 90, my + 72, 360, 24, if active_field == ActiveField::SettingsVmName { COL_INPUT_BORDER } else { COL_CARD_BORDER });
                        canvas.draw_text(set_x + 96, my + 76, &cur_p.name, COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 116, "Tipo y Version:", COL_TEXT_MUTED);
                        let os_btn_hov = canvas.register_btn("set_cycle_os_ver", set_x + 120, my + 112, 330, 24);
                        canvas.fill_rect(set_x + 120, my + 112, 330, 24, if os_btn_hov { 0xE6F0FA } else { COL_BTN_GRAY });
                        canvas.draw_border(set_x + 120, my + 112, 330, 24, COL_BTN_GRAY_BORDER);
                        canvas.draw_text(set_x + 128, my + 116, &cur_p.os_type, COL_TEXT_MAIN);
                        canvas.draw_text(set_x + 460, my + 116, "(Clic: cambiar)", COL_TEXT_MUTED);

                        canvas.draw_text(set_x, my + 160, "Instalacion desatendida (OEMDRV):", COL_TEXT_MAIN);
                        let _u_hov = canvas.register_btn("set_toggle_unatt", set_x, my + 184, 420, 20);
                        canvas.draw_border(set_x, my + 184, 14, 14, COL_BRAND_BLUE);
                        if cur_p.unattended {
                            canvas.fill_rect(set_x + 3, my + 187, 8, 8, COL_BRAND_BLUE);
                        }
                        canvas.draw_text(set_x + 22, my + 184, "Inyeccion automatica de credenciales y controladores", COL_TEXT_MAIN);
                    }
                    SettingsCategory::System => {
                        let cur_p = &mut profiles[sel_idx];
                        canvas.draw_text(set_x, my + 46, "Memoria Base y Procesadores (Placa Base y CPU):", COL_TEXT_MAIN);

                        // Slider interactivo de Memoria RAM (Arrastrable + Clic directo + Botones +/-)
                        let ram_ratio = ((cur_p.ram_mb.clamp(RAM_MIN_MB, RAM_MAX_MB) - RAM_MIN_MB) as f32)
                            / ((RAM_MAX_MB - RAM_MIN_MB) as f32);
                        canvas.draw_text(set_x, my + 74, "Memoria base:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 120, my + 74, &format!("{} MB", cur_p.ram_mb), COL_TEXT_MAIN);

                        let bar_x = set_x + 120;
                        let bar_w = 340;
                        canvas.draw_slider(
                            ActiveSlider::SettingsRam,
                            bar_x,
                            my + 96,
                            bar_w,
                            ram_ratio,
                            0.60,
                            0.85,
                            active_slider == ActiveSlider::SettingsRam,
                        );
                        canvas.draw_text(bar_x, my + 115, "512 MB", COL_TEXT_MUTED);
                        canvas.draw_text(bar_x + bar_w - 64, my + 115, "16384 MB", COL_TEXT_MUTED);

                        canvas.draw_button_gray("set_ram_dec", set_x + 480, my + 72, 30, 24, "-");
                        canvas.draw_button_gray("set_ram_inc", set_x + 516, my + 72, 30, 24, "+");

                        // Slider interactivo de Procesadores (CPUs)
                        let cpu_ratio = ((cur_p.cpus.clamp(CPU_MIN, CPU_MAX) - CPU_MIN) as f32)
                            / ((CPU_MAX - CPU_MIN) as f32);
                        canvas.draw_text(set_x, my + 140, "Procesadores:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 120, my + 140, &format!("{} vCPU", cur_p.cpus), COL_TEXT_MAIN);
                        canvas.draw_slider(
                            ActiveSlider::SettingsCpu,
                            bar_x,
                            my + 162,
                            bar_w,
                            cpu_ratio,
                            0.50,
                            0.80,
                            active_slider == ActiveSlider::SettingsCpu,
                        );
                        canvas.draw_text(bar_x, my + 181, "1 CPU", COL_TEXT_MUTED);
                        canvas.draw_text(bar_x + bar_w - 56, my + 181, "16 CPUs", COL_TEXT_MUTED);

                        canvas.draw_button_gray("set_cpu_dec", set_x + 480, my + 136, 30, 24, "-");
                        canvas.draw_button_gray("set_cpu_inc", set_x + 516, my + 136, 30, 24, "+");

                        // Características extendidas
                        canvas.draw_text(set_x, my + 210, "Caracteristicas extendidas:", COL_TEXT_MAIN);

                        let _chk_efi_hov = canvas.register_btn("set_toggle_efi", set_x, my + 234, 360, 20);
                        canvas.draw_border(set_x, my + 234, 14, 14, COL_BRAND_BLUE);
                        if cur_p.use_efi {
                            canvas.fill_rect(set_x + 3, my + 237, 8, 8, COL_BRAND_BLUE);
                        }
                        canvas.draw_text(set_x + 22, my + 234, "Habilitar EFI (especial para OVMF / x64)", COL_TEXT_MAIN);

                        canvas.draw_border(set_x, my + 260, 14, 14, COL_BRAND_BLUE);
                        canvas.fill_rect(set_x + 3, my + 263, 8, 8, COL_BRAND_BLUE);
                        canvas.draw_text(set_x + 22, my + 260, "Habilitar I/O APIC y PAE/NX (Requerido para x64)", COL_TEXT_MAIN);

                        canvas.draw_border(set_x, my + 286, 14, 14, COL_BRAND_BLUE);
                        canvas.fill_rect(set_x + 3, my + 289, 8, 8, COL_BRAND_BLUE);
                        canvas.draw_text(set_x + 22, my + 286, "Reloj hardware en tiempo UTC (RTC)", COL_TEXT_MAIN);
                    }
                    SettingsCategory::Display => {
                        canvas.draw_text(set_x, my + 46, "Pantalla y Aceleracion Grafica:", COL_TEXT_MAIN);
                        canvas.draw_text(set_x, my + 74, "Memoria de video (VRAM):", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 190, my + 74, &format!("{} MB (Bochs VBE LFB)", vram_mb), COL_TEXT_MAIN);

                        let vram_ratio = ((vram_mb.clamp(16, 256) - 16) as f32) / 240.0;
                        let bar_x = set_x + 190;
                        let bar_w = 270;
                        canvas.draw_slider(
                            ActiveSlider::SettingsVram,
                            bar_x,
                            my + 96,
                            bar_w,
                            vram_ratio,
                            0.75,
                            0.95,
                            active_slider == ActiveSlider::SettingsVram,
                        );
                        canvas.draw_text(bar_x, my + 115, "16 MB", COL_TEXT_MUTED);
                        canvas.draw_text(bar_x + bar_w - 48, my + 115, "256 MB", COL_TEXT_MUTED);
                        canvas.draw_button_gray("set_vram_dec", set_x + 476, my + 72, 30, 24, "-");
                        canvas.draw_button_gray("set_vram_inc", set_x + 512, my + 72, 30, 24, "+");

                        canvas.draw_text(set_x, my + 146, "Controlador grafico:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 190, my + 146, "255-VBE (Bochs VBE / LFB Acelerado)", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 182, "Aceleracion:", COL_TEXT_MAIN);
                        canvas.draw_border(set_x, my + 206, 14, 14, COL_BRAND_BLUE);
                        canvas.fill_rect(set_x + 3, my + 209, 8, 8, COL_BRAND_BLUE);
                        canvas.draw_text(set_x + 22, my + 206, "Habilitar Aceleracion 2D y Escala Lineal VBE", COL_TEXT_MAIN);
                    }
                    SettingsCategory::Storage => {
                        let cur_p = &mut profiles[sel_idx];
                        canvas.draw_text(set_x, my + 46, "Arbol de Almacenamiento (Storage Tree):", COL_TEXT_MAIN);

                        // IDE Controller
                        canvas.fill_rect(set_x, my + 70, 520, 70, 0xF8F9FA);
                        canvas.draw_border(set_x, my + 70, 520, 70, COL_CARD_BORDER);
                        canvas.draw_text(set_x + 10, my + 76, "Controlador: IDE (PIIX4 PnP 8086:7111)", COL_TEXT_MAIN);
                        let iso_display = cur_p.iso_path.as_deref().unwrap_or("[Unidad optica vacia]");
                        canvas.draw_text_clipped(set_x + 24, my + 98, 38, &format!("Optica (Maestro): {}", iso_display), COL_TEXT_MUTED);
                        canvas.draw_button_gray("set_pick_iso", set_x + 360, my + 92, 75, 24, "Cambiar");
                        canvas.draw_button_gray("set_eject_iso", set_x + 440, my + 92, 70, 24, "Expulsar");

                        // SATA Controller
                        canvas.fill_rect(set_x, my + 150, 520, 95, 0xF8F9FA);
                        canvas.draw_border(set_x, my + 150, 520, 95, COL_CARD_BORDER);
                        canvas.draw_text(set_x + 10, my + 156, "Controlador: SATA (AHCI Intel 82801 8086:2829)", COL_TEXT_MAIN);
                        let disk_display = cur_p.disk_path.as_deref().unwrap_or("[Sin disco duro conectado]");
                        canvas.draw_text_clipped(set_x + 24, my + 178, 38, &format!("Puerto 0: {}", disk_display), COL_TEXT_MUTED);
                        canvas.draw_button_gray("set_pick_disk", set_x + 340, my + 172, 75, 24, "Cambiar");
                        canvas.draw_button_gray("set_create_vdi_wiz", set_x + 420, my + 172, 90, 24, "Crear VDI");
                        if cur_p.disk_path.is_some() {
                            canvas.draw_button_gray("set_compact_vdi", set_x + 24, my + 212, 130, 22, "Compactar VDI");
                        }
                    }
                    SettingsCategory::Audio => {
                        canvas.draw_text(set_x, my + 46, "Audio y Controladores de Sonido:", COL_TEXT_MAIN);

                        canvas.draw_border(set_x, my + 76, 14, 14, COL_BRAND_BLUE);
                        canvas.fill_rect(set_x + 3, my + 79, 8, 8, COL_BRAND_BLUE);
                        canvas.draw_text(set_x + 22, my + 76, "Habilitar audio", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 116, "Controlador de audio anfitrion:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 220, my + 116, "PulseAudio / ALSA Driver", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 146, "Controlador de audio invitado:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 220, my + 146, "ICH AC'97 (Realtek ALC655)", COL_TEXT_MAIN);
                    }
                    SettingsCategory::Network => {
                        canvas.draw_text(set_x, my + 46, "Adaptadores de Red de la Maquina:", COL_TEXT_MAIN);

                        canvas.draw_border(set_x, my + 76, 14, 14, COL_BRAND_BLUE);
                        canvas.fill_rect(set_x + 3, my + 79, 8, 8, COL_BRAND_BLUE);
                        canvas.draw_text(set_x + 22, my + 76, "Habilitar adaptador de red (Adaptador 1)", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 116, "Conectado a:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 140, my + 116, "NAT (Traduccion de Direcciones de Red)", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 146, "Tipo de adaptador:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 140, my + 146, "Intel PRO/1000 MT Desktop (82540EM)", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 176, "Direccion MAC:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 140, my + 176, "02:55:00:6C:5B:42 (Prefijo MAC Two Five Five)", COL_TEXT_MAIN);
                        canvas.draw_button_gray("set_regen_mac", set_x + 440, my + 172, 85, 24, "Regenerar");
                    }
                    SettingsCategory::Usb => {
                        canvas.draw_text(set_x, my + 46, "Controlador USB y Filtros de Dispositivos:", COL_TEXT_MAIN);

                        canvas.draw_border(set_x, my + 76, 14, 14, COL_BRAND_BLUE);
                        canvas.fill_rect(set_x + 3, my + 79, 8, 8, COL_BRAND_BLUE);
                        canvas.draw_text(set_x + 22, my + 76, "Habilitar controlador USB (UHCI 1.1 + EHCI 2.0)", COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 116, "Dispositivos conectados:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 20, my + 144, "* Tableta USB Two Five Five (Puntero absoluto HID)", COL_TEXT_MAIN);
                        canvas.draw_text(set_x + 20, my + 170, "* Teclado y raton PS/2 legado (Fallback garantizado)", COL_TEXT_MAIN);
                    }
                }

                canvas.draw_button_primary("set_ok_btn", mx + m_w - 225, my + m_h - 40, 95, 28, "Aceptar");
                canvas.draw_button_gray("set_cancel_btn", mx + m_w - 115, my + m_h - 40, 95, 28, "Cancelar");
                }
            }

            ModalDialog::AboutDialog => {
                if let Some(ref fw) = floating_state {
                    let m_w = fw.w;
                    let m_h = fw.h;
                    let mx = fw.x;
                    let my = fw.y;

                    canvas.draw_drop_shadow(mx, my, m_w, m_h, 8);
                    canvas.draw_floating_titlebar(&assets, fw, "Acerca de Two Five Five (255) KVM Hypervisor", Some("logo_16px.png"));

                    assets.draw("logo_48px.png", &mut canvas.buf, WIN_W, WIN_H, (mx + 20) as isize, (my + 46) as isize);

                    let info_x = mx + 78;
                    canvas.draw_text(info_x, my + 44, "Two Five Five (255) — Hipervisor KVM Tipo-2", COL_TEXT_MAIN);
                    canvas.draw_text(info_x, my + 66, "Two Five Five (255) Engine v0.80 (Rust + Linux KVM)", COL_LINK_BLUE);
                    canvas.draw_text(info_x, my + 92, "Emulacion x86_64, 255-VBE Display (hasta 4K UHD),", COL_TEXT_MUTED);
                    canvas.draw_text(info_x, my + 112, "VirtIO & VMMDev Host Integration, Intel AC97, E1000,", COL_TEXT_MUTED);
                    canvas.draw_text(info_x, my + 132, "Discos dinamicos VDI, QCOW2 e imagenes RAW.", COL_TEXT_MUTED);
                    canvas.draw_text(info_x, my + 158, "Soporta Windows 7, 10, 11 y distribuciones Linux.", COL_TEXT_MAIN);

                    canvas.draw_text(info_x, my + 195, "(C) 2026 Two Five Five Project. Licencia GPLv2.", COL_TEXT_MUTED);
                    canvas.draw_text(info_x, my + 215, "Arquitectura nativa en Rust sobre Linux KVM (/dev/kvm).", COL_TEXT_MUTED);

                    canvas.draw_button_primary("about_ok_btn", mx + m_w - 115, my + m_h - 40, 95, 28, "Aceptar");
                }
            }

            ModalDialog::LogsDialog => {
                if let Some(ref fw) = floating_state {
                    let m_w = fw.w;
                    let m_h = fw.h;
                    let mx = fw.x;
                    let my = fw.y;

                    canvas.draw_drop_shadow(mx, my, m_w, m_h, 8);
                    canvas.draw_floating_titlebar(&assets, fw, "Two Five Five — Registros de Actividad (VM Logs)", Some("tools_menu_24px.png"));

                    let p = &profiles[sel_idx];
                    canvas.draw_text(mx + 20, my + 42, &format!("Maquina seleccionada: {} [{}]", p.name, p.os_type), COL_TEXT_MAIN);
                    canvas.draw_text(mx + 20, my + 62, &format!("CPU: {} nucleos | RAM: {} MB | Modo EFI: {}", p.cpus, p.ram_mb, if p.use_efi { "Si" } else { "No" }), COL_TEXT_MUTED);

                    let log_box_y = my + 86;
                    let log_box_h = m_h.saturating_sub(140);
                    let log_box_w = m_w.saturating_sub(40);
                    canvas.fill_rect(mx + 20, log_box_y, log_box_w, log_box_h, 0x1A1E24);
                    canvas.draw_border(mx + 20, log_box_y, log_box_w, log_box_h, 0x353D4A);

                    let log_entries = [
                        "[INFO] Two Five Five (255) Manager UI iniciado en modo grafico (1140x720).".to_string(),
                        "[INFO] KVM Hypervisor disponible en /dev/kvm (Soporte hardware activo).".to_string(),
                        format!("[INFO] Perfil cargado: '{}' con {} vCPUs y {} MB RAM.", p.name, p.cpus, p.ram_mb),
                        format!("[DISCO] Imagen principal: {}", p.disk_path.as_deref().unwrap_or("Sin disco")),
                        format!("[ISO] Unidad optica IDE: {}", p.iso_path.as_deref().unwrap_or("Vacia")),
                        "[RED] Interfaz de red: Intel 82540EM (e1000) NAT User-Mode.".to_string(),
                        "[AUDIO] Controlador de sonido: Intel AC97 ICH compatible.".to_string(),
                        "[DISPLAY] Controlador de video: 255-VBE (Bochs VBE LFB, resolucion dinamica hasta 4K).".to_string(),
                        "[VMMDEV] Canal de comunicacion Guest-Host registrado.".to_string(),
                        "[STATUS] Sistema listo para arrancar en modo ejecucion.".to_string(),
                    ];

                    let mut ey = log_box_y + 10;
                    for entry in &log_entries {
                        if ey + 16 > log_box_y + log_box_h { break; }
                        canvas.draw_text_clipped(mx + 28, ey, log_box_w / 8 - 4, entry, 0x61AFEF);
                        ey += 20;
                    }

                    canvas.draw_button_gray("logs_refresh_btn", mx + 20, my + m_h - 40, 110, 28, "Actualizar");
                    canvas.draw_button_primary("logs_close_btn", mx + m_w - 115, my + m_h - 40, 95, 28, "Cerrar");
                }
            }

            ModalDialog::None => {}
        }

        // ─── Arrastre y Clic en Sliders Interactivos (CPU / RAM / VRAM / Disco) ───
        if clicked {
            let can_hit_slider = if let Some(ref fw) = floating_state {
                fw.contains_point(mx, my)
            } else {
                true
            };
            if can_hit_slider {
                if let Some(s) = canvas.sliders.iter().rev().find(|s| s.contains(mx, my)) {
                    active_slider = s.kind;
                }
            }
        }

        if mouse_down && active_slider != ActiveSlider::None {
            if let Some(s) = canvas.sliders.iter().find(|s| s.kind == active_slider) {
                let ratio = s.ratio_at(mx);
                match active_slider {
                    ActiveSlider::MainRam | ActiveSlider::SettingsRam => {
                        let raw = RAM_MIN_MB as f32 + ratio * ((RAM_MAX_MB - RAM_MIN_MB) as f32);
                        let stepped = (((raw as usize) + 32) / 64 * 64).clamp(RAM_MIN_MB, RAM_MAX_MB);
                        if profiles[sel_idx].ram_mb != stepped {
                            profiles[sel_idx].ram_mb = stepped;
                            if active_slider == ActiveSlider::MainRam {
                                let _ = save_vm_profiles(&profiles);
                            }
                        }
                    }
                    ActiveSlider::MainCpu | ActiveSlider::SettingsCpu => {
                        let raw = CPU_MIN as f32 + ratio * ((CPU_MAX - CPU_MIN) as f32);
                        let stepped = (raw.round() as u32).clamp(CPU_MIN, CPU_MAX);
                        if profiles[sel_idx].cpus != stepped {
                            profiles[sel_idx].cpus = stepped;
                            if active_slider == ActiveSlider::MainCpu {
                                let _ = save_vm_profiles(&profiles);
                            }
                        }
                    }
                    ActiveSlider::SettingsVram => {
                        let raw = 16.0 + ratio * 240.0;
                        vram_mb = (((raw as usize) + 4) / 8 * 8).clamp(16, 256);
                    }
                    ActiveSlider::WizardRam => {
                        let raw = RAM_MIN_MB as f32 + ratio * ((RAM_MAX_MB - RAM_MIN_MB) as f32);
                        new_vm_ram_mb = (((raw as usize) + 32) / 64 * 64).clamp(RAM_MIN_MB, RAM_MAX_MB);
                    }
                    ActiveSlider::WizardCpu => {
                        let raw = CPU_MIN as f32 + ratio * ((CPU_MAX - CPU_MIN) as f32);
                        new_vm_cpus = (raw.round() as u32).clamp(CPU_MIN, CPU_MAX);
                    }
                    ActiveSlider::WizardDisk => {
                        let raw = 5.0 + ratio * 495.0;
                        new_vm_disk_gb = (raw.round() as u64).clamp(5, 500);
                    }
                    ActiveSlider::CreateDiskSize => {
                        let raw = 5.0 + ratio * 495.0;
                        new_disk_size_gb = (raw.round() as u64).clamp(5, 500);
                    }
                    ActiveSlider::None => {}
                }
            }
        }

        // ─── Despacho de Eventos de Clic ────────────────────────────────────────
        if clicked && active_slider == ActiveSlider::None {
            // Si hay un modal flotante abierto, solo permitir interacción con elementos dentro de su marco
            let hit = if let Some(ref fw) = floating_state {
                if fw.contains_point(mx, my) {
                    canvas.buttons.iter().find(|b| b.contains(mx, my)).map(|b| b.id.clone())
                } else {
                    None
                }
            } else {
                canvas.buttons.iter().find(|b| b.contains(mx, my)).map(|b| b.id.clone())
            };

            if let Some(id) = hit {
                match id.as_str() {
                    // Barra de Menús Superior
                    "menu_help" => {
                        current_modal = ModalDialog::AboutDialog;
                        floating_state = Some(FloatingWindowState::new_centered(560, 330, WIN_W, WIN_H));
                    }
                    "menu_tools" => {
                        current_modal = ModalDialog::LogsDialog;
                        floating_state = Some(FloatingWindowState::new_centered(740, 480, WIN_W, WIN_H));
                    }
                    "menu_machine" => {
                        profile_backup = Some(profiles[sel_idx].clone());
                        current_modal = ModalDialog::SettingsDialog;
                        settings_category = SettingsCategory::System;
                    }
                    "menu_file" => {
                        current_view = MainViewTab::Media;
                    }

                    // Toolbar Acciones
                    "tb_start" => {
                        launch_vm_external(
                            &profiles[sel_idx],
                            &mut running_vms,
                            &mut status_msg,
                            &mut status_is_err,
                        );
                    }
                    "tb_new" => {
                        new_vm_name = format!("Maquina Virtual {}", profiles.len() + 1);
                        new_vm_iso_idx = 0;
                        new_vm_ram_mb = 4096;
                        new_vm_cpus = 4;
                        new_vm_create_disk = true;
                        new_vm_disk_gb = 20;
                        current_modal = ModalDialog::NewVmWizard;
                    }
                    "tb_add" => {
                        let p = &profiles[sel_idx];
                        let mut cloned = p.clone();
                        cloned.name = format!("{} (Copia)", p.name);
                        profiles.push(cloned);
                        sel_idx = profiles.len() - 1;
                        let _ = save_vm_profiles(&profiles);
                        status_msg = format!("VM clonada: '{}'", profiles[sel_idx].name);
                    }
                    "tb_settings" => {
                        profile_backup = Some(profiles[sel_idx].clone());
                        current_modal = ModalDialog::SettingsDialog;
                        settings_category = SettingsCategory::General;
                    }
                    "tb_discard" => {
                        if running_vms.contains_key(&profiles[sel_idx].name) {
                            status_msg = format!("No se puede descartar la VM '{}' porque se encuentra en ejecucion.", profiles[sel_idx].name);
                            status_is_err = true;
                        } else if profiles.len() > 1 {
                            let removed = profiles.remove(sel_idx);
                            sel_idx = sel_idx.min(profiles.len() - 1);
                            let _ = save_vm_profiles(&profiles);
                            status_msg = format!("VM '{}' eliminada de vms.conf", removed.name);
                        } else {
                            status_msg = "No se puede eliminar la unica VM existente.".to_string();
                            status_is_err = true;
                        }
                    }
                    "tb_media" | "tab_media" | "item_tools" => {
                        current_view = MainViewTab::Media;
                    }
                    "tab_details" => {
                        current_view = MainViewTab::Details;
                    }
                    "tab_logs" => {
                        current_view = MainViewTab::Logs;
                    }

                    // Pestañas de Medios
                    "med_tab_hd" => media_tab = MediaTab::HardDisks,
                    "med_tab_cd" => media_tab = MediaTab::OpticalDisks,
                    "btn_med_refresh" => {
                        available_isos = scan_iso_files();
                        available_disks = scan_disk_files();
                        status_msg = format!("Medios actualizados: {} discos y {} ISOs.", available_disks.len(), available_isos.len());
                    }
                    "btn_med_create_vdi" | "btn_create_vdi_quick" => {
                        new_disk_name = suggest_disk_filename(&profiles[sel_idx].name, ".vdi");
                        new_disk_size_gb = 20;
                        new_disk_fixed = false;
                        current_modal = ModalDialog::CreateDiskWizard;
                    }

                    // Acciones Rápidas en Tarjetas de Detalles
                    "act_edit_name" | "set_vm_name_box" => {
                        active_field = ActiveField::SettingsVmName;
                    }
                    "act_edit_os" => {
                        let os_list = [
                            "Windows 7 (64-bit)",
                            "Windows 10 (64-bit)",
                            "Windows 11 (64-bit)",
                            "Ubuntu (64-bit)",
                            "Debian (64-bit)",
                            "Arch Linux (64-bit)",
                            "Linux (64-bit)",
                            "Other/Unknown (64-bit)",
                        ];
                        let cur = &profiles[sel_idx].os_type;
                        let cur_pos = os_list.iter().position(|s| s == cur).unwrap_or(0);
                        let next_pos = (cur_pos + 1) % os_list.len();
                        profiles[sel_idx].os_type = os_list[next_pos].to_string();
                        let _ = save_vm_profiles(&profiles);
                    }
                    "act_toggle_unatt" | "set_toggle_unatt" => {
                        profiles[sel_idx].unattended = !profiles[sel_idx].unattended;
                        let _ = save_vm_profiles(&profiles);
                    }
                    "act_toggle_efi" | "set_toggle_efi" => {
                        profiles[sel_idx].use_efi = !profiles[sel_idx].use_efi;
                        let _ = save_vm_profiles(&profiles);
                    }
                    "ram_dec" | "set_ram_dec" => {
                        if profiles[sel_idx].ram_mb > RAM_MIN_MB {
                            profiles[sel_idx].ram_mb = profiles[sel_idx].ram_mb.saturating_sub(512).max(RAM_MIN_MB);
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "ram_inc" | "set_ram_inc" => {
                        if profiles[sel_idx].ram_mb < RAM_MAX_MB {
                            profiles[sel_idx].ram_mb = (profiles[sel_idx].ram_mb + 512).min(RAM_MAX_MB);
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "cpu_dec" | "set_cpu_dec" => {
                        if profiles[sel_idx].cpus > CPU_MIN {
                            profiles[sel_idx].cpus -= 1;
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "cpu_inc" | "set_cpu_inc" => {
                        if profiles[sel_idx].cpus < CPU_MAX {
                            profiles[sel_idx].cpus += 1;
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "set_vram_dec" => {
                        vram_mb = vram_mb.saturating_sub(16).max(16);
                    }
                    "set_vram_inc" => {
                        vram_mb = (vram_mb + 16).min(256);
                    }
                    "act_pick_iso" | "btn_browse_iso_quick" | "btn_med_browse_iso" => {
                        if let Some(chosen) = open_iso_file_dialog(profiles[sel_idx].iso_path.as_deref()) {
                            if !available_isos.contains(&chosen) {
                                available_isos.push(chosen.clone());
                                available_isos.sort();
                            }
                            profiles[sel_idx].os_type = detect_os_type_from_iso(&chosen);
                            profiles[sel_idx].iso_path = Some(chosen.clone());
                            let _ = save_vm_profiles(&profiles);
                            status_msg = format!("Imagen ISO seleccionada: {}", chosen);
                            status_is_err = false;
                        }
                    }
                    "btn_eject_iso_quick" => {
                        profiles[sel_idx].iso_path = None;
                        let _ = save_vm_profiles(&profiles);
                        status_msg = "Unidad optica virtual expulsada.".to_string();
                    }
                    "act_pick_disk" => {
                        if let Some(chosen) = open_disk_file_dialog(profiles[sel_idx].disk_path.as_deref()) {
                            if !available_disks.contains(&chosen) {
                                available_disks.push(chosen.clone());
                                available_disks.sort();
                            }
                            profiles[sel_idx].disk_path = Some(chosen.clone());
                            last_selected_disk_info = VirtualDisk::probe_info(&chosen).ok();
                            let _ = save_vm_profiles(&profiles);
                            status_msg = format!("Disco virtual seleccionado: {}", chosen);
                            status_is_err = false;
                        }
                    }
                    "btn_compact_quick" => {
                        if let Some(ref d) = profiles[sel_idx].disk_path {
                            match VirtualDisk::open(d).and_then(|mut v| v.compact()) {
                                Ok(saved) => {
                                    status_msg = format!("VDI '{}' compactado ({:.2} MiB liberados).", d, saved as f64 / (1024.0 * 1024.0));
                                    status_is_err = false;
                                }
                                Err(e) => {
                                    status_msg = format!("Error al compactar: {}", e);
                                    status_is_err = true;
                                }
                            }
                        }
                    }

                    // Botones de Modales y Ventanas Flotantes
                    "modal_floating_close_btn" | "wiz_cancel" | "vd_cancel" | "about_ok_btn" | "logs_close_btn" => {
                        if current_modal == ModalDialog::SettingsDialog {
                            if let Some(snap) = profile_backup.take() {
                                profiles[sel_idx] = snap;
                            }
                        }
                        current_modal = ModalDialog::None;
                        floating_state = None;
                        active_field = ActiveField::None;
                    }
                    "logs_refresh_btn" => {
                        status_msg = "Registros de actividad actualizados.".to_string();
                    }
                    "set_cancel_btn" => {
                        if let Some(snap) = profile_backup.take() {
                            profiles[sel_idx] = snap;
                        }
                        current_modal = ModalDialog::None;
                        floating_state = None;
                        active_field = ActiveField::None;
                    }
                    "set_ok_btn" => {
                        let _ = save_vm_profiles(&profiles);
                        profile_backup = None;
                        current_modal = ModalDialog::None;
                        floating_state = None;
                        active_field = ActiveField::None;
                        status_msg = "Configuracion de maquina guardada en vms.conf.".to_string();
                    }
                    "set_cat_general" => settings_category = SettingsCategory::General,
                    "set_cat_system" => settings_category = SettingsCategory::System,
                    "set_cat_display" => settings_category = SettingsCategory::Display,
                    "set_cat_storage" => settings_category = SettingsCategory::Storage,
                    "set_cat_audio" => settings_category = SettingsCategory::Audio,
                    "set_cat_network" => settings_category = SettingsCategory::Network,
                    "set_cat_usb" => settings_category = SettingsCategory::Usb,
                    "set_cycle_os_ver" => {
                        let os_list = [
                            "Windows 7 (64-bit)",
                            "Windows 10 (64-bit)",
                            "Windows 11 (64-bit)",
                            "Ubuntu (64-bit)",
                            "Debian (64-bit)",
                            "Arch Linux (64-bit)",
                            "Linux (64-bit)",
                            "Other/Unknown (64-bit)",
                        ];
                        let cur = &profiles[sel_idx].os_type;
                        let cur_pos = os_list.iter().position(|s| s == cur).unwrap_or(0);
                        let next_pos = (cur_pos + 1) % os_list.len();
                        profiles[sel_idx].os_type = os_list[next_pos].to_string();
                    }
                    "set_pick_iso" => {
                        if let Some(chosen) = open_iso_file_dialog(profiles[sel_idx].iso_path.as_deref()) {
                            if !available_isos.contains(&chosen) {
                                available_isos.push(chosen.clone());
                                available_isos.sort();
                            }
                            profiles[sel_idx].os_type = detect_os_type_from_iso(&chosen);
                            profiles[sel_idx].iso_path = Some(chosen.clone());
                            let _ = save_vm_profiles(&profiles);
                            status_msg = format!("Imagen ISO seleccionada: {}", chosen);
                            status_is_err = false;
                        }
                    }
                    "set_eject_iso" => {
                        profiles[sel_idx].iso_path = None;
                        let _ = save_vm_profiles(&profiles);
                    }
                    "set_pick_disk" => {
                        if let Some(chosen) = open_disk_file_dialog(profiles[sel_idx].disk_path.as_deref()) {
                            if !available_disks.contains(&chosen) {
                                available_disks.push(chosen.clone());
                                available_disks.sort();
                            }
                            profiles[sel_idx].disk_path = Some(chosen.clone());
                            last_selected_disk_info = VirtualDisk::probe_info(&chosen).ok();
                            let _ = save_vm_profiles(&profiles);
                            status_msg = format!("Disco virtual seleccionado: {}", chosen);
                            status_is_err = false;
                        }
                    }
                    "set_create_vdi_wiz" => {
                        new_disk_name = suggest_disk_filename(&profiles[sel_idx].name, ".vdi");
                        new_disk_size_gb = 20;
                        new_disk_fixed = false;
                        current_modal = ModalDialog::CreateDiskWizard;
                    }
                    "set_compact_vdi" => {
                        if let Some(ref d) = profiles[sel_idx].disk_path {
                            match VirtualDisk::open(d).and_then(|mut v| v.compact()) {
                                Ok(saved) => {
                                    status_msg = format!("VDI '{}' compactado ({:.2} MiB liberados).", d, saved as f64 / (1024.0 * 1024.0));
                                    status_is_err = false;
                                }
                                Err(e) => {
                                    status_msg = format!("Error al compactar: {}", e);
                                    status_is_err = true;
                                }
                            }
                        }
                    }
                    "set_regen_mac" => {
                        status_msg = "Nueva direccion MAC generada para Adaptador 1 (02:55:00).".to_string();
                    }
                    "wiz_vm_name_box" => active_field = ActiveField::WizardVmName,
                    "vd_name_box" => active_field = ActiveField::WizardDiskName,
                    "wiz_cycle_iso" => {
                        if !available_isos.is_empty() {
                            new_vm_iso_idx = (new_vm_iso_idx + 1) % available_isos.len();
                            let iso_name = &available_isos[new_vm_iso_idx];
                            new_vm_name = friendly_vm_name_from_iso(iso_name, profiles.len());
                        }
                    }
                    "wiz_browse_iso" => {
                        let cur_iso = available_isos.get(new_vm_iso_idx).map(|s| s.as_str());
                        if let Some(chosen) = open_iso_file_dialog(cur_iso) {
                            let idx = if let Some(pos) = available_isos.iter().position(|s| s == &chosen) {
                                pos
                            } else {
                                available_isos.push(chosen.clone());
                                available_isos.len() - 1
                            };
                            new_vm_iso_idx = idx;
                            new_vm_name = friendly_vm_name_from_iso(&chosen, profiles.len());
                            status_msg = format!("Imagen ISO seleccionada para nueva VM: {}", chosen);
                            status_is_err = false;
                        }
                    }
                    "wiz_ram_dec" => new_vm_ram_mb = new_vm_ram_mb.saturating_sub(512).max(RAM_MIN_MB),
                    "wiz_ram_inc" => new_vm_ram_mb = (new_vm_ram_mb + 512).min(RAM_MAX_MB),
                    "wiz_cpu_dec" => new_vm_cpus = new_vm_cpus.saturating_sub(1).max(CPU_MIN),
                    "wiz_cpu_inc" => new_vm_cpus = (new_vm_cpus + 1).min(CPU_MAX),
                    "wiz_toggle_create_disk" => new_vm_create_disk = !new_vm_create_disk,
                    "wiz_disk_dec" => new_vm_disk_gb = new_vm_disk_gb.saturating_sub(5).max(5),
                    "wiz_disk_inc" => new_vm_disk_gb = (new_vm_disk_gb + 5).min(2000),
                    "wiz_finish" => {
                        let iso_choice = available_isos.get(new_vm_iso_idx).cloned();
                        let disk_choice = if new_vm_create_disk {
                            let disk_file = suggest_disk_filename(&new_vm_name, ".vdi");
                            let bytes = new_vm_disk_gb * 1024 * 1024 * 1024;
                            let _ = VirtualDisk::create_vdi(&disk_file, bytes, false, "Created by Two Five Five (255) Manager");
                            available_disks = scan_disk_files();
                            Some(disk_file)
                        } else {
                            available_disks.first().cloned()
                        };

                        let mut new_prof = VmProfile::new_default(&new_vm_name, iso_choice.clone(), disk_choice);
                        if let Some(ref iso_p) = iso_choice {
                            new_prof.os_type = detect_os_type_from_iso(iso_p);
                        }
                        new_prof.ram_mb = new_vm_ram_mb;
                        new_prof.cpus = new_vm_cpus;
                        profiles.push(new_prof);
                        sel_idx = profiles.len() - 1;
                        let _ = save_vm_profiles(&profiles);
                        current_modal = ModalDialog::None;
                        active_field = ActiveField::None;
                        status_msg = format!("Maquina Virtual '{}' creada exitosamente.", profiles[sel_idx].name);
                    }
                    "vd_set_dyn" => new_disk_fixed = false,
                    "vd_set_fix" => new_disk_fixed = true,
                    "vd_size_dec" => new_disk_size_gb = new_disk_size_gb.saturating_sub(5).max(1),
                    "vd_size_inc" => new_disk_size_gb = (new_disk_size_gb + 5).min(2000),
                    "vd_preset_20" => new_disk_size_gb = 20,
                    "vd_preset_40" => new_disk_size_gb = 40,
                    "vd_preset_80" => new_disk_size_gb = 80,
                    "vd_create_btn" => {
                        let ext = if new_disk_name.to_lowercase().ends_with(".vdi") { "" } else { ".vdi" };
                        let full_disk_name = format!("{}{}", new_disk_name, ext);
                        let bytes = new_disk_size_gb * 1024 * 1024 * 1024;
                        match VirtualDisk::create_vdi(&full_disk_name, bytes, new_disk_fixed, "Created by Two Five Five (255) Manager") {
                            Ok(_) => {
                                available_disks = scan_disk_files();
                                profiles[sel_idx].disk_path = Some(full_disk_name.clone());
                                last_selected_disk_info = VirtualDisk::probe_info(&full_disk_name).ok();
                                let _ = save_vm_profiles(&profiles);
                                current_modal = ModalDialog::None;
                                active_field = ActiveField::None;
                                status_msg = format!("Disco VDI '{}' creado exitosamente.", full_disk_name);
                                status_is_err = false;
                            }
                            Err(e) => {
                                status_msg = format!("Error al crear VDI: {}", e);
                                status_is_err = true;
                            }
                        }
                    }

                    _ => {
                        // Selección de máquina en el Chooser (con detección de doble clic para iniciarla)
                        if let Some(stripped) = id.strip_prefix("vm_sel_") {
                            if let Ok(idx) = stripped.parse::<usize>() {
                                if idx < profiles.len() {
                                    let now = std::time::Instant::now();
                                    let is_double_click = last_click_sel_idx == Some(idx)
                                        && now.duration_since(last_click_time).as_millis() < 400;
                                    last_click_sel_idx = Some(idx);
                                    last_click_time = now;

                                    sel_idx = idx;
                                    current_view = MainViewTab::Details;
                                    if let Some(ref d) = profiles[sel_idx].disk_path {
                                        last_selected_disk_info = VirtualDisk::probe_info(d).ok();
                                    }

                                    if is_double_click {
                                        launch_vm_external(
                                            &profiles[sel_idx],
                                            &mut running_vms,
                                            &mut status_msg,
                                            &mut status_is_err,
                                        );
                                    }
                                }
                            }
                        } else if let Some(stripped) = id.strip_prefix("btn_assign_disk_") {
                            if let Ok(idx) = stripped.parse::<usize>() {
                                if let Some(d) = available_disks.get(idx) {
                                    profiles[sel_idx].disk_path = Some(d.clone());
                                    let _ = save_vm_profiles(&profiles);
                                    status_msg = format!("Disco '{}' asignado a {}", d, profiles[sel_idx].name);
                                }
                            }
                        } else if let Some(stripped) = id.strip_prefix("btn_assign_iso_") {
                            if let Ok(idx) = stripped.parse::<usize>() {
                                if let Some(iso) = available_isos.get(idx) {
                                    profiles[sel_idx].os_type = detect_os_type_from_iso(iso);
                                    profiles[sel_idx].iso_path = Some(iso.clone());
                                    let _ = save_vm_profiles(&profiles);
                                    status_msg = format!("ISO '{}' montada en {}", iso, profiles[sel_idx].name);
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Err(e) = window.update_with_buffer(&canvas.buf, WIN_W, WIN_H) {
            eprintln!("[GUI] Error al actualizar ventana: {}", e);
            break;
        }
        needs_redraw = false;
    }

    GuiOutcome::Cancelled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_friendly_name_detection() {
        assert_eq!(friendly_vm_name_from_iso("en_windows_7_professional_x64_dvd.iso", 0), "Windows 7 Pro x64");
        assert_eq!(friendly_vm_name_from_iso("ubuntu-24.04-desktop-amd64.iso", 1), "Ubuntu Linux x64");
        assert_eq!(friendly_vm_name_from_iso("CorePlus-current.iso", 2), "TinyCore Linux");
        assert_eq!(friendly_vm_name_from_iso("kali.iso", 3), "Kali Linux x64");
        assert_eq!(friendly_vm_name_from_iso("mint.iso", 4), "Linux Mint x64");
        assert_eq!(detect_os_type_from_iso("kali.iso"), "Debian (64-bit)");
        assert!(default_iso_browse_dir(None).ends_with('/'));
    }

    #[test]
    fn test_suggest_disk_filename() {
        assert_eq!(suggest_disk_filename("Windows 7 Pro x64", ".vdi"), "windows_7_pro_x64.vdi");
        assert_eq!(suggest_disk_filename("Ubuntu Linux", ".vdi"), "ubuntu_linux.vdi");
    }

    #[test]
    fn test_normalize_rel_path() {
        assert_eq!(normalize_rel_path("./disk.vdi"), "disk.vdi");
        assert_eq!(normalize_rel_path("disk.vdi"), "disk.vdi");
    }

    #[test]
    fn test_floating_window_state_geometry_and_hits() {
        let fw = FloatingWindowState::new_centered(600, 400, 1140, 720);
        assert_eq!(fw.x, 270);
        assert_eq!(fw.y, 160);
        assert_eq!(fw.w, 600);
        assert_eq!(fw.h, 400);

        // Contención de puntos
        assert!(fw.contains_point(270, 160));
        assert!(fw.contains_point(869, 559));
        assert!(!fw.contains_point(269, 160));
        assert!(!fw.contains_point(870, 560));

        // Detección de Barra de Título (excluyendo botón de cierre [X])
        assert!(fw.is_in_titlebar(300, 170));
        let (cx, cy, _cw, _ch) = fw.close_btn_rect();
        assert!(!fw.is_in_titlebar(cx + 2, cy + 2)); // El botón de cierre no debe arrastrar
        assert!(fw.is_in_close_btn(cx + 2, cy + 2));
    }

    #[test]
    fn test_external_vm_status_tracking() {
        let mut running_vms: HashMap<String, std::process::Child> = HashMap::new();
        assert!(!running_vms.contains_key("Windows 7 Pro"));

        let child = std::process::Command::new("true").spawn().expect("spawn true");
        running_vms.insert("Windows 7 Pro".to_string(), child);
        assert!(running_vms.contains_key("Windows 7 Pro"));

        // Intentar lanzar la misma VM mientras está registrada como activa
        let p = VmProfile::new_default("Windows 7 Pro", None, None);
        let mut status_msg = String::new();
        let mut status_is_err = false;
        launch_vm_external(&p, &mut running_vms, &mut status_msg, &mut status_is_err);
        assert!(status_msg.contains("ya está en ejecución") || status_msg.contains("iniciada"));
    }
}
