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

pub struct DisplayManager {
    running: Arc<AtomicBool>,
}

impl DisplayManager {
    pub fn start(
        vga_state: Arc<Mutex<VgaState>>,
        guest_mem: Arc<GuestMemory>,
        kbd_queue: Arc<Mutex<VecDeque<u8>>>,
        mouse_queue: Arc<Mutex<VecDeque<(i16, i16, u8)>>>,
    ) -> Option<Self> {
        if std::env::var("DISPLAY").is_err() && std::env::var("WAYLAND_DISPLAY").is_err() {
            eprintln!("[DISPLAY] No se detectó servidor gráfico (DISPLAY/WAYLAND). Ejecutando en modo headless.");
            return None;
        }

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        thread::spawn(move || {
            let initial_width = 640;
            let initial_height = 400;
            // (tarea 15) El buffer LÓGICO guarda el frame renderizado al
            // tamaño nativo del modo; el buffer de ventana lo re-escala al
            // tamaño actual de la ventana (resize sin distorsión).
            let mut render_buf: Vec<u32> = vec![0; initial_width * initial_height];
            let mut window_buf: Vec<u32> = vec![0; initial_width * initial_height];

            let mut window = match Window::new(
                "mi-vmm — VGA Display",
                initial_width,
                initial_height,
                WindowOptions {
                    resize: true,
                    scale: minifb::Scale::X2,
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

            while running_clone.load(Ordering::Relaxed)
                && window.is_open()
                && !window.is_key_down(Key::Escape)
            {
                // ── Snapshot del modo actual ────────────────────────
                let (is_vbe, is_std_gfx, lw, lh, bpp, virt_w, x_off, y_off, dac8) = {
                    let st = vga_state.lock().unwrap();
                    let (w, h, b) = st.get_resolution();
                    let is_vbe = st.is_vbe_enabled();
                    let is_std_gfx = !is_vbe && st.is_standard_vga_graphics();
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
                        lw,
                        lh,
                        b,
                        st.dispi_regs[VBE_DISPI_INDEX_VIRT_WIDTH as usize] as usize,
                        st.dispi_regs[VBE_DISPI_INDEX_X_OFFSET as usize] as usize,
                        st.dispi_regs[VBE_DISPI_INDEX_Y_OFFSET as usize] as usize,
                        st.dispi_regs[VBE_DISPI_INDEX_ENABLE as usize] & VBE_DISPI_8BIT_DAC != 0,
                    )
                };

                if render_buf.len() != lw * lh {
                    render_buf.resize(lw * lh, 0);
                }

                if is_vbe {
                    // Modo gráfico VBE: leer directamente de VRAM
                    let st = vga_state.lock().unwrap();
                    if !st.vram_ptr.is_null() {
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
                        );
                    }
                } else if is_std_gfx {
                    // (tarea 2) Modo gráfico VGA estándar
                    let st = vga_state.lock().unwrap();
                    if !st.vram_ptr.is_null() {
                        render_vga_graphics(&mut render_buf, &st, lw, lh);
                    }
                } else {
                    // Modo texto VGA: renderizar buffer 80x25 desde 0xB8000
                    render_vga_text_mode(&mut render_buf, &mem);
                }

                // ── (tarea 15) Escalar al tamaño actual de la ventana ──
                let (ww, wh) = window.get_size();
                let (ww, wh) = (ww.max(1), wh.max(1));
                if window_buf.len() != ww * wh {
                    window_buf.resize(ww * wh, 0);
                }
                scale_nearest(&mut window_buf, ww, wh, &render_buf, lw, lh);

                if let Err(e) = window.update_with_buffer(&window_buf, ww, wh) {
                    eprintln!("[DISPLAY] Error de actualización de ventana: {}", e);
                    break;
                }

                // ── Entrada del host: make/break de teclas de control ──
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

                // ── (tarea 9) Capturar el ratón del host ─────────────
                match window.get_mouse_pos(MouseMode::Discard) {
                    Some((mx, my)) => {
                        let buttons = (window.get_mouse_down(MouseButton::Left) as u8)
                            | ((window.get_mouse_down(MouseButton::Right) as u8) << 1)
                            | ((window.get_mouse_down(MouseButton::Middle) as u8) << 2);
                        let mut dx: i16 = 0;
                        let mut dy: i16 = 0;
                        if let Some((lx, ly)) = last_mouse {
                            dx = (mx - lx) as i16;
                            dy = (my - ly) as i16;
                        }
                        last_mouse = Some((mx, my));
                        if dx != 0 || dy != 0 || buttons != last_buttons {
                            let mut q = mouse_queue.lock().unwrap();
                            q.push_back((dx, dy, buttons));
                            last_buttons = buttons;
                        }
                    }
                    None => {
                        last_mouse = None;
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
            eprintln!("[DISPLAY] Ventana de visualización cerrada — apagando la VM...");
        });

        Some(Self { running })
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// Renderiza la memoria de texto VGA en 0xB8000 a un buffer RGB de 640x400.
fn render_vga_text_mode(buffer: &mut [u32], mem: &GuestMemory) {
    const COLS: usize = 80;
    const ROWS: usize = 25;
    const CHAR_W: usize = 8;
    const CHAR_H: usize = 16;
    const WIDTH: usize = COLS * CHAR_W; // 640
    const TEXT_OFFSET: usize = 0xB8000;
    const TEXT_SIZE: usize = COLS * ROWS * 2;

    if mem.size() < TEXT_OFFSET + TEXT_SIZE {
        buffer.fill(0);
        return;
    }

    // Copia acotada de la pantalla de texto (el guest la escribe en paralelo).
    let mut text = [0u8; TEXT_SIZE];
    let _ = mem.copy_from(TEXT_OFFSET, &mut text);

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
fn render_vga_graphics(buffer: &mut [u32], st: &VgaState, width: usize, height: usize) {
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
        return;
    };

    let vram = st.vram_ptr;
    let vram_size = st.vram_size;

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
) {
    if width == 0 || height == 0 || vram_size == 0 {
        return;
    }
    let vw = if virt_width >= width { virt_width } else { width };
    let bytes_pp = match bpp {
        32 => 4usize,
        24 => 3,
        16 | 15 => 2,
        8 | 4 => 1,
        _ => {
            buffer.fill(0);
            return;
        }
    };
    let row_bytes = vw * bytes_pp;
    let row_bytes = if y_offset + height > 0 && (y_offset + height - 1) * row_bytes + row_bytes > vram_size {
        width * bytes_pp
    } else {
        row_bytes
    };
    let (xo, yo) = if x_offset + width <= vw { (x_offset, y_offset) } else { (0, y_offset) };

    match bpp {
        32 => {
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo * 4;
                if src_row + width * 4 > vram_size {
                    continue;
                }
                let src_slice =
                    unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row) as *const u32, width) };
                for (x, &src) in src_slice.iter().enumerate() {
                    let b = src & 0xFF;
                    let g = (src >> 8) & 0xFF;
                    let r = (src >> 16) & 0xFF;
                    buffer[y * width + x] = (r << 16) | (g << 8) | b;
                }
            }
        }
        24 => {
            for y in 0..height {
                let src_row = (yo + y) * row_bytes + xo * 3;
                if src_row + width * 3 > vram_size {
                    continue;
                }
                let src = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width * 3) };
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
                    continue;
                }
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
                    continue;
                }
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
                    continue;
                }
                let src = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), width) };
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
                    continue;
                }
                let src = unsafe { std::slice::from_raw_parts(vram_ptr.add(src_row), (width + 1) / 2) };
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
}

/// (tarea 15) Escalado nearest-neighbor del framebuffer lógico al tamaño
/// actual de la ventana. `dst` debe ser exactamente dw*dh píxeles.
fn scale_nearest(dst: &mut [u32], dw: usize, dh: usize, src: &[u32], sw: usize, sh: usize) {
    if dw == 0 || dh == 0 || sw == 0 || sh == 0 {
        dst.fill(0);
        return;
    }
    if dw == sw && dh == sh && dst.len() == src.len() {
        dst.copy_from_slice(src);
        return;
    }
    for y in 0..dh {
        let sy = y * sh / dh;
        let src_row = &src[sy * sw..(sy + 1) * sw];
        let dst_row = &mut dst[y * dw..(y + 1) * dw];
        for (x, px) in dst_row.iter_mut().enumerate() {
            *px = src_row[x * sw / dw];
        }
    }
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
}

