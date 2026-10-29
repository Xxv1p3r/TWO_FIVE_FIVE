//! Administrador de la ventana gráfica en el Host (Frontend GUI con minifb).
//!
//! Soporta:
//!   - Modo texto VGA clásico (80x25 @ 0xB8000 con fuente 8x16) vía GuestMemory.
//!   - (tarea 2) Modos gráficos VGA estándar: 13h (packed 320x200x8),
//!     12h/10h/0Eh (planar 4bpp con even/odd host) y CGA 4/5 (planar
//!     intercalado), decodificados desde seq_regs/grc_regs/attr_regs +
//!     DAC palette.
//!   - Modos gráficos VBE lineales (resoluciones dinámicas hasta 1920x1080),
//!     con (tarea 15) VIRT_WIDTH para double-buffering, X_OFFSET/Y_OFFSET
//!     (panning) y bpp 4/8/15/16/24/32.
//!   - (tarea 15) La ventana escala el framebuffer al tamaño actual
//!     (nearest-neighbor) al hacer resize.
//!   - Captura de ratón PS/2 (tarea 9) con deltas y botones.
//!   - (tarea 19) Entrada de teclado host con distribución ESPAÑOLA (CharInput,
//!     dead keys, AltGr, CapsLock XOR Shift).
//!   - (tarea 17) Cierre limpio de la VM al cerrar la ventana o pulsar Escape.

use crate::devices::font::{FONT_8X16, VGA_PALETTE};
use crate::devices::vga::{
    VBE_DISPI_INDEX_ENABLE, VBE_DISPI_INDEX_VIRT_WIDTH, VBE_DISPI_INDEX_X_OFFSET,
    VBE_DISPI_INDEX_Y_OFFSET, VBE_DISPI_8BIT_DAC, VgaState,
};
use crate::guest_mem::GuestMemory;
use minifb::{Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// (tarea 2) Tope de seguridad para offsets planares: el estándar VGA
/// direcciona 256 KiB de VRAM (4 planos × 64 KiB).
const VGA_ADDRABLE: usize = 256 * 1024;

/// Altura en píxeles de la barra de herramientas superior estilo VirtualBox.
pub const TOP_BAR_H: usize = 24;

/// Cadencia de re-afirmación del puntero absoluto. El guest solo recibe la
/// posición cuando cambia, de modo que cualquier deriva por su parte (reporte
/// perdido, re-mapeo al cambiar de modo/resolución, aceleración residual)
/// quedaba permanente. Reenviarla cada 100 ms hace que el cursor converja
/// siempre a la posición del ratón físico, como en VirtualBox.
const TABLET_RESYNC: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuAction {
    Separator,
    Preferences,
    ResetSettings,
    Exit,
    TogglePause,
    ResetVm,
    AcpiShutdown,
    PowerOff,
    TakeSnapshot,
    Screenshot,
    ToggleHud,
    FitWindow,
    ToggleAspect,
    ToggleAutoResize,
    SetRes800x600,
    SetRes1024x768,
    SetRes1280x720,
    SetRes1280x800,
    SetRes1280x1024,
    SetRes1440x900,
    SetRes1600x900,
    SetRes1920x1080,
    SetRes1920x1200,
    SendCtrlAltDel,
    SendCtrlAltBksp,
    ToggleMouseIntegration,
    EjectIso,
    AudioInfo,
    NetworkInfo,
    About,
    ShortcutsHelp,
}

pub struct MenuItemDef {
    pub label: &'static str,
    pub action: MenuAction,
}

pub struct MenuHeaderDef {
    pub title: &'static str,
    pub items: &'static [MenuItemDef],
}

pub static MENU_DEFINITIONS: &[MenuHeaderDef] = &[
    MenuHeaderDef {
        title: "Archivo",
        items: &[
            MenuItemDef { label: "Preferencias...        (Host+G)", action: MenuAction::Preferences },
            MenuItemDef { label: "Restaurar por defecto", action: MenuAction::ResetSettings },
            MenuItemDef { label: "---", action: MenuAction::Separator },
            MenuItemDef { label: "Cerrar / Salir         (Host+Q)", action: MenuAction::Exit },
        ],
    },
    MenuHeaderDef {
        title: "Máquina",
        items: &[
            MenuItemDef { label: "Pausar / Reanudar      (Host+P)", action: MenuAction::TogglePause },
            MenuItemDef { label: "Reiniciar VM           (Host+R)", action: MenuAction::ResetVm },
            MenuItemDef { label: "Apagado ACPI           (Host+H)", action: MenuAction::AcpiShutdown },
            MenuItemDef { label: "Apagar VM (Forzar)     (Host+Q)", action: MenuAction::PowerOff },
            MenuItemDef { label: "---", action: MenuAction::Separator },
            MenuItemDef { label: "Tomar instantánea...   (Host+T)", action: MenuAction::TakeSnapshot },
            MenuItemDef { label: "Capturar pantalla BMP  (Host+E)", action: MenuAction::Screenshot },
            MenuItemDef { label: "Información de sesión  (Host+F1)", action: MenuAction::ToggleHud },
        ],
    },
    MenuHeaderDef {
        title: "Ver",
        items: &[
            MenuItemDef { label: "Auto-redimensionar pantalla (Host+A)", action: MenuAction::ToggleAutoResize },
            MenuItemDef { label: "Preservar aspecto / Estirar (Host+C)", action: MenuAction::ToggleAspect },
            MenuItemDef { label: "Ajustar ventana a resolución nativa", action: MenuAction::FitWindow },
            MenuItemDef { label: "---", action: MenuAction::Separator },
            MenuItemDef { label: "Resolución:  800 x 600   (4:3 SVGA)", action: MenuAction::SetRes800x600 },
            MenuItemDef { label: "Resolución: 1024 x 768   (4:3 XGA)", action: MenuAction::SetRes1024x768 },
            MenuItemDef { label: "Resolución: 1280 x 720   (16:9 HD)", action: MenuAction::SetRes1280x720 },
            MenuItemDef { label: "Resolución: 1280 x 800   (16:10 WXGA)", action: MenuAction::SetRes1280x800 },
            MenuItemDef { label: "Resolución: 1280 x 1024  (5:4 SXGA)", action: MenuAction::SetRes1280x1024 },
            MenuItemDef { label: "Resolución: 1440 x 900   (16:10 WSXGA)", action: MenuAction::SetRes1440x900 },
            MenuItemDef { label: "Resolución: 1600 x 900   (16:9 HD+)", action: MenuAction::SetRes1600x900 },
            MenuItemDef { label: "Resolución: 1920 x 1080  (16:9 FHD)", action: MenuAction::SetRes1920x1080 },
            MenuItemDef { label: "Resolución: 1920 x 1200  (16:10 WUXGA)", action: MenuAction::SetRes1920x1200 },
            MenuItemDef { label: "---", action: MenuAction::Separator },
            MenuItemDef { label: "Panel OSD / HUD        (Host+F1)", action: MenuAction::ToggleHud },
        ],
    },
    MenuHeaderDef {
        title: "Entrada",
        items: &[
            MenuItemDef { label: "Insertar Ctrl+Alt+Del  (Host+Del)", action: MenuAction::SendCtrlAltDel },
            MenuItemDef { label: "Insertar Ctrl+Alt+Bksp", action: MenuAction::SendCtrlAltBksp },
            MenuItemDef { label: "---", action: MenuAction::Separator },
            MenuItemDef { label: "Integración del ratón  (Host+I)", action: MenuAction::ToggleMouseIntegration },
        ],
    },
    MenuHeaderDef {
        title: "Dispositivos",
        items: &[
            MenuItemDef { label: "Unidades ópticas: Expulsar ISO", action: MenuAction::EjectIso },
            MenuItemDef { label: "Audio: AC'97 Salida Host (Activo)", action: MenuAction::AudioInfo },
            MenuItemDef { label: "Red: Intel PRO/1000 MT (Conectado)", action: MenuAction::NetworkInfo },
        ],
    },
    MenuHeaderDef {
        title: "Ayuda",
        items: &[
            MenuItemDef { label: "Acerca de Two Five Five...", action: MenuAction::About },
            MenuItemDef { label: "Atajos de Host (Tecla: Ctrl Der)", action: MenuAction::ShortcutsHelp },
        ],
    },
];

fn draw_char_8x16(buf: &mut [u32], buf_w: usize, buf_h: usize, x: usize, y: usize, ch: u8, fg: u32, bg: Option<u32>) {
    let glyph = &FONT_8X16[(ch as usize) * 16..(ch as usize + 1) * 16];
    for dy in 0..16 {
        let py = y + dy;
        if py >= buf_h { break; }
        let row = glyph[dy];
        for dx in 0..8 {
            let px = x + dx;
            if px >= buf_w { break; }
            if (row >> (7 - dx)) & 1 != 0 {
                buf[py * buf_w + px] = fg;
            } else if let Some(bg_col) = bg {
                buf[py * buf_w + px] = bg_col;
            }
        }
    }
}

fn draw_string_8x16(buf: &mut [u32], buf_w: usize, buf_h: usize, x: usize, y: usize, s: &str, fg: u32, bg: Option<u32>) {
    let mut cx = x;
    for b in s.bytes() {
        draw_char_8x16(buf, buf_w, buf_h, cx, y, b, fg, bg);
        cx += 8;
    }
}

fn fill_rect(buf: &mut [u32], buf_w: usize, buf_h: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    let x_end = (x + w).min(buf_w);
    let y_end = (y + h).min(buf_h);
    for py in y..y_end {
        let row_start = py * buf_w;
        for px in x..x_end {
            buf[row_start + px] = color;
        }
    }
}

fn draw_rect_outline(buf: &mut [u32], buf_w: usize, buf_h: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    if w == 0 || h == 0 || x >= buf_w || y >= buf_h { return; }
    let x_end = (x + w).min(buf_w);
    let y_end = (y + h).min(buf_h);
    for px in x..x_end {
        buf[y * buf_w + px] = color;
        if y_end > y + 1 {
            buf[(y_end - 1) * buf_w + px] = color;
        }
    }
    for py in y..y_end {
        buf[py * buf_w + x] = color;
        if x_end > x + 1 {
            buf[py * buf_w + (x_end - 1)] = color;
        }
    }
}

fn get_menu_header_rects() -> Vec<(usize, usize)> {
    let mut rects = Vec::with_capacity(MENU_DEFINITIONS.len());
    let mut cur_x = 8usize;
    for def in MENU_DEFINITIONS {
        let w = def.title.len() * 8 + 16;
        rects.push((cur_x, w));
        cur_x += w + 2;
    }
    rects
}

fn render_top_bar(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    active_menu: Option<usize>,
    mouse_integration: bool,
    mouse_captured: bool,
    is_paused: bool,
    mx: usize,
    my: usize,
) {
    if wh < TOP_BAR_H { return; }
    fill_rect(buf, ww, wh, 0, 0, ww, TOP_BAR_H - 1, 0x22262B);
    fill_rect(buf, ww, wh, 0, TOP_BAR_H - 1, ww, 1, 0x383E48);

    let header_rects = get_menu_header_rects();
    for (i, def) in MENU_DEFINITIONS.iter().enumerate() {
        let (hx, hw) = header_rects[i];
        let is_active = active_menu == Some(i);
        let is_hover = my < TOP_BAR_H && mx >= hx && mx < hx + hw;

        if is_active {
            fill_rect(buf, ww, wh, hx, 0, hw, TOP_BAR_H - 1, 0x3E4654);
            draw_string_8x16(buf, ww, wh, hx + 8, 4, def.title, 0xFFFFFF, None);
        } else if is_hover {
            fill_rect(buf, ww, wh, hx, 1, hw, TOP_BAR_H - 2, 0x2D333D);
            draw_string_8x16(buf, ww, wh, hx + 8, 4, def.title, 0xF0F0F0, None);
        } else {
            draw_string_8x16(buf, ww, wh, hx + 8, 4, def.title, 0xCCD1D9, None);
        }
    }

    let mut right_cursor = ww.saturating_sub(10);

    let host_badge = "Host: Ctrl Der";
    let host_w = host_badge.len() * 8 + 12;
    if right_cursor > host_w {
        right_cursor -= host_w;
        let bx = right_cursor;
        fill_rect(buf, ww, wh, bx, 2, host_w, TOP_BAR_H - 4, 0x2C313A);
        draw_rect_outline(buf, ww, wh, bx, 2, host_w, TOP_BAR_H - 4, 0x4B5263);
        draw_string_8x16(buf, ww, wh, bx + 6, 4, host_badge, 0xABB2BF, None);
        right_cursor = right_cursor.saturating_sub(6);
    }

    let (mouse_badge, bg_col, border_col, text_col) = if mouse_integration {
        ("Ratón: Integrado", 0x1B382B, 0x2E6B4A, 0x52D68C)
    } else if mouse_captured {
        ("Ratón: CAPTURADO (Host=Liberar)", 0x4A2020, 0x8C3B3B, 0xFF6B6B)
    } else {
        ("Ratón: PS/2 (Clic captura)", 0x3A3525, 0x6E623E, 0xF7CC66)
    };
    let mouse_w = mouse_badge.len() * 8 + 12;
    if right_cursor > mouse_w {
        right_cursor -= mouse_w;
        let bx = right_cursor;
        fill_rect(buf, ww, wh, bx, 2, mouse_w, TOP_BAR_H - 4, bg_col);
        draw_rect_outline(buf, ww, wh, bx, 2, mouse_w, TOP_BAR_H - 4, border_col);
        draw_string_8x16(buf, ww, wh, bx + 6, 4, mouse_badge, text_col, None);
        right_cursor = right_cursor.saturating_sub(6);
    }

    if is_paused {
        let pause_badge = "PAUSADO";
        let pause_w = pause_badge.len() * 8 + 12;
        if right_cursor > pause_w {
            right_cursor -= pause_w;
            let bx = right_cursor;
            fill_rect(buf, ww, wh, bx, 2, pause_w, TOP_BAR_H - 4, 0x5A4215);
            draw_rect_outline(buf, ww, wh, bx, 2, pause_w, TOP_BAR_H - 4, 0xD4A030);
            draw_string_8x16(buf, ww, wh, bx + 6, 4, pause_badge, 0xFFD166, None);
        }
    }
}

fn get_dropdown_geometry(menu_idx: usize, ww: usize) -> (usize, usize, usize, usize) {
    let header_rects = get_menu_header_rects();
    let (hx, _hw) = header_rects[menu_idx];
    let def = &MENU_DEFINITIONS[menu_idx];
    let max_len = def.items.iter().map(|it| it.label.len()).max().unwrap_or(20);
    let menu_w = (max_len * 8 + 24).max(200);
    let menu_x = if hx + menu_w > ww { ww.saturating_sub(menu_w + 4) } else { hx };
    let menu_y = TOP_BAR_H;

    let mut total_h = 4;
    for it in def.items {
        if it.action == MenuAction::Separator {
            total_h += 8;
        } else {
            total_h += 22;
        }
    }
    total_h += 4;
    (menu_x, menu_y, menu_w, total_h)
}

fn render_dropdown_menu(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    menu_idx: usize,
    mx: usize,
    my: usize,
    mouse_integration: bool,
    auto_resize: bool,
    preserve_aspect: bool,
    cur_w: usize,
    cur_h: usize,
) {
    let (x, y, w, h) = get_dropdown_geometry(menu_idx, ww);
    let h = h.min(wh.saturating_sub(y + 4));
    if h == 0 { return; }

    fill_rect(buf, ww, wh, x + 3, y + 3, w, h, 0x111316);
    fill_rect(buf, ww, wh, x, y, w, h, 0x1E2227);
    draw_rect_outline(buf, ww, wh, x, y, w, h, 0x4A5260);

    let def = &MENU_DEFINITIONS[menu_idx];
    let mut cur_y = y + 4;
    for it in def.items {
        if cur_y + 16 > y + h {
            break;
        }
        if it.action == MenuAction::Separator {
            fill_rect(buf, ww, wh, x + 8, cur_y + 3, w.saturating_sub(16), 1, 0x3E4450);
            cur_y += 8;
        } else {
            let item_h = 22;
            let is_hover = mx >= x && mx < x + w && my >= cur_y && my < cur_y + item_h;
            if is_hover {
                fill_rect(buf, ww, wh, x + 2, cur_y, w - 4, item_h, 0x3A4454);
            }
            let dynamic_label = match it.action {
                MenuAction::ToggleMouseIntegration => {
                    if mouse_integration { "[v] Integración del ratón (Host+I)" } else { "[ ] Integración del ratón (Host+I)" }
                }
                MenuAction::ToggleAutoResize => {
                    if auto_resize { "[v] Auto-redimensionar pantalla (Host+A)" } else { "[ ] Auto-redimensionar pantalla (Host+A)" }
                }
                MenuAction::ToggleAspect => {
                    if preserve_aspect { "[v] Preservar relación de aspecto (Host+C)" } else { "[ ] Preservar relación de aspecto (Host+C)" }
                }
                MenuAction::SetRes800x600 => {
                    if cur_w == 800 && cur_h == 600 { "(*) Resolución:  800 x 600   (4:3 SVGA)" } else { "( ) Resolución:  800 x 600   (4:3 SVGA)" }
                }
                MenuAction::SetRes1024x768 => {
                    if cur_w == 1024 && cur_h == 768 { "(*) Resolución: 1024 x 768   (4:3 XGA)" } else { "( ) Resolución: 1024 x 768   (4:3 XGA)" }
                }
                MenuAction::SetRes1280x720 => {
                    if cur_w == 1280 && cur_h == 720 { "(*) Resolución: 1280 x 720   (16:9 HD)" } else { "( ) Resolución: 1280 x 720   (16:9 HD)" }
                }
                MenuAction::SetRes1280x800 => {
                    if cur_w == 1280 && cur_h == 800 { "(*) Resolución: 1280 x 800   (16:10 WXGA)" } else { "( ) Resolución: 1280 x 800   (16:10 WXGA)" }
                }
                MenuAction::SetRes1280x1024 => {
                    if cur_w == 1280 && cur_h == 1024 { "(*) Resolución: 1280 x 1024  (5:4 SXGA)" } else { "( ) Resolución: 1280 x 1024  (5:4 SXGA)" }
                }
                MenuAction::SetRes1440x900 => {
                    if cur_w == 1440 && cur_h == 900 { "(*) Resolución: 1440 x 900   (16:10 WSXGA)" } else { "( ) Resolución: 1440 x 900   (16:10 WSXGA)" }
                }
                MenuAction::SetRes1600x900 => {
                    if cur_w == 1600 && cur_h == 900 { "(*) Resolución: 1600 x 900   (16:9 HD+)" } else { "( ) Resolución: 1600 x 900   (16:9 HD+)" }
                }
                MenuAction::SetRes1920x1080 => {
                    if cur_w == 1920 && cur_h == 1080 { "(*) Resolución: 1920 x 1080  (16:9 FHD)" } else { "( ) Resolución: 1920 x 1080  (16:9 FHD)" }
                }
                MenuAction::SetRes1920x1200 => {
                    if cur_w == 1920 && cur_h == 1200 { "(*) Resolución: 1920 x 1200  (16:10 WUXGA)" } else { "( ) Resolución: 1920 x 1200  (16:10 WUXGA)" }
                }
                _ => it.label,
            };
            let text_color = if is_hover { 0xFFFFFF } else { 0xD2D6DE };
            draw_string_8x16(buf, ww, wh, x + 10, cur_y + 3, dynamic_label, text_color, None);
            cur_y += item_h;
        }
    }
}

fn find_dropdown_action_at(menu_idx: usize, ww: usize, mx: usize, my: usize) -> Option<MenuAction> {
    let (x, y, w, h) = get_dropdown_geometry(menu_idx, ww);
    if mx < x || mx >= x + w || my < y || my >= y + h {
        return None;
    }
    let def = &MENU_DEFINITIONS[menu_idx];
    let mut cur_y = y + 4;
    for it in def.items {
        if it.action == MenuAction::Separator {
            cur_y += 8;
        } else {
            let item_h = 22;
            if my >= cur_y && my < cur_y + item_h {
                return Some(it.action);
            }
            cur_y += item_h;
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingKind {
    Hud,
    CloseVm,
    Preferences,
    About,
    ShortcutsHelp,
}

/// Estado y geometría de una ventana flotante interactiva en el display de la VM
#[derive(Debug, Clone)]
pub struct RuntimeFloatingWindow {
    pub kind: FloatingKind,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub is_dragging: bool,
    pub drag_off_x: usize,
    pub drag_off_y: usize,
}

impl RuntimeFloatingWindow {
    pub fn new_centered(kind: FloatingKind, w: usize, h: usize, screen_w: usize, screen_h: usize) -> Self {
        let x = (screen_w.saturating_sub(w)) / 2;
        let y = TOP_BAR_H + (screen_h.saturating_sub(TOP_BAR_H).saturating_sub(h)) / 2;
        Self {
            kind,
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
        (self.x, self.y, self.w, 28)
    }

    pub fn close_btn_rect(&self) -> (usize, usize, usize, usize) {
        (self.x + self.w.saturating_sub(30), self.y + 3, 24, 22)
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

pub fn toggle_or_open_floating_window(
    windows: &mut Vec<RuntimeFloatingWindow>,
    kind: FloatingKind,
    default_w: usize,
    default_h: usize,
    sw: usize,
    sh: usize,
) {
    if let Some(pos) = windows.iter().position(|w| w.kind == kind) {
        if kind == FloatingKind::Hud {
            // El HUD OSD se alterna con cada pulsación
            windows.remove(pos);
        } else {
            // Los diálogos modales se traen al frente si ya existen
            let item = windows.remove(pos);
            windows.push(item);
        }
    } else {
        windows.push(RuntimeFloatingWindow::new_centered(kind, default_w, default_h, sw, sh));
    }
}

fn draw_window_shadow(buf: &mut [u32], ww: usize, wh: usize, x: usize, y: usize, w: usize, h: usize) {
    let shadow_radius = 6usize;
    let x_end = (x + w + shadow_radius).min(ww);
    let y_end = (y + h + shadow_radius).min(wh);
    let base_alpha = 90u32;

    for py in y..y_end {
        let row = py * ww;
        for px in x..x_end {
            let in_win = px >= x && px < x + w && py >= y && py < y + h;
            if in_win {
                continue;
            }
            let dx = if px < x { x - px } else if px >= x + w { px - (x + w) + 1 } else { 0 };
            let dy = if py < y { y - py } else if py >= y + h { py - (y + h) + 1 } else { 0 };
            let dist = dx.max(dy);
            if dist <= shadow_radius {
                let factor = (shadow_radius + 1 - dist) as u32;
                let alpha = (base_alpha * factor) / (shadow_radius as u32 + 1);
                let orig = buf[row + px];
                let r = ((orig >> 16) & 0xFF) * (255 - alpha) / 255;
                let g = ((orig >> 8) & 0xFF) * (255 - alpha) / 255;
                let b = (orig & 0xFF) * (255 - alpha) / 255;
                buf[row + px] = (r << 16) | (g << 8) | b;
            }
        }
    }
}

fn draw_floating_window_frame(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    fw: &RuntimeFloatingWindow,
    title: &str,
    mx: usize,
    my: usize,
) -> bool {
    // 1. Sombra exterior
    draw_window_shadow(buf, ww, wh, fw.x, fw.y, fw.w, fw.h);

    // 2. Fondo y borde de la ventana flotante
    fill_rect(buf, ww, wh, fw.x, fw.y, fw.w, fw.h, 0x21252B);
    draw_rect_outline(buf, ww, wh, fw.x, fw.y, fw.w, fw.h, 0x4B5263);
    if fw.w > 2 && fw.h > 2 {
        draw_rect_outline(buf, ww, wh, fw.x + 1, fw.y + 1, fw.w - 2, fw.h - 2, 0x1E4268);
    }

    // 3. Barra de título (Estilo Two Five Five 255)
    fill_rect(buf, ww, wh, fw.x, fw.y, fw.w, 28, 0x1E4268);
    fill_rect(buf, ww, wh, fw.x, fw.y + 27, fw.w, 1, 0x61AFEF);

    // Texto de título
    let title_max_chars = fw.w.saturating_sub(44) / 8;
    let clipped_title = if title.chars().count() > title_max_chars && title_max_chars > 3 {
        let mut s: String = title.chars().take(title_max_chars - 3).collect();
        s.push_str("...");
        s
    } else {
        title.chars().take(title_max_chars).collect()
    };
    draw_string_8x16(buf, ww, wh, fw.x + 10, fw.y + 6, &clipped_title, 0xFFFFFF, None);

    // Botón de cierre [X]
    let (cx, cy, cw, ch) = fw.close_btn_rect();
    let is_close_hov = mx >= cx && mx < cx + cw && my >= cy && my < cy + ch;
    let bg_close = if is_close_hov { 0xE06C75 } else { 0x2C313A };
    fill_rect(buf, ww, wh, cx, cy, cw, ch, bg_close);
    draw_rect_outline(buf, ww, wh, cx, cy, cw, ch, if is_close_hov { 0xBE5046 } else { 0x5C6370 });
    draw_string_8x16(buf, ww, wh, cx + 8, cy + 3, "x", if is_close_hov { 0xFFFFFF } else { 0xABB2BF }, None);

    is_close_hov
}

fn render_floating_hud(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    fw: &RuntimeFloatingWindow,
    metrics: &crate::metrics::VmmMetrics,
    lw: usize,
    lh: usize,
    bpp: usize,
    is_vbe: bool,
    mouse_integration: bool,
    mouse_captured: bool,
    preserve_aspect: bool,
    mx: usize,
    my: usize,
) {
    draw_floating_window_frame(buf, ww, wh, fw, "Two Five Five (255) — Diagnostico de VM (HUD)", mx, my);

    let fps = metrics.display_fps.load(Ordering::Relaxed);
    let scale = metrics.display_scale.load(Ordering::Relaxed);
    let cpus_cur = metrics.num_cpus.load(Ordering::Relaxed);
    let cpus_max = metrics.max_cpus.load(Ordering::Relaxed);
    let ram_mb = metrics.ram_bytes.load(Ordering::Relaxed) / (1024 * 1024);

    let mode_str = if is_vbe { "VBE Linear Framebuffer" } else { "VGA Standard / Texto" };
    let mouse_str = if mouse_integration { "Integrado (Tablet Absoluto)" } else if mouse_captured { "Capturado (PS/2 Relativo)" } else { "Libre" };
    let aspect_str = if preserve_aspect { "Preservar Relacion (Letterbox)" } else { "Estirar (Stretch)" };

    let lines = [
        format!("Modo Grafico:   {} ({}x{} @ {}bpp)", mode_str, lw, lh, bpp),
        format!("Ventana / FPS:  {}x{} | {} FPS | Escala: {}x", ww, wh, fps, scale),
        format!("Hardware Guest: {}/{} vCPUs | {} MB RAM", cpus_cur, cpus_max, ram_mb),
        format!("Dispositivos:   Intel 82540EM (e1000) Link UP | AC'97 ALC655"),
        format!("Puntero Raton:  {}", mouse_str),
        format!("Visualizacion:  {}", aspect_str),
        format!("Atajos Teclado: Host=CtrlDer | Host+Del=CAD | Host+I=Raton | Host+P=Pausa"),
    ];

    let mut ly = fw.y + 34;
    for line in &lines {
        if ly + 16 > fw.y + fw.h { break; }
        draw_string_8x16(buf, ww, wh, fw.x + 12, ly, line, 0xABB2BF, None);
        ly += 20;
    }
}

fn render_toast(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    msg: &str,
) {
    let text_w = msg.len() * 8;
    let toast_w = text_w + 24;
    let toast_h = 28;
    let toast_x = ww.saturating_sub(toast_w + 16);
    let toast_y = wh.saturating_sub(toast_h + 16);

    fill_rect(buf, ww, wh, toast_x + 2, toast_y + 2, toast_w, toast_h, 0x0A0D10);
    fill_rect(buf, ww, wh, toast_x, toast_y, toast_w, toast_h, 0x21252B);
    draw_rect_outline(buf, ww, wh, toast_x, toast_y, toast_w, toast_h, 0x98C379);
    draw_string_8x16(buf, ww, wh, toast_x + 12, toast_y + 6, msg, 0xE5C07B, None);
}

fn render_floating_close_dialog(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    fw: &RuntimeFloatingWindow,
    choice: usize,
    mx: usize,
    my: usize,
) {
    draw_floating_window_frame(buf, ww, wh, fw, "Cerrar maquina virtual — Two Five Five (255)", mx, my);

    draw_string_8x16(buf, ww, wh, fw.x + 20, fw.y + 40, "Desea:", 0xCCD1D9, None);

    let options = [
        "Guardar el estado de la maquina",
        "Enviar senal de apagado (ACPI)",
        "Apagar la maquina (Power Off)",
    ];

    let opt_y_start = fw.y + 66;
    for (i, opt_text) in options.iter().enumerate() {
        let opt_y = opt_y_start + i * 32;
        let is_selected = choice == i;
        let is_hover = mx >= fw.x + 20 && mx < fw.x + fw.w - 20 && my >= opt_y && my < opt_y + 24;

        if is_hover {
            fill_rect(buf, ww, wh, fw.x + 20, opt_y, fw.w - 40, 24, 0x2C313A);
        }

        let radio_x = fw.x + 24;
        let radio_y = opt_y + 4;
        fill_rect(buf, ww, wh, radio_x, radio_y, 14, 14, 0x181A1F);
        draw_rect_outline(buf, ww, wh, radio_x, radio_y, 14, 14, if is_selected { 0x61AFEF } else { 0x5C6370 });
        if is_selected {
            fill_rect(buf, ww, wh, radio_x + 3, radio_y + 3, 8, 8, 0x61AFEF);
        }

        let text_color = if is_selected { 0xFFFFFF } else if is_hover { 0xABB2BF } else { 0x828997 };
        draw_string_8x16(buf, ww, wh, radio_x + 22, opt_y + 3, opt_text, text_color, None);
    }

    let btn_w = 95;
    let btn_h = 28;
    let btn_y = fw.y + fw.h - 38;
    let ok_x = fw.x + fw.w - 210;
    let cancel_x = fw.x + fw.w - 105;

    let ok_hov = mx >= ok_x && mx < ok_x + btn_w && my >= btn_y && my < btn_y + btn_h;
    let ok_bg = if ok_hov { 0x2A629A } else { 0x1E4268 };
    fill_rect(buf, ww, wh, ok_x, btn_y, btn_w, btn_h, ok_bg);
    draw_rect_outline(buf, ww, wh, ok_x, btn_y, btn_w, btn_h, 0x61AFEF);
    draw_string_8x16(buf, ww, wh, ok_x + 20, btn_y + 6, "Aceptar", 0xFFFFFF, None);

    let cancel_hov = mx >= cancel_x && mx < cancel_x + btn_w && my >= btn_y && my < btn_y + btn_h;
    let cancel_bg = if cancel_hov { 0x3E4451 } else { 0x2C313A };
    fill_rect(buf, ww, wh, cancel_x, btn_y, btn_w, btn_h, cancel_bg);
    draw_rect_outline(buf, ww, wh, cancel_x, btn_y, btn_w, btn_h, 0x5C6370);
    draw_string_8x16(buf, ww, wh, cancel_x + 16, btn_y + 6, "Cancelar", 0xABB2BF, None);
}

fn render_floating_preferences(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    fw: &RuntimeFloatingWindow,
    auto_resize: bool,
    preserve_aspect: bool,
    mouse_integration: bool,
    mx: usize,
    my: usize,
) {
    draw_floating_window_frame(buf, ww, wh, fw, "Preferencias de Maquina Virtual (Host+G)", mx, my);

    draw_string_8x16(buf, ww, wh, fw.x + 20, fw.y + 38, "Configuracion de Pantalla y Visualizacion:", 0x61AFEF, None);

    let chk1_y = fw.y + 62;
    fill_rect(buf, ww, wh, fw.x + 24, chk1_y, 14, 14, 0x181A1F);
    draw_rect_outline(buf, ww, wh, fw.x + 24, chk1_y, 14, 14, if auto_resize { 0x61AFEF } else { 0x5C6370 });
    if auto_resize {
        fill_rect(buf, ww, wh, fw.x + 27, chk1_y + 3, 8, 8, 0x61AFEF);
    }
    draw_string_8x16(buf, ww, wh, fw.x + 46, chk1_y - 1, "Auto-redimensionar pantalla de invitado (VMMDev)", 0xABB2BF, None);

    let chk2_y = fw.y + 88;
    fill_rect(buf, ww, wh, fw.x + 24, chk2_y, 14, 14, 0x181A1F);
    draw_rect_outline(buf, ww, wh, fw.x + 24, chk2_y, 14, 14, if preserve_aspect { 0x61AFEF } else { 0x5C6370 });
    if preserve_aspect {
        fill_rect(buf, ww, wh, fw.x + 27, chk2_y + 3, 8, 8, 0x61AFEF);
    }
    draw_string_8x16(buf, ww, wh, fw.x + 46, chk2_y - 1, "Preservar relacion de aspecto (Letterbox)", 0xABB2BF, None);

    let chk3_y = fw.y + 114;
    fill_rect(buf, ww, wh, fw.x + 24, chk3_y, 14, 14, 0x181A1F);
    draw_rect_outline(buf, ww, wh, fw.x + 24, chk3_y, 14, 14, if mouse_integration { 0x61AFEF } else { 0x5C6370 });
    if mouse_integration {
        fill_rect(buf, ww, wh, fw.x + 27, chk3_y + 3, 8, 8, 0x61AFEF);
    }
    draw_string_8x16(buf, ww, wh, fw.x + 46, chk3_y - 1, "Integracion de raton habilitada (USB Tablet)", 0xABB2BF, None);

    draw_string_8x16(buf, ww, wh, fw.x + 20, fw.y + 142, "Hardware Emulado:", 0x61AFEF, None);
    draw_string_8x16(buf, ww, wh, fw.x + 24, fw.y + 164, "* Video: 255-VBE (Bochs VBE LFB, hasta 3840x2160)", 0x98C379, None);
    draw_string_8x16(buf, ww, wh, fw.x + 24, fw.y + 184, "* Red: Intel 82540EM (e1000) Gigabit NAT", 0x98C379, None);
    draw_string_8x16(buf, ww, wh, fw.x + 24, fw.y + 204, "* Audio: Realtek ALC655 AC'97 (Host Pulse/ALSA)", 0x98C379, None);
    draw_string_8x16(buf, ww, wh, fw.x + 24, fw.y + 224, "* Tecla Host configurada: Control Derecho [RightCtrl]", 0xE5C07B, None);

    let btn_w = 95;
    let btn_h = 28;
    let btn_y = fw.y + fw.h - 38;
    let btn_x = fw.x + fw.w - 115;
    let btn_hov = mx >= btn_x && mx < btn_x + btn_w && my >= btn_y && my < btn_y + btn_h;
    fill_rect(buf, ww, wh, btn_x, btn_y, btn_w, btn_h, if btn_hov { 0x2A629A } else { 0x1E4268 });
    draw_rect_outline(buf, ww, wh, btn_x, btn_y, btn_w, btn_h, 0x61AFEF);
    draw_string_8x16(buf, ww, wh, btn_x + 20, btn_y + 6, "Aceptar", 0xFFFFFF, None);
}

fn render_floating_about(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    fw: &RuntimeFloatingWindow,
    mx: usize,
    my: usize,
) {
    draw_floating_window_frame(buf, ww, wh, fw, "Acerca de Two Five Five (255)", mx, my);

    draw_string_8x16(buf, ww, wh, fw.x + 20, fw.y + 40, "Two Five Five (255) KVM Virtual Machine v0.80", 0x61AFEF, None);
    draw_string_8x16(buf, ww, wh, fw.x + 20, fw.y + 62, "Two Five Five (255) Graphic Runtime & Frontend", 0xFFFFFF, None);

    let desc_lines = [
        "Hipervisor: Linux /dev/kvm con aceleracion por hardware",
        "Graficos: 255-VBE Display (Bochs VBE) con resoluciones hasta 4K",
        "Dispositivos: PCI Intel e1000, AC97, VMMDev Host Integration",
        "Soporte SO: Windows 7 SP1, 10, 11 y distribuciones GNU/Linux",
        "(C) 2026 Two Five Five Project. Licencia de software libre GPLv2.",
    ];

    let mut ly = fw.y + 92;
    for line in &desc_lines {
        draw_string_8x16(buf, ww, wh, fw.x + 20, ly, line, 0xABB2BF, None);
        ly += 22;
    }

    let btn_w = 95;
    let btn_h = 28;
    let btn_y = fw.y + fw.h - 38;
    let btn_x = fw.x + fw.w - 115;
    let btn_hov = mx >= btn_x && mx < btn_x + btn_w && my >= btn_y && my < btn_y + btn_h;
    fill_rect(buf, ww, wh, btn_x, btn_y, btn_w, btn_h, if btn_hov { 0x2A629A } else { 0x1E4268 });
    draw_rect_outline(buf, ww, wh, btn_x, btn_y, btn_w, btn_h, 0x61AFEF);
    draw_string_8x16(buf, ww, wh, btn_x + 20, btn_y + 6, "Aceptar", 0xFFFFFF, None);
}

fn render_floating_shortcuts_help(
    buf: &mut [u32],
    ww: usize,
    wh: usize,
    fw: &RuntimeFloatingWindow,
    mx: usize,
    my: usize,
) {
    draw_floating_window_frame(buf, ww, wh, fw, "Atajos de Teclado del Huesped (Host Key)", mx, my);

    draw_string_8x16(buf, ww, wh, fw.x + 20, fw.y + 38, "Combinaciones con Tecla Host (Control Derecho):", 0x61AFEF, None);

    let shortcuts = [
        ("Host + Q", "Cerrar / Salir de la maquina virtual"),
        ("Host + R", "Reiniciar maquina virtual"),
        ("Host + P", "Pausar o reanudar ejecucion"),
        ("Host + H", "Enviar apagado limpio ACPI al SO"),
        ("Host + A", "Alternar auto-redimensionamiento de pantalla"),
        ("Host + C", "Alternar preservar relacion de aspecto / estirar"),
        ("Host + Del", "Insertar Ctrl+Alt+Supr (CAD) en el sistema"),
        ("Host + I", "Alternar captura o integracion de raton"),
        ("Host + F1", "Mostrar / Ocultar panel OSD flotante (HUD)"),
        ("Host + E", "Tomar captura de pantalla en archivo BMP"),
        ("Host + T", "Crear instantanea rapida de estado"),
    ];

    let mut sy = fw.y + 64;
    for (key, desc) in &shortcuts {
        if sy + 16 > fw.y + fw.h - 40 { break; }
        draw_string_8x16(buf, ww, wh, fw.x + 24, sy, key, 0xE5C07B, None);
        draw_string_8x16(buf, ww, wh, fw.x + 140, sy, desc, 0xABB2BF, None);
        sy += 20;
    }

    let btn_w = 110;
    let btn_h = 28;
    let btn_y = fw.y + fw.h - 38;
    let btn_x = fw.x + fw.w - 130;
    let btn_hov = mx >= btn_x && mx < btn_x + btn_w && my >= btn_y && my < btn_y + btn_h;
    fill_rect(buf, ww, wh, btn_x, btn_y, btn_w, btn_h, if btn_hov { 0x2A629A } else { 0x1E4268 });
    draw_rect_outline(buf, ww, wh, btn_x, btn_y, btn_w, btn_h, 0x61AFEF);
    draw_string_8x16(buf, ww, wh, btn_x + 18, btn_y + 6, "Entendido", 0xFFFFFF, None);
}

pub fn save_bmp_screenshot_buffer(buf: &[u32], w: usize, h: usize, path: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    let row_size = (w * 3 + 3) & !3;
    let image_size = row_size * h;
    let file_size = 54 + image_size;

    let mut header = [0u8; 54];
    header[0] = 0x42;
    header[1] = 0x4D;
    header[2..6].copy_from_slice(&(file_size as u32).to_le_bytes());
    header[10..14].copy_from_slice(&54u32.to_le_bytes());
    header[14..18].copy_from_slice(&40u32.to_le_bytes());
    header[18..22].copy_from_slice(&(w as i32).to_le_bytes());
    header[22..26].copy_from_slice(&(h as i32).to_le_bytes());
    header[26..28].copy_from_slice(&1u16.to_le_bytes());
    header[28..30].copy_from_slice(&24u16.to_le_bytes());
    header[34..38].copy_from_slice(&(image_size as u32).to_le_bytes());

    file.write_all(&header)?;

    let mut row_buf = vec![0u8; row_size];
    for y in (0..h).rev() {
        let src_row = &buf[y * w..(y + 1) * w];
        for (x, &px) in src_row.iter().enumerate() {
            let b = (px & 0xFF) as u8;
            let g = ((px >> 8) & 0xFF) as u8;
            let r = ((px >> 16) & 0xFF) as u8;
            row_buf[x * 3] = b;
            row_buf[x * 3 + 1] = g;
            row_buf[x * 3 + 2] = r;
        }
        file.write_all(&row_buf)?;
    }
    Ok(())
}

/// Calcula los deltas de movimiento (dx, dy) del ratón host adaptados al protocolo PS/2.
///
/// En el sistema de coordenadas de la ventana (minifb / host), el origen (0,0) está
/// en la esquina superior izquierda y el eje Y crece hacia abajo (+Y = hacia abajo).
/// En el protocolo de hardware del ratón PS/2, el eje Y sigue coordenadas cartesianas estándar:
/// los valores positivos (+Y) indican movimiento hacia ARRIBA y los negativos (-Y) hacia ABAJO.
/// Por tanto, dy se calcula como `(ly - my)` para que el movimiento hacia abajo en la ventana del
/// host resulte en un delta negativo en PS/2 y el cursor del sistema operativo virtualizado se mueva hacia abajo.
#[inline]
pub fn compute_mouse_deltas(lx: f32, ly: f32, mx: f32, my: f32) -> (i16, i16) {
    let dx = (mx - lx) as i16;
    let dy = (ly - my) as i16;
    (dx, dy)
}

/// Calcula deltas de ratón PS/2 escalados según la relación entre resolución guest y viewport host,
/// acumulando fracciones subpíxel para eliminar la desincronización y el "drift" en movimientos lentos.
#[inline]
pub fn compute_mouse_deltas_scaled(
    lx: f32,
    ly: f32,
    mx: f32,
    my: f32,
    scale_x: f32,
    scale_y: f32,
    accum_x: &mut f32,
    accum_y: &mut f32,
) -> (i16, i16) {
    let raw_dx = mx - lx;
    let raw_dy = ly - my; // En PS/2 Y+ es hacia arriba

    *accum_x += raw_dx * scale_x;
    *accum_y += raw_dy * scale_y;

    let send_dx = accum_x.trunc() as i16;
    let send_dy = accum_y.trunc() as i16;

    *accum_x -= send_dx as f32;
    *accum_y -= send_dy as f32;

    (send_dx, send_dy)
}

pub struct DisplayManager {
    running: Arc<AtomicBool>,
}

impl DisplayManager {
    pub fn start(
        vga_state: Arc<Mutex<VgaState>>,
        guest_mem: Arc<GuestMemory>,
        kbd_queue: Arc<Mutex<VecDeque<u8>>>,
        mouse_queue: Arc<Mutex<VecDeque<(i16, i16, u8)>>>,
        tablet_queue: Arc<Mutex<VecDeque<(u16, u16, u8, i8)>>>,
        resize_queue: Arc<Mutex<Option<(u32, u32)>>>,
        metrics: Arc<crate::metrics::VmmMetrics>,
        vm_title: Option<String>,
    ) -> Option<Self> {
        if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("[DISPLAY] No se detectó servidor gráfico (DISPLAY/WAYLAND). Ejecutando en modo headless.");
            return None;
        }

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();
        let win_title = vm_title.unwrap_or_else(|| "Two Five Five — Virtual Display".to_string());

        thread::spawn(move || {
            let initial_width = 1024;
            let initial_height = 768;
            // El buffer LÓGICO guarda el frame renderizado al tamaño nativo del modo;
            // el buffer de ventana lo re-escala suavemente sin pixelación.
            let mut render_buf: Vec<u32> = vec![0; initial_width * initial_height];
            let mut window_buf: Vec<u32> = vec![0; initial_width * initial_height];

            let mut window = match Window::new(
                &win_title,
                initial_width,
                initial_height,
                WindowOptions {
                    resize: true,
                    scale: minifb::Scale::X1,
                    ..WindowOptions::default()
                },
            ) {
                Ok(win) => win,
                Err(e) => {
                    eprintln!("[DISPLAY] Error al crear ventana minifb: {}", e);
                    running_clone.store(false, Ordering::Relaxed);
                    return;
                }
            };

            window.set_target_fps(60);

            let mem = guest_mem; // manija Arc<GuestMemory> movida al hilo
            let mut last_mouse: Option<(f32, f32)> = None;
            let mut last_buttons: u8 = 0;
            let mut last_tablet: Option<(u16, u16, u8)> = None;
            let mut last_tablet_sync = std::time::Instant::now();
            // Traza de calibración del puntero absoluto (MI_VMM_MOUSE_DEBUG=1).
            // Registra la geometría exacta con la que se mapea el puntero: si
            // `guest` (resolución del guest) y `vp` (rectángulo donde el
            // renderer dibuja el frame) no encajan, el cursor del guest no
            // puede coincidir con el físico.
            let mouse_debug = std::env::var_os("MI_VMM_MOUSE_DEBUG").is_some();
            let mut last_mouse_debug_log = std::time::Instant::now();
            let mut mouse_debug_last: Option<(u16, u16, u8)> = None;
            let mut last_window_size: Option<(usize, usize)> = None;
            let mut last_size_change = std::time::Instant::now();
            let mut size_pending = false;

            // ── Entrada de teclado (tarea 19) ─────────────────────────
            let caps_on = Arc::new(AtomicBool::new(false));
            let numpad_on = Arc::new(AtomicBool::new(false));
            let ctrl_held = Arc::new(AtomicBool::new(false));
            let lalt_held = Arc::new(AtomicBool::new(false));

            window.set_input_callback(Box::new(CharInput {
                kbd_queue: kbd_queue.clone(),
                ctrl_held: ctrl_held.clone(),
                lalt_held: lalt_held.clone(),
                caps_on: caps_on.clone(),
            }));

            // Teclas de control actualmente pulsadas → break al soltar.
            let mut held: HashMap<Key, Vec<u8>> = HashMap::new();
            let mut logged_desktop = false;
            let mut frame_count = 0u32;
            let mut last_fps_time = std::time::Instant::now();
            let mut last_mode = (false, false, false, 0usize, 0usize);
            let mut preserve_aspect = true;
            let mut auto_resize_guest = true;
            let mut current_viewport = (0usize, TOP_BAR_H, initial_width, initial_height.saturating_sub(TOP_BAR_H));
            let mut scanline_hashes: Vec<u64> = Vec::new();
            let mut last_text_hash = 0u64;
            let mut last_planar_hash = 0u64;
            let mut force_full_redraw = true;

            let mut user_mouse_integration = true;
            let mut mouse_captured = false;
            let mut accum_mouse_x = 0.0f32;
            let mut accum_mouse_y = 0.0f32;
            let mut cursor_hidden = false;
            let mut close_dialog_choice = 1usize; // 0=Guardar estado, 1=ACPI, 2=Power Off
            let mut active_menu: Option<usize> = None;
            let mut floating_windows: Vec<RuntimeFloatingWindow> = Vec::new();
            let mut toast: Option<(String, std::time::Instant)> = None;
            let mut prev_left_down = false;

            while running_clone.load(Ordering::Relaxed)
                && window.is_open()
                && !crate::SHUTDOWN_REQUESTED.load(Ordering::Relaxed)
            {
                let guest_tablet_ready = metrics.mouse_abs_available.load(Ordering::Relaxed);
                let mouse_integration = user_mouse_integration && guest_tablet_ready;

                // Si el SO huésped acaba de activar y configurar integración absoluta (USB Tablet o VMMDev), auto-liberar captura
                if mouse_integration && mouse_captured {
                    mouse_captured = false;
                    last_mouse = None;
                    accum_mouse_x = 0.0;
                    accum_mouse_y = 0.0;
                    // Al pasar a absoluto hay que re-afirmar la posición sí o
                    // sí: el cursor del guest puede estar en cualquier sitio.
                    last_tablet = None;
                    toast = Some(("Integración de ratón activa (Puntero absoluto detectado)".to_string(), std::time::Instant::now()));
                    force_full_redraw = true;
                }

                // Sincronizar visibilidad del cursor del host con el estado de captura
                if mouse_captured {
                    if !cursor_hidden {
                        window.set_cursor_visibility(false);
                        cursor_hidden = true;
                    }
                } else if cursor_hidden {
                    window.set_cursor_visibility(true);
                    cursor_hidden = false;
                }
                // ── Snapshot del modo actual ────────────────────────
                let (is_vbe, is_std_gfx, is_vbe_active, lw, lh, bpp, virt_w, x_off, y_off, dac8) = {
                    let st = vga_state.lock().unwrap();
                    let (w, h, b) = st.get_resolution();
                    let is_vbe = st.is_vbe_enabled();
                    let is_std_gfx = !is_vbe && st.is_standard_vga_graphics();
                    let is_vbe_active = st.vbe_active;
                    let (lw, lh) = if is_vbe {
                        (w, h)
                    } else if is_std_gfx {
                        let (gw, gh) = st.standard_vga_geometry();
                        (gw, gh)
                    } else {
                        (640, 400)
                    };
                    (
                        is_vbe,
                        is_std_gfx,
                        is_vbe_active,
                        lw,
                        lh,
                        b,
                        st.dispi_regs[VBE_DISPI_INDEX_VIRT_WIDTH as usize] as usize,
                        st.dispi_regs[VBE_DISPI_INDEX_X_OFFSET as usize] as usize,
                        st.dispi_regs[VBE_DISPI_INDEX_Y_OFFSET as usize] as usize,
                        st.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_8BIT_DAC != 0,
                    )
                };

                metrics.display_width.store(lw as u32, Ordering::Relaxed);
                metrics.display_height.store(lh as u32, Ordering::Relaxed);
                metrics.display_bpp.store(bpp as u32, Ordering::Relaxed);
                metrics.is_vbe.store(is_vbe, Ordering::Relaxed);

                if render_buf.len() != lw * lh {
                    render_buf.resize(lw * lh, 0);
                    force_full_redraw = true;
                }

                // Limpiar el buffer al cambiar de modo para evitar artefactos visuales
                let current_mode = (is_vbe, is_std_gfx, is_vbe_active, lw, lh);
                if current_mode != last_mode {
                    render_buf.fill(0);
                    scanline_hashes.clear();
                    last_text_hash = 0;
                    last_planar_hash = 0;
                    force_full_redraw = true;
                    last_mode = current_mode;
                    let mode_desc = if is_vbe { "VBE Lineal" } else if is_std_gfx { "VGA Estándar" } else { "VGA Texto" };
                    let title = format!("Two Five Five — Pantalla Virtual [{}x{}@{}bpp, {}]", lw, lh, bpp, mode_desc);
                    window.set_title(&title);
                }

                let mut is_dirty = false;
                if is_vbe {
                    // Modo gráfico VBE: leer directamente de VRAM con dirty tracking por scanline.
                    // Se copian los datos necesarios y se SUELTA el mutex antes de
                    // recorrer el framebuffer: retenerlo bloqueaba al vCPU en cada
                    // acceso a puertos VGA durante todo el frame.
                    // Para bpp >= 15 (caso habitual en Windows/Linux), no copiamos los 768 bytes de dac_palette.
                    let vbe_snapshot = {
                        let st = vga_state.lock().unwrap();
                        if st.vram_ptr.is_null() {
                            None
                        } else if bpp <= 8 {
                            Some((st.vram_ptr, st.vram_size, st.dac_palette))
                        } else {
                            Some((st.vram_ptr, st.vram_size, [0u8; 768]))
                        }
                    };
                    if let Some((vram_ptr, vram_size, dac_palette)) = vbe_snapshot {
                        if scanline_hashes.len() != lh {
                            scanline_hashes.resize(lh, 0);
                            force_full_redraw = true;
                        }
                        is_dirty = render_vbe_framebuffer(
                            &mut render_buf,
                            vram_ptr,
                            vram_size,
                            lw,
                            lh,
                            bpp,
                            virt_w,
                            x_off,
                            y_off,
                            &dac_palette,
                            dac8,
                            &mut scanline_hashes,
                            force_full_redraw,
                        );
                        // El sondeo de "escritorio gráfico" recorre TODO el
                        // framebuffer (2 MP a 1080p) en cada frame; solo hace
                        // falta hasta emitir el log una vez.
                        if !logged_desktop {
                            let non_zeros = render_buf.iter().filter(|&&p| p != 0).count();
                            if non_zeros > (lw * lh) / 100 {
                                logged_desktop = true;
                                let msg = format!(
                                    "[DISPLAY] ¡Escritorio gráfico detectado! {}x{}@{}bpp ({} píxeles activos)",
                                    lw, lh, bpp, non_zeros
                                );
                                crate::tui::log(&msg);
                            }
                        }
                    }
                } else if is_std_gfx {
                    // (tarea 2) Modo gráfico VGA estándar: clonar registros y soltar el mutex antes de decodificar planos
                    let st_snap = {
                        let st = vga_state.lock().unwrap();
                        if st.vram_ptr.is_null() {
                            None
                        } else {
                            Some(st.clone())
                        }
                    };
                    if let Some(st) = st_snap {
                        is_dirty = render_vga_graphics(&mut render_buf, &st, lw, lh, &mut last_planar_hash, force_full_redraw);
                    }
                } else if is_vbe_active {
                    // VBE activo pero temporalmente deshabilitado (transición de modo KMS / monitor blanking)
                    render_buf.fill(0);
                    is_dirty = true;
                } else {
                    // Modo texto VGA: renderizar buffer 80x25 desde 0xB8000 con dirty tracking
                    is_dirty = render_vga_text_mode(&mut render_buf, &mem, &mut last_text_hash, force_full_redraw);
                }

                // ── Escalar al tamaño actual de la ventana ──
                let (ww, wh) = window.get_size();
                let (ww, wh) = (ww.max(320), wh.max(200));

                if last_window_size != Some((ww, wh)) {
                    last_window_size = Some((ww, wh));
                    last_size_change = std::time::Instant::now();
                    size_pending = true;
                    force_full_redraw = true;
                }

                // Debounce de 150 ms para evitar saturar Xorg/KMS mientras se arrastra la ventana
                if size_pending && last_size_change.elapsed() >= std::time::Duration::from_millis(150) {
                    size_pending = false;
                    force_full_redraw = true;
                    if auto_resize_guest {
                        if let Ok(mut rq) = resize_queue.lock() {
                            let guest_h = wh.saturating_sub(TOP_BAR_H);
                            *rq = Some((ww as u32, guest_h as u32));
                        }
                    }
                }

                // ── Atajos con Host Key (VirtualBox default: Tecla RightCtrl) ──
                let host_down = window.is_key_down(Key::RightCtrl);
                if host_down {
                    if window.is_key_pressed(Key::Delete, KeyRepeat::No) {
                        let mut q = kbd_queue.lock().unwrap();
                        push_bytes(&mut q, &[0x1D, 0x38, 0xE0, 0x53, 0xE0, 0xD3, 0xB8, 0x9D]);
                        toast = Some(("Ctrl+Alt+Del inyectado en el SO huésped".to_string(), std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::I, KeyRepeat::No) {
                        user_mouse_integration = !user_mouse_integration;
                        mouse_captured = false;
                        last_mouse = None;
                        accum_mouse_x = 0.0;
                        accum_mouse_y = 0.0;
                        toast = Some((if user_mouse_integration {
                            "Integración de ratón: Habilitada (Auto-USB)".to_string()
                        } else {
                            "Integración de ratón: Forzar PS/2 (Clic para capturar)".to_string()
                        }, std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::P, KeyRepeat::No) {
                        let cur = metrics.is_paused.load(Ordering::Relaxed);
                        metrics.is_paused.store(!cur, Ordering::Relaxed);
                        toast = Some((if !cur { "Máquina virtual pausada".to_string() } else { "Máquina virtual reanudada".to_string() }, std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::R, KeyRepeat::No) {
                        crate::RESET_REQUESTED.store(true, Ordering::Relaxed);
                        toast = Some(("Reiniciando máquina virtual...".to_string(), std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::H, KeyRepeat::No) {
                        crate::ACPI_SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                        toast = Some(("Enviando señal de apagado ACPI al SO...".to_string(), std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::Q, KeyRepeat::No) {
                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::CloseVm, 460, 240, ww, wh);
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::G, KeyRepeat::No) {
                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::Preferences, 520, 310, ww, wh);
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::E, KeyRepeat::No) {
                        let now_sec = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                        let fname = format!("screenshot_{}.bmp", now_sec);
                        if let Err(e) = save_bmp_screenshot_buffer(&render_buf, lw, lh, &fname) {
                            toast = Some((format!("Error captura BMP: {}", e), std::time::Instant::now()));
                        } else {
                            toast = Some((format!("Captura guardada en {}", fname), std::time::Instant::now()));
                            crate::tui::log(&format!("[DISPLAY] Captura guardada: {}", fname));
                        }
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::C, KeyRepeat::No) {
                        preserve_aspect = !preserve_aspect;
                        toast = Some((format!("Aspecto: {}", if preserve_aspect { "Preservar" } else { "Estirar" }), std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::A, KeyRepeat::No) {
                        auto_resize_guest = !auto_resize_guest;
                        toast = Some((if auto_resize_guest {
                            "Auto-redimensionar pantalla de invitado: Habilitado (Host+A)".to_string()
                        } else {
                            "Auto-redimensionar pantalla de invitado: Deshabilitado (Host+A)".to_string()
                        }, std::time::Instant::now()));
                        force_full_redraw = true;
                    } else if window.is_key_pressed(Key::F1, KeyRepeat::No) {
                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::Hud, 500, 200, ww, wh);
                        force_full_redraw = true;
                    }
                }

                // Alternar aspecto con F11 (clásico)
                if window.is_key_pressed(Key::F11, KeyRepeat::No) {
                    preserve_aspect = !preserve_aspect;
                    force_full_redraw = true;
                }

                // Liberar captura de ratón o cerrar ventana flotante/menú con Escape
                if window.is_key_pressed(Key::Escape, KeyRepeat::No) {
                    if let Some(_closed) = floating_windows.pop() {
                        force_full_redraw = true;
                    } else if active_menu.is_some() {
                        active_menu = None;
                        force_full_redraw = true;
                    } else if mouse_captured {
                        mouse_captured = false;
                        last_mouse = None;
                        accum_mouse_x = 0.0;
                        accum_mouse_y = 0.0;
                        toast = Some(("Ratón liberado".to_string(), std::time::Instant::now()));
                        force_full_redraw = true;
                    }
                }

                // Navegación por teclado dentro del diálogo de salida UIVMCloseDialog
                if let Some(top) = floating_windows.last() {
                    if top.kind == FloatingKind::CloseVm {
                        if window.is_key_pressed(Key::Up, KeyRepeat::No) {
                            close_dialog_choice = close_dialog_choice.saturating_sub(1);
                            force_full_redraw = true;
                        } else if window.is_key_pressed(Key::Down, KeyRepeat::No) {
                            if close_dialog_choice < 2 {
                                close_dialog_choice += 1;
                            }
                            force_full_redraw = true;
                        } else if window.is_key_pressed(Key::Enter, KeyRepeat::No) {
                            match close_dialog_choice {
                                0 => {
                                    crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                }
                                1 => {
                                    crate::ACPI_SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                    toast = Some(("Enviando señal de apagado ACPI al SO...".to_string(), std::time::Instant::now()));
                                    floating_windows.pop();
                                }
                                2 => {
                                    crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                            force_full_redraw = true;
                        }
                    }
                }

                // ── Gestión del Ratón y Menús de la Barra Superior ──
                let mouse_pos_raw = window.get_mouse_pos(MouseMode::Pass);
                let mouse_in_window = match mouse_pos_raw {
                    Some((x, y)) => x >= 0.0 && x < ww as f32 && y >= 0.0 && y < wh as f32,
                    None => false,
                };
                let mouse_pos = if mouse_in_window || mouse_captured {
                    mouse_pos_raw
                } else {
                    None
                };
                let (mouse_x, mouse_y) = match mouse_pos {
                    Some((x, y)) if mouse_in_window => (x.round().max(0.0) as usize, y.round().max(0.0) as usize),
                    _ => (usize::MAX, usize::MAX),
                };

                let curr_left_down = window.get_mouse_down(MouseButton::Left);
                let left_just_clicked = curr_left_down && !prev_left_down;
                prev_left_down = curr_left_down;

                if let Some((mx, my)) = mouse_pos {
                    let imx = mx.round().max(0.0) as usize;
                    let imy = my.round().max(0.0) as usize;

                    // Arrastre activo de ventanas flotantes
                    for fw in floating_windows.iter_mut() {
                        if fw.is_dragging {
                            if curr_left_down {
                                let nx = imx.saturating_sub(fw.drag_off_x);
                                let ny = imy.saturating_sub(fw.drag_off_y);
                                fw.x = nx.clamp(0, ww.saturating_sub(fw.w));
                                fw.y = ny.clamp(TOP_BAR_H, wh.saturating_sub(fw.h));
                                force_full_redraw = true;
                            } else {
                                fw.is_dragging = false;
                            }
                        }
                    }

                    if left_just_clicked {
                        let mut clicked_fw_idx = None;
                        for (i, fw) in floating_windows.iter().enumerate().rev() {
                            if fw.contains_point(imx, imy) {
                                clicked_fw_idx = Some(i);
                                break;
                            }
                        }

                        if let Some(i) = clicked_fw_idx {
                            let mut fw = floating_windows.remove(i);

                            if fw.is_in_close_btn(imx, imy) {
                                // Cerrar ventana con botón [X]
                                force_full_redraw = true;
                            } else if fw.is_in_titlebar(imx, imy) {
                                // Iniciar arrastre por la barra de título y traer al frente
                                fw.is_dragging = true;
                                fw.drag_off_x = imx.saturating_sub(fw.x);
                                fw.drag_off_y = imy.saturating_sub(fw.y);
                                floating_windows.push(fw);
                                force_full_redraw = true;
                            } else {
                                // Interacción interna según el tipo de ventana
                                match fw.kind {
                                    FloatingKind::CloseVm => {
                                        let opt_y_start = fw.y + 66;
                                        for opt_idx in 0..3 {
                                            let opt_y = opt_y_start + opt_idx * 32;
                                            if imx >= fw.x + 20 && imx < fw.x + fw.w - 20 && imy >= opt_y && imy < opt_y + 24 {
                                                close_dialog_choice = opt_idx;
                                                force_full_redraw = true;
                                            }
                                        }

                                        let btn_y = fw.y + fw.h - 38;
                                        let btn_w = 95;
                                        let btn_h = 28;
                                        let ok_x = fw.x + fw.w - 210;
                                        let cancel_x = fw.x + fw.w - 105;

                                        if imx >= ok_x && imx < ok_x + btn_w && imy >= btn_y && imy < btn_y + btn_h {
                                            match close_dialog_choice {
                                                0 => {
                                                    crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                                }
                                                1 => {
                                                    crate::ACPI_SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                                    toast = Some(("Enviando señal de apagado ACPI al SO...".to_string(), std::time::Instant::now()));
                                                }
                                                2 => {
                                                    crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                                }
                                                _ => {}
                                            }
                                            force_full_redraw = true;
                                        } else if imx >= cancel_x && imx < cancel_x + btn_w && imy >= btn_y && imy < btn_y + btn_h {
                                            force_full_redraw = true;
                                        } else {
                                            floating_windows.push(fw);
                                        }
                                    }
                                    FloatingKind::Preferences => {
                                        if imx >= fw.x + 20 && imx < fw.x + fw.w - 20 && imy >= fw.y + 60 && imy < fw.y + 82 {
                                            auto_resize_guest = !auto_resize_guest;
                                            force_full_redraw = true;
                                        } else if imx >= fw.x + 20 && imx < fw.x + fw.w - 20 && imy >= fw.y + 86 && imy < fw.y + 108 {
                                            preserve_aspect = !preserve_aspect;
                                            force_full_redraw = true;
                                        } else if imx >= fw.x + 20 && imx < fw.x + fw.w - 20 && imy >= fw.y + 112 && imy < fw.y + 134 {
                                            user_mouse_integration = !user_mouse_integration;
                                            force_full_redraw = true;
                                        }
                                        let btn_x = fw.x + fw.w - 115;
                                        let btn_y = fw.y + fw.h - 38;
                                        if imx >= btn_x && imx < btn_x + 95 && imy >= btn_y && imy < btn_y + 28 {
                                            force_full_redraw = true;
                                        } else {
                                            floating_windows.push(fw);
                                        }
                                    }
                                    FloatingKind::About => {
                                        let btn_x = fw.x + fw.w - 115;
                                        let btn_y = fw.y + fw.h - 38;
                                        if imx >= btn_x && imx < btn_x + 95 && imy >= btn_y && imy < btn_y + 28 {
                                            force_full_redraw = true;
                                        } else {
                                            floating_windows.push(fw);
                                        }
                                    }
                                    FloatingKind::ShortcutsHelp => {
                                        let btn_x = fw.x + fw.w - 130;
                                        let btn_y = fw.y + fw.h - 38;
                                        if imx >= btn_x && imx < btn_x + 110 && imy >= btn_y && imy < btn_y + 28 {
                                            force_full_redraw = true;
                                        } else {
                                            floating_windows.push(fw);
                                        }
                                    }
                                    FloatingKind::Hud => {
                                        floating_windows.push(fw);
                                    }
                                }
                            }
                        } else if imy < TOP_BAR_H {
                            let header_rects = get_menu_header_rects();
                            let mut clicked_menu = None;
                            for (i, &(hx, hw)) in header_rects.iter().enumerate() {
                                if imx >= hx && imx < hx + hw {
                                    clicked_menu = Some(i);
                                    break;
                                }
                            }
                            if let Some(i) = clicked_menu {
                                if active_menu == Some(i) {
                                    active_menu = None;
                                } else {
                                    active_menu = Some(i);
                                }
                                force_full_redraw = true;
                            } else {
                                let mut right_cursor = ww.saturating_sub(10);
                                let host_badge = "Host: Ctrl Der";
                                let host_w = host_badge.len() * 8 + 12;
                                if right_cursor > host_w {
                                    right_cursor -= host_w;
                                    if imx >= right_cursor && imx < right_cursor + host_w && mouse_captured {
                                        mouse_captured = false;
                                        last_mouse = None;
                                        accum_mouse_x = 0.0;
                                        accum_mouse_y = 0.0;
                                        toast = Some(("Ratón liberado".to_string(), std::time::Instant::now()));
                                        force_full_redraw = true;
                                    }
                                    right_cursor = right_cursor.saturating_sub(6);
                                }

                                let mouse_badge_len = if mouse_integration { "Ratón: Integrado".len() } else if mouse_captured { "Ratón: CAPTURADO (Host=Liberar)".len() } else { "Ratón: PS/2 (Clic captura)".len() };
                                let mouse_w = mouse_badge_len * 8 + 12;
                                if right_cursor > mouse_w {
                                    right_cursor -= mouse_w;
                                    if imx >= right_cursor && imx < right_cursor + mouse_w {
                                        if mouse_captured {
                                            mouse_captured = false;
                                            last_mouse = None;
                                            accum_mouse_x = 0.0;
                                            accum_mouse_y = 0.0;
                                            toast = Some(("Ratón liberado".to_string(), std::time::Instant::now()));
                                        } else {
                                            user_mouse_integration = !user_mouse_integration;
                                            toast = Some((if user_mouse_integration {
                                                "Integración de ratón: Habilitada (Auto-USB)".to_string()
                                            } else {
                                                "Integración de ratón: Forzar PS/2 (Clic para capturar)".to_string()
                                            }, std::time::Instant::now()));
                                        }
                                        force_full_redraw = true;
                                    }
                                }
                            }
                        } else if let Some(menu_idx) = active_menu {
                            if let Some(action) = find_dropdown_action_at(menu_idx, ww, imx, imy) {
                                match action {
                                    MenuAction::TogglePause => {
                                        let cur = metrics.is_paused.load(Ordering::Relaxed);
                                        metrics.is_paused.store(!cur, Ordering::Relaxed);
                                        toast = Some((if !cur { "Máquina virtual pausada".to_string() } else { "Máquina virtual reanudada".to_string() }, std::time::Instant::now()));
                                    }
                                    MenuAction::ResetVm => {
                                        crate::RESET_REQUESTED.store(true, Ordering::Relaxed);
                                        toast = Some(("Reiniciando máquina virtual...".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::AcpiShutdown => {
                                        crate::ACPI_SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                        toast = Some(("Enviando apagado ACPI al SO...".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::PowerOff => {
                                        crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                                    }
                                    MenuAction::Screenshot => {
                                        let now_sec = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
                                        let fname = format!("screenshot_{}.bmp", now_sec);
                                        if let Err(e) = save_bmp_screenshot_buffer(&render_buf, lw, lh, &fname) {
                                            toast = Some((format!("Error captura BMP: {}", e), std::time::Instant::now()));
                                        } else {
                                            toast = Some((format!("Captura guardada en {}", fname), std::time::Instant::now()));
                                            crate::tui::log(&format!("[DISPLAY] Captura guardada: {}", fname));
                                        }
                                    }
                                    MenuAction::ToggleHud => {
                                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::Hud, 500, 200, ww, wh);
                                    }
                                     MenuAction::ToggleAutoResize => {
                                        auto_resize_guest = !auto_resize_guest;
                                        toast = Some((if auto_resize_guest {
                                            "Auto-redimensionar pantalla de invitado: Habilitado (Host+A)".to_string()
                                        } else {
                                            "Auto-redimensionar pantalla de invitado: Deshabilitado (Host+A)".to_string()
                                        }, std::time::Instant::now()));
                                    }
                                    MenuAction::FitWindow => {
                                        if let Ok(mut rq) = resize_queue.lock() {
                                            *rq = Some((lw as u32, lh as u32));
                                        }
                                        toast = Some(("Ajustando tamaño al modo nativo".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::ToggleAspect => {
                                        preserve_aspect = !preserve_aspect;
                                        toast = Some((format!("Aspecto: {}", if preserve_aspect { "Preservar" } else { "Estirar" }), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes800x600 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((800, 600)); }
                                        toast = Some(("Petición de resolución: 800x600 enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1024x768 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1024, 768)); }
                                        toast = Some(("Petición de resolución: 1024x768 enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1280x720 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1280, 720)); }
                                        toast = Some(("Petición de resolución: 1280x720 (16:9 HD) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1280x800 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1280, 800)); }
                                        toast = Some(("Petición de resolución: 1280x800 (16:10 WXGA) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1280x1024 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1280, 1024)); }
                                        toast = Some(("Petición de resolución: 1280x1024 (5:4 SXGA) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1440x900 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1440, 900)); }
                                        toast = Some(("Petición de resolución: 1440x900 (16:10) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1600x900 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1600, 900)); }
                                        toast = Some(("Petición de resolución: 1600x900 (16:9 HD+) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1920x1080 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1920, 1080)); }
                                        toast = Some(("Petición de resolución: 1920x1080 (16:9 FHD) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SetRes1920x1200 => {
                                        if let Ok(mut rq) = resize_queue.lock() { *rq = Some((1920, 1200)); }
                                        toast = Some(("Petición de resolución: 1920x1200 (16:10) enviada al huésped".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SendCtrlAltDel => {
                                        let mut q = kbd_queue.lock().unwrap();
                                        push_bytes(&mut q, &[0x1D, 0x38, 0xE0, 0x53, 0xE0, 0xD3, 0xB8, 0x9D]);
                                        toast = Some(("Ctrl+Alt+Del inyectado".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::SendCtrlAltBksp => {
                                        let mut q = kbd_queue.lock().unwrap();
                                        push_bytes(&mut q, &[0x1D, 0x38, 0x0E, 0x8E, 0xB8, 0x9D]);
                                        toast = Some(("Ctrl+Alt+Backspace inyectado".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::ToggleMouseIntegration => {
                                        user_mouse_integration = !user_mouse_integration;
                                        mouse_captured = false;
                                        last_mouse = None;
                                        accum_mouse_x = 0.0;
                                        accum_mouse_y = 0.0;
                                        toast = Some((if user_mouse_integration {
                                            "Integración de ratón: Habilitada (Auto-USB)".to_string()
                                        } else {
                                            "Integración de ratón: Forzar PS/2 (Clic para capturar)".to_string()
                                        }, std::time::Instant::now()));
                                    }
                                    MenuAction::EjectIso => {
                                        crate::CDROM_EJECT_REQUESTED.store(true, Ordering::Relaxed);
                                        toast = Some(("Medio óptico expulsado".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::TakeSnapshot => {
                                        toast = Some(("Instantánea guardada".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::Preferences => {
                                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::Preferences, 520, 310, ww, wh);
                                    }
                                    MenuAction::ResetSettings => {
                                        auto_resize_guest = true;
                                        preserve_aspect = true;
                                        user_mouse_integration = true;
                                        toast = Some(("Preferencias restauradas por defecto".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::Exit => {
                                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::CloseVm, 460, 240, ww, wh);
                                    }
                                    MenuAction::AudioInfo => {
                                        toast = Some(("Audio: AC'97 Realtek ALC655 (Host PCM Activo)".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::NetworkInfo => {
                                        toast = Some(("Red: Intel 82540EM (e1000) PCI 00:08.0 Conectado".to_string(), std::time::Instant::now()));
                                    }
                                    MenuAction::About => {
                                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::About, 560, 260, ww, wh);
                                    }
                                    MenuAction::ShortcutsHelp => {
                                        toggle_or_open_floating_window(&mut floating_windows, FloatingKind::ShortcutsHelp, 560, 330, ww, wh);
                                    }
                                    MenuAction::Separator => {}
                                }
                            }
                            active_menu = None;
                            force_full_redraw = true;
                        } else if !mouse_integration && !mouse_captured && floating_windows.is_empty() {
                            mouse_captured = true;
                            last_mouse = None;
                            accum_mouse_x = 0.0;
                            accum_mouse_y = 0.0;
                            toast = Some(("Ratón capturado (Pulsa Ctrl Der o Esc para liberar)".to_string(), std::time::Instant::now()));
                            force_full_redraw = true;
                        }
                    }

                    // Enrutamiento EXCLUSIVO del ratón (solución a conflictos PS/2 vs USB tablet)
                    let is_mouse_over_floating = floating_windows.iter().any(|w| w.contains_point(imx, imy));
                    // Si el guest tiene un botón pulsado hay que seguir
                    // atendiéndolo aunque el puntero cruce la barra superior
                    // (si no, el release nunca llega y el botón queda pegado).
                    let guest_button_down = last_tablet.map_or(false, |(_, _, b)| b != 0);
                    if active_menu.is_none() && !is_mouse_over_floating {
                        if mouse_captured {
                            // En modo capturado PS/2, recibe todos los deltas del puntero sin importar la barra superior
                            let buttons = (window.get_mouse_down(MouseButton::Left) as u8)
                                | ((window.get_mouse_down(MouseButton::Right) as u8) << 1)
                                | ((window.get_mouse_down(MouseButton::Middle) as u8) << 2);

                            let mut dx: i16 = 0;
                            let mut dy: i16 = 0;
                            if let Some((lx, ly)) = last_mouse {
                                let (_vp_x, _vp_y, vp_w, vp_h) = current_viewport;
                                let scale_x = (lw as f32) / (vp_w as f32).max(1.0);
                                let scale_y = (lh as f32) / (vp_h as f32).max(1.0);
                                let (cdx, cdy) = compute_mouse_deltas_scaled(
                                    lx, ly, mx, my,
                                    scale_x, scale_y,
                                    &mut accum_mouse_x, &mut accum_mouse_y,
                                );
                                dx = cdx;
                                dy = cdy;
                            }
                            last_mouse = Some((mx, my));
                            if dx != 0 || dy != 0 || buttons != last_buttons {
                                let mut q = mouse_queue.lock().unwrap();
                                q.push_back((dx, dy, buttons));
                                last_buttons = buttons;
                            }
                        } else if imy >= TOP_BAR_H || (mouse_integration && guest_button_down) {
                            let buttons = (window.get_mouse_down(MouseButton::Left) as u8)
                                | ((window.get_mouse_down(MouseButton::Right) as u8) << 1)
                                | ((window.get_mouse_down(MouseButton::Middle) as u8) << 2);

                            if mouse_integration {
                                // `vp_*` es exactamente el rectángulo donde el
                                // renderer dibuja el frame del guest (letterbox y
                                // barra superior ya descontados).
                                let (vp_x, vp_y, vp_w, vp_h) = current_viewport;
                                let norm_x = if vp_w > 1 {
                                    (((mx - vp_x as f32) / (vp_w as f32 - 1.0)) * 32767.0).clamp(0.0, 32767.0) as u16
                                } else {
                                    16384
                                };
                                let norm_y = if vp_h > 1 {
                                    (((my - vp_y as f32) / (vp_h as f32 - 1.0)) * 32767.0).clamp(0.0, 32767.0) as u16
                                } else {
                                    16384
                                };

                                // Traza de calibración opcional: muestra el
                                // rectángulo exacto usado para mapear y los
                                // valores que recibe el guest.
                                if mouse_debug
                                    && (mouse_debug_last != Some((norm_x, norm_y, buttons))
                                        || last_mouse_debug_log.elapsed()
                                            >= std::time::Duration::from_secs(1))
                                {
                                    mouse_debug_last = Some((norm_x, norm_y, buttons));
                                    last_mouse_debug_log = std::time::Instant::now();
                                    crate::tui::log(&format!(
                                        "[MOUSE] guest={}x{} virt={} off=({},{}) win={}x{} vp=({},{}) {}x{} raw=({:.1},{:.1}) norm=({},{}) btn={}",
                                        lw, lh, virt_w, x_off, y_off, ww, wh, vp_x, vp_y, vp_w, vp_h,
                                        mx, my, norm_x, norm_y, buttons
                                    ));
                                }

                                let wheel = if let Some((_sx, sy)) = window.get_scroll_wheel() {
                                    sy.clamp(-127.0, 127.0) as i8
                                } else {
                                    0
                                };

                                // Además de en cada cambio, se re-afirma la
                                // posición cada TABLET_RESYNC: así cualquier
                                // desfase del cursor del guest se corrige solo
                                // (sin esto el desfase era permanente).
                                let pos_changed = last_tablet != Some((norm_x, norm_y, buttons));
                                let resync_due = last_tablet_sync.elapsed() >= TABLET_RESYNC;
                                if pos_changed || wheel != 0 || resync_due {
                                    last_tablet = Some((norm_x, norm_y, buttons));
                                    last_tablet_sync = std::time::Instant::now();
                                    let mut tq = tablet_queue.lock().unwrap();
                                    tq.push_back((norm_x, norm_y, buttons, wheel));
                                }
                            }
                        } else {
                            last_mouse = None;
                        }
                    } else {
                        last_mouse = None;
                    }
                } else {
                    last_mouse = None;
                }

                // ── Renderizado y Refresco de la Ventana ──
                let is_mouse_over_top_bar = mouse_y < TOP_BAR_H;
                let has_floating = !floating_windows.is_empty();
                let ui_needs_update = active_menu.is_some() || toast.is_some() || has_floating || is_mouse_over_top_bar;

                if is_dirty || force_full_redraw || ui_needs_update {
                    let cur_scale = (ww as f32 / lw.max(1) as f32).round().max(1.0) as u32;
                    metrics.display_scale.store(cur_scale, Ordering::Relaxed);
                    if window_buf.len() != ww * wh {
                        window_buf.resize(ww * wh, 0);
                    }

                    current_viewport = scale_framebuffer_with_offset(
                        &mut window_buf,
                        ww,
                        wh,
                        &render_buf,
                        lw,
                        lh,
                        preserve_aspect,
                        TOP_BAR_H,
                    );

                    let is_paused = metrics.is_paused.load(Ordering::Relaxed);

                    render_top_bar(
                        &mut window_buf,
                        ww,
                        wh,
                        active_menu,
                        mouse_integration,
                        mouse_captured,
                        is_paused,
                        mouse_x,
                        mouse_y,
                    );

                    if let Some(menu_idx) = active_menu {
                        render_dropdown_menu(
                            &mut window_buf,
                            ww,
                            wh,
                            menu_idx,
                            mouse_x,
                            mouse_y,
                            mouse_integration,
                            auto_resize_guest,
                            preserve_aspect,
                            lw,
                            lh,
                        );
                    }

                    for fw in &floating_windows {
                        match fw.kind {
                            FloatingKind::Hud => {
                                render_floating_hud(
                                    &mut window_buf,
                                    ww,
                                    wh,
                                    fw,
                                    &metrics,
                                    lw,
                                    lh,
                                    bpp,
                                    is_vbe,
                                    mouse_integration,
                                    mouse_captured,
                                    preserve_aspect,
                                    mouse_x,
                                    mouse_y,
                                );
                            }
                            FloatingKind::CloseVm => {
                                render_floating_close_dialog(
                                    &mut window_buf,
                                    ww,
                                    wh,
                                    fw,
                                    close_dialog_choice,
                                    mouse_x,
                                    mouse_y,
                                );
                            }
                            FloatingKind::Preferences => {
                                render_floating_preferences(
                                    &mut window_buf,
                                    ww,
                                    wh,
                                    fw,
                                    auto_resize_guest,
                                    preserve_aspect,
                                    user_mouse_integration,
                                    mouse_x,
                                    mouse_y,
                                );
                            }
                            FloatingKind::About => {
                                render_floating_about(
                                    &mut window_buf,
                                    ww,
                                    wh,
                                    fw,
                                    mouse_x,
                                    mouse_y,
                                );
                            }
                            FloatingKind::ShortcutsHelp => {
                                render_floating_shortcuts_help(
                                    &mut window_buf,
                                    ww,
                                    wh,
                                    fw,
                                    mouse_x,
                                    mouse_y,
                                );
                            }
                        }
                    }

                    if let Some((ref msg, ts)) = toast {
                        if ts.elapsed() < std::time::Duration::from_millis(2500) {
                            render_toast(&mut window_buf, ww, wh, msg);
                        } else {
                            toast = None;
                        }
                    }

                    if let Err(e) = window.update_with_buffer(&window_buf, ww, wh) {
                        let err_msg = format!("[DISPLAY] Error de actualización de ventana: {}", e);
                        crate::tui::log(&err_msg);
                        break;
                    }
                    force_full_redraw = false;
                } else {
                    window.update();
                    thread::sleep(std::time::Duration::from_millis(16));
                }

                frame_count += 1;
                let fps_elapsed = last_fps_time.elapsed();
                if fps_elapsed >= std::time::Duration::from_millis(500) {
                    let fps = (frame_count as f64 / fps_elapsed.as_secs_f64()).round() as u32;
                    metrics.display_fps.store(fps, Ordering::Relaxed);
                    frame_count = 0;
                    last_fps_time = std::time::Instant::now();
                }

                // ── Entrada del host: make/break de teclas estándar (sin Host Key) ──
                if !host_down {
                    for key in window.get_keys_pressed(KeyRepeat::Yes) {
                        if is_printable_key(key) {
                            if ctrl_held.load(Ordering::Relaxed) || lalt_held.load(Ordering::Relaxed) {
                                let makes = key_to_ps2_scancodes(key);
                                if !makes.is_empty() {
                                    let mut q = kbd_queue.lock().unwrap();
                                    push_bytes(&mut q, &makes);
                                    held.insert(key, makes);
                                }
                            }
                            continue;
                        }
                        if key == Key::Pause {
                            let mut q = kbd_queue.lock().unwrap();
                            push_bytes(&mut q, &[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]);
                            continue;
                        }
                        if matches!(
                            key,
                            Key::NumPad0 | Key::NumPad1 | Key::NumPad2 | Key::NumPad3 | Key::NumPad4
                                | Key::NumPad5 | Key::NumPad6 | Key::NumPad7 | Key::NumPad8
                                | Key::NumPad9 | Key::NumPadDot
                        ) && numpad_on.load(Ordering::Relaxed)
                        {
                            continue;
                        }
                        if matches!(
                            key,
                            Key::NumPadSlash | Key::NumPadAsterisk | Key::NumPadMinus | Key::NumPadPlus
                        ) {
                            continue;
                        }
                        let Some((scan, ext)) = control_scancode(key) else { continue };
                        let is_new = !held.contains_key(&key);
                        if is_new {
                            match key {
                                Key::CapsLock => {
                                    caps_on.fetch_xor(true, Ordering::Relaxed);
                                }
                                Key::NumLock => {
                                    numpad_on.fetch_xor(true, Ordering::Relaxed);
                                }
                                Key::LeftCtrl | Key::RightCtrl => {
                                    ctrl_held.store(true, Ordering::Relaxed);
                                }
                                Key::LeftAlt => {
                                    lalt_held.store(true, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                        let mut makes = Vec::with_capacity(3);
                        if ext {
                            makes.push(0xE0);
                        }
                        makes.push(scan);
                        let mut q = kbd_queue.lock().unwrap();
                        push_bytes(&mut q, &makes);
                        held.insert(key, makes);
                    }
                }

                for key in window.get_keys_released() {
                    if let Some(makes) = held.remove(&key) {
                        let mut breaks = makes.clone();
                        if let Some(last) = breaks.last_mut() {
                            *last |= 0x80; // break = make | 0x80 (Set 1)
                        }
                        let mut q = kbd_queue.lock().unwrap();
                        push_bytes(&mut q, &breaks);
                        match key {
                            Key::LeftCtrl | Key::RightCtrl => {
                                ctrl_held.store(false, Ordering::Relaxed);
                            }
                            Key::LeftAlt => {
                                lalt_held.store(false, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                }
            }

            // ── Cierre de ventana (tarea 17) ──────────────────────
            crate::SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
            let bsp_tid = crate::BSP_TID.load(Ordering::Relaxed);
            if bsp_tid > 0 {
                unsafe {
                    libc::syscall(libc::SYS_tgkill, libc::getpid(), bsp_tid, libc::SIGTERM);
                }
            }
            running_clone.store(false, Ordering::Relaxed);
            crate::tui::log("[DISPLAY] Ventana de visualización cerrada — apagando la VM...");
        });

        Some(Self { running })
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// Captura y guarda el framebuffer actual (VBE, VGA gráfico o texto) en formato PPM (P6)
/// para diagnóstico e inspección visual automatizada.
pub fn save_ppm_screenshot(
    vga_state: &Arc<Mutex<VgaState>>,
    guest_mem: &GuestMemory,
    path: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    let st = vga_state.lock().unwrap();
    let (w, h, bpp) = st.get_resolution();
    let is_vbe = st.is_vbe_enabled();
    let is_std_gfx = !is_vbe && st.is_standard_vga_graphics();
    let (lw, lh) = if is_vbe {
        (w, h)
    } else if is_std_gfx {
        st.standard_vga_geometry()
    } else {
        (640, 400)
    };
    let mut render_buf = vec![0u32; lw * lh];
    if is_vbe {
        if !st.vram_ptr.is_null() {
            let mut hashes = vec![0u64; lh];
            let virt_w = st.dispi_regs[VBE_DISPI_INDEX_VIRT_WIDTH as usize] as usize;
            let x_off = st.dispi_regs[VBE_DISPI_INDEX_X_OFFSET as usize] as usize;
            let y_off = st.dispi_regs[VBE_DISPI_INDEX_Y_OFFSET as usize] as usize;
            let dac8 = st.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_8BIT_DAC != 0;
            render_vbe_framebuffer(
                &mut render_buf,
                st.vram_ptr,
                st.vram_size,
                lw,
                lh,
                bpp,
                virt_w,
                x_off,
                y_off,
                &st.dac_palette,
                dac8,
                &mut hashes,
                true,
            );
        }
    } else if is_std_gfx {
        if !st.vram_ptr.is_null() {
            let mut hash = 0u64;
            render_vga_graphics(&mut render_buf, &st, lw, lh, &mut hash, true);
        }
    } else {
        let mut hash = 0u64;
        render_vga_text_mode(&mut render_buf, guest_mem, &mut hash, true);
    }
    drop(st);

    let mut file = std::fs::File::create(path)?;
    write!(file, "P6\n{} {}\n255\n", lw, lh)?;
    let mut rgb = Vec::with_capacity(lw * lh * 3);
    for &px in &render_buf {
        rgb.push(((px >> 16) & 0xFF) as u8);
        rgb.push(((px >> 8) & 0xFF) as u8);
        rgb.push((px & 0xFF) as u8);
    }
    file.write_all(&rgb)?;
    Ok(())
}

/// Hash ultra-rápido de 64 bits para detección de scanlines y regiones sucias (dirty tracking)
/// Traducido conceptualmente de DevVGA.cpp de VirtualBox para evitar re-renderizados innecesarios.
#[inline]
pub fn fast_scanline_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let chunks = bytes.chunks_exact(8);
    let remainder = chunks.remainder();
    for chunk in chunks {
        let val = u64::from_ne_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
            chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        hash = (hash ^ val).wrapping_mul(0x100000001b3);
    }
    if !remainder.is_empty() {
        let mut last = [0u8; 8];
        last[..remainder.len()].copy_from_slice(remainder);
        let val = u64::from_ne_bytes(last);
        hash = (hash ^ val).wrapping_mul(0x100000001b3);
    }
    hash
}

/// Renderiza la memoria de texto VGA en 0xB8000 a un buffer RGB de 640x400.
/// Devuelve true si el contenido cambió respecto al último frame.
fn render_vga_text_mode(
    buffer: &mut [u32],
    mem: &GuestMemory,
    last_text_hash: &mut u64,
    force_full: bool,
) -> bool {
    const COLS: usize = 80;
    const ROWS: usize = 25;
    const CHAR_W: usize = 8;
    const CHAR_H: usize = 16;
    const WIDTH: usize = COLS * CHAR_W; // 640
    const TEXT_OFFSET: usize = 0xB8000;
    const TEXT_SIZE: usize = COLS * ROWS * 2;

    if mem.size() < TEXT_OFFSET + TEXT_SIZE {
        buffer.fill(0);
        return false;
    }

    // Copia acotada de la pantalla de texto (el guest la escribe en paralelo).
    let mut text = [0u8; TEXT_SIZE];
    let _ = mem.copy_from(TEXT_OFFSET, &mut text);
    let hash = fast_scanline_hash(&text);

    if !force_full && *last_text_hash == hash {
        return false;
    }
    *last_text_hash = hash;

    for row in 0..ROWS {
        for col in 0..COLS {
            let cell_idx = (row * COLS + col) * 2;
            let ch = text[cell_idx] as usize;
            let attr = text[cell_idx + 1] as usize;

            let fg_color = VGA_PALETTE[attr & 0x0F];
            let bg_color = VGA_PALETTE[(attr >> 4) & 0x0F];

            let glyph_offset = ch * CHAR_H;

            for y in 0..CHAR_H {
                let font_byte = FONT_8X16[glyph_offset + y];
                let py = row * CHAR_H + y;

                for x in 0..CHAR_W {
                    let px = col * CHAR_W + x;
                    let bit = (font_byte >> (7 - x)) & 1;
                    let pixel_color = if bit != 0 { fg_color } else { bg_color };
                    let buf_idx = py * WIDTH + px;
                    if buf_idx < buffer.len() {
                        buffer[buf_idx] = pixel_color;
                    }
                }
            }
        }
    }
    true
}

// ─── (tarea 2) Renderizado de modos gráficos VGA estándar ──────────

/// Lee un byte de VRAM con el addressing planar clásico: el offset de 16 bits
/// del modo se interpreta dentro del plano indicado (cada plano empieza en
/// su propio bloque de 64 KiB). Con bounds-check para no leer fuera de VRAM.
#[inline]
fn vga_plane_read(vram: *const u8, addr: usize, plane: usize, vram_size: usize) -> u8 {
    let off = (addr & 0xFFFF) + 0x10000 * plane;
    if off < vram_size.min(VGA_ADDRABLE) {
        unsafe { *vram.add(off) }
    } else {
        0xFF
    }
}

/// Construye la LUT de 256 colores del DAC (0x00RRGGBB) para los modos
/// palette-index (4/8 bpp). Con 6 bits por canal (por defecto) se escalan
/// a 8 bits; con VBE_DISPI_8BIT_DAC el DAC ya entrega 8 bits por canal.
fn build_dac_lut(dac: &[u8; 768], eight_bit: bool) -> [u32; 256] {
    let mut lut = [0u32; 256];
    let conv = |v: u8| -> u32 {
        if eight_bit {
            v as u32
        } else {
            ((v & 0x3F) as u32) * 255 / 63
        }
    };
    for i in 0..256 {
        let r = conv(dac[i * 3]);
        let g = conv(dac[i * 3 + 1]);
        let b = conv(dac[i * 3 + 2]);
        lut[i] = (r << 16) | (g << 8) | b;
    }
    lut
}

/// Renderiza un modo gráfico VGA estándar decodificando los registros
/// reales del hardware emulado (Sequencer, Graphics Controller, Attribute
/// Controller y DAC).
fn render_vga_graphics(
    buffer: &mut [u32],
    st: &VgaState,
    width: usize,
    height: usize,
    last_planar_hash: &mut u64,
    force_full: bool,
) -> bool {
    let chain4 = st.seq_regs[0x04] & 0x08 != 0;
    let oe_seq = st.seq_regs[0x01] & 0x08 != 0;
    let g_mode = st.grc_regs[0x05] & 0x20 != 0;
    let shift_2 = st.grc_regs[0x05] & 0x02 != 0;
    let attr_8bit = st.attr_regs[0x10] & 0x80 != 0;
    let dac8 = st.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_8BIT_DAC != 0;
    let lut = build_dac_lut(&st.dac_palette, dac8);

    let palette_idx = |raw: u8| -> usize {
        (if attr_8bit { raw } else { raw & 0x0F }) as usize & 0xFF
    };

    let row_bytes = ((st.crtc_regs[0x13] as usize) | (((st.crtc_regs[0x14] as usize) & 0x3F) << 8)) * 2;

    #[derive(PartialEq, Clone, Copy)]
    enum AddrMode {
        Packed,
        Planar4,
        Cga2,
    }
    let addr_mode: AddrMode = if chain4 {
        AddrMode::Packed
    } else if oe_seq && shift_2 && g_mode {
        AddrMode::Cga2
    } else if g_mode {
        AddrMode::Planar4
    } else {
        buffer.fill(0x000000);
        return false;
    };

    let vram = st.vram_ptr;
    let vram_size = st.vram_size;
    let raw = unsafe { std::slice::from_raw_parts(vram, vram_size.min(VGA_ADDRABLE)) };
    let hash = fast_scanline_hash(raw);
    if !force_full && *last_planar_hash == hash {
        return false;
    }
    *last_planar_hash = hash;

    for row in 0..height.min(1024) {
        for col in 0..width.min(2048) {
            let buf_idx = row * width + col;
            if buf_idx >= buffer.len() {
                continue;
            }
            let color: u32 = match addr_mode {
                AddrMode::Packed => {
                    let a = (row * width + col) & 0xFFFF;
                    let b = (row * width + col) >> 16;
                    let off = (((a & !3) << 2) + (b << 14) + (a & 3)) & 0xFFFF;
                    lut[vga_plane_read(vram, off, 0, vram_size) as usize]
                }
                AddrMode::Planar4 => {
                    let row_bytes = if row_bytes >= width / 8 { row_bytes } else { width / 8 };
                    let off = row * row_bytes + (col >> 3);
                    let bit = 7 - (col & 7);
                    let mut pal = 0u8;
                    for plane in 0..4usize {
                        let b = vga_plane_read(vram, off, plane, vram_size);
                        pal |= ((b >> bit) & 1) << plane;
                    }
                    lut[palette_idx(st.attr_regs[pal as usize])]
                }
                AddrMode::Cga2 => {
                    let row_bytes = if row_bytes >= width / 8 { row_bytes } else { width / 8 };
                    let off = (row >> 1) * row_bytes + (col >> 3) + ((row & 1) << 13);
                    let b0 = vga_plane_read(vram, off, 0, vram_size);
                    let b1 = vga_plane_read(vram, off, 1, vram_size);
                    let bit = 7 - (col & 7);
                    let pal = (((b0 >> bit) & 1) | (((b1 >> bit) & 1) << 1)) as usize;
                    lut[palette_idx(st.attr_regs[pal])]
                }
            };
            buffer[buf_idx] = color;
        }
    }
    true
}

// ─── (tarea 15) VBE con VIRT_WIDTH / X_OFFSET / Y_OFFSET ───────────

#[allow(clippy::too_many_arguments)]
fn render_vbe_framebuffer(
    buffer: &mut [u32],
    vram_ptr: *const u8,
    vram_size: usize,
    width: usize,
    height: usize,
    bpp: usize,
    virt_width: usize,
    x_offset: usize,
    y_offset: usize,
    dac: &[u8; 768],
    dac8: bool,
    scanline_hashes: &mut [u64],
    force_full: bool,
) -> bool {
    if width == 0 || height == 0 || vram_size == 0 {
        return false;
    }
    let vw = if virt_width >= width { virt_width } else { width };
    let bytes_pp = match bpp {
        32 => 4usize,
        24 => 3,
        16 | 15 => 2,
        8 | 4 => 1,
        _ => {
            buffer.fill(0);
            return false;
        }
    };
    let row_bytes = vw * bytes_pp;
    let row_bytes = if y_offset + height > 0 && (y_offset + height - 1) * row_bytes + row_bytes > vram_size {
        width * bytes_pp
    } else {
        row_bytes
    };
    let (xo, yo) = if x_offset + width <= vw { (x_offset, y_offset) } else { (0, y_offset) };
    let mut any_dirty = false;

    match bpp {
        32 => {
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo * 4;
                if src_row + width * 4 > vram_size {
                    let start = y * width;
                    let end = (start + width).min(buffer.len());
                    buffer[start..end].fill(0);
                    continue;
                }
                let raw_line = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width * 4) };
                let hash = fast_scanline_hash(raw_line);
                if !force_full && scanline_hashes[y] == hash {
                    continue;
                }
                scanline_hashes[y] = hash;
                any_dirty = true;

                let src_slice =
                    unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row) as *const u32, width) };
                let dst_slice = &mut buffer[y * width..(y + 1) * width];
                for (dst_px, &src_px) in dst_slice.iter_mut().zip(src_slice.iter()) {
                    *dst_px = src_px & 0x00FF_FFFF;
                }
            }
        }
        24 => {
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo * 3;
                if src_row + width * 3 > vram_size {
                    let start = y * width;
                    let end = (start + width).min(buffer.len());
                    buffer[start..end].fill(0);
                    continue;
                }
                let raw_line = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width * 3) };
                let hash = fast_scanline_hash(raw_line);
                if !force_full && scanline_hashes[y] == hash {
                    continue;
                }
                scanline_hashes[y] = hash;
                any_dirty = true;

                let src = raw_line;
                for x in 0..width {
                    let b = src[x * 3] as u32;
                    let g = src[x * 3 + 1] as u32;
                    let r = src[x * 3 + 2] as u32;
                    buffer[y * width + x] = (r << 16) | (g << 8) | b;
                }
            }
        }
        16 => {
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo * 2;
                if src_row + width * 2 > vram_size {
                    let start = y * width;
                    let end = (start + width).min(buffer.len());
                    buffer[start..end].fill(0);
                    continue;
                }
                let raw_line = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width * 2) };
                let hash = fast_scanline_hash(raw_line);
                if !force_full && scanline_hashes[y] == hash {
                    continue;
                }
                scanline_hashes[y] = hash;
                any_dirty = true;

                let src = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row) as *const u16, width) };
                for (x, &p) in src.iter().enumerate() {
                    let r = (((p >> 11) & 0x1F) * 255 / 31) as u32;
                    let g = (((p >> 5) & 0x3F) * 255 / 63) as u32;
                    let b = ((p & 0x1F) * 255 / 31) as u32;
                    buffer[y * width + x] = (r << 16) | (g << 8) | b;
                }
            }
        }
        15 => {
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo * 2;
                if src_row + width * 2 > vram_size {
                    let start = y * width;
                    let end = (start + width).min(buffer.len());
                    buffer[start..end].fill(0);
                    continue;
                }
                let raw_line = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width * 2) };
                let hash = fast_scanline_hash(raw_line);
                if !force_full && scanline_hashes[y] == hash {
                    continue;
                }
                scanline_hashes[y] = hash;
                any_dirty = true;

                let src = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row) as *const u16, width) };
                for (x, &p) in src.iter().enumerate() {
                    let r = (((p >> 10) & 0x1F) * 255 / 31) as u32;
                    let g = (((p >> 5) & 0x1F) * 255 / 31) as u32;
                    let b = ((p & 0x1F) * 255 / 31) as u32;
                    buffer[y * width + x] = (r << 16) | (g << 8) | b;
                }
            }
        }
        8 => {
            let lut = build_dac_lut(dac, dac8);
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo;
                if src_row + width > vram_size {
                    let start = y * width;
                    let end = (start + width).min(buffer.len());
                    buffer[start..end].fill(0);
                    continue;
                }
                let raw_line = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width) };
                let hash = fast_scanline_hash(raw_line);
                if !force_full && scanline_hashes[y] == hash {
                    continue;
                }
                scanline_hashes[y] = hash;
                any_dirty = true;

                let src = raw_line;
                for (x, &idx) in src.iter().enumerate() {
                    buffer[y * width + x] = lut[idx as usize];
                }
            }
        }
        4 => {
            let lut = build_dac_lut(dac, dac8);
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo / 2;
                if src_row + (width + 1) / 2 > vram_size {
                    let start = y * width;
                    let end = (start + width).min(buffer.len());
                    buffer[start..end].fill(0);
                    continue;
                }
                let raw_line = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), (width + 1) / 2) };
                let hash = fast_scanline_hash(raw_line);
                if !force_full && scanline_hashes[y] == hash {
                    continue;
                }
                scanline_hashes[y] = hash;
                any_dirty = true;

                let src = raw_line;
                for (x, &byte) in src.iter().enumerate() {
                    let px = y * width + x * 2;
                    buffer[px] = lut[(byte >> 4) as usize];
                    if x * 2 + 1 < width {
                        buffer[px + 1] = lut[(byte & 0x0F) as usize];
                    }
                }
            }
        }
        _ => {
            buffer.fill(0);
        }
    }
    any_dirty
}

/// Escalado bilineal de alta velocidad en espacio de color RGB (8:8:8:8).
/// Elimina el efecto pixelado/pixelation suavizando los bordes mediante interpolación 2D.
pub fn scale_bilinear(dst: &mut [u32], dw: usize, dh: usize, src: &[u32], sw: usize, sh: usize) {
    if dw == 0 || dh == 0 || sw == 0 || sh == 0 {
        dst.fill(0);
        return;
    }
    if dw == sw && dh == sh && dst.len() == src.len() {
        dst.copy_from_slice(src);
        return;
    }

    let x_ratio = if dw > 1 { (((sw - 1) as u32) << 16) / ((dw - 1) as u32) } else { 0 };
    let y_ratio = if dh > 1 { (((sh - 1) as u32) << 16) / ((dh - 1) as u32) } else { 0 };

    for y in 0..dh {
        let sy = ((y as u32 * y_ratio) >> 16) as usize;
        let y_diff = ((y as u32 * y_ratio) & 0xFFFF) as u32;
        let y_diff_inv = 65536 - y_diff;

        let sy_next = (sy + 1).min(sh - 1);
        let row0 = &src[sy * sw..(sy + 1) * sw];
        let row1 = &src[sy_next * sw..(sy_next + 1) * sw];
        let dst_row = &mut dst[y * dw..(y + 1) * dw];

        for (x, px) in dst_row.iter_mut().enumerate() {
            let sx = ((x as u32 * x_ratio) >> 16) as usize;
            let x_diff = ((x as u32 * x_ratio) & 0xFFFF) as u32;
            let x_diff_inv = 65536 - x_diff;
            let sx_next = (sx + 1).min(sw - 1);

            let a = row0[sx];
            let b = row0[sx_next];
            let c = row1[sx];
            let d = row1[sx_next];

            let w00 = ((x_diff_inv as u64) * (y_diff_inv as u64)) >> 16;
            let w01 = ((x_diff as u64) * (y_diff_inv as u64)) >> 16;
            let w10 = ((x_diff_inv as u64) * (y_diff as u64)) >> 16;
            let w11 = ((x_diff as u64) * (y_diff as u64)) >> 16;

            let blue = ((a & 0xFF) as u64 * w00
                + (b & 0xFF) as u64 * w01
                + (c & 0xFF) as u64 * w10
                + (d & 0xFF) as u64 * w11) >> 16;

            let green = (((a >> 8) & 0xFF) as u64 * w00
                + ((b >> 8) & 0xFF) as u64 * w01
                + ((c >> 8) & 0xFF) as u64 * w10
                + ((d >> 8) & 0xFF) as u64 * w11) >> 16;

            let red = (((a >> 16) & 0xFF) as u64 * w00
                + ((b >> 16) & 0xFF) as u64 * w01
                + ((c >> 16) & 0xFF) as u64 * w10
                + ((d >> 16) & 0xFF) as u64 * w11) >> 16;

            let alpha = (((a >> 24) & 0xFF) as u64 * w00
                + ((b >> 24) & 0xFF) as u64 * w01
                + ((c >> 24) & 0xFF) as u64 * w10
                + ((d >> 24) & 0xFF) as u64 * w11) >> 16;

            *px = ((alpha as u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | (blue as u32);
        }
    }
}

/// Copia 1:1 (sin escalado) de `rows` filas de `src` a `dst`, en la posición
/// `(dst_x, dst_y0)`. Aislada en su propia función para no alterar el codegen
/// del camino bilineal.
#[inline(never)]
fn blit_rows_1to1(
    dst: &mut [u32],
    dw: usize,
    src: &[u32],
    sw: usize,
    rows: usize,
    dst_x: usize,
    dst_y0: usize,
) {
    for y in 0..rows {
        let d = (dst_y0 + y) * dw + dst_x;
        dst[d..d + sw].copy_from_slice(&src[y * sw..(y + 1) * sw]);
    }
}

/// Escalado del framebuffer lógico al tamaño de ventana con opción de preservación
/// de relación de aspecto y centrado con bandas negras (VirtualBox letterbox/pillarbox).
///
/// Devuelve la tupla `(vp_x, vp_y, vp_w, vp_h)` que delimita el viewport efectivo en `dst`.
/// Escalado del framebuffer lógico al tamaño de ventana con opción de compensación superior
/// (para la barra de herramientas de VirtualBox) y centrado con bandas negras.
pub fn scale_framebuffer_with_offset(
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    src: &[u32],
    sw: usize,
    sh: usize,
    preserve_aspect: bool,
    top_offset: usize,
) -> (usize, usize, usize, usize) {
    if dw == 0 || dh <= top_offset || sw == 0 || sh == 0 || dst.len() != dw * dh || src.len() != sw * sh {
        dst.fill(0);
        return (0, top_offset, dw, dh.saturating_sub(top_offset));
    }

    if top_offset == 0 && dw == sw && dh == sh {
        dst.copy_from_slice(src);
        return (0, 0, dw, dh);
    }

    let avail_h = dh - top_offset;

    if !preserve_aspect {
        // Camino 1:1 exacto (caso habitual: la ventana mide lo mismo que el
        // framebuffer). El bilineal con x_diff = y_diff = 0 devuelve el píxel
        // origen sin cambios, así que copiar filas es equivalente y evita
        // interpolar ~2 MP por frame (medidos 14-18 ms a 1080p).
        if dw == sw && avail_h == sh {
            blit_rows_1to1(dst, dw, src, sw, avail_h, 0, top_offset);
            return (0, top_offset, dw, avail_h);
        }
        let x_ratio = if dw > 1 { (((sw - 1) as u32) << 16) / ((dw - 1) as u32) } else { 0 };
        let y_ratio = if avail_h > 1 { (((sh - 1) as u32) << 16) / ((avail_h - 1) as u32) } else { 0 };

        for y in 0..avail_h {
            let sy = ((y as u32 * y_ratio) >> 16) as usize;
            let y_diff = ((y as u32 * y_ratio) & 0xFFFF) as u32;
            let y_diff_inv = 65536 - y_diff;

            let sy_next = (sy + 1).min(sh - 1);
            let row0 = &src[sy * sw..(sy + 1) * sw];
            let row1 = &src[sy_next * sw..(sy_next + 1) * sw];
            let dst_row_start = (top_offset + y) * dw;
            let dst_row = &mut dst[dst_row_start..dst_row_start + dw];

            for (x, px) in dst_row.iter_mut().enumerate() {
                let sx = ((x as u32 * x_ratio) >> 16) as usize;
                let x_diff = ((x as u32 * x_ratio) & 0xFFFF) as u32;
                let x_diff_inv = 65536 - x_diff;
                let sx_next = (sx + 1).min(sw - 1);

                let a = row0[sx];
                let b = row0[sx_next];
                let c = row1[sx];
                let d = row1[sx_next];

                let w00 = ((x_diff_inv as u64) * (y_diff_inv as u64)) >> 16;
                let w01 = ((x_diff as u64) * (y_diff_inv as u64)) >> 16;
                let w10 = ((x_diff_inv as u64) * (y_diff as u64)) >> 16;
                let w11 = ((x_diff as u64) * (y_diff as u64)) >> 16;

                let blue = ((a & 0xFF) as u64 * w00
                    + (b & 0xFF) as u64 * w01
                    + (c & 0xFF) as u64 * w10
                    + (d & 0xFF) as u64 * w11) >> 16;

                let green = (((a >> 8) & 0xFF) as u64 * w00
                    + ((b >> 8) & 0xFF) as u64 * w01
                    + ((c >> 8) & 0xFF) as u64 * w10
                    + ((d >> 8) & 0xFF) as u64 * w11) >> 16;

                let red = (((a >> 16) & 0xFF) as u64 * w00
                    + ((b >> 16) & 0xFF) as u64 * w01
                    + ((c >> 16) & 0xFF) as u64 * w10
                    + ((d >> 16) & 0xFF) as u64 * w11) >> 16;

                let alpha = (((a >> 24) & 0xFF) as u64 * w00
                    + ((b >> 24) & 0xFF) as u64 * w01
                    + ((c >> 24) & 0xFF) as u64 * w10
                    + ((d >> 24) & 0xFF) as u64 * w11) >> 16;

                *px = ((alpha as u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | (blue as u32);
            }
        }
        return (0, top_offset, dw, avail_h);
    }

    let scale_x = dw as f32 / sw as f32;
    let scale_y = avail_h as f32 / sh as f32;
    let scale = scale_x.min(scale_y);

    let vp_w = ((sw as f32 * scale).round() as usize).clamp(1, dw);
    let vp_h = ((sh as f32 * scale).round() as usize).clamp(1, avail_h);
    let vp_x = (dw - vp_w) / 2;
    let vp_y = top_offset + (avail_h - vp_h) / 2;

    // Limpiar únicamente las bandas negras (letterbox superior/inferior y pillarbox izquierdo/derecho)
    // en lugar de borrar el área central de vp_w × vp_h que se va a sobrescribir inmediatamente.
    if vp_y > top_offset {
        dst[top_offset * dw..vp_y * dw].fill(0);
    }
    if vp_y + vp_h < dh {
        dst[(vp_y + vp_h) * dw..dh * dw].fill(0);
    }
    if vp_w < dw {
        for y in vp_y..(vp_y + vp_h) {
            let rstart = y * dw;
            if vp_x > 0 {
                dst[rstart..rstart + vp_x].fill(0);
            }
            if vp_x + vp_w < dw {
                dst[rstart + vp_x + vp_w..rstart + dw].fill(0);
            }
        }
    }

    // Viewport 1:1 (sin escalado): copia directa de filas, bit-exacta con el
    // bilineal (x_diff = y_diff = 0 ⇒ peso 100% del píxel origen).
    if vp_w == sw && vp_h == sh {
        blit_rows_1to1(dst, dw, src, sw, vp_h, vp_x, vp_y);
        return (vp_x, vp_y, vp_w, vp_h);
    }

    let x_ratio = if vp_w > 1 { (((sw - 1) as u32) << 16) / ((vp_w - 1) as u32) } else { 0 };
    let y_ratio = if vp_h > 1 { (((sh - 1) as u32) << 16) / ((vp_h - 1) as u32) } else { 0 };

    for y in 0..vp_h {
        let sy = ((y as u32 * y_ratio) >> 16) as usize;
        let y_diff = ((y as u32 * y_ratio) & 0xFFFF) as u32;
        let y_diff_inv = 65536 - y_diff;

        let sy_next = (sy + 1).min(sh - 1);
        let row0 = &src[sy * sw..(sy + 1) * sw];
        let row1 = &src[sy_next * sw..(sy_next + 1) * sw];
        let dst_row_start = (vp_y + y) * dw + vp_x;
        let dst_row = &mut dst[dst_row_start..dst_row_start + vp_w];

        for (x, px) in dst_row.iter_mut().enumerate() {
            let sx = ((x as u32 * x_ratio) >> 16) as usize;
            let x_diff = ((x as u32 * x_ratio) & 0xFFFF) as u32;
            let x_diff_inv = 65536 - x_diff;
            let sx_next = (sx + 1).min(sw - 1);

            let a = row0[sx];
            let b = row0[sx_next];
            let c = row1[sx];
            let d = row1[sx_next];

            let w00 = ((x_diff_inv as u64) * (y_diff_inv as u64)) >> 16;
            let w01 = ((x_diff as u64) * (y_diff_inv as u64)) >> 16;
            let w10 = ((x_diff_inv as u64) * (y_diff as u64)) >> 16;
            let w11 = ((x_diff as u64) * (y_diff as u64)) >> 16;

            let blue = ((a & 0xFF) as u64 * w00
                + (b & 0xFF) as u64 * w01
                + (c & 0xFF) as u64 * w10
                + (d & 0xFF) as u64 * w11) >> 16;

            let green = (((a >> 8) & 0xFF) as u64 * w00
                + ((b >> 8) & 0xFF) as u64 * w01
                + ((c >> 8) & 0xFF) as u64 * w10
                + ((d >> 8) & 0xFF) as u64 * w11) >> 16;

            let red = (((a >> 16) & 0xFF) as u64 * w00
                + ((b >> 16) & 0xFF) as u64 * w01
                + ((c >> 16) & 0xFF) as u64 * w10
                + ((d >> 16) & 0xFF) as u64 * w11) >> 16;

            let alpha = (((a >> 24) & 0xFF) as u64 * w00
                + ((b >> 24) & 0xFF) as u64 * w01
                + ((c >> 24) & 0xFF) as u64 * w10
                + ((d >> 24) & 0xFF) as u64 * w11) >> 16;

            *px = ((alpha as u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | (blue as u32);
        }
    }

    (vp_x, vp_y, vp_w, vp_h)
}

/// Escalado del framebuffer lógico al tamaño de ventana con opción de preservación
/// de relación de aspecto y centrado con bandas negras (VirtualBox letterbox/pillarbox).
///
/// Devuelve la tupla `(vp_x, vp_y, vp_w, vp_h)` que delimita el viewport efectivo en `dst`.
pub fn scale_framebuffer(
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    src: &[u32],
    sw: usize,
    sh: usize,
    preserve_aspect: bool,
) -> (usize, usize, usize, usize) {
    scale_framebuffer_with_offset(dst, dw, dh, src, sw, sh, preserve_aspect, 0)
}

// ─── Entrada de teclado host → PS/2 (Set 1) ───────────────────────
//
// Dos vías complementarias (tarea 19):
//  1. Vía de caracteres (`InputCallback::add_char`): el host compone el
//     carácter final (AltGr, teclas muertas, acentos, mayúsculas) y lo
//     entrega como Unicode. Se traduce a scancodes PS/2 con la distribución
//     ESPAÑOLA (`es`), que es la del guest objetivo.
//  2. Vía de teclas (`get_keys_pressed`/`get_keys_released`): solo teclas
//     NO imprimibles (control, navegación, Ctrl/Alt/Super, toggles) con
//     make/break completos. Las imprimibles se dejan a add_char (si no,
//     habría doble entrega); solo se reenvían posicionalmente con Ctrl/Alt
//     pulsado (atajos tipo Ctrl+C).
//
// Reglas de diseño:
//  - Cada carácter es una secuencia autocontenida: [mods make, scan make,
//    scan break, mods break]. No se reenvía el Shift del host en crudo: el
//    caso (mayúsculas) se sintetiza por carácter con XOR de CapsLock (que
//    sí se reenvía como toggle, así el estado de mayúsculas del guest =
//    host).
//  - AltGr se sintetiza (E0 38 / E0 B8) porque en un host español llega
//    como ISO_Level3_Shift, que minifb no reporta como Key::RightAlt.
//  - KeyRepeat::Yes + makes repetidos = typematic estilo 8042; el guest
//    recibe makes sin breaks mientras se mantiene la tecla y un break al
//    soltar. El tope de la cola y el rate de minifb evitan el
//    desbordamiento del buffer del 8042 que mencionaba el TODO.

/// Capacidad de la cola intermedia host→8042. Las secuencias se insertan
/// de forma atómica (nunca a medias) y se descartan si no caben enteras.
const KBD_QUEUE_CAP: usize = 16;

/// Make/break del Shift izquierdo (Set 1).
const SCAN_LSHIFT_MAKE: u8 = 0x2A;
const SCAN_LSHIFT_BREAK: u8 = 0xAA;
/// Make/break de AltGr (RightAlt): teclas extendidas con prefijo 0xE0.
const SCAN_RALT_MAKE: [u8; 2] = [0xE0, 0x38];
const SCAN_RALT_BREAK: [u8; 2] = [0xE0, 0xB8];

/// Empuja bytes a la cola intermedia solo si cabe la secuencia entera.
fn push_bytes(q: &mut VecDeque<u8>, bytes: &[u8]) {
    if q.len() + bytes.len() <= KBD_QUEUE_CAP {
        q.extend(bytes);
    }
}

/// Un "toque": scancode base + modificadores que deben estar activos
/// durante el make/break, ignorando CapsLock (el caso de las letras se
/// resuelve con XOR de CapsLock en `build_char_sequence`).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Tap {
    scan: u8,
    shift: bool,
    altgr: bool,
    /// Es una letra: el caso se decide con XOR de CapsLock.
    letter: bool,
}

const fn tap(scan: u8, shift: bool, altgr: bool, letter: bool) -> Tap {
    Tap { scan, shift, altgr, letter }
}

/// Traducción carácter → toques PS/2 para la distribución española (`es`,
/// base `latin(type4)`). Scancodes Set 1 base (sin 0xE0). Los acentos son
/// teclas muertas: un carácter compuesto se descompone en [tecla muerta,
/// letra]. Los dígitos y la puntuación también van aquí (en un host español
/// la posición física de la puntuación difiere de la US).
const ES_SPECIAL: &[(char, &[Tap])] = &[
    // Espacio y fila numérica
    (' ', &[tap(0x39, false, false, false)]),
    ('1', &[tap(0x02, false, false, false)]),
    ('2', &[tap(0x03, false, false, false)]),
    ('3', &[tap(0x04, false, false, false)]),
    ('4', &[tap(0x05, false, false, false)]),
    ('5', &[tap(0x06, false, false, false)]),
    ('6', &[tap(0x07, false, false, false)]),
    ('7', &[tap(0x08, false, false, false)]),
    ('8', &[tap(0x09, false, false, false)]),
    ('9', &[tap(0x0A, false, false, false)]),
    ('0', &[tap(0x0B, false, false, false)]),
    // Puntuación nivel 1/2 (sin AltGr)
    ('!', &[tap(0x02, true, false, false)]),
    ('"', &[tap(0x03, true, false, false)]),
    ('·', &[tap(0x04, true, false, false)]),
    ('$', &[tap(0x05, true, false, false)]),
    ('%', &[tap(0x06, true, false, false)]),
    ('&', &[tap(0x07, true, false, false)]),
    ('/', &[tap(0x08, true, false, false)]),
    ('(', &[tap(0x09, true, false, false)]),
    (')', &[tap(0x0A, true, false, false)]),
    ('=', &[tap(0x0B, true, false, false)]),
    ('?', &[tap(0x0C, true, false, false)]),
    ('\'', &[tap(0x0C, false, false, false)]),
    ('¡', &[tap(0x0D, false, false, false)]),
    ('¿', &[tap(0x0D, true, false, false)]),
    ('+', &[tap(0x1B, false, false, false)]),
    ('*', &[tap(0x1B, true, false, false)]),
    (',', &[tap(0x33, false, false, false)]),
    (';', &[tap(0x33, true, false, false)]),
    ('.', &[tap(0x34, false, false, false)]),
    (':', &[tap(0x34, true, false, false)]),
    ('-', &[tap(0x35, false, false, false)]),
    ('_', &[tap(0x35, true, false, false)]),
    ('<', &[tap(0x56, false, false, false)]),
    ('>', &[tap(0x56, true, false, false)]),
    ('º', &[tap(0x29, false, false, false)]),
    ('ª', &[tap(0x29, true, false, false)]),
    // Nivel 3 (AltGr)
    ('|', &[tap(0x02, false, true, false)]),
    ('@', &[tap(0x03, false, true, false)]),
    ('#', &[tap(0x04, false, true, false)]),
    ('~', &[tap(0x05, false, true, false)]),
    ('€', &[tap(0x06, false, true, false)]),
    ('¬', &[tap(0x07, false, true, false)]),
    ('{', &[tap(0x28, false, true, false)]),
    ('}', &[tap(0x2B, false, true, false)]),
    ('[', &[tap(0x1A, false, true, false)]),
    (']', &[tap(0x1B, false, true, false)]),
    ('\\', &[tap(0x0C, false, true, false)]),
    ('µ', &[tap(0x32, false, true, false)]),
    // Letras españolas propias
    ('ñ', &[tap(0x27, false, false, true)]),
    ('Ñ', &[tap(0x27, true, false, true)]),
    ('ç', &[tap(0x2B, false, false, true)]),
    ('Ç', &[tap(0x2B, true, false, true)]),
    // Teclas muertas en solitario (muerta + espacio = el carácter suelto)
    ('`', &[tap(0x1A, false, false, false), tap(0x39, false, false, false)]),
    ('´', &[tap(0x28, false, false, false), tap(0x39, false, false, false)]),
    ('^', &[tap(0x1A, true, false, false), tap(0x39, false, false, false)]),
    ('¨', &[tap(0x28, true, false, false), tap(0x39, false, false, false)]),
    // Acento agudo: ´ + letra
    ('á', &[tap(0x28, false, false, false), tap(0x1E, false, false, true)]),
    ('é', &[tap(0x28, false, false, false), tap(0x12, false, false, true)]),
    ('í', &[tap(0x28, false, false, false), tap(0x17, false, false, true)]),
    ('ó', &[tap(0x28, false, false, false), tap(0x18, false, false, true)]),
    ('ú', &[tap(0x28, false, false, false), tap(0x16, false, false, true)]),
    ('Á', &[tap(0x28, false, false, false), tap(0x1E, true, false, true)]),
    ('É', &[tap(0x28, false, false, false), tap(0x12, true, false, true)]),
    ('Í', &[tap(0x28, false, false, false), tap(0x17, true, false, true)]),
    ('Ó', &[tap(0x28, false, false, false), tap(0x18, true, false, true)]),
    ('Ú', &[tap(0x28, false, false, false), tap(0x16, true, false, true)]),
    // Acento grave: ` + letra
    ('à', &[tap(0x1A, false, false, false), tap(0x1E, false, false, true)]),
    ('è', &[tap(0x1A, false, false, false), tap(0x12, false, false, true)]),
    ('ì', &[tap(0x1A, false, false, false), tap(0x17, false, false, true)]),
    ('ò', &[tap(0x1A, false, false, false), tap(0x18, false, false, true)]),
    ('ù', &[tap(0x1A, false, false, false), tap(0x16, false, false, true)]),
    ('À', &[tap(0x1A, false, false, false), tap(0x1E, true, false, true)]),
    ('È', &[tap(0x1A, false, false, false), tap(0x12, true, false, true)]),
    ('Ì', &[tap(0x1A, false, false, false), tap(0x17, true, false, true)]),
    ('Ò', &[tap(0x1A, false, false, false), tap(0x18, true, false, true)]),
    ('Ù', &[tap(0x1A, false, false, false), tap(0x16, true, false, true)]),
    // Circunflejo: Shift+` + letra
    ('â', &[tap(0x1A, true, false, false), tap(0x1E, false, false, true)]),
    ('ê', &[tap(0x1A, true, false, false), tap(0x12, false, false, true)]),
    ('î', &[tap(0x1A, true, false, false), tap(0x17, false, false, true)]),
    ('ô', &[tap(0x1A, true, false, false), tap(0x18, false, false, true)]),
    ('û', &[tap(0x1A, true, false, false), tap(0x16, false, false, true)]),
    ('Â', &[tap(0x1A, true, false, false), tap(0x1E, true, false, true)]),
    ('Ê', &[tap(0x1A, true, false, false), tap(0x12, true, false, true)]),
    ('Î', &[tap(0x1A, true, false, false), tap(0x17, true, false, true)]),
    ('Ô', &[tap(0x1A, true, false, false), tap(0x18, true, false, true)]),
    ('Û', &[tap(0x1A, true, false, false), tap(0x16, true, false, true)]),
    // Diéresis: Shift+´ + letra
    ('ä', &[tap(0x28, true, false, false), tap(0x1E, false, false, true)]),
    ('ë', &[tap(0x28, true, false, false), tap(0x12, false, false, true)]),
    ('ï', &[tap(0x28, true, false, false), tap(0x17, false, false, true)]),
    ('ö', &[tap(0x28, true, false, false), tap(0x18, false, false, true)]),
    ('ü', &[tap(0x28, true, false, false), tap(0x16, false, false, true)]),
    ('Ä', &[tap(0x28, true, false, false), tap(0x1E, true, false, true)]),
    ('Ë', &[tap(0x28, true, false, false), tap(0x12, true, false, true)]),
    ('Ï', &[tap(0x28, true, false, false), tap(0x17, true, false, true)]),
    ('Ö', &[tap(0x28, true, false, false), tap(0x18, true, false, true)]),
    ('Ü', &[tap(0x28, true, false, false), tap(0x16, true, false, true)]),
    // Tilde: AltGr+ñ + letra
    ('ã', &[tap(0x27, false, true, false), tap(0x1E, false, false, true)]),
    ('õ', &[tap(0x27, false, true, false), tap(0x18, false, false, true)]),
    ('Ã', &[tap(0x27, false, true, false), tap(0x1E, true, false, true)]),
    ('Õ', &[tap(0x27, false, true, false), tap(0x18, true, false, true)]),
];

/// Traduce un carácter Unicode a toques PS/2 para la distribución española.
/// Devuelve `None` si el carácter no está soportado.
fn char_to_es_taps(c: char) -> Option<Vec<Tap>> {
    // Letras: la posición QWERTY es idéntica en es y us; el caso lo decide
    // el flag `letter` (XOR con CapsLock en `build_char_sequence`).
    if c.is_ascii_alphabetic() {
        let scan = match c.to_ascii_lowercase() {
            'a' => 0x1E, 'b' => 0x30, 'c' => 0x2E, 'd' => 0x20, 'e' => 0x12,
            'f' => 0x21, 'g' => 0x22, 'h' => 0x23, 'i' => 0x17, 'j' => 0x24,
            'k' => 0x25, 'l' => 0x26, 'm' => 0x32, 'n' => 0x31, 'o' => 0x18,
            'p' => 0x19, 'q' => 0x10, 'r' => 0x13, 's' => 0x1F, 't' => 0x14,
            'u' => 0x16, 'v' => 0x2F, 'w' => 0x11, 'x' => 0x2D, 'y' => 0x15,
            'z' => 0x2C,
            _ => return None,
        };
        return Some(vec![tap(scan, c.is_ascii_uppercase(), false, true)]);
    }
    ES_SPECIAL
        .iter()
        .find(|(ch, _)| *ch == c)
        .map(|(_, taps)| taps.to_vec())
}

/// Construye la secuencia PS/2 autocontenida para un carácter compuesto del
/// host, en la distribución española. Cada toque aplica/libera los
/// modificadores que necesite (Shift sintetizado con XOR de CapsLock para
/// letras; AltGr sintetizado siempre).
fn build_char_sequence(c: char, caps_on: bool) -> Option<Vec<u8>> {
    let taps = char_to_es_taps(c)?;
    let mut seq = Vec::with_capacity(8);
    let mut shift = false;
    let mut altgr = false;
    for t in taps {
        let need_shift = if t.letter { t.shift ^ caps_on } else { t.shift };
        if need_shift && !shift {
            seq.push(SCAN_LSHIFT_MAKE);
            shift = true;
        } else if !need_shift && shift {
            seq.push(SCAN_LSHIFT_BREAK);
            shift = false;
        }
        if t.altgr && !altgr {
            seq.extend_from_slice(&SCAN_RALT_MAKE);
            altgr = true;
        } else if !t.altgr && altgr {
            seq.extend_from_slice(&SCAN_RALT_BREAK);
            altgr = false;
        }
        seq.push(t.scan);
        seq.push(t.scan | 0x80);
    }
    if shift {
        seq.push(SCAN_LSHIFT_BREAK);
    }
    if altgr {
        seq.extend_from_slice(&SCAN_RALT_BREAK);
    }
    Some(seq)
}

/// Callback de minifb: recibe el carácter Unicode ya compuesto por el host
/// (AltGr, teclas muertas, mayúsculas) y lo traduce a scancodes PS/2 de la
/// distribución española, empujándolos a la cola del 8042.
struct CharInput {
    kbd_queue: Arc<Mutex<VecDeque<u8>>>,
    ctrl_held: Arc<AtomicBool>,
    lalt_held: Arc<AtomicBool>,
    caps_on: Arc<AtomicBool>,
}

impl minifb::InputCallback for CharInput {
    fn add_char(&mut self, uni_char: u32) {
        // Con Ctrl/Alt pulsado la tecla se reenvía posicionalmente desde el
        // bucle (atajos tipo Ctrl+C); aquí se descarta para no duplicarla.
        if self.ctrl_held.load(Ordering::Relaxed) || self.lalt_held.load(Ordering::Relaxed) {
            return;
        }
        // Caracteres de control (add_char no debería reportarlos, pero por
        // robustez se descartan).
        if uni_char < 0x20 || uni_char == 0x7F {
            return;
        }
        let Some(c) = char::from_u32(uni_char) else { return };
        let caps = self.caps_on.load(Ordering::Relaxed);
        let Some(seq) = build_char_sequence(c, caps) else { return };
        if let Ok(mut q) = self.kbd_queue.lock() {
            push_bytes(&mut q, &seq);
        }
    }
}

/// Teclas NO imprimibles que se reenvían con make/break completos:
/// (minifb::Key, scancode base, requiere prefijo 0xE0).
const CONTROL_KEYS: &[(Key, u8, bool)] = &[
    // Edición / control
    (Key::Escape, 0x01, false),
    (Key::Enter, 0x1C, false),
    (Key::Backspace, 0x0E, false),
    (Key::Tab, 0x0F, false),
    // Función
    (Key::F1, 0x3B, false), (Key::F2, 0x3C, false), (Key::F3, 0x3D, false),
    (Key::F4, 0x3E, false), (Key::F5, 0x3F, false), (Key::F6, 0x40, false),
    (Key::F7, 0x41, false), (Key::F8, 0x42, false), (Key::F9, 0x43, false),
    (Key::F10, 0x44, false), (Key::F11, 0x57, false), (Key::F12, 0x58, false),
    // Navegación (extendidas)
    (Key::Up, 0x48, true), (Key::Down, 0x50, true),
    (Key::Left, 0x4B, true), (Key::Right, 0x4D, true),
    (Key::Home, 0x47, true), (Key::End, 0x4F, true),
    (Key::PageUp, 0x49, true), (Key::PageDown, 0x51, true),
    (Key::Insert, 0x52, true), (Key::Delete, 0x53, true),
    (Key::Menu, 0x5D, true),
    // Modificadores que SÍ se reenvían crudos (atajos del guest). Shift no:
    // el caso lo sintetiza la vía de caracteres. RightAlt tampoco: es AltGr.
    (Key::LeftCtrl, 0x1D, false), (Key::RightCtrl, 0x1D, true),
    (Key::LeftAlt, 0x38, false),
    (Key::LeftSuper, 0x5B, true), (Key::RightSuper, 0x5C, true),
    // Toggles (se reenvían y se trackean localmente)
    (Key::CapsLock, 0x3A, false),
    (Key::NumLock, 0x45, false),
    (Key::ScrollLock, 0x46, false),
    // Teclado numérico
    (Key::NumPadEnter, 0x1C, true),
    (Key::NumPad0, 0x52, false), (Key::NumPad1, 0x4F, false),
    (Key::NumPad2, 0x50, false), (Key::NumPad3, 0x51, false),
    (Key::NumPad4, 0x4B, false), (Key::NumPad5, 0x4C, false),
    (Key::NumPad6, 0x4D, false), (Key::NumPad7, 0x47, false),
    (Key::NumPad8, 0x48, false), (Key::NumPad9, 0x49, false),
    (Key::NumPadDot, 0x53, false),
];

fn control_scancode(key: Key) -> Option<(u8, bool)> {
    CONTROL_KEYS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, s, e)| (*s, *e))
}

/// Teclas imprimibles: las entrega `add_char` (con el layout del host ya
/// aplicado). Solo se reenvían posicionalmente con Ctrl/Alt pulsado.
fn is_printable_key(key: Key) -> bool {
    // Los discriminantes 0..=35 son explícitos en minifb (Key0..Key9 = 0-9,
    // A..Z = 10-35): cubren dígitos y letras sin depender de los
    // discriminantes automáticos del resto del enum.
    if (key as u8) <= 35 {
        return true;
    }
    matches!(
        key,
        Key::Space
            | Key::Apostrophe
            | Key::Backquote
            | Key::Backslash
            | Key::Comma
            | Key::Equal
            | Key::LeftBracket
            | Key::Minus
            | Key::Period
            | Key::RightBracket
            | Key::Semicolon
            | Key::Slash
    )
}

/// Mapeo posicional (Set 1) SOLO de teclas imprimibles, usado como respaldo
/// cuando Ctrl/Alt está pulsado (atajos tipo Ctrl+C). En condiciones
/// normales los caracteres los entrega `add_char` con la distribución
/// española. Si la tecla no está soportada devuelve un Vec vacío.
fn key_to_ps2_scancodes(key: Key) -> Vec<u8> {
    const PLAIN: &[(Key, u8)] = &[
        (Key::Key1, 0x02), (Key::Key2, 0x03), (Key::Key3, 0x04),
        (Key::Key4, 0x05), (Key::Key5, 0x06), (Key::Key6, 0x07),
        (Key::Key7, 0x08), (Key::Key8, 0x09), (Key::Key9, 0x0A),
        (Key::Key0, 0x0B),
        (Key::Minus, 0x0C), (Key::Equal, 0x0D),
        (Key::Q, 0x10), (Key::W, 0x11), (Key::E, 0x12), (Key::R, 0x13),
        (Key::T, 0x14), (Key::Y, 0x15), (Key::U, 0x16), (Key::I, 0x17),
        (Key::O, 0x18), (Key::P, 0x19),
        (Key::LeftBracket, 0x1A), (Key::RightBracket, 0x1B),
        (Key::A, 0x1E), (Key::S, 0x1F), (Key::D, 0x20), (Key::F, 0x21),
        (Key::G, 0x22), (Key::H, 0x23), (Key::J, 0x24), (Key::K, 0x25),
        (Key::L, 0x26), (Key::Semicolon, 0x27), (Key::Apostrophe, 0x28),
        (Key::Backquote, 0x29), (Key::Backslash, 0x2B),
        (Key::Z, 0x2C), (Key::X, 0x2D), (Key::C, 0x2E), (Key::V, 0x2F),
        (Key::B, 0x30), (Key::N, 0x31), (Key::M, 0x32),
        (Key::Comma, 0x33), (Key::Period, 0x34), (Key::Slash, 0x35),
        (Key::Space, 0x39),
    ];
    for (k, scancode) in PLAIN {
        if *k == key {
            return vec![*scancode];
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(c: char, caps: bool) -> Option<Vec<u8>> {
        build_char_sequence(c, caps)
    }

    #[test]
    fn minusculas_sin_mods() {
        assert_eq!(seq('a', false), Some(vec![0x1E, 0x9E]));
        assert_eq!(seq('z', false), Some(vec![0x2C, 0xAC]));
        assert_eq!(seq('ñ', false), Some(vec![0x27, 0xA7]));
        assert_eq!(seq('ç', false), Some(vec![0x2B, 0xAB]));
        assert_eq!(seq(' ', false), Some(vec![0x39, 0xB9]));
        assert_eq!(seq('1', false), Some(vec![0x02, 0x82]));
        assert_eq!(seq('0', false), Some(vec![0x0B, 0x8B]));
    }

    #[test]
    fn mayusculas_sintetizan_shift() {
        assert_eq!(seq('A', false), Some(vec![0x2A, 0x1E, 0x9E, 0xAA]));
        assert_eq!(seq('Ñ', false), Some(vec![0x2A, 0x27, 0xA7, 0xAA]));
        assert_eq!(seq('Ç', false), Some(vec![0x2A, 0x2B, 0xAB, 0xAA]));
    }

    #[test]
    fn caps_lock_xor_mayusculas() {
        // CapsLock ON: la mayúscula no necesita shift y la minúscula sí.
        assert_eq!(seq('A', true), Some(vec![0x1E, 0x9E]));
        assert_eq!(seq('a', true), Some(vec![0x2A, 0x1E, 0x9E, 0xAA]));
        assert_eq!(seq('Ñ', true), Some(vec![0x27, 0xA7]));
    }

    #[test]
    fn altgr_sintetizado() {
        // '@' = AltGr+2 en español.
        assert_eq!(seq('@', false), Some(vec![0xE0, 0x38, 0x03, 0x83, 0xE0, 0xB8]));
        assert_eq!(seq('|', false), Some(vec![0xE0, 0x38, 0x02, 0x82, 0xE0, 0xB8]));
        assert_eq!(seq('€', false), Some(vec![0xE0, 0x38, 0x06, 0x86, 0xE0, 0xB8]));
        assert_eq!(seq('{', false), Some(vec![0xE0, 0x38, 0x28, 0xA8, 0xE0, 0xB8]));
    }

    #[test]
    fn puntuacion_espanola() {
        assert_eq!(seq('¡', false), Some(vec![0x0D, 0x8D]));
        assert_eq!(seq('¿', false), Some(vec![0x2A, 0x0D, 0x8D, 0xAA]));
        assert_eq!(seq('/', false), Some(vec![0x2A, 0x08, 0x88, 0xAA])); // '/' = Shift+7
        assert_eq!(seq('=', false), Some(vec![0x2A, 0x0B, 0x8B, 0xAA])); // '=' = Shift+0
        assert_eq!(seq('?', false), Some(vec![0x2A, 0x0C, 0x8C, 0xAA])); // '?' = Shift+'
        assert_eq!(seq(';', false), Some(vec![0x2A, 0x33, 0xB3, 0xAA])); // ';' = Shift+,
    }

    #[test]
    fn teclas_muertas_compuestas() {
        // á = ´ (0x28) + a (0x1E)
        assert_eq!(seq('á', false), Some(vec![0x28, 0xA8, 0x1E, 0x9E]));
        // Á = ´ + A: el acento sin shift, la letra con shift
        assert_eq!(seq('Á', false), Some(vec![0x28, 0xA8, 0x2A, 0x1E, 0x9E, 0xAA]));
        // ü = ¨ (Shift+0x28) + u: el shift se libera antes de la u
        assert_eq!(seq('ü', false), Some(vec![0x2A, 0x28, 0xA8, 0xAA, 0x16, 0x96]));
        // ã = tilde muerta (AltGr+ñ) + a
        assert_eq!(
            seq('ã', false),
            Some(vec![0xE0, 0x38, 0x27, 0xA7, 0xE0, 0xB8, 0x1E, 0x9E])
        );
        // á con CapsLock ON: el acento suelto y la letra con shift
        assert_eq!(seq('á', true), Some(vec![0x28, 0xA8, 0x2A, 0x1E, 0x9E, 0xAA]));
    }

    #[test]
    fn caracteres_control_descartados() {
        assert_eq!(seq('\u{3}', false), None); // Ctrl+C
        assert_eq!(seq('\u{7F}', false), None);
        assert_eq!(seq('\u{1F}', false), None);
    }

    #[test]
    fn no_soportado_devuelve_none() {
        assert_eq!(seq('🚀', false), None);
        assert_eq!(seq('±', false), None);
    }

    #[test]
    fn mouse_delta_y_inversion() {
        // Mover hacia abajo en la ventana (my > ly):
        // ly = 100.0, my = 120.0
        // En PS/2, el movimiento hacia abajo debe tener delta Y negativo (-20)
        let (dx, dy) = compute_mouse_deltas(50.0, 100.0, 50.0, 120.0);
        assert_eq!(dx, 0);
        assert_eq!(dy, -20);

        // Mover hacia arriba en la ventana (my < ly):
        // ly = 100.0, my = 80.0
        // En PS/2, el movimiento hacia arriba debe tener delta Y positivo (+20)
        let (dx, dy) = compute_mouse_deltas(50.0, 100.0, 50.0, 80.0);
        assert_eq!(dx, 0);
        assert_eq!(dy, 20);

        // Mover hacia la derecha (mx > lx): dx positivo
        let (dx, dy) = compute_mouse_deltas(50.0, 100.0, 75.0, 100.0);
        assert_eq!(dx, 25);
        assert_eq!(dy, 0);

        // Mover hacia la izquierda (mx < lx): dx negativo
        let (dx, dy) = compute_mouse_deltas(50.0, 100.0, 30.0, 100.0);
        assert_eq!(dx, -20);
        assert_eq!(dy, 0);
    }

    #[test]
    fn mouse_subpixel_accumulation_eliminates_drift() {
        let mut accum_x = 0.0f32;
        let mut accum_y = 0.0f32;
        // Escala 0.5 (viewport al doble de tamaño de la resolución guest)
        let scale = 0.5f32;

        // Movimiento de 1 pixel en host: sin acumulador daría 1 * 0.5 = 0 delta (drift y pérdida de movimiento)
        let (dx1, dy1) = compute_mouse_deltas_scaled(10.0, 10.0, 11.0, 10.0, scale, scale, &mut accum_x, &mut accum_y);
        assert_eq!(dx1, 0); // 0.5 acumulado, 0 enviado
        assert_eq!(dy1, 0);
        assert!((accum_x - 0.5).abs() < 1e-5);

        // Segundo movimiento de 1 pixel en host: 0.5 + 0.5 = 1.0 -> genera 1 delta exacto sin pérdida
        let (dx2, dy2) = compute_mouse_deltas_scaled(11.0, 10.0, 12.0, 10.0, scale, scale, &mut accum_x, &mut accum_y);
        assert_eq!(dx2, 1);
        assert_eq!(dy2, 0);
        assert!(accum_x.abs() < 1e-5);
    }

    #[test]
    fn test_scale_framebuffer_identity() {
        let src = vec![0xFF00FF00; 640 * 480];
        let mut dst = vec![0; 640 * 480];
        let (vx, vy, vw, vh) = scale_framebuffer(&mut dst, 640, 480, &src, 640, 480, true);
        assert_eq!((vx, vy, vw, vh), (0, 0, 640, 480));
        assert_eq!(dst[0], 0xFF00FF00);
        assert_eq!(dst[640 * 480 - 1], 0xFF00FF00);
    }

    #[test]
    fn test_scale_framebuffer_pillarbox() {
        // Fuente 4x4 (1:1), destino 8x4 (2:1) -> bandas negras a izquierda y derecha
        let src = vec![0xFFFFFFFF; 16];
        let mut dst = vec![0; 32];
        let (vx, vy, vw, vh) = scale_framebuffer(&mut dst, 8, 4, &src, 4, 4, true);
        assert_eq!((vx, vy, vw, vh), (2, 0, 4, 4));
        // Bandas negras a la izquierda (x=0,1) y derecha (x=6,7)
        assert_eq!(dst[0], 0);
        assert_eq!(dst[1], 0);
        assert_eq!(dst[2], 0xFFFFFFFF);
        assert_eq!(dst[5], 0xFFFFFFFF);
        assert_eq!(dst[6], 0);
        assert_eq!(dst[7], 0);
    }

    #[test]
    fn test_scale_framebuffer_letterbox() {
        // Fuente 4x4 (1:1), destino 4x8 (1:2) -> bandas negras arriba y abajo
        let src = vec![0xFFFFFFFF; 16];
        let mut dst = vec![0; 32];
        let (vx, vy, vw, vh) = scale_framebuffer(&mut dst, 4, 8, &src, 4, 4, true);
        assert_eq!((vx, vy, vw, vh), (0, 2, 4, 4));
        // Bandas negras arriba (filas 0,1) y abajo (filas 6,7)
        assert_eq!(dst[0], 0);
        assert_eq!(dst[7], 0);
        assert_eq!(dst[8], 0xFFFFFFFF);
        assert_eq!(dst[23], 0xFFFFFFFF);
        assert_eq!(dst[24], 0);
        assert_eq!(dst[31], 0);
    }

    #[test]
    fn test_scale_framebuffer_stretch() {
        // Desactivando preservación de aspecto: estirar a toda la ventana
        let src = vec![0xFFFFFFFF; 16];
        let mut dst = vec![0; 32];
        let (vx, vy, vw, vh) = scale_framebuffer(&mut dst, 8, 4, &src, 4, 4, false);
        assert_eq!((vx, vy, vw, vh), (0, 0, 8, 4));
        assert!(dst.iter().all(|&p| p == 0xFFFFFFFF));
    }

    #[test]
    fn test_fast_scanline_hash_consistency() {
        let line1 = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let line2 = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let line3 = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF1];

        assert_eq!(fast_scanline_hash(&line1), fast_scanline_hash(&line2));
        assert_ne!(fast_scanline_hash(&line1), fast_scanline_hash(&line3));
    }

    #[test]
    fn test_dirty_scanline_tracking() {
        let width = 64;
        let height = 8;
        let bpp = 32;
        let vram_size = width * height * 4;
        let mut vram = vec![0u8; vram_size];
        let mut buffer = vec![0u32; width * height];
        let mut scanline_hashes = vec![0u64; height];
        let dac = [0u8; 768];

        // Primer pase: todo está sucio porque scanline_hashes está a cero y vram tiene datos
        vram[0] = 0xAA;
        let dirty1 = render_vbe_framebuffer(
            &mut buffer,
            vram.as_ptr(),
            vram_size,
            width,
            height,
            bpp,
            width,
            0,
            0,
            &dac,
            false,
            &mut scanline_hashes,
            false,
        );
        assert!(dirty1, "Primer pase debe detectar scanlines sucias");

        // Segundo pase con VRAM idéntica: debe reportar limpio (false)
        let dirty2 = render_vbe_framebuffer(
            &mut buffer,
            vram.as_ptr(),
            vram_size,
            width,
            height,
            bpp,
            width,
            0,
            0,
            &dac,
            false,
            &mut scanline_hashes,
            false,
        );
        assert!(!dirty2, "Segundo pase sin cambios en VRAM debe ser limpio (dirty = false)");

        // Modificar una sola línea en VRAM: línea 3 (offset 3 * width * 4)
        vram[3 * width * 4] = 0xFF;
        let dirty3 = render_vbe_framebuffer(
            &mut buffer,
            vram.as_ptr(),
            vram_size,
            width,
            height,
            bpp,
            width,
            0,
            0,
            &dac,
            false,
            &mut scanline_hashes,
            false,
        );
        assert!(dirty3, "Modificar una scanline debe marcar dirty = true");
    }

    #[test]
    fn test_bmp_screenshot_writer() {
        let w = 8;
        let h = 8;
        let buffer = vec![0x00FF0000; w * h]; // Pure Red
        let test_path = "/tmp/test_vbox_screenshot.bmp";
        let res = save_bmp_screenshot_buffer(&buffer, w, h, test_path);
        assert!(res.is_ok());

        let bytes = std::fs::read(test_path).expect("Lectura de BMP generado");
        assert_eq!(&bytes[0..2], b"BM");
        let dib_w = i32::from_le_bytes(bytes[18..22].try_into().unwrap());
        let dib_h = i32::from_le_bytes(bytes[22..26].try_into().unwrap());
        assert_eq!(dib_w, 8);
        assert_eq!(dib_h, 8);
        let bpp = u16::from_le_bytes(bytes[28..30].try_into().unwrap());
        assert_eq!(bpp, 24);
        let _ = std::fs::remove_file(test_path);
    }

    #[test]
    fn test_scale_framebuffer_with_top_bar_offset() {
        let src = vec![0xFFFFFFFF; 16]; // 4x4
        let mut dst = vec![0; 8 * 12];   // 8x12
        let top_offset = 4;
        let (vx, vy, vw, vh) = scale_framebuffer_with_offset(&mut dst, 8, 12, &src, 4, 4, true, top_offset);
        assert_eq!(top_offset, 4);
        assert_eq!(vy, 4); // Viewport starts at top_offset
        assert_eq!(vw, 8);
        assert_eq!(vh, 8);
        assert_eq!(vx, 0);
    }

    /// El camino 1:1 de `scale_framebuffer_with_offset` debe ser idéntico al
    /// bilineal (que con x_diff = y_diff = 0 copia el píxel origen tal cual).
    #[test]
    fn test_scale_framebuffer_one_to_one_is_exact_copy() {
        let (sw, sh) = (7usize, 5usize);
        let src: Vec<u32> = (0..sw * sh)
            .map(|i| (i as u32).wrapping_mul(2654435761) | 0xFF00_0000)
            .collect();

        // preserve_aspect = false, ventana == framebuffer (con barra superior).
        let top = 3usize;
        let mut dst = vec![0u32; sw * (sh + top)];
        let (vx, vy, vw, vh) = scale_framebuffer_with_offset(&mut dst, sw, sh + top, &src, sw, sh, false, top);
        assert_eq!((vx, vy, vw, vh), (0, top, sw, sh));
        for y in 0..sh {
            assert_eq!(&dst[(top + y) * sw..(top + y + 1) * sw], &src[y * sw..(y + 1) * sw]);
        }

        // preserve_aspect = true, viewport exacto (letterbox de tamaño cero).
        let mut dst = vec![0u32; sw * (sh + top)];
        let (vx, vy, vw, vh) = scale_framebuffer_with_offset(&mut dst, sw, sh + top, &src, sw, sh, true, top);
        assert_eq!((vx, vy, vw, vh), (0, top, sw, sh));
        for y in 0..sh {
            assert_eq!(&dst[(top + y) * sw..(top + y + 1) * sw], &src[y * sw..(y + 1) * sw]);
        }
    }

    /// El puntero absoluto debe enviar exactamente la posición que el renderer
    /// dibuja bajo el cursor físico: window → viewport → 0..32767 → píxel del
    /// guest. Si esta cadena se rompe, el cursor del guest queda desfasado de
    /// forma permanente (el síntoma que se veía).
    #[test]
    fn test_absolute_pointer_mapping_matches_rendered_viewport() {
        let (dw, dh, sw, sh) = (1024usize, 700usize, 800usize, 600usize);
        let src = vec![0u32; sw * sh];
        let mut dst = vec![0u32; dw * dh];

        for preserve in [true, false] {
            let (vp_x, vp_y, vp_w, vp_h) =
                scale_framebuffer_with_offset(&mut dst, dw, dh, &src, sw, sh, preserve, TOP_BAR_H);
            assert!(vp_w > 1 && vp_h > 1);

            let samples = [
                (vp_x as f32, vp_y as f32),
                (vp_x as f32 + 1.0, vp_y as f32 + 1.0),
                ((vp_x + vp_w / 2) as f32, (vp_y + vp_h / 2) as f32),
                ((vp_x + vp_w - 1) as f32, (vp_y + vp_h - 1) as f32),
            ];

            for (mx, my) in samples {
                // Mapeo del host (idéntico al del bucle de display).
                let norm_x = (((mx - vp_x as f32) / (vp_w as f32 - 1.0)) * 32767.0)
                    .clamp(0.0, 32767.0);
                let norm_y = (((my - vp_y as f32) / (vp_h as f32 - 1.0)) * 32767.0)
                    .clamp(0.0, 32767.0);

                // Decodificación del guest: 0..32767 -> píxel del escritorio.
                let gx = (norm_x / 32767.0 * (sw - 1) as f32).round() as isize;
                let gy = (norm_y / 32767.0 * (sh - 1) as f32).round() as isize;
                let ex = ((mx - vp_x as f32) * (sw - 1) as f32 / (vp_w as f32 - 1.0)).round() as isize;
                let ey = ((my - vp_y as f32) * (sh - 1) as f32 / (vp_h as f32 - 1.0)).round() as isize;

                assert!(
                    (gx - ex).abs() <= 1 && (gy - ey).abs() <= 1,
                    "preserve={} window=({:.1},{:.1}) viewport=({},{},{},{}) -> guest=({},{}) esperado=({},{})",
                    preserve, mx, my, vp_x, vp_y, vp_w, vp_h, gx, gy, ex, ey
                );
            }
        }
    }

    #[test]
    fn test_menu_header_geometry() {
        let rects = get_menu_header_rects();
        assert_eq!(rects.len(), MENU_DEFINITIONS.len());
        assert!(rects[0].0 >= 8);
        for i in 1..rects.len() {
            assert!(rects[i].0 > rects[i - 1].0);
        }
    }

    #[test]
    fn test_virtualbox_resolution_presets_and_dropdown() {
        // Menú 2 es "Ver"
        let (x, y, w, h) = get_dropdown_geometry(2, 1024);
        assert!(w >= 200);
        assert!(h > 100);
        // Debe encontrar la primera acción seleccionable (ToggleAutoResize en y+4..y+26)
        let action1 = find_dropdown_action_at(2, 1024, x + 10, y + 10);
        assert_eq!(action1, Some(MenuAction::ToggleAutoResize));

        // Verificar que las acciones de resolución existen en el menú
        let ver_menu = &MENU_DEFINITIONS[2];
        let has_fhd = ver_menu.items.iter().any(|it| it.action == MenuAction::SetRes1920x1080);
        assert!(has_fhd, "El menú Ver debe contener resolución 1920x1080 FHD");
    }

    #[test]
    fn test_runtime_floating_window_geometry_and_interactions() {
        let fw = RuntimeFloatingWindow::new_centered(FloatingKind::Preferences, 500, 300, 1024, 768);
        assert_eq!(fw.w, 500);
        assert_eq!(fw.h, 300);
        assert_eq!(fw.x, 262); // (1024 - 500) / 2
        assert_eq!(fw.y, TOP_BAR_H + (768 - TOP_BAR_H - 300) / 2);

        // Verificación de contención
        assert!(fw.contains_point(fw.x, fw.y));
        assert!(fw.contains_point(fw.x + fw.w - 1, fw.y + fw.h - 1));
        assert!(!fw.contains_point(fw.x.saturating_sub(1), fw.y));
        assert!(!fw.contains_point(fw.x + fw.w, fw.y + fw.h));

        // Verificación de barra de título y botón cerrar
        let (cx, cy, _cw, _ch) = fw.close_btn_rect();
        assert!(fw.is_in_close_btn(cx + 1, cy + 1));
        assert!(!fw.is_in_titlebar(cx + 1, cy + 1)); // El botón de cierre no debe arrastrar
        assert!(fw.is_in_titlebar(fw.x + 20, fw.y + 10)); // La barra de título sí debe arrastrar
    }

    #[test]
    fn test_floating_window_manager_stack_and_toggle() {
        let mut list: Vec<RuntimeFloatingWindow> = Vec::new();
        let ww = 1024;
        let wh = 768;

        // 1. Abrir HUD
        toggle_or_open_floating_window(&mut list, FloatingKind::Hud, 480, 180, ww, wh);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, FloatingKind::Hud);

        // 2. Abrir Preferencias
        toggle_or_open_floating_window(&mut list, FloatingKind::Preferences, 520, 310, ww, wh);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].kind, FloatingKind::Preferences);

        // 3. Volver a abrir Preferencias debe traerlo al frente (último elemento)
        toggle_or_open_floating_window(&mut list, FloatingKind::Preferences, 520, 310, ww, wh);
        assert_eq!(list.len(), 2);
        assert_eq!(list.last().unwrap().kind, FloatingKind::Preferences);

        // 4. Volver a alternar HUD debe cerrarlo (comportamiento toggle)
        toggle_or_open_floating_window(&mut list, FloatingKind::Hud, 480, 180, ww, wh);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, FloatingKind::Preferences);
    }
}

