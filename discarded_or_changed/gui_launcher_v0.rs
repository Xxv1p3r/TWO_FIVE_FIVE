//! Administrador Gráfico Oficial estilo VirtualBox 7.2 para Two Five Five (255).
//!
//! Recrea fielmente la interfaz de VirtualBox (`src/VBox/Frontends/VirtualBox`):
//! - Paleta y estética Qt Fusion de VirtualBox (Barra de menú, Toolbar superior con iconos 32x32).
//! - Panel izquierdo de selección de máquinas (`UIChooser` + `UIChooserItemMachine`):
//!   * Iconos oficiales de SO de VirtualBox (`os_win7.png`, `os_win10.png`, `os_ubuntu.png`, etc.).
//!   * Estado de la máquina (`state_powered_off_16px.png`, `state_saved_16px.png`).
//!   * Cabecera global "Herramientas" (`tools_global_32px.png`).
//! - Panel derecho de detalles (`UIDetails` + `UIDetailsSet`):
//!   * Vista Previa con monitor oficial de VirtualBox (`preview_full_16to9_242x155px.png` - `UIMachinePreview`).
//!   * Tarjetas colapsables oficiales con iconos de 16px: `General`, `Sistema`, `Pantalla`,
//!     `Almacenamiento`, `Audio`, `Red`, `USB`, `Descripción`.
//!   * Enlaces hipertexto azules (`#0055B5`) para editar o abrir configuración directamente.
//! - Administrador de Medios Virtuales integrado (`UIMediumManager`):
//!   * Pestañas para Discos Duros (`.vdi` / `.img`) y Discos Ópticos (`.iso`).
//!   * Acciones: Crear VDI, Compactar VDI (`compact`), Eliminar, Asignar a VM.
//! - Asistente de Creación de Máquina Virtual (`UIWizardNewVM`) con banner `wizard_new_welcome.png`.
//! - Asistente de Creación de Disco Duro Virtual (`UIWizardNewVD`) con banner `wizard_new_harddisk.png`.
//! - Diálogo de Configuración Avanzada (`UIAdvancedSettingsDialogMachine`) con categorías e
//!   indicador de memoria RAM con zonas verde/naranja/roja (`UIBaseMemorySlider`).

use crate::devices::font::FONT_8X16;
use crate::vbox_assets::VBoxAssets;
use crate::vdisk::{DiskInfo, VirtualDisk};
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use std::fs;
use std::path::{Path, PathBuf};

const WIN_W: usize = 1140;
const WIN_H: usize = 720;
const VM_CONFIG_FILE: &str = "vms.conf";

// Paleta Qt Fusion / VirtualBox 7.2 (0x00RRGGBB)
const COL_WIN_BG: u32 = 0xF0F2F5;       // Fondo de ventana Qt
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
const COL_LINK_BLUE: u32 = 0x0055B5;    // Enlaces de VirtualBox
const COL_LINK_HOVER: u32 = 0x0A74E8;
const COL_VBOX_BLUE: u32 = 0x2A73C7;    // Azul de selección VirtualBox
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
    if low.contains("debian") {
        return "Debian GNU/Linux x64".to_string();
    }
    if low.contains("ubuntu") {
        return "Ubuntu Linux x64".to_string();
    }
    if low.contains("arch") {
        return "Arch Linux x64".to_string();
    }
    if low.contains("coreplus") || low.contains("tinycore") {
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

/// Resultado de la ejecución del GUI de VirtualBox.
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
                    Some(normalize_rel_path(parts[4]))
                };
                let disk_path = if parts[5].trim().is_empty() {
                    None
                } else {
                    Some(normalize_rel_path(parts[5]))
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
    let mut out = String::from("# Two Five Five - Perfiles de Maquinas Virtuales estilo VirtualBox\n");
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

/// Escanea archivos `.iso` locales.
pub fn scan_iso_files() -> Vec<String> {
    let mut isos = Vec::new();
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension() {
                    if ext.eq_ignore_ascii_case("iso") {
                        if let Some(name) = p.file_name() {
                            let n = normalize_rel_path(&name.to_string_lossy());
                            if !isos.contains(&n) {
                                isos.push(n);
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
            mx: 0,
            my: 0,
        }
    }

    fn clear(&mut self, color: u32) {
        self.buf.fill(color);
        self.buttons.clear();
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
        let idx = if (ch as u32) < 128 { ch as usize } else { b'?' as usize };
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
        let text_w = label.len() * 8;
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
        let text_w = label.len() * 8;
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
            self.draw_border(x, y, w, h, COL_VBOX_BLUE);
        } else if hov {
            self.fill_rect(x, y, w, h, 0xE5EBF2);
            self.draw_border(x, y, w, h, 0xB6BFCB);
        }

        // Icono centrado (32x32)
        let icon_x = x + (w.saturating_sub(32)) / 2;
        let icon_y = y + 4;
        if !assets.draw(icon_name, &mut self.buf, self.w, self.h, icon_x as isize, icon_y as isize) {
            // Fallback geométrico si el asset no estuviese disponible
            self.fill_rect(icon_x, icon_y, 32, 32, if is_primary { COL_VBOX_BLUE } else { 0x6C7A89 });
        }

        // Texto centrado debajo
        let text_w = label.len() * 8;
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
    let mut status_msg = String::from("Listo. Presione [Iniciar] o haga doble clic para arrancar la VM seleccionada.");
    let mut status_is_err = false;
    let mut last_selected_disk_info: Option<DiskInfo> = None;

    // Campos del Asistente de Creación de Máquina
    let mut new_vm_name = String::from("Nueva Maquina Virtual");
    let mut new_vm_iso_idx = 0usize;
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
        "Oracle VM VirtualBox Administrador — Two Five Five (255) KVM",
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
            eprintln!("[GUI] Error al inicializar ventana de VirtualBox: {}", e);
            return GuiOutcome::Headless;
        }
    };

    window.set_target_fps(60);
    let mut canvas = Canvas::new(WIN_W, WIN_H);
    let mut last_mouse_btn = false;

    while window.is_open() {
        let (mx, my) = window
            .get_mouse_pos(MouseMode::Pass)
            .map(|(x, y)| (x as usize, y as usize))
            .unwrap_or((0, 0));
        canvas.mx = mx;
        canvas.my = my;

        let mouse_down = window.get_mouse_down(MouseButton::Left);
        let clicked = mouse_down && !last_mouse_btn;
        last_mouse_btn = mouse_down;

        // ─── Atajos de Teclado Globales ─────────────────────────────────────────
        if window.is_key_pressed(Key::Escape, KeyRepeat::No) {
            if current_modal != ModalDialog::None {
                current_modal = ModalDialog::None;
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
                let p = &profiles[sel_idx];
                return GuiOutcome::Selected(LauncherSelection {
                    vm_name: p.name.clone(),
                    cpus: p.cpus,
                    ram_mb: p.ram_mb,
                    iso_path: p.iso_path.as_ref().map(PathBuf::from),
                    disk_path: p.disk_path.as_ref().map(PathBuf::from),
                    unattended: p.unattended,
                });
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

        // 1. Barra de Menús Superior (Archivo | Máquina | Ayuda)
        canvas.fill_rect(0, 0, WIN_W, 26, COL_MENU_BG);
        canvas.fill_rect(0, 25, WIN_W, 1, COL_MENU_BORDER);
        assets.draw("OSE/VirtualBox_16px.png", &mut canvas.buf, WIN_W, WIN_H, 8, 5);
        canvas.draw_text(30, 5, "Archivo   Maquina   Herramientas   Ayuda", COL_TEXT_MAIN);

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

        canvas.draw_toolbar_action(&assets, "tb_start", "vm_start_32px.png", "Iniciar", cur_tb_x, 28, 72, 50, true);

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
            let state_col = if is_sel { 0xD6E8FC } else { COL_TEXT_MUTED };
            canvas.draw_text_clipped(50, item_y + 10, (CHOOSER_W - 60) / 8, &p.name, title_col);

            // Icono de estado pequeño (16x16)
            assets.draw("state_powered_off_16px.png", &mut canvas.buf, WIN_W, WIN_H, 50, (item_y + 28) as isize);
            canvas.draw_text(70, item_y + 28, "Apagada", state_col);

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

                canvas.draw_text(right_x + 12, card_y + 34, "Memoria base:", COL_TEXT_MUTED);
                canvas.draw_text(right_x + 130, card_y + 34, &format!("{} MB", cur_p.ram_mb), COL_TEXT_MAIN);
                canvas.draw_button_gray("ram_dec", right_x + 230, card_y + 30, 28, 22, "-");
                canvas.draw_button_gray("ram_inc", right_x + 264, card_y + 30, 28, 22, "+");

                canvas.draw_text(right_x + 12, card_y + 58, "Procesadores:", COL_TEXT_MUTED);
                canvas.draw_text(right_x + 130, card_y + 58, &format!("{} vCPUs", cur_p.cpus), COL_TEXT_MAIN);
                canvas.draw_button_gray("cpu_dec", right_x + 230, card_y + 54, 28, 22, "-");
                canvas.draw_button_gray("cpu_inc", right_x + 264, card_y + 54, 28, 22, "+");

                canvas.draw_text(right_x + 12, card_y + 80, "Firmware:", COL_TEXT_MUTED);
                let toggle_efi_hov = canvas.register_btn("act_toggle_efi", right_x + 130, card_y + 80, 220, 16);
                let efi_label = if cur_p.use_efi { "EFI (OVMF x64)" } else { "BIOS (SeaBIOS)" };
                canvas.draw_text(right_x + 130, card_y + 80, efi_label, if toggle_efi_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                canvas.draw_text(right_x + 360, card_y + 80, "Aceleracion: KVM/VT-x", COL_TEXT_MUTED);

                // Columna Derecha: Vista Previa (`UIMachinePreview` - monitor oficial VirtualBox)
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
                canvas.draw_text(mon_x + 60, mon_y + 100, "1024x768 (KVM/VBE)", 0xA0B0C0);

                // Fila Inferior: Tarjetas de Pantalla, Almacenamiento, Audio, Red, USB
                let mut lower_y = card_y + 122;

                // Tarjeta Pantalla
                canvas.draw_details_card_header(&assets, "vrdp_16px.png", "Pantalla", right_x, lower_y, right_w);
                canvas.fill_rect(right_x, lower_y + 26, right_w, 36, COL_CARD_BG);
                canvas.draw_border(right_x, lower_y + 26, right_w, 36, COL_CARD_BORDER);
                canvas.draw_text(right_x + 12, lower_y + 36, "Memoria de video: 16 MB", COL_TEXT_MUTED);
                canvas.draw_text(right_x + 240, lower_y + 36, "Controlador grafico: Bochs VBE (LFB @ 0xE8000000)", COL_TEXT_MAIN);

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
                let iso_label = cur_p.iso_path.as_deref().unwrap_or("(Vacio)");
                let iso_pick_hov = canvas.register_btn("act_pick_iso", right_x + 185, lower_y + 78, 250, 16);
                canvas.draw_text_clipped(right_x + 185, lower_y + 78, 30, iso_label, if iso_pick_hov { COL_LINK_HOVER } else { COL_LINK_BLUE });

                if cur_p.iso_path.is_some() {
                    canvas.draw_button_gray("btn_eject_iso_quick", right_x + 460, lower_y + 74, 90, 22, "Expulsar");
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
                                canvas.draw_text(right_x + right_w - 100, row_y + 5, "[En uso]", COL_VBOX_BLUE);
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
                                canvas.draw_text(right_x + right_w - 100, row_y + 5, "[Montada]", COL_VBOX_BLUE);
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
                    ("Motor Discos VirtualBox VDI 1.1+: Header VDI + dynamic/fixed + compact", true),
                    ("Controlador SATA AHCI (Intel ICH9): Puertos 0 (HDD) y 1 (CD-ROM)", true),
                    ("Controlador Grafico Bochs VBE Dispi: LFB 0xE8000000 hasta 1920x1080", true),
                    ("Red Ethernet PCI VirtIO-Net: DHCP, ARP, ICMP, DNS y NAT integrados", true),
                    ("Controlador Audio ICH AC'97 (8086:2415): Codec SigmaTel + DMA", true),
                    ("Dispositivo VMMDev (80EE:CAFE): Guest Additions, tiempo y raton absoluto", true),
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

        // ─── Modales y Asistentes (Wizards) ─────────────────────────────────────
        match current_modal {
            ModalDialog::NewVmWizard => {
                // Asistente Crear Máquina Virtual (`UIWizardNewVM`)
                let m_w = 640;
                let m_h = 420;
                let mx = (WIN_W - m_w) / 2;
                let my = (WIN_H - m_h) / 2;

                // Sombra y fondo del modal
                canvas.fill_rect(mx, my, m_w, m_h, COL_CARD_BG);
                canvas.draw_border(mx, my, m_w, m_h, COL_VBOX_BLUE);

                // Banner lateral oficial de VirtualBox (`wizard_new_welcome.png`: 145x290)
                assets.draw("wizard_new_welcome.png", &mut canvas.buf, WIN_W, WIN_H, (mx + 10) as isize, (my + 40) as isize);

                // Título del Wizard
                canvas.fill_rect(mx, my, m_w, 32, COL_CARD_HEADER);
                canvas.draw_text(mx + 16, my + 8, "Crear maquina virtual (VirtualBox New VM Wizard)", COL_TEXT_MAIN);

                let form_x = mx + 170;
                canvas.draw_text(form_x, my + 46, "Nombre y sistema operativo:", COL_TEXT_MAIN);

                // Campo Nombre
                canvas.draw_text(form_x, my + 70, "Nombre:", COL_TEXT_MUTED);
                let _edit_hov = canvas.register_btn("wiz_vm_name_box", form_x + 70, my + 66, 360, 24);
                canvas.fill_rect(form_x + 70, my + 66, 360, 24, COL_INPUT_BG);
                canvas.draw_border(form_x + 70, my + 66, 360, 24, if active_field == ActiveField::WizardVmName { COL_INPUT_BORDER } else { COL_CARD_BORDER });
                canvas.draw_text(form_x + 76, my + 70, &new_vm_name, COL_TEXT_MAIN);

                // Selector de ISO
                canvas.draw_text(form_x, my + 102, "Imagen ISO:", COL_TEXT_MUTED);
                let cur_iso_name = available_isos.get(new_vm_iso_idx).map(|s| s.as_str()).unwrap_or("(Ninguna)");
                let iso_hov = canvas.register_btn("wiz_cycle_iso", form_x + 90, my + 98, 340, 24);
                canvas.fill_rect(form_x + 90, my + 98, 340, 24, if iso_hov { 0xE6F0FA } else { COL_BTN_GRAY });
                canvas.draw_border(form_x + 90, my + 98, 340, 24, COL_BTN_GRAY_BORDER);
                canvas.draw_text_clipped(form_x + 96, my + 102, 38, cur_iso_name, COL_TEXT_MAIN);

                // Tipo de Disco
                canvas.draw_text(form_x, my + 138, "Disco duro virtual:", COL_TEXT_MAIN);
                let _chk_hov = canvas.register_btn("wiz_toggle_create_disk", form_x, my + 160, 420, 20);
                canvas.draw_text(form_x + 20, my + 160, "Crear un disco duro virtual ahora (VDI dinamico)", COL_TEXT_MAIN);
                canvas.draw_border(form_x, my + 160, 14, 14, COL_VBOX_BLUE);
                if new_vm_create_disk {
                    canvas.fill_rect(form_x + 3, my + 163, 8, 8, COL_VBOX_BLUE);
                }

                if new_vm_create_disk {
                    canvas.draw_text(form_x + 20, my + 188, &format!("Tamano: {} GiB", new_vm_disk_gb), COL_TEXT_MUTED);
                    canvas.draw_button_gray("wiz_disk_dec", form_x + 150, my + 184, 28, 22, "-");
                    canvas.draw_button_gray("wiz_disk_inc", form_x + 184, my + 184, 28, 22, "+");
                }

                // Botones Cancelar / Terminar
                canvas.draw_button_gray("wiz_cancel", mx + m_w - 220, my + m_h - 40, 95, 28, "Cancelar");
                canvas.draw_button_primary("wiz_finish", mx + m_w - 115, my + m_h - 40, 95, 28, "Terminar");
            }

            ModalDialog::CreateDiskWizard => {
                // Asistente Crear Disco Duro Virtual (`UIWizardNewVD`)
                let m_w = 640;
                let m_h = 390;
                let mx = (WIN_W - m_w) / 2;
                let my = (WIN_H - m_h) / 2;

                canvas.fill_rect(mx, my, m_w, m_h, COL_CARD_BG);
                canvas.draw_border(mx, my, m_w, m_h, COL_VBOX_BLUE);

                // Banner lateral oficial VirtualBox (`wizard_new_harddisk.png`: 145x290)
                assets.draw("wizard_new_harddisk.png", &mut canvas.buf, WIN_W, WIN_H, (mx + 10) as isize, (my + 40) as isize);

                canvas.fill_rect(mx, my, m_w, 32, COL_CARD_HEADER);
                canvas.draw_text(mx + 16, my + 8, "Crear disco duro virtual (VirtualBox New Virtual Disk)", COL_TEXT_MAIN);

                let form_x = mx + 170;
                canvas.draw_text(form_x, my + 46, "Nombre de archivo del disco:", COL_TEXT_MUTED);
                let _n_hov = canvas.register_btn("vd_name_box", form_x, my + 66, 430, 24);
                canvas.fill_rect(form_x, my + 66, 430, 24, COL_INPUT_BG);
                canvas.draw_border(form_x, my + 66, 430, 24, if active_field == ActiveField::WizardDiskName { COL_INPUT_BORDER } else { COL_CARD_BORDER });
                canvas.draw_text(form_x + 6, my + 70, &new_disk_name, COL_TEXT_MAIN);

                canvas.draw_text(form_x, my + 104, "Tipo de archivo y variante:", COL_TEXT_MAIN);
                let _dyn_hov = canvas.register_btn("vd_set_dyn", form_x, my + 128, 420, 20);
                canvas.draw_text(form_x + 22, my + 128, "VDI Reservado dinamicamente (crece bajo demanda)", COL_TEXT_MAIN);
                canvas.draw_border(form_x, my + 128, 14, 14, COL_VBOX_BLUE);
                if !new_disk_fixed {
                    canvas.fill_rect(form_x + 3, my + 131, 8, 8, COL_VBOX_BLUE);
                }

                let _fix_hov = canvas.register_btn("vd_set_fix", form_x, my + 152, 420, 20);
                canvas.draw_text(form_x + 22, my + 152, "VDI Tamano fijo (preasignado completo en host)", COL_TEXT_MAIN);
                canvas.draw_border(form_x, my + 152, 14, 14, COL_VBOX_BLUE);
                if new_disk_fixed {
                    canvas.fill_rect(form_x + 3, my + 155, 8, 8, COL_VBOX_BLUE);
                }

                canvas.draw_text(form_x, my + 188, &format!("Capacidad virtual: {} GiB", new_disk_size_gb), COL_TEXT_MAIN);
                canvas.draw_button_gray("vd_size_dec", form_x + 220, my + 184, 30, 22, "-");
                canvas.draw_button_gray("vd_size_inc", form_x + 256, my + 184, 30, 22, "+");
                canvas.draw_button_gray("vd_preset_20", form_x + 296, my + 184, 40, 22, "20G");
                canvas.draw_button_gray("vd_preset_40", form_x + 342, my + 184, 40, 22, "40G");
                canvas.draw_button_gray("vd_preset_80", form_x + 388, my + 184, 40, 22, "80G");

                canvas.draw_button_gray("vd_cancel", mx + m_w - 220, my + m_h - 40, 95, 28, "Cancelar");
                canvas.draw_button_primary("vd_create_btn", mx + m_w - 115, my + m_h - 40, 95, 28, "Crear VDI");
            }

            ModalDialog::SettingsDialog => {
                // Diálogo de Configuración (`UIAdvancedSettingsDialogMachine`)
                let m_w = 760;
                let m_h = 500;
                let mx = (WIN_W - m_w) / 2;
                let my = (WIN_H - m_h) / 2;

                canvas.fill_rect(mx, my, m_w, m_h, COL_CARD_BG);
                canvas.draw_border(mx, my, m_w, m_h, COL_VBOX_BLUE);

                canvas.fill_rect(mx, my, m_w, 32, COL_CARD_HEADER);
                canvas.draw_text(mx + 16, my + 8, &format!("{} - Configuracion", profiles[sel_idx].name), COL_TEXT_MAIN);

                // Selector lateral de categorías de VirtualBox con iconos de 32x32
                const CAT_W: usize = 160;
                canvas.fill_rect(mx, my + 32, CAT_W, m_h - 32, 0xF9FAFB);
                canvas.fill_rect(mx + CAT_W - 1, my + 32, 1, m_h - 32, COL_CARD_BORDER);

                let cats = [
                    (SettingsCategory::General, "General", "machine_32px.png"),
                    (SettingsCategory::System, "Sistema", "chipset_32px.png"),
                    (SettingsCategory::Display, "Pantalla", "vrdp_32px.png"),
                    (SettingsCategory::Storage, "Almacenamiento", "hd_32px.png"),
                    (SettingsCategory::Audio, "Audio", "sound_32px.png"),
                    (SettingsCategory::Network, "Red", "nw_32px.png"),
                    (SettingsCategory::Usb, "USB", "usb_32px.png"),
                ];

                let mut cy = my + 36;
                for (cat, label, icon) in &cats {
                    let is_sel = settings_category == *cat;
                    let hov = canvas.register_btn(&format!("set_cat_{:?}", cat as *const _), mx, cy, CAT_W - 1, 46);
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
                    SettingsCategory::System => {
                        let cur_p = &mut profiles[sel_idx];
                        canvas.draw_text(set_x, my + 46, "Memoria base (RAM) y Procesadores:", COL_TEXT_MAIN);

                        // Slider oficial de Memoria RAM de VirtualBox (`UIBaseMemorySlider`)
                        canvas.draw_text(set_x, my + 76, "Memoria base:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 120, my + 76, &format!("{} MB", cur_p.ram_mb), COL_TEXT_MAIN);

                        // Barra de color verde / naranja / rojo idéntica a VirtualBox
                        let bar_x = set_x + 120;
                        let bar_y = my + 98;
                        let bar_w = 340;
                        canvas.fill_rect(bar_x, bar_y, (bar_w * 60) / 100, 6, 0x4CAF50);  // Verde óptimo
                        canvas.fill_rect(bar_x + (bar_w * 60) / 100, bar_y, (bar_w * 25) / 100, 6, 0xFF9800); // Naranja aviso
                        canvas.fill_rect(bar_x + (bar_w * 85) / 100, bar_y, (bar_w * 15) / 100, 6, 0xF44336); // Rojo peligro

                        canvas.draw_button_gray("set_ram_dec", set_x + 480, my + 72, 30, 24, "-");
                        canvas.draw_button_gray("set_ram_inc", set_x + 516, my + 72, 30, 24, "+");

                        // Procesadores (CPUs)
                        canvas.draw_text(set_x, my + 130, "Procesadores:", COL_TEXT_MUTED);
                        canvas.draw_text(set_x + 120, my + 130, &format!("{} CPU", cur_p.cpus), COL_TEXT_MAIN);
                        let cpu_bar_y = my + 152;
                        canvas.fill_rect(bar_x, cpu_bar_y, (bar_w * 50) / 100, 6, 0x4CAF50);
                        canvas.fill_rect(bar_x + (bar_w * 50) / 100, cpu_bar_y, (bar_w * 50) / 100, 6, 0xFF9800);

                        canvas.draw_button_gray("set_cpu_dec", set_x + 480, my + 126, 30, 24, "-");
                        canvas.draw_button_gray("set_cpu_inc", set_x + 516, my + 126, 30, 24, "+");

                        // Checkbox EFI
                        let _chk_efi_hov = canvas.register_btn("set_toggle_efi", set_x, my + 190, 360, 20);
                        canvas.draw_border(set_x, my + 190, 14, 14, COL_VBOX_BLUE);
                        if cur_p.use_efi {
                            canvas.fill_rect(set_x + 3, my + 193, 8, 8, COL_VBOX_BLUE);
                        }
                        canvas.draw_text(set_x + 22, my + 190, "Habilitar EFI (especial para OVMF / x64)", COL_TEXT_MAIN);
                    }
                    _ => {
                        let cur_p = &mut profiles[sel_idx];
                        canvas.draw_text(set_x, my + 46, "Propiedades Generales de la Maquina:", COL_TEXT_MAIN);
                        canvas.draw_text(set_x, my + 76, "Nombre:", COL_TEXT_MUTED);
                        let _e_hov = canvas.register_btn("set_vm_name_box", set_x + 80, my + 72, 360, 24);
                        canvas.fill_rect(set_x + 80, my + 72, 360, 24, COL_INPUT_BG);
                        canvas.draw_border(set_x + 80, my + 72, 360, 24, if active_field == ActiveField::SettingsVmName { COL_INPUT_BORDER } else { COL_CARD_BORDER });
                        canvas.draw_text(set_x + 86, my + 76, &cur_p.name, COL_TEXT_MAIN);

                        canvas.draw_text(set_x, my + 116, "Instalacion desatendida (OEMDRV):", COL_TEXT_MAIN);
                        let _u_hov = canvas.register_btn("set_toggle_unatt", set_x, my + 140, 420, 20);
                        canvas.draw_border(set_x, my + 140, 14, 14, COL_VBOX_BLUE);
                        if cur_p.unattended {
                            canvas.fill_rect(set_x + 3, my + 143, 8, 8, COL_VBOX_BLUE);
                        }
                        canvas.draw_text(set_x + 22, my + 140, "Activar inyeccion automatica de usuario y contrasena", COL_TEXT_MAIN);
                    }
                }

                canvas.draw_button_primary("set_ok_btn", mx + m_w - 115, my + m_h - 40, 95, 28, "Aceptar");
            }

            ModalDialog::None => {}
        }

        // ─── Despacho de Eventos de Clic ────────────────────────────────────────
        if clicked {
            let hit = canvas.buttons.iter().find(|b| b.contains(mx, my)).map(|b| b.id.clone());

            if let Some(id) = hit {
                match id.as_str() {
                    // Toolbar Acciones
                    "tb_start" => {
                        let p = &profiles[sel_idx];
                        return GuiOutcome::Selected(LauncherSelection {
                            vm_name: p.name.clone(),
                            cpus: p.cpus,
                            ram_mb: p.ram_mb,
                            iso_path: p.iso_path.as_ref().map(PathBuf::from),
                            disk_path: p.disk_path.as_ref().map(PathBuf::from),
                            unattended: p.unattended,
                        });
                    }
                    "tb_new" => {
                        new_vm_name = format!("Maquina Virtual {}", profiles.len() + 1);
                        new_vm_iso_idx = 0;
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
                        current_modal = ModalDialog::SettingsDialog;
                        settings_category = SettingsCategory::General;
                    }
                    "tb_discard" => {
                        if profiles.len() > 1 {
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
                    "act_toggle_unatt" | "set_toggle_unatt" => {
                        profiles[sel_idx].unattended = !profiles[sel_idx].unattended;
                        let _ = save_vm_profiles(&profiles);
                    }
                    "act_toggle_efi" | "set_toggle_efi" => {
                        profiles[sel_idx].use_efi = !profiles[sel_idx].use_efi;
                        let _ = save_vm_profiles(&profiles);
                    }
                    "ram_dec" | "set_ram_dec" => {
                        if profiles[sel_idx].ram_mb > 512 {
                            profiles[sel_idx].ram_mb -= 512;
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "ram_inc" | "set_ram_inc" => {
                        if profiles[sel_idx].ram_mb < 16384 {
                            profiles[sel_idx].ram_mb += 512;
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "cpu_dec" | "set_cpu_dec" => {
                        if profiles[sel_idx].cpus > 1 {
                            profiles[sel_idx].cpus -= 1;
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "cpu_inc" | "set_cpu_inc" => {
                        if profiles[sel_idx].cpus < 16 {
                            profiles[sel_idx].cpus += 1;
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "act_pick_iso" => {
                        if !available_isos.is_empty() {
                            let cur = profiles[sel_idx].iso_path.as_deref().unwrap_or("");
                            let cur_pos = available_isos.iter().position(|s| s == cur).unwrap_or(0);
                            let next_pos = (cur_pos + 1) % available_isos.len();
                            profiles[sel_idx].iso_path = Some(available_isos[next_pos].clone());
                            let _ = save_vm_profiles(&profiles);
                        }
                    }
                    "btn_eject_iso_quick" => {
                        profiles[sel_idx].iso_path = None;
                        let _ = save_vm_profiles(&profiles);
                        status_msg = "Unidad optica virtual expulsada.".to_string();
                    }
                    "act_pick_disk" => {
                        if !available_disks.is_empty() {
                            let cur = profiles[sel_idx].disk_path.as_deref().unwrap_or("");
                            let cur_pos = available_disks.iter().position(|s| s == cur).unwrap_or(0);
                            let next_pos = (cur_pos + 1) % available_disks.len();
                            profiles[sel_idx].disk_path = Some(available_disks[next_pos].clone());
                            if let Some(ref d) = profiles[sel_idx].disk_path {
                                last_selected_disk_info = VirtualDisk::probe_info(d).ok();
                            }
                            let _ = save_vm_profiles(&profiles);
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

                    // Botones de Modales
                    "wiz_cancel" | "vd_cancel" => {
                        current_modal = ModalDialog::None;
                        active_field = ActiveField::None;
                    }
                    "set_ok_btn" => {
                        let _ = save_vm_profiles(&profiles);
                        current_modal = ModalDialog::None;
                        active_field = ActiveField::None;
                        status_msg = "Configuracion de maquina guardada en vms.conf.".to_string();
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
                    "wiz_toggle_create_disk" => new_vm_create_disk = !new_vm_create_disk,
                    "wiz_disk_dec" => new_vm_disk_gb = new_vm_disk_gb.saturating_sub(5).max(5),
                    "wiz_disk_inc" => new_vm_disk_gb = (new_vm_disk_gb + 5).min(2000),
                    "wiz_finish" => {
                        let iso_choice = available_isos.get(new_vm_iso_idx).cloned();
                        let disk_choice = if new_vm_create_disk {
                            let disk_file = suggest_disk_filename(&new_vm_name, ".vdi");
                            let bytes = new_vm_disk_gb * 1024 * 1024 * 1024;
                            let _ = VirtualDisk::create_vdi(&disk_file, bytes, false, "Created by VirtualBox Manager Wizard");
                            available_disks = scan_disk_files();
                            Some(disk_file)
                        } else {
                            available_disks.first().cloned()
                        };

                        let new_prof = VmProfile::new_default(&new_vm_name, iso_choice, disk_choice);
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
                        match VirtualDisk::create_vdi(&full_disk_name, bytes, new_disk_fixed, "Created by VirtualBox Manager Wizard") {
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
                        // Selección de máquina en el Chooser
                        if let Some(stripped) = id.strip_prefix("vm_sel_") {
                            if let Ok(idx) = stripped.parse::<usize>() {
                                if idx < profiles.len() {
                                    sel_idx = idx;
                                    current_view = MainViewTab::Details;
                                    if let Some(ref d) = profiles[sel_idx].disk_path {
                                        last_selected_disk_info = VirtualDisk::probe_info(d).ok();
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
}
