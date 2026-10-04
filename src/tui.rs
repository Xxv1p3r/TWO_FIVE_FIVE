//! Dashboard interactivo TUI (Terminal User Interface) para Two Five Five (255).
//!
//! Proporciona una interfaz rica en la consola estilo btop/htop con:
//! - Monitoreo en tiempo real de vCPUs (cores activos, % uso, salidas KVM/s).
//! - Monitoreo de memoria RAM y mapa de memoria.
//! - Gestión de almacenamiento (CD-ROM / ISO montada, disco ATA, sectores e I/O).
//! - Estado de pantalla (modo VBE, resolución, bpp, LFB, FPS y escala de ventana).
//! - Controles interactivos: Pausar/Reanudar, Hotplug de vCPUs, Expulsar/Insertar ISO,
//!   Apagado ACPI limpio, Reinicio de VM, Escala de vídeo y visor de logs completo.

use crate::devices::DeviceBus;
use crate::devices::vga::VgaState;
use crate::metrics::VmmMetrics;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Buffer global de logs para mostrarlos de forma limpia dentro del TUI.
static LOG_QUEUE: Mutex<Option<VecDeque<String>>> = Mutex::new(None);
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Registra una línea de log en el buffer del TUI (o en stderr si el TUI está inactivo).
pub fn log(line: impl Into<String>) {
    let msg = line.into();
    if TUI_ACTIVE.load(Ordering::Relaxed) {
        if let Ok(mut q) = LOG_QUEUE.lock() {
            if let Some(buf) = q.as_mut() {
                buf.push_back(msg);
                if buf.len() > 500 {
                    buf.pop_front();
                }
                return;
            }
        }
    }
    eprintln!("{}", msg);
}

/// Comprueba si el TUI está activo actualmente.
pub fn is_active() -> bool {
    TUI_ACTIVE.load(Ordering::Relaxed)
}

/// Comandos interactivos que el usuario puede accionar desde el Dashboard TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardCommand {
    TogglePause,
    AddCpu,
    RemoveCpu,
    EjectCdrom,
    CycleScale,
    ShutdownAcpi,
    ResetVm,
    Quit,
}

/// Guarda y restaura el estado original del terminal (raw mode, alternate buffer).
struct RawTerminalGuard {
    orig_termios: libc::termios,
    active: bool,
}

impl RawTerminalGuard {
    fn enter() -> Option<Self> {
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 || libc::isatty(libc::STDOUT_FILENO) != 1 {
                return None;
            }
            let mut orig = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
                return None;
            }
            let mut raw = orig;
            // Modo no canónico (lectura byte a byte sin pulsar enter) y sin eco
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            // Entrar al búfer de pantalla alterno y ocultar cursor
            print!("\x1b[?1049h\x1b[?25l");
            let _ = std::io::stdout().flush();
            Some(Self {
                orig_termios: orig,
                active: true,
            })
        }
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                // Mostrar cursor y volver al búfer de pantalla principal
                print!("\x1b[?25h\x1b[?1049l");
                let _ = std::io::stdout().flush();
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig_termios);
            }
        }
    }
}

/// Obtiene las dimensiones actuales del terminal en columnas y filas.
fn terminal_size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
        && ws.ws_col > 0
        && ws.ws_row > 0
    {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (80, 25)
    }
}

/// Lee una tecla de stdin de forma no bloqueante.
fn read_key() -> Option<char> {
    let mut buf = [0u8; 1];
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    if n == 1 {
        Some(buf[0] as char)
    } else {
        None
    }
}

/// Genera una barra gráfica de progreso estilizada `[████░░░░]`.
fn progress_bar(pct: u32, width: usize) -> String {
    let width = width.max(4);
    let filled = ((pct as usize * width) / 100).min(width);
    let empty = width.saturating_sub(filled);
    let bar_str = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
    if pct >= 80 {
        format!("\x1b[31;1m[{}]\x1b[0m", bar_str)
    } else if pct >= 50 {
        format!("\x1b[33;1m[{}]\x1b[0m", bar_str)
    } else {
        format!("\x1b[32;1m[{}]\x1b[0m", bar_str)
    }
}

/// Formatea tamaños en bytes a unidades legibles (B, KB, MB, GB).
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

pub struct DashboardHandle {
    pub command_rx: Arc<Mutex<VecDeque<DashboardCommand>>>,
    pub running: Arc<AtomicBool>,
}

impl DashboardHandle {
    /// Obtiene el siguiente comando pendiente emitido por el usuario en el dashboard.
    pub fn pop_command(&self) -> Option<DashboardCommand> {
        if let Ok(mut q) = self.command_rx.lock() {
            q.pop_front()
        } else {
            None
        }
    }
}

/// Inicia el Dashboard TUI en un hilo independiente si stdin/stdout es un terminal interactivo.
pub fn start_tui(
    metrics: Arc<VmmMetrics>,
    vga_state: Arc<Mutex<VgaState>>,
    bus: Arc<Mutex<DeviceBus>>,
    iso_name: Option<String>,
    disk_name: Option<String>,
    running: Arc<AtomicBool>,
) -> Option<DashboardHandle> {
    let raw_guard = RawTerminalGuard::enter()?;

    // Inicializar buffer de logs
    {
        let mut q = LOG_QUEUE.lock().unwrap();
        *q = Some(VecDeque::with_capacity(512));
    }
    TUI_ACTIVE.store(true, Ordering::Relaxed);

    let command_queue = Arc::new(Mutex::new(VecDeque::new()));
    let command_tx = Arc::clone(&command_queue);
    let running_clone = Arc::clone(&running);

    std::thread::Builder::new()
        .name("vmm-tui-dashboard".to_string())
        .spawn(move || {
            let _guard = raw_guard;
            let start_time = Instant::now();
            let mut last_exits = 0u64;
            let mut last_rate_time = Instant::now();
            let mut exit_rate = 0u64;
            let mut view_mode_logs = false;
            let mut log_scroll_offset = 0usize;
            let mut status_notification: Option<(String, Instant)> = None;

            // Historial de salidas por core para estimar tasa por CPU
            let mut prev_vcpu_exits = [0u64; 16];
            let mut prev_vcpu_nanos = [0u64; 16];

            while running_clone.load(Ordering::Relaxed) {
                let (cols, rows) = terminal_size();
                let now = Instant::now();

                // ─── Entrada de Teclado no Bloqueante ───────────────
                while let Some(ch) = read_key() {
                    match ch {
                        ' ' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::TogglePause);
                            }
                            let current = metrics.is_paused.load(Ordering::Relaxed);
                            status_notification = Some((
                                if current { "Reanudando VM...".into() } else { "VM en pausa".into() },
                                Instant::now(),
                            ));
                        }
                        '+' | 'a' | 'A' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::AddCpu);
                            }
                            status_notification = Some(("Solicitud hotplug: +1 vCPU".into(), Instant::now()));
                        }
                        '-' | 'd' | 'D' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::RemoveCpu);
                            }
                            status_notification = Some(("Solicitud hotplug: -1 vCPU".into(), Instant::now()));
                        }
                        'e' | 'E' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::EjectCdrom);
                            }
                            status_notification = Some(("CD-ROM expulsado/alternado".into(), Instant::now()));
                        }
                        'r' | 'R' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::CycleScale);
                            }
                            status_notification = Some(("Alternando escala de visualización".into(), Instant::now()));
                        }
                        's' | 'S' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::ShutdownAcpi);
                            }
                            status_notification = Some(("Enviando apagado limpio ACPI...".into(), Instant::now()));
                        }
                        'l' | 'L' => {
                            view_mode_logs = !view_mode_logs;
                            log_scroll_offset = 0;
                        }
                        'j' if view_mode_logs => {
                            log_scroll_offset = log_scroll_offset.saturating_add(1);
                        }
                        'k' if view_mode_logs => {
                            log_scroll_offset = log_scroll_offset.saturating_sub(1);
                        }
                        'q' | 'Q' | '\x1b' => {
                            if let Ok(mut q) = command_tx.lock() {
                                q.push_back(DashboardCommand::Quit);
                            }
                            running_clone.store(false, Ordering::Relaxed);
                            break;
                        }
                        _ => {}
                    }
                }

                // ─── Cálculo de métricas periódicas (1 vez por segundo) ───
                let elapsed_rate = now.duration_since(last_rate_time).as_secs_f64();
                if elapsed_rate >= 0.5 {
                    let cur_total = metrics.exits_total.load(Ordering::Relaxed);
                    exit_rate = ((cur_total.saturating_sub(last_exits)) as f64 / elapsed_rate) as u64;
                    last_exits = cur_total;
                    last_rate_time = now;

                    // Actualizar estimación de uso por CPU
                    let active_cpus = metrics.num_cpus.load(Ordering::Relaxed) as usize;
                    for id in 0..active_cpus.min(16) {
                        let cur_exits = metrics.vcpu_exits[id].load(Ordering::Relaxed);
                        let diff_exits = cur_exits.saturating_sub(prev_vcpu_exits[id]);
                        prev_vcpu_exits[id] = cur_exits;

                        let cur_nanos = metrics.vcpu_active_nanos[id].load(Ordering::Relaxed);
                        let diff_nanos = cur_nanos.saturating_sub(prev_vcpu_nanos[id]);
                        prev_vcpu_nanos[id] = cur_nanos;

                        let total_nanos = (elapsed_rate * 1_000_000_000.0) as u64;
                        let pct = if total_nanos > 0 {
                            ((diff_nanos as f64 / total_nanos as f64) * 100.0)
                                .min(100.0)
                                .max(0.0) as u32
                        } else {
                            0
                        };
                        // Fallback con tasa de exits si los nanos no están instrumentados
                        let pct_estimated = if pct > 0 {
                            pct
                        } else {
                            ((diff_exits as f64 / 2500.0) * 100.0).clamp(5.0, 95.0) as u32
                        };
                        metrics.vcpu_usage_pct[id].store(pct_estimated, Ordering::Relaxed);
                    }
                }

                // Sincronizar métricas de vídeo desde VgaState
                if let Ok(vga) = vga_state.try_lock() {
                    let (w, h, b) = vga.get_resolution();
                    metrics.display_width.store(w as u32, Ordering::Relaxed);
                    metrics.display_height.store(h as u32, Ordering::Relaxed);
                    metrics.display_bpp.store(b as u32, Ordering::Relaxed);
                    metrics.is_vbe.store(vga.is_vbe_enabled(), Ordering::Relaxed);
                    if let Some(lfb) = vga.lfb_base {
                        metrics.lfb_gpa.store(lfb, Ordering::Relaxed);
                    }
                }

                // ─── Renderizado del Buffer TUI ─────────────────────
                let mut out = String::with_capacity(cols * rows * 2);
                out.push_str("\x1b[H"); // Cursor a inicio (0,0)

                let uptime_secs = start_time.elapsed().as_secs();
                let uptime_str = format!(
                    "{:02}:{:02}:{:02}",
                    uptime_secs / 3600,
                    (uptime_secs % 3600) / 60,
                    uptime_secs % 60
                );

                let is_paused = metrics.is_paused.load(Ordering::Relaxed);
                let status_str = if is_paused {
                    "\x1b[33;1m❚❚ PAUSADA\x1b[0m"
                } else {
                    "\x1b[32;1m● EJECUTANDO\x1b[0m"
                };

                let border_width = cols.min(100);

                if view_mode_logs {
                    // ─── Modo Visor de Logs Completo ────────────────
                    out.push_str(&format!(
                        "\x1b[36;1m┌{}┐\x1b[0m\r\n",
                        "─".repeat(border_width.saturating_sub(2))
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m \x1b[1mREGISTRO DE LOGS DEL VMM (Desplazamiento: j/k | Salir: L)\x1b[0m\x1b[36;1m{:>width$}│\x1b[0m\r\n",
                        "",
                        width = border_width.saturating_sub(60)
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m├{}┤\x1b[0m\r\n",
                        "─".repeat(border_width.saturating_sub(2))
                    ));

                    let log_rows = rows.saturating_sub(6).max(4);
                    let logs = LOG_QUEUE.lock().unwrap();
                    let empty_vec = VecDeque::new();
                    let buf = logs.as_ref().unwrap_or(&empty_vec);
                    let total_logs = buf.len();
                    let start_idx = total_logs
                        .saturating_sub(log_rows + log_scroll_offset)
                        .min(total_logs);
                    let end_idx = (start_idx + log_rows).min(total_logs);

                    for i in start_idx..end_idx {
                        let line = &buf[i];
                        let clean_line = if line.len() > border_width.saturating_sub(4) {
                            &line[..border_width.saturating_sub(4)]
                        } else {
                            line.as_str()
                        };
                        let padding = border_width.saturating_sub(4 + clean_line.len());
                        out.push_str(&format!(
                            "\x1b[36;1m│\x1b[0m {}{}\x1b[36;1m│\x1b[0m\r\n",
                            clean_line,
                            " ".repeat(padding)
                        ));
                    }
                    for _ in (end_idx - start_idx)..log_rows {
                        out.push_str(&format!(
                            "\x1b[36;1m│\x1b[0m{}\x1b[36;1m│\x1b[0m\r\n",
                            " ".repeat(border_width.saturating_sub(2))
                        ));
                    }
                    out.push_str(&format!(
                        "\x1b[36;1m└{}┘\x1b[0m\r\n",
                        "─".repeat(border_width.saturating_sub(2))
                    ));
                } else {
                    // ─── Modo Dashboard Principal ───────────────────
                    // Cabecera principal
                    out.push_str(&format!(
                        "\x1b[36;1m┌{}┐\x1b[0m\r\n",
                        "─".repeat(border_width.saturating_sub(2))
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m \x1b[1;37mTwo Five Five Hypervisor v0.1\x1b[0m  ESTADO: {}  UPTIME: \x1b[36m{}\x1b[0m  FIRMWARE: \x1b[35mSeaBIOS\x1b[0m{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        status_str,
                        uptime_str,
                        "",
                        pad = border_width.saturating_sub(83)
                    ));

                    // Sección CPU
                    out.push_str(&format!(
                        "\x1b[36;1m├{:─^width$}┤\x1b[0m\r\n",
                        " PROCESADORES (vCPUs) & EXITS ",
                        width = border_width.saturating_sub(2)
                    ));

                    let cpus = metrics.num_cpus.load(Ordering::Relaxed);
                    let max_cpus = metrics.max_cpus.load(Ordering::Relaxed);
                    let total_exits = metrics.exits_total.load(Ordering::Relaxed);
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  Cores: \x1b[32;1m{}/{}\x1b[0m [+/a: Añadir, -/d: Quitar]  Total Salidas: \x1b[1m{}\x1b[0m ({}/s){:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        cpus,
                        max_cpus,
                        total_exits,
                        exit_rate,
                        "",
                        pad = border_width.saturating_sub(74)
                    ));

                    // Listado de vCPUs activas con barras de uso
                    for id in 0..(cpus as usize).min(4) {
                        let pct = metrics.vcpu_usage_pct[id].load(Ordering::Relaxed);
                        let bar = progress_bar(pct, 16);
                        let role = if id == 0 { "BSP" } else { " AP" };
                        let vcpu_exit = metrics.vcpu_exits[id].load(Ordering::Relaxed);
                        out.push_str(&format!(
                            "\x1b[36;1m│\x1b[0m   vCPU {:02} [{}]: {} {:3}% | Salidas: {:>8}{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                            id,
                            role,
                            bar,
                            pct,
                            vcpu_exit,
                            "",
                            pad = border_width.saturating_sub(62)
                        ));
                    }

                    let hlt = metrics.exits_hlt.load(Ordering::Relaxed);
                    let io_in = metrics.exits_io_in.load(Ordering::Relaxed);
                    let io_out = metrics.exits_io_out.load(Ordering::Relaxed);
                    let mmio = metrics.exits_mmio_read.load(Ordering::Relaxed)
                        + metrics.exits_mmio_write.load(Ordering::Relaxed);
                    let irq_win = metrics.exits_irq_window.load(Ordering::Relaxed);
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m   Desglose: HLT: \x1b[33m{}\x1b[0m | I/O: \x1b[32m{}\x1b[0m (IN: {}, OUT: {}) | MMIO: \x1b[34m{}\x1b[0m | IRQ-win: {}{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        hlt,
                        io_in + io_out,
                        io_in,
                        io_out,
                        mmio,
                        irq_win,
                        "",
                        pad = border_width.saturating_sub(78)
                    ));

                    // Sección Memoria
                    out.push_str(&format!(
                        "\x1b[36;1m├{:─^width$}┤\x1b[0m\r\n",
                        " MEMORIA RAM & MAPA FÍSICO ",
                        width = border_width.saturating_sub(2)
                    ));
                    let ram_size = metrics.ram_bytes.load(Ordering::Relaxed);
                    let high_size = metrics.high_mem_bytes.load(Ordering::Relaxed);
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  RAM Principal: \x1b[32;1m{}\x1b[0m (0x00000000..0x10000000)  High RAM: \x1b[36;1m{}\x1b[0m (0xE0000000){:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        format_bytes(ram_size),
                        format_bytes(high_size),
                        "",
                        pad = border_width.saturating_sub(76)
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  Zonas Especiales: EBDA@\x1b[33m0x9FC00\x1b[0m | VGA ROM@\x1b[33m0xC0000\x1b[0m | BIOS FSEG@\x1b[33m0xF0000\x1b[0m | LAPIC@\x1b[33m0xFEE00000\x1b[0m{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        "",
                        pad = border_width.saturating_sub(78)
                    ));

                    // Sección Almacenamiento
                    out.push_str(&format!(
                        "\x1b[36;1m├{:─^width$}┤\x1b[0m\r\n",
                        " ALMACENAMIENTO & DISPOSITIVOS I/O ",
                        width = border_width.saturating_sub(2)
                    ));

                    let cd_inserted = bus.lock().map(|b| b.is_cdrom_inserted()).unwrap_or(true);
                    let cd_name = iso_name.as_deref().unwrap_or("Ninguna");
                    let cd_sectors = bus.lock().map(|b| b.cdrom_sectors_read()).unwrap_or(0);
                    let cd_status = if cd_inserted {
                        "\x1b[32;1m[MONTADO]\x1b[0m"
                    } else {
                        "\x1b[31;1m[EXPULSADO]\x1b[0m"
                    };
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  CD-ROM ATAPI: {} \"{}\" | Leídos: {} sectores ({}){:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        cd_status,
                        cd_name,
                        cd_sectors,
                        format_bytes(cd_sectors * 2048),
                        "",
                        pad = border_width.saturating_sub(76)
                    ));

                    let disk_sectors_r = bus.lock().map(|b| b.disk_sectors_read()).unwrap_or(0);
                    let disk_sectors_w = bus.lock().map(|b| b.disk_sectors_written()).unwrap_or(0);
                    let hd_name = disk_name.as_deref().unwrap_or("(sin disco)");
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  Disco ATA:    \x1b[36m\"{}\"\x1b[0m | Lecturas: {} sect ({}) | Escrituras: {} sect ({}){:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        hd_name,
                        disk_sectors_r,
                        format_bytes(disk_sectors_r * 512),
                        disk_sectors_w,
                        format_bytes(disk_sectors_w * 512),
                        "",
                        pad = border_width.saturating_sub(78)
                    ));

                    // Sección Pantalla / Resolución
                    out.push_str(&format!(
                        "\x1b[36;1m├{:─^width$}┤\x1b[0m\r\n",
                        " MONITOR GRÁFICO & RESOLUCIÓN ",
                        width = border_width.saturating_sub(2)
                    ));

                    let w = metrics.display_width.load(Ordering::Relaxed);
                    let h = metrics.display_height.load(Ordering::Relaxed);
                    let b = metrics.display_bpp.load(Ordering::Relaxed);
                    let fps = metrics.display_fps.load(Ordering::Relaxed);
                    let scale = metrics.display_scale.load(Ordering::Relaxed);
                    let is_vbe = metrics.is_vbe.load(Ordering::Relaxed);
                    let lfb = metrics.lfb_gpa.load(Ordering::Relaxed);
                    let mode_desc = if is_vbe {
                        format!("\x1b[32;1mVBE GFX Lineal\x1b[0m ({}x{}@{}bpp)", w, h, b)
                    } else {
                        "\x1b[33;1mModo Texto VGA\x1b[0m (80x25 @ 0xB8000)".into()
                    };

                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  Modo: {} | FPS: \x1b[1m{}\x1b[0m | Escala: \x1b[36m{}x\x1b[0m [R: Alternar]{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        mode_desc,
                        fps,
                        scale,
                        "",
                        pad = border_width.saturating_sub(76)
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m  LFB Base GPA: \x1b[35m0x{:08X}\x1b[0m | Backend GUI: \x1b[32;1mminifb Host Display\x1b[0m{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                        lfb,
                        "",
                        pad = border_width.saturating_sub(72)
                    ));

                    // Sección Logs Recientes (últimas 5 líneas)
                    out.push_str(&format!(
                        "\x1b[36;1m├{:─^width$}┤\x1b[0m\r\n",
                        " LOGS RECIENTES [L: Pantalla Completa] ",
                        width = border_width.saturating_sub(2)
                    ));

                    let recent_logs = {
                        let logs = LOG_QUEUE.lock().unwrap();
                        let empty = VecDeque::new();
                        let buf = logs.as_ref().unwrap_or(&empty);
                        let take_count = 5.min(buf.len());
                        buf.iter().rev().take(take_count).cloned().collect::<Vec<_>>()
                    };

                    for log_line in recent_logs.iter().rev() {
                        let clean = if log_line.len() > border_width.saturating_sub(4) {
                            &log_line[..border_width.saturating_sub(4)]
                        } else {
                            log_line.as_str()
                        };
                        let pad = border_width.saturating_sub(4 + clean.len());
                        out.push_str(&format!(
                            "\x1b[36;1m│\x1b[0m \x1b[2m{}\x1b[0m{}\x1b[36;1m│\x1b[0m\r\n",
                            clean,
                            " ".repeat(pad)
                        ));
                    }
                    for _ in recent_logs.len()..5 {
                        out.push_str(&format!(
                            "\x1b[36;1m│\x1b[0m{}\x1b[36;1m│\x1b[0m\r\n",
                            " ".repeat(border_width.saturating_sub(2))
                        ));
                    }

                    // Notificación de estado temporal si existe
                    if let Some((ref msg, t)) = status_notification {
                        if t.elapsed().as_secs() < 3 {
                            out.push_str(&format!(
                                "\x1b[36;1m│\x1b[0m  \x1b[33;1m⚡ {}\x1b[0m{:>pad$} \x1b[36;1m│\x1b[0m\r\n",
                                msg,
                                "",
                                pad = border_width.saturating_sub(6 + msg.len())
                            ));
                        } else {
                            status_notification = None;
                        }
                    }

                    // Barra de Acciones y Ayuda inferior
                    out.push_str(&format!(
                        "\x1b[36;1m├{}┤\x1b[0m\r\n",
                        "─".repeat(border_width.saturating_sub(2))
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m│\x1b[0m \x1b[1m[Espacio]\x1b[0m Pausar \x1b[1m[+/a]\x1b[0m +CPU \x1b[1m[-/d]\x1b[0m -CPU \x1b[1m[E]\x1b[0m Expulsar ISO \x1b[1m[R]\x1b[0m Escala \x1b[1m[S]\x1b[0m Apagar \x1b[1m[L]\x1b[0m Logs \x1b[1m[Q]\x1b[0m Salir \x1b[36;1m│\x1b[0m\r\n"
                    ));
                    out.push_str(&format!(
                        "\x1b[36;1m└{}┘\x1b[0m\r\n",
                        "─".repeat(border_width.saturating_sub(2))
                    ));
                }

                // Escribir el frame en la salida estándar de una sola vez
                let _ = std::io::stdout().write_all(out.as_bytes());
                let _ = std::io::stdout().flush();

                // Frecuencia de actualización del dashboard (10 FPS)
                std::thread::sleep(Duration::from_millis(100));
            }

            TUI_ACTIVE.store(false, Ordering::Relaxed);
        })
        .ok()?;

    Some(DashboardHandle {
        command_rx: command_queue,
        running,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn test_progress_bar() {
        let b0 = progress_bar(0, 10);
        assert!(b0.contains("░░░░░░░░░░"));
        let b100 = progress_bar(100, 10);
        assert!(b100.contains("██████████"));
        let b50 = progress_bar(50, 10);
        assert!(b50.contains("█████░░░░░"));
    }

    #[test]
    fn test_dashboard_command_queue() {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let handle = DashboardHandle {
            command_rx: Arc::clone(&q),
            running: Arc::new(AtomicBool::new(true)),
        };

        assert_eq!(handle.pop_command(), None);
        q.lock().unwrap().push_back(DashboardCommand::TogglePause);
        q.lock().unwrap().push_back(DashboardCommand::AddCpu);

        assert_eq!(handle.pop_command(), Some(DashboardCommand::TogglePause));
        assert_eq!(handle.pop_command(), Some(DashboardCommand::AddCpu));
        assert_eq!(handle.pop_command(), None);
    }

    #[test]
    fn test_tui_log_buffer() {
        // Inicializar cola
        {
            let mut lq = LOG_QUEUE.lock().unwrap();
            *lq = Some(VecDeque::new());
        }
        TUI_ACTIVE.store(true, Ordering::Relaxed);

        log("test line 1");
        log("test line 2");

        {
            let lq = LOG_QUEUE.lock().unwrap();
            let q = lq.as_ref().unwrap();
            assert_eq!(q.len(), 2);
            assert_eq!(q[0], "test line 1");
            assert_eq!(q[1], "test line 2");
        }

        TUI_ACTIVE.store(false, Ordering::Relaxed);
    }
}
