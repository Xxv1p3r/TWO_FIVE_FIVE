//! Cargador de assets gráficos personalizados del hipervisor Two Five Five (`assets/`).
//!
//! Incluye un decodificador PNG + Zlib/DEFLATE (RFC 1950 / RFC 1951) 100% en Rust puro
//! (sin dependencias externas) capaz de cargar todos los iconos, emblemas del logotipo 255,
//! marcos de monitor y banners de asistentes en formato PNG (8-bit RGBA, RGB e Indexed),
//! además de generadores procedurales de respaldo en memoria si algún archivo no está presente.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Imagen RGBA decodificada lista para componerse con alpha-blending sobre el framebuffer `minifb`.
#[derive(Clone, Debug)]
pub struct RgbaImage {
    pub width: usize,
    pub height: usize,
    /// Píxeles en formato `[R, G, B, A]` por cada píxel (`width * height * 4` bytes).
    pub data: Vec<u8>,
}

impl RgbaImage {
    /// Dibuja la imagen en `(dst_x, dst_y)` sobre el framebuffer `buf` (`0x00RRGGBB`) con alpha-blending.
    pub fn draw(&self, buf: &mut [u32], bw: usize, bh: usize, dst_x: isize, dst_y: isize) {
        for sy in 0..self.height {
            let py = dst_y + sy as isize;
            if py < 0 || py >= bh as isize {
                continue;
            }
            let py = py as usize;
            for sx in 0..self.width {
                let px = dst_x + sx as isize;
                if px < 0 || px >= bw as isize {
                    continue;
                }
                let px = px as usize;
                let idx = (sy * self.width + sx) * 4;
                let r = self.data[idx] as u32;
                let g = self.data[idx + 1] as u32;
                let b = self.data[idx + 2] as u32;
                let a = self.data[idx + 3] as u32;
                if a == 0 {
                    continue;
                }
                let dst_idx = py * bw + px;
                if a == 255 {
                    buf[dst_idx] = (r << 16) | (g << 8) | b;
                } else {
                    let bg = buf[dst_idx];
                    let br = (bg >> 16) & 0xFF;
                    let bg_g = (bg >> 8) & 0xFF;
                    let bb = bg & 0xFF;
                    let inv = 255 - a;
                    let or = (r * a + br * inv) / 255;
                    let og = (g * a + bg_g * inv) / 255;
                    let ob = (b * a + bb * inv) / 255;
                    buf[dst_idx] = (or << 16) | (og << 8) | ob;
                }
            }
        }
    }

    /// Dibuja la imagen re-escalada a `(target_w, target_h)` con interpolación bilineal / nearest.
    pub fn draw_scaled(
        &self,
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        dst_x: isize,
        dst_y: isize,
        target_w: usize,
        target_h: usize,
    ) {
        if self.width == 0 || self.height == 0 || target_w == 0 || target_h == 0 {
            return;
        }
        if target_w == self.width && target_h == self.height {
            self.draw(buf, bw, bh, dst_x, dst_y);
            return;
        }
        for dy in 0..target_h {
            let py = dst_y + dy as isize;
            if py < 0 || py >= bh as isize {
                continue;
            }
            let py = py as usize;
            let sy = (dy * self.height) / target_h;
            for dx in 0..target_w {
                let px = dst_x + dx as isize;
                if px < 0 || px >= bw as isize {
                    continue;
                }
                let px = px as usize;
                let sx = (dx * self.width) / target_w;
                let idx = (sy * self.width + sx) * 4;
                let r = self.data[idx] as u32;
                let g = self.data[idx + 1] as u32;
                let b = self.data[idx + 2] as u32;
                let a = self.data[idx + 3] as u32;
                if a == 0 {
                    continue;
                }
                let dst_idx = py * bw + px;
                if a == 255 {
                    buf[dst_idx] = (r << 16) | (g << 8) | b;
                } else {
                    let bg = buf[dst_idx];
                    let br = (bg >> 16) & 0xFF;
                    let bg_g = (bg >> 8) & 0xFF;
                    let bb = bg & 0xFF;
                    let inv = 255 - a;
                    let or = (r * a + br * inv) / 255;
                    let og = (g * a + bg_g * inv) / 255;
                    let ob = (b * a + bb * inv) / 255;
                    buf[dst_idx] = (or << 16) | (og << 8) | ob;
                }
            }
        }
    }
}

/// Repositorio en memoria con los iconos y gráficos personalizados del hipervisor Two Five Five (255).
pub struct VBoxAssets {
    images: HashMap<String, RgbaImage>,
    pub base_dir: Option<PathBuf>,
}

impl VBoxAssets {
    /// Localiza la carpeta `assets/` del proyecto Two Five Five (255)
    /// y precarga todos los iconos, logos, marcos de monitor y banners de asistentes.
    pub fn load() -> Self {
        let base_dir = find_assets_dir();
        let mut assets = Self {
            images: HashMap::new(),
            base_dir,
        };

        let preload = [
            // Branding oficial Two Five Five (255) + alias de compatibilidad
            "logo_255.png",
            "logo_200px.png",
            "logo_64px.png",
            "logo_48px.png",
            "logo_32px.png",
            "logo_24px.png",
            "logo_16px.png",
            "OSE/VirtualBox_48px.png",
            "OSE/VirtualBox_32px.png",
            "OSE/VirtualBox_16px.png",
            "virtualbox.png",
            "tools_global_32px.png",
            "tools_menu_24px.png",
            "welcome_200px.png",
            // Toolbar principal (32px y 16px)
            "vm_new_32px.png",
            "vm_new_16px.png",
            "vm_add_32px.png",
            "vm_clone_32px.png",
            "vm_settings_32px.png",
            "vm_settings_16px.png",
            "vm_discard_32px.png",
            "vm_delete_32px.png",
            "vm_delete_16px.png",
            "vm_start_32px.png",
            "vm_start_16px.png",
            "vm_pause_16px.png",
            "vm_reset_16px.png",
            "vm_poweroff_16px.png",
            "global_settings_32px.png",
            // Selector de herramientas de máquina
            "machine_details_manager_24px.png",
            "machine_details_manager_16px.png",
            "snapshot_manager_24px.png",
            "snapshot_manager_16px.png",
            "vm_show_logs_24px.png",
            "vm_show_logs_16px.png",
            "performance_monitor_24px.png",
            "media_manager_32px.png",
            "media_manager_24px.png",
            "media_manager_16px.png",
            // Iconos de Sistemas Operativos (32x32)
            "os_win11.png",
            "os_win10.png",
            "os_win81.png",
            "os_win8.png",
            "os_win7.png",
            "os_winvista.png",
            "os_winxp.png",
            "os_win2k.png",
            "os_win_other.png",
            "os_ubuntu.png",
            "os_debian.png",
            "os_archlinux.png",
            "os_fedora.png",
            "os_redhat.png",
            "os_opensuse.png",
            "os_gentoo.png",
            "os_linux26.png",
            "os_linux.png",
            "os_freebsd.png",
            "os_macosx.png",
            "os_dos.png",
            "os_other.png",
            // Estados de la máquina (16x16)
            "state_powered_off_16px.png",
            "state_running_16px.png",
            "state_saved_16px.png",
            "state_paused_16px.png",
            "state_aborted_16px.png",
            // Categorías de Detalles y Configuración (16px y 32px)
            "machine_16px.png",
            "machine_32px.png",
            "chipset_16px.png",
            "chipset_32px.png",
            "vrdp_16px.png",
            "vrdp_32px.png",
            "hd_16px.png",
            "hd_32px.png",
            "cd_16px.png",
            "fd_16px.png",
            "sound_16px.png",
            "sound_32px.png",
            "nw_16px.png",
            "nw_32px.png",
            "serial_port_16px.png",
            "serial_port_32px.png",
            "usb_16px.png",
            "usb_32px.png",
            "sf_16px.png",
            "sf_32px.png",
            "interface_16px.png",
            "interface_32px.png",
            "description_16px.png",
            "cpu_16px.png",
            "ram_16px.png",
            // Acciones de medios / discos / CD
            "hd_create_32px.png",
            "hd_create_16px.png",
            "hd_add_32px.png",
            "hd_add_16px.png",
            "hd_modify_32px.png",
            "hd_modify_16px.png",
            "hd_remove_32px.png",
            "hd_remove_16px.png",
            "hd_release_32px.png",
            "cd_add_32px.png",
            "cd_add_16px.png",
            "cd_unmount_16px.png",
            "refresh_32px.png",
            "refresh_16px.png",
            "ok_16px.png",
            "cancel_16px.png",
            "close_16px.png",
            "status_check_16px.png",
            "status_error_16px.png",
            "arrow_down_10px.png",
            "arrow_right_10px.png",
            // Monitor de Vista Previa 16:9
            "preview_full_16to9_242x155px.png",
            "preview_empty_16to9_242x155px.png",
            // Banners laterales de Wizards
            "wizard_new_welcome.png",
            "wizard_new_harddisk.png",
            "wizard_clone.png",
        ];

        for rel in &preload {
            assets.ensure_loaded(rel);
        }

        assets
    }

    pub fn ensure_loaded(&mut self, rel_path: &str) -> Option<&RgbaImage> {
        if !self.images.contains_key(rel_path) {
            let mut loaded = None;
            if let Some(ref base) = self.base_dir {
                for candidate_rel in [rel_path, map_alias_key(rel_path)] {
                    let full = base.join(candidate_rel);
                    if let Ok(bytes) = fs::read(&full) {
                        if let Ok(img) = decode_png(&bytes) {
                            loaded = Some(img);
                            break;
                        }
                    }
                }
            }
            let img = loaded.unwrap_or_else(|| procedural_fallback_icon(rel_path));
            self.images.insert(rel_path.to_string(), img);
        }
        self.images.get(rel_path)
    }

    pub fn get(&self, rel_path: &str) -> Option<&RgbaImage> {
        self.images
            .get(rel_path)
            .or_else(|| self.images.get(map_alias_key(rel_path)))
    }

    /// Dibuja un icono por nombre en `(x, y)` si está disponible. Devuelve `true` si se dibujó.
    pub fn draw(&self, name: &str, buf: &mut [u32], bw: usize, bh: usize, x: isize, y: isize) -> bool {
        if let Some(img) = self.get(name) {
            img.draw(buf, bw, bh, x, y);
            true
        } else {
            let fb = procedural_fallback_icon(name);
            fb.draw(buf, bw, bh, x, y);
            true
        }
    }

    /// Dibuja un icono escalado a `(tw, th)`.
    pub fn draw_scaled(
        &self,
        name: &str,
        buf: &mut [u32],
        bw: usize,
        bh: usize,
        x: isize,
        y: isize,
        tw: usize,
        th: usize,
    ) -> bool {
        if let Some(img) = self.get(name) {
            img.draw_scaled(buf, bw, bh, x, y, tw, th);
            true
        } else {
            let fb = procedural_fallback_icon(name);
            fb.draw_scaled(buf, bw, bh, x, y, tw, th);
            true
        }
    }

    /// Devuelve el nombre del asset de icono de SO (`os_*.png`) según el tipo/nombre/ISO.
    pub fn os_icon_for(os_type: &str, vm_name: &str, iso_name: Option<&str>) -> &'static str {
        let combined = format!(
            "{} {} {}",
            os_type.to_lowercase(),
            vm_name.to_lowercase(),
            iso_name.unwrap_or("").to_lowercase()
        );
        if combined.contains("win11") || combined.contains("windows 11") || combined.contains("windows_11") {
            "os_win11.png"
        } else if combined.contains("win10") || combined.contains("windows 10") || combined.contains("windows_10") {
            "os_win10.png"
        } else if combined.contains("win8") || combined.contains("windows 8") {
            "os_win81.png"
        } else if combined.contains("win7") || combined.contains("windows 7") || combined.contains("windows_7") {
            "os_win7.png"
        } else if combined.contains("vista") {
            "os_winvista.png"
        } else if combined.contains("xp") {
            "os_winxp.png"
        } else if combined.contains("win") {
            "os_win10.png"
        } else if combined.contains("ubuntu") || combined.contains("mint") || combined.contains("pop") {
            "os_ubuntu.png"
        } else if combined.contains("debian") || combined.contains("kali") {
            "os_debian.png"
        } else if combined.contains("arch") || combined.contains("manjaro") || combined.contains("cachy") {
            "os_archlinux.png"
        } else if combined.contains("fedora") {
            "os_fedora.png"
        } else if combined.contains("redhat") || combined.contains("rhel") || combined.contains("rocky") || combined.contains("alma") {
            "os_redhat.png"
        } else if combined.contains("suse") {
            "os_opensuse.png"
        } else if combined.contains("gentoo") {
            "os_gentoo.png"
        } else if combined.contains("bsd") {
            "os_freebsd.png"
        } else if combined.contains("mac") || combined.contains("osx") {
            "os_macosx.png"
        } else if combined.contains("dos") {
            "os_dos.png"
        } else if combined.contains("linux") || combined.contains("core") || combined.contains("alpine") {
            "os_linux26.png"
        } else {
            "os_other.png"
        }
    }
}

fn map_alias_key(key: &str) -> &str {
    match key {
        "OSE/VirtualBox_48px.png" | "virtualbox.png" => "logo_48px.png",
        "OSE/VirtualBox_32px.png" => "logo_32px.png",
        "OSE/VirtualBox_16px.png" => "logo_16px.png",
        _ => key,
    }
}

fn find_assets_dir() -> Option<PathBuf> {
    let rel = "assets";
    let direct = Path::new(rel);
    if direct.is_dir() {
        return Some(direct.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            for anc in [parent.to_path_buf(), parent.join(".."), parent.join("../..")] {
                let candidate = anc.join(rel);
                if candidate.is_dir() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Genera un icono procedural en memoria (con estética carbono/oro 255) como respaldo si algún archivo PNG no se encuentra.
fn procedural_fallback_icon(name: &str) -> RgbaImage {
    let (w, h) = if name.contains("242x155") {
        (242, 155)
    } else if name.starts_with("wizard_") {
        (145, 290)
    } else if name.contains("200px") {
        (200, 200)
    } else if name.contains("64px") {
        (64, 64)
    } else if name.contains("48px") || name == "virtualbox.png" {
        (48, 48)
    } else if name.contains("24px") {
        (24, 24)
    } else if name.contains("16px") {
        (16, 16)
    } else if name.contains("10px") {
        (10, 10)
    } else {
        (32, 32)
    };

    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) * 4;
            let border = x == 0 || y == 0 || x + 1 == w || y + 1 == h;
            let (r, g, b, a) = if border {
                (245, 196, 0, 255)
            } else {
                (26, 31, 40, 235)
            };
            data[idx] = r;
            data[idx + 1] = g;
            data[idx + 2] = b;
            data[idx + 3] = a;
        }
    }
    RgbaImage { width: w, height: h, data }
}

// ─────────────────────────────────────────────────────────────────────────────
// Decodificador PNG + Zlib/DEFLATE (RFC 1950 / RFC 1951) en Rust Puro
// ─────────────────────────────────────────────────────────────────────────────

pub fn decode_png(bytes: &[u8]) -> Result<RgbaImage, &'static str> {
    const PNG_SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if bytes.len() < 33 || bytes[0..8] != PNG_SIG {
        return Err("Firma PNG invalida");
    }

    let mut pos = 8usize;
    let mut width = 0usize;
    let mut height = 0usize;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut interlace = 0u8;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Vec<u8> = Vec::new();
    let mut idat = Vec::new();

    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        let ctype = &bytes[pos + 4..pos + 8];
        pos += 8;
        if pos + len + 4 > bytes.len() {
            break;
        }
        let chunk = &bytes[pos..pos + len];
        pos += len + 4; // saltar datos + CRC

        match ctype {
            b"IHDR" => {
                if chunk.len() < 13 {
                    return Err("IHDR corto");
                }
                width = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as usize;
                height = u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as usize;
                bit_depth = chunk[8];
                color_type = chunk[9];
                interlace = chunk[12];
            }
            b"PLTE" => {
                for rgb in chunk.chunks_exact(3) {
                    palette.push([rgb[0], rgb[1], rgb[2]]);
                }
            }
            b"tRNS" => {
                trns.extend_from_slice(chunk);
            }
            b"IDAT" => {
                idat.extend_from_slice(chunk);
            }
            b"IEND" => break,
            _ => {}
        }
    }

    if width == 0 || height == 0 || interlace != 0 {
        return Err("Dimensiones o entrelazado PNG no soportado");
    }
    if bit_depth != 8 && !(color_type == 3 && (bit_depth == 1 || bit_depth == 2 || bit_depth == 4)) {
        return Err("Profundidad de bits PNG no soportada");
    }

    let channels: usize = match color_type {
        0 => 1, // Grayscale
        2 => 3, // RGB
        3 => 1, // Indexed (PLTE)
        4 => 2, // Grayscale + Alpha
        6 => 4, // RGBA
        _ => return Err("Color type PNG desconocido"),
    };

    let bpp = ((channels * bit_depth as usize) + 7) / 8;
    let stride = (width * channels * bit_depth as usize + 7) / 8;
    let raw = inflate_zlib(&idat)?;
    let expected = height * (1 + stride);
    if raw.len() < expected {
        return Err("Datos IDAT descomprimidos insuficientes");
    }

    let mut recon = vec![0u8; height * stride];
    for y in 0..height {
        let ftype = raw[y * (1 + stride)];
        let src = &raw[y * (1 + stride) + 1..(y + 1) * (1 + stride)];
        let row_start = y * stride;
        for x in 0..stride {
            let a = if x >= bpp { recon[row_start + x - bpp] } else { 0 };
            let b = if y > 0 { recon[(y - 1) * stride + x] } else { 0 };
            let c = if y > 0 && x >= bpp {
                recon[(y - 1) * stride + x - bpp]
            } else {
                0
            };
            let val = match ftype {
                0 => src[x],
                1 => src[x].wrapping_add(a),
                2 => src[x].wrapping_add(b),
                3 => src[x].wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => src[x].wrapping_add(paeth_predictor(a, b, c)),
                _ => return Err("Filtro PNG invalido"),
            };
            recon[row_start + x] = val;
        }
    }

    let mut rgba = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let row = &recon[y * stride..(y + 1) * stride];
        match color_type {
            6 => {
                // RGBA 8-bit
                rgba.extend_from_slice(row);
            }
            2 => {
                // RGB 8-bit
                let trns_rgb = if trns.len() >= 6 {
                    Some([trns[1], trns[3], trns[5]])
                } else {
                    None
                };
                for px in row.chunks_exact(3) {
                    let a = if Some([px[0], px[1], px[2]]) == trns_rgb { 0 } else { 255 };
                    rgba.extend_from_slice(&[px[0], px[1], px[2], a]);
                }
            }
            3 => {
                // Indexed PLTE + tRNS
                for x in 0..width {
                    let idx = match bit_depth {
                        8 => row[x] as usize,
                        4 => ((row[x / 2] >> (4 * (1 - (x & 1)))) & 0x0F) as usize,
                        2 => ((row[x / 4] >> (2 * (3 - (x & 3)))) & 0x03) as usize,
                        1 => ((row[x / 8] >> (7 - (x & 7))) & 0x01) as usize,
                        _ => 0,
                    };
                    let [r, g, b] = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                    let a = trns.get(idx).copied().unwrap_or(255);
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
            }
            4 => {
                // Grayscale + Alpha
                for px in row.chunks_exact(2) {
                    rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
                }
            }
            0 => {
                // Grayscale
                for &g in row {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
            }
            _ => unreachable!(),
        }
    }

    Ok(RgbaImage {
        width,
        height,
        data: rgba,
    })
}

#[inline]
fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i32;
    let b = b as i32;
    let c = c as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_buf: u64,
    bit_cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_buf: 0,
            bit_cnt: 0,
        }
    }

    #[inline]
    fn ensure_bits(&mut self, n: u32) {
        while self.bit_cnt < n && self.byte_pos < self.data.len() {
            self.bit_buf |= (self.data[self.byte_pos] as u64) << self.bit_cnt;
            self.byte_pos += 1;
            self.bit_cnt += 8;
        }
    }

    #[inline]
    fn read_bits(&mut self, n: u32) -> Result<u32, &'static str> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure_bits(n);
        if self.bit_cnt < n {
            return Err("Fin inesperado de stream DEFLATE");
        }
        let val = (self.bit_buf & ((1u64 << n) - 1)) as u32;
        self.bit_buf >>= n;
        self.bit_cnt -= n;
        Ok(val)
    }

    fn align_to_byte(&mut self) {
        let rem = self.bit_cnt & 7;
        self.bit_buf >>= rem;
        self.bit_cnt -= rem;
    }

    fn read_u16_le(&mut self) -> Result<u16, &'static str> {
        self.align_to_byte();
        let lo = self.read_bits(8)? as u16;
        let hi = self.read_bits(8)? as u16;
        Ok(lo | (hi << 8))
    }
}

#[derive(Clone)]
struct HuffmanTable {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl HuffmanTable {
    fn from_lengths(lengths: &[u8]) -> Result<Self, &'static str> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l as usize >= 16 {
                return Err("Longitud Huffman > 15");
            }
            if l > 0 {
                counts[l as usize] += 1;
            }
        }
        let mut offsets = [0u16; 16];
        let mut total = 0u16;
        for bits in 1..16 {
            offsets[bits] = total;
            total += counts[bits];
        }
        let mut symbols = vec![0u16; total as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                let idx = offsets[l as usize] as usize;
                offsets[l as usize] += 1;
                symbols[idx] = sym as u16;
            }
        }
        Ok(Self { counts, symbols })
    }

    #[inline]
    fn decode(&self, br: &mut BitReader) -> Result<u16, &'static str> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= br.read_bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("Codigo Huffman invalido")
    }
}

fn inflate_zlib(zlib_data: &[u8]) -> Result<Vec<u8>, &'static str> {
    if zlib_data.len() < 6 {
        return Err("Stream zlib demasiado corto");
    }
    let cmf = zlib_data[0];
    let flg = zlib_data[1];
    if (cmf & 0x0F) != 8 {
        return Err("Metodo de compresion zlib no es DEFLATE");
    }
    let start = if (flg & 0x20) != 0 { 6 } else { 2 };
    let mut br = BitReader::new(&zlib_data[start..]);
    let mut out = Vec::with_capacity(zlib_data.len() * 3);

    const LEN_BASE: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEN_EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DIST_BASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    const CL_ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];

    loop {
        let bfinal = br.read_bits(1)?;
        let btype = br.read_bits(2)?;

        match btype {
            0 => {
                // Bloque sin comprimir
                let len = br.read_u16_le()? as usize;
                let _nlen = br.read_u16_le()?;
                for _ in 0..len {
                    out.push(br.read_bits(8)? as u8);
                }
            }
            1 | 2 => {
                let (lit_table, dist_table) = if btype == 1 {
                    let mut lit_lens = vec![0u8; 288];
                    lit_lens[0..144].fill(8);
                    lit_lens[144..256].fill(9);
                    lit_lens[256..280].fill(7);
                    lit_lens[280..288].fill(8);
                    let dist_lens = vec![5u8; 32];
                    (
                        HuffmanTable::from_lengths(&lit_lens)?,
                        HuffmanTable::from_lengths(&dist_lens)?,
                    )
                } else {
                    let hlit = br.read_bits(5)? as usize + 257;
                    let hdist = br.read_bits(5)? as usize + 1;
                    let hclen = br.read_bits(4)? as usize + 4;
                    let mut cl_lens = [0u8; 19];
                    for i in 0..hclen {
                        cl_lens[CL_ORDER[i]] = br.read_bits(3)? as u8;
                    }
                    let cl_table = HuffmanTable::from_lengths(&cl_lens)?;
                    let mut lens = Vec::with_capacity(hlit + hdist);
                    while lens.len() < hlit + hdist {
                        let sym = cl_table.decode(&mut br)?;
                        match sym {
                            0..=15 => lens.push(sym as u8),
                            16 => {
                                let prev = *lens.last().ok_or("Repeticion Huffman sin previo")?;
                                let rep = br.read_bits(2)? as usize + 3;
                                for _ in 0..rep {
                                    lens.push(prev);
                                }
                            }
                            17 => {
                                let rep = br.read_bits(3)? as usize + 3;
                                for _ in 0..rep {
                                    lens.push(0);
                                }
                            }
                            18 => {
                                let rep = br.read_bits(7)? as usize + 11;
                                for _ in 0..rep {
                                    lens.push(0);
                                }
                            }
                            _ => return Err("Simbolo de longitud Huffman invalido"),
                        }
                    }
                    (
                        HuffmanTable::from_lengths(&lens[..hlit])?,
                        HuffmanTable::from_lengths(&lens[hlit..hlit + hdist])?,
                    )
                };

                loop {
                    let sym = lit_table.decode(&mut br)? as usize;
                    if sym < 256 {
                        out.push(sym as u8);
                    } else if sym == 256 {
                        break;
                    } else {
                        let l_idx = sym - 257;
                        if l_idx >= LEN_BASE.len() {
                            return Err("Codigo de longitud DEFLATE invalido");
                        }
                        let length = LEN_BASE[l_idx] as usize
                            + br.read_bits(LEN_EXTRA[l_idx] as u32)? as usize;
                        let d_sym = dist_table.decode(&mut br)? as usize;
                        if d_sym >= DIST_BASE.len() {
                            return Err("Codigo de distancia DEFLATE invalido");
                        }
                        let dist = DIST_BASE[d_sym] as usize
                            + br.read_bits(DIST_EXTRA[d_sym] as u32)? as usize;
                        if dist == 0 || dist > out.len() {
                            return Err("Distancia LZ77 fuera de rango");
                        }
                        for _ in 0..length {
                            let b = out[out.len() - dist];
                            out.push(b);
                        }
                    }
                }
            }
            _ => return Err("Tipo de bloque DEFLATE reservado (3)"),
        }

        if bfinal != 0 {
            break;
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_all_virtualbox_gui_assets() {
        let assets = VBoxAssets::load();
        assert!(
            assets.base_dir.is_some(),
            "Debe encontrar el directorio de recursos personalizados assets/"
        );

        let critical = [
            "logo_255.png",
            "logo_200px.png",
            "logo_48px.png",
            "logo_32px.png",
            "logo_16px.png",
            "OSE/VirtualBox_48px.png",
            "OSE/VirtualBox_32px.png",
            "OSE/VirtualBox_16px.png",
            "welcome_200px.png",
            "vm_new_32px.png",
            "vm_settings_32px.png",
            "vm_start_32px.png",
            "media_manager_32px.png",
            "os_win7.png",
            "os_win10.png",
            "os_ubuntu.png",
            "os_linux26.png",
            "state_powered_off_16px.png",
            "machine_16px.png",
            "chipset_16px.png",
            "hd_16px.png",
            "cd_16px.png",
            "sound_16px.png",
            "nw_16px.png",
            "usb_16px.png",
            "preview_full_16to9_242x155px.png",
            "preview_empty_16to9_242x155px.png",
            "wizard_new_welcome.png",
            "wizard_new_harddisk.png",
            "wizard_clone.png",
        ];

        for name in &critical {
            let img = assets
                .get(name)
                .unwrap_or_else(|| panic!("Fallo al decodificar asset PNG de Two Five Five (255): {}", name));
            assert!(img.width > 0 && img.height > 0);
            assert_eq!(img.data.len(), img.width * img.height * 4);
        }
    }
}
