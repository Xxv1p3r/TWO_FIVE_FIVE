//! Two Five Five (255) — Hipervisor Tipo-2 minimalista sobre KVM/Linux.

mod devices;
mod display;
mod guest_mem;
pub mod gui_launcher;
mod metrics;
mod snapshot;
pub mod tui;
pub mod unattended;
pub mod vbox_assets;
pub mod vdisk;

use devices::DeviceBus;
use guest_mem::GuestMemory;
use kvm_bindings::{kvm_fpu, kvm_mp_state, kvm_regs, kvm_sregs, kvm_vcpu_events, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit};
use std::fs::File;
use std::io::Read;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
const GUEST_MEM_SIZE: usize = 4096 * 1024 * 1024;
const RESET_VECTOR_CS: u64 = 0xF000;
const RESET_VECTOR_RIP: u64 = 0xFFF0;
/// Máximo de reboots provocados por crashes del guest (triple fault o
/// KVM InternalError) antes de rendirse. Evita un bucle infinito de
/// reinicios cuando el guest siempre crashea en el mismo punto.
const MAX_CRASH_REBOOTS: u32 = 5;

static IS_UEFI: AtomicBool = AtomicBool::new(false);

/// Helper para consultar variables de entorno soportando TWO_FIVE_FIVE_*, TFF_* y MI_VMM_*
pub fn get_vmm_env(key: &str) -> Result<String, std::env::VarError> {
    std::env::var(format!("TWO_FIVE_FIVE_{}", key))
        .or_else(|_| std::env::var(format!("TFF_{}", key)))
        .or_else(|_| std::env::var(format!("MI_VMM_{}", key)))
}

/// Resetea la VM entera al POST (equivalente a un reset por hardware tras
/// un crash del guest) e incrementa el contador de reboots.
fn crash_reboot(
    vcpu: &kvm_ioctls::VcpuFd,
    vm: &kvm_ioctls::VmFd,
    bus: &mut DeviceBus,
    guest_mem: &mut [u8],
    high_mem: Option<&mut [u8]>,
    bios: Option<&[u8]>,
    vga_rom: Option<&[u8]>,
    total_reboots: &mut u32,
) {
    *total_reboots += 1;
    eprintln!("[VMM] Crash/Reset del guest — reseteando VM a POST (reboot {} de {})...",
        total_reboots, MAX_CRASH_REBOOTS);
    reset_vcpu_to_post(vcpu, vm, bus, guest_mem, high_mem, bios, vga_rom);
}

fn bios_load_addr(bios_len: usize) -> u64 {
    if IS_UEFI.load(Ordering::Relaxed) {
        (0x1_0000_0000u64).saturating_sub(bios_len as u64)
    } else {
        (0x0010_0000u64).saturating_sub(bios_len as u64)
    }
}

fn usage() -> ! {
    eprintln!("Uso: two-five-five [opciones] [bios.bin] [imagen.iso] [disco.vdi|disco.img]");
    eprintln!("O simplemente: two-five-five                  (Abre el Mini GUI de Maquinas y Discos VDI)");
    eprintln!("Opciones de Mini GUI y Recursos:");
    eprintln!("  -g, --gui, --launcher         Abrir el Administrador Grafico de VMs y Discos VDI");
    eprintln!("  --startvm, --vm-name <nombre> Iniciar la VM especificada directamente");
    eprintln!("  --no-gui                      Omitir el lanzador grafico inicial");
    eprintln!("  --cpus <1..16>                Numero de vCPUs para la VM (def: 4)");
    eprintln!("  --ram <MiB>                   Memoria RAM en MiB para la VM (def: 4096)");
    eprintln!("  --disk, --drive <archivo.vdi> Especificar imagen de disco virtual");
    eprintln!("Opciones de Discos Virtuales (Motor VirtualBox VDI 1.1+):");
    eprintln!("  --create-vdi <file.vdi> <GiB> [--fixed]  Crear disco virtual VDI dinamico o fijo");
    eprintln!("  --compact-vdi <file.vdi>                 Compactar bloques cero en imagen VDI");
    eprintln!("  --disk-info <file.vdi|img>               Mostrar metadatos de imagen de disco");
    eprintln!("Opciones de instalación desatendida:");
    eprintln!("  -u, --unattended              Activar instalación desatendida");
    eprintln!("  --unattended-user <usuario>   Usuario para el sistema (def: two55)");
    eprintln!("  --unattended-pass <password>  Contraseña de usuario y root (def: two55)");
    eprintln!("  --unattended-host <hostname>  Hostname de la máquina virtual (def: two55-vm)");
    eprintln!("  --cdrom, --iso <archivo.iso>  Especificar imagen de instalación ISO");
    eprintln!("Ejemplos:");
    eprintln!("  two-five-five --gui");
    eprintln!("  two-five-five --create-vdi mi_disco.vdi 20");
    eprintln!("  two-five-five CorePlus-current.iso mi_disco.vdi");
    exit(1);
}

fn find_default_bios() -> Option<PathBuf> {
    let candidates = [
        "bios/bios-256k.bin",
        "bios-256k.bin",
        "/usr/share/seabios/bios-256k.bin",
        "/usr/share/seabios/bios-128k.bin",
        "/usr/share/qemu/bios-256k.bin",
        "/usr/share/qemu/bios-128k.bin",
        "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    ];
    for c in &candidates {
        let p = Path::new(c);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let root = exe_dir.join("../..");
            for c in &candidates {
                let p = root.join(c);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn resolve_iso_file(requested: &str) -> Option<PathBuf> {
    if is_disk_file(requested) || is_bios_file(requested) {
        return None;
    }
    let p = Path::new(requested);
    if p.is_file() && requested.to_lowercase().ends_with(".iso") {
        return Some(p.to_path_buf());
    }
    for ext in &[".iso", "-current.iso"] {
        let test = format!("{}{}", requested, ext);
        let tp = Path::new(&test);
        if tp.is_file() {
            return Some(tp.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let root = exe_dir.join("../..");
            for dir in &[exe_dir, root.as_path()] {
                let test = dir.join(requested);
                if test.is_file() {
                    return Some(test);
                }
                for ext in &[".iso", "-current.iso"] {
                    let test = dir.join(format!("{}{}", requested, ext));
                    if test.is_file() {
                        return Some(test);
                    }
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(".") {
        let req_lower = requested.to_lowercase();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext.eq_ignore_ascii_case("iso") {
                        let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                        if name.contains(&req_lower) {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

fn auto_detect_iso() -> Option<PathBuf> {
    let known = [
        "CorePlus-current.iso",
        "TinyCore-current.iso",
        "Core-current.iso",
    ];
    for name in &known {
        let p = Path::new(name);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let root = exe_dir.join("../..");
            for name in &known {
                let p = root.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(".") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension() {
                    if ext.eq_ignore_ascii_case("iso") {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

fn is_bios_file(p: &str) -> bool {
    let s = p.to_lowercase();
    s.ends_with(".bin") || s.ends_with(".fd") || s.ends_with(".rom") || s.contains("seabios") || s.contains("ovmf")
}

fn is_disk_file(p: &str) -> bool {
    let s = p.to_lowercase();
    s.ends_with(".img") || s.ends_with(".raw") || s.ends_with(".vdi") || s.ends_with(".qcow2")
}

fn auto_detect_disk() -> Option<PathBuf> {
    for name in &[
        "disk.vdi",
        "hdd.vdi",
        "vm_disk.vdi",
        "disk.img",
        "hdd.img",
        "rootfs.img",
        "vm_disk.img",
    ] {
        let p = Path::new(name);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let root = exe_dir.join("../..");
            for name in &[
                "disk.vdi",
                "hdd.vdi",
                "vm_disk.vdi",
                "disk.img",
                "hdd.img",
                "rootfs.img",
                "vm_disk.img",
            ] {
                let p = root.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Set by SIGTERM/SIGINT handler; checked in the vCPU loop to dump state and exit.
pub static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static ACPI_SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static RESET_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static CDROM_EJECT_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Lista de TIDs de los hilos AP para poder interrumpirlos con señales en reinicios globales.
pub static AP_TIDS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

/// TID del hilo BSP (el principal). Lo usa el hilo de display para despertar
/// al BSP con tgkill cuando se cierra la ventana (tarea 17): una señal de
/// proceso podría caer en un hilo AP y nadie procesaría el cierre.
pub static BSP_TID: AtomicI32 = AtomicI32::new(0);

pub fn request_vm_reset() {
    RESET_REQUESTED.store(true, Ordering::SeqCst);
    let bsp_tid = BSP_TID.load(Ordering::Relaxed);
    if bsp_tid > 0 {
        unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), bsp_tid, libc::SIGUSR1); }
    }
}

extern "C" fn sigusr1_handler(_: libc::c_int) {}

extern "C" fn shutdown_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Dump the VGA text screen (80x25 @ 0xB8000) to stderr.
/// Useful to see SeaBIOS's on-screen messages (e.g. "No bootable device").
fn dump_vga_text_screen(guest_mem: &[u8]) {
    let base = 0xB8000usize;
    if base + 80 * 25 * 2 > guest_mem.len() { return; }
    eprintln!("[VMM] ─── Pantalla VGA (0xB8000) ───");
    for row in 0..25 {
        let mut line = String::with_capacity(80);
        for col in 0..80 {
            let ch = guest_mem[base + (row * 80 + col) * 2];
            line.push(if (32..127).contains(&ch) { ch as char } else { ' ' });
        }
        eprintln!("{:2}|{}", row, line.trim_end());
    }
    eprintln!("[VMM] ──────────────────────────────");
}

/// Inicializa la BIOS Data Area (0x400-0x4FF), la EBDA en 0x9FC00 y el
/// buffer de texto VGA (0xB8000) con el banner de arranque. Se ejecuta
/// tanto al encender la VM como en cada reset (0xCF9 / crash / triple fault):
/// tras un reboot el guest debe ver el hardware de memoria en el mismo
/// estado que al arrancar (el EBDA y el banner quedan pisados por el boot).
fn init_bios_data_area(guest_mem: &mut [u8]) {
    // ─── VGA text buffer init (0xB8000) ─────────────────────────
    // Clear the VGA text buffer so SeaBIOS messages appear cleanly.
    {
        let vga_off = 0xB8000usize;
        let vga_end = vga_off + 80 * 25 * 2;
        if vga_end <= guest_mem.len() {
            guest_mem[vga_off..vga_end].fill(0x00);
            // Write a startup banner in the first row
            let banner = b"two-five-five: waiting for BIOS...";
            for (i, &ch) in banner.iter().enumerate() {
                guest_mem[vga_off + i * 2] = ch;
                guest_mem[vga_off + i * 2 + 1] = 0x07;
            }
        }
    }

    // ─── BDA init (BIOS Data Area, phys 0x400-0x4FF) ────────────
    // SeaBIOS expects certain BDA fields to be initialized before it
    // starts polling the keyboard buffer. We pre-initialize them so
    // early polling loops don't spin forever.
    {
        if guest_mem.len() >= 0x500 {
            guest_mem[0x400..0x500].fill(0x00);
        }
        // COM1-COM4 base I/O addresses (0x400-0x407)
        guest_mem[0x400] = 0xF8; guest_mem[0x401] = 0x03; // COM1 = 0x3F8
        guest_mem[0x402] = 0xF8; guest_mem[0x403] = 0x02; // COM2 = 0x2F8
        guest_mem[0x404] = 0xE8; guest_mem[0x405] = 0x03; // COM3 = 0x3E8
        guest_mem[0x406] = 0xE8; guest_mem[0x407] = 0x02; // COM4 = 0x2E8

        // Equipment word (0x410): keyboard installed, 80x25 mono
        guest_mem[0x410] = 0x21; // bit0=keyboard, bits5-4=01=CGA 40x25 → actually 01=40x25, 10=80x25
        guest_mem[0x411] = 0x00;

        // Conventional memory size in KB (0x413)
        guest_mem[0x413] = 0x80; // 640 KB = 0x280 → low byte
        guest_mem[0x414] = 0x02; // high byte

        // Keyboard buffer head (0x41A) and tail (0x41C)
        // Buffer area is 0x41E-0x43D (32 bytes, 16 scancodes)
        guest_mem[0x41A] = 0x1E; guest_mem[0x41B] = 0x00; // head = 0x1E
        guest_mem[0x41C] = 0x1E; guest_mem[0x41D] = 0x00; // tail = 0x1E (empty)

        // Keyboard buffer start (0x480) and end (0x482)
        guest_mem[0x480] = 0x1E; guest_mem[0x481] = 0x00; // start = 0x1E
        guest_mem[0x482] = 0x3E; guest_mem[0x483] = 0x00; // end = 0x3E

        // ─── EBDA (Extended BIOS Data Area) ──────────────────────
        // SeaBIOS reads CMOS registers 0x34-0x35 for the EBDA segment.
        // Set it to 0x9FC0 (standard location: phys 0x9FC00).
        guest_mem[0x40E] = 0xC0; // EBDA segment low byte (0xC0)
        guest_mem[0x40F] = 0x9F; // EBDA segment high byte (0x9F) → 0x9FC0

        // Initialize a minimal EBDA at phys 0x9FC00 (1KB)
        let ebda_off = 0x9FC00usize;
        if ebda_off + 0x400 <= guest_mem.len() {
            guest_mem[ebda_off..ebda_off + 0x400].fill(0x00);
            guest_mem[ebda_off] = 0x01; // EBDA size in KB
            guest_mem[ebda_off + 0x30] = 0xC0;
            guest_mem[ebda_off + 0x31] = 0x9F;
        }

        // ─── TSS Real para prevención de Triple Fault (Item 23) ─────
        // Configura una estructura TSS válida en 0x1000 con pila en 0x7000
        // para que un Double Fault (#DF) disponga de un stack limpio.
        devices::legacy::init_guest_tss(guest_mem, 0x1000, 0x7000);

        // ─── BDA Video Display Area (phys 0x449 - 0x48A) ─────────────
        // Parámetros de modo texto estándar (80x25 modo 3): esenciales para
        // que cargadores como ISOLINUX/menu.c32 detecten columnas > 0.
        guest_mem[0x449] = 0x03; // Modo de vídeo actual: 80x25 texto color
        guest_mem[0x44A] = 80;   // Columnas de texto (LE u16): 80
        guest_mem[0x44B] = 0;
        guest_mem[0x44C] = 0x00; // Tamaño del buffer de regeneración: 4096 bytes
        guest_mem[0x44D] = 0x10;
        guest_mem[0x44E] = 0x00; // Offset de página activa: 0x0000
        guest_mem[0x44F] = 0x00;
        for p in 0..8 {
            guest_mem[0x450 + p * 2] = 0;     // Cursor col = 0
            guest_mem[0x450 + p * 2 + 1] = 0; // Cursor row = 0
        }
        guest_mem[0x460] = 0x06; // Línea de escaneo inicial cursor
        guest_mem[0x461] = 0x07; // Línea de escaneo final cursor
        guest_mem[0x462] = 0x00; // Página de visualización activa: 0
        guest_mem[0x463] = 0xD4; // Puerto base I/O CRTC: 0x03D4 (color)
        guest_mem[0x464] = 0x03;
        guest_mem[0x465] = 0x09; // Registro de selección de modo
        guest_mem[0x466] = 0x00; // Registro de paleta
        guest_mem[0x484] = 24;   // Filas de texto menos 1: 24 (25 filas)
        guest_mem[0x485] = 16;   // Altura del carácter en escaneos (fuente 8x16)
        guest_mem[0x486] = 0x00;
        guest_mem[0x487] = 0x60; // Combinación pantalla VGA
        guest_mem[0x488] = 0x09; // Interruptores EGA/VGA

        eprintln!("[VMM] BDA inicializado: mem=640KB, kb_buf=head=tail=0x1E, EBDA=0x9FC0, TSS=0x1000, VGA=80x25");
    }
}

/// Con `KVM_CREATE_IRQCHIP` el LAPIC (base 0xFEE00000) y el IOAPIC
/// (0xFEC00000) viven DENTRO de KVM. Si un memslot de RAM respalda esas
/// páginas, la EPT resuelve los accesos como memoria normal y el irqchip
/// del kernel jamás ve un registro: el guest no puede sondear CPUs
/// (ICR→INIT-SIPI) ni enrutar INTx (IOREDTBL). Estas ventanas se recortan
/// al registrar high_mem, y los antiguos "stubs falsos" se eliminaron:
/// eran exactamente lo que pisaba al irqchip real.
#[cfg(test)]
const KERNEL_IRQCHIP_HOLES: [(u64, u64); 2] = [
    (0xFEC0_0000, 0x1000), // IOAPIC: página de 4 KiB
    (0xFEE0_0000, 0x1000), // LAPIC: página de 4 KiB (APIC base por defecto)
];

/// Ventana PCI MMIO no respaldada por RAM para que los accesos a BARs MMIO
/// (como el BAR2 de VGA en 0xFE01F000) generen exits MMIO a userspace en vez
/// de ser resueltos silenciosamente como memoria física en RAM por la EPT.
pub const PCI_MMIO_HOLE_START: u64 = 0xFE00_0000;
pub const PCI_MMIO_HOLE_SIZE: u64 = 0x00C0_0000; // 12 MiB hasta 0xFEC0_0000 (IOAPIC)

pub const HIGH_MEM_HOLES: [(u64, u64); 3] = [
    (PCI_MMIO_HOLE_START, PCI_MMIO_HOLE_SIZE),
    (0xFEC0_0000, 0x1000), // IOAPIC: página de 4 KiB
    (0xFEE0_0000, 0x1000), // LAPIC: página de 4 KiB (APIC base por defecto)
];

/// Resta `holes` del rango `[base, base+size)` y devuelve los sub-rangos
/// libres resultantes. Función pura para poder testear el layout de
/// memslots sin KVM.
fn carve_reserved_holes(base: u64, size: u64, holes: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let end = base.saturating_add(size);
    let mut regions: Vec<(u64, u64)> = vec![(base, end)];
    for &(hstart, hlen) in holes {
        if hlen == 0 {
            continue; // hueco degenerado
        }
        let hend = hstart.saturating_add(hlen);
        let mut next: Vec<(u64, u64)> = Vec::with_capacity(regions.len() + 2);
        for (s, e) in regions {
            if hend <= s || hstart >= e {
                next.push((s, e)); // sin intersección
            } else {
                if s < hstart {
                    next.push((s, hstart));
                }
                if hend < e {
                    next.push((hend, e));
                }
            }
        }
        regions = next;
    }
    regions
}

/// BDA tick counter physical address (BIOS Data Area, offset 0x400 base + 0x6C).
const BDA_TICK_ADDR: usize = 0x46C;

// ─── Temporización del PIT (tarea 16) ──────────────────────────────
// Antes el PIT dependía de un setitimer(1ms)+SIGALRM: la señal es global
// (podía caer en cualquier hilo, no solo el BSP), obligaba a interrumpir
// vcpu.run() con EINTR cada milisegundo y era el único reloj del sistema.
// Ahora un hilo dedicado duerme con clock_nanosleep ABSOLUTO sobre
// CLOCK_MONOTONIC (sin deriva acumulada) y cada 1 ms:
//   - avanza el PIT emulado por el tiempo real transcurrido y pulsa IRQ0
//     en el PIC del kernel (el irqchip despierta al vCPU aunque esté en HLT),
//   - mantiene el tick del BDA (0x46C) a 18.2 Hz para los wait_ms() de SeaBIOS,
//   - inyecta el input del host (teclado/ratón/UART) aunque el BSP esté
//     bloqueado dentro de vcpu.run().

/// Tiempo monotónico actual (CLOCK_MONOTONIC).
fn mono_now() -> libc::timespec {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
    ts
}

/// Suma `ms` milisegundos a un timespec (para deadlines absolutos).
fn ts_add_ms(ts: libc::timespec, ms: i64) -> libc::timespec {
    let mut r = ts;
    r.tv_nsec += ms * 1_000_000;
    if r.tv_nsec >= 1_000_000_000 {
        r.tv_sec += r.tv_nsec / 1_000_000_000;
        r.tv_nsec %= 1_000_000_000;
    }
    r
}

/// Milisegundos transcurridos entre `a` y `b` (a >= b, reloj monotónico).
fn ts_elapsed_ms(a: libc::timespec, b: libc::timespec) -> u32 {
    let ms = (a.tv_sec - b.tv_sec) * 1000 + (a.tv_nsec - b.tv_nsec) / 1_000_000;
    if ms < 0 { 0 } else { ms as u32 }
}

/// Hilo de temporización del VMM (sustituye al setitimer de 1 ms).
fn pit_timer_thread(
    vm: Arc<kvm_ioctls::VmFd>,
    bus: Arc<Mutex<DeviceBus>>,
    guest_mem: Arc<GuestMemory>,
    kbd_queue: Arc<Mutex<VecDeque<u8>>>,
    mouse_queue: Arc<Mutex<VecDeque<(i16, i16, u8)>>>,
    tablet_queue: Arc<Mutex<VecDeque<(u16, u16, u8, i8)>>>,
    resize_queue: Arc<Mutex<Option<(u32, u32)>>>,
    irq0_raised: Arc<AtomicBool>,
    bda_tick_count: Arc<AtomicU32>,
    metrics: Arc<metrics::VmmMetrics>,
) {
    let mut ms_since_bda_tick: u32 = 0;
    let mut last = mono_now();
    loop {
        let now = mono_now();
        let elapsed_ms = ts_elapsed_ms(now, last).max(1);
        last = now;

        // Avanzar el PIT (1.193182 MHz), el RTC CMOS (IRQ8), inyectar eventos de
        // entrada del host y avanzar dispositivos PCI bajo UNA SOLA adquisición
        // de `bus.lock()` por tick de 1 ms para minimizar la contención con los vCPUs.
        let resize_req = resize_queue.try_lock().ok().and_then(|mut rq| rq.take());
        let (underflow, rtc_irq8, ps2_irq, mouse_irq, uart_irq, (ide14, ide15)) = {
            let mut b = bus.lock().unwrap_or_else(|p| p.into_inner());
            let u = b.legacy_irq.pit_advance_ticks(elapsed_ms * 1193);
            let r = b.cmos.rtc_advance_ns((elapsed_ms as u64) * 1_000_000);

            if let Ok(mut q) = kbd_queue.try_lock() {
                while let Some(sc) = q.pop_front() {
                    b.legacy_irq.inject_scancode(sc);
                }
            }
            if let Ok(mut q) = mouse_queue.try_lock() {
                while let Some((dx, dy, buttons)) = q.pop_front() {
                    b.legacy_irq.inject_mouse_delta(dx, dy, buttons);
                }
            }
            if let Ok(mut q) = tablet_queue.try_lock() {
                while let Some((x, y, buttons, wheel)) = q.pop_front() {
                    b.inject_tablet_event(x, y, buttons, wheel);
                }
            }

            let ps2 = b.take_ps2_irq();
            let mouse = b.take_mouse_irq();

            if let Some((w, h)) = resize_req {
                b.request_resolution(w, h);
            }

            b.step_usb(&guest_mem);
            b.step_virtio_serial(&guest_mem);
            b.step_virtio_net(&guest_mem);
            b.step_e1000(&guest_mem);
            sync_pci_irqs(&mut b, &vm, None);

            let uart = b.take_uart_irq();
            let ide = b.take_ide_irq();
            (u, r, ps2, mouse, uart, ide)
        };
        if underflow {
            // Flanco fresco bajo→alto en IRQ0 (PIC edge-triggered). No se
            // baja después: un tick pendiente sin entregar no debe perderse.
            // El irqchip del kernel despierta al vCPU aunque esté en HLT.
            vm.set_irq_line(0, false).ok();
            vm.set_irq_line(0, true).ok();
            irq0_raised.store(true, Ordering::Relaxed);
            metrics.record_irq(0);
        }
        if rtc_irq8 {
            vm.set_irq_line(8, false).ok();
            vm.set_irq_line(8, true).ok();
            metrics.record_irq(8);
        }

        // Tick del BDA (0x46C) a 18.2 Hz (~55 ms): SeaBIOS lo sondea en
        // wait_ms(). Se escribe aquí (hilo de tiempo real), no en el BSP.
        ms_since_bda_tick += elapsed_ms;
        while ms_since_bda_tick >= 55 {
            ms_since_bda_tick -= 55;
            let ticks = bda_tick_count.fetch_add(1, Ordering::Relaxed) + 1;
            // Escritura acotada y volatile (GuestMemory): el BSP/guest lo lee
            // sin sincronización (tick del BDA 0x46C).
            guest_mem.write_u32_volatile(BDA_TICK_ADDR, ticks);
        }

        // Mantener geometría de pantalla BDA (evita que syslinux/menu.c32 lea cols=0
        // si SeaBIOS limpió la BDA o no inicializó la consola VGA antes del bootloader).
        if guest_mem.read_u16(0x44A) == 0 {
            guest_mem.write_u8(0x449, 0x03);
            guest_mem.write_u16(0x44A, 80);
            guest_mem.write_u8(0x484, 24);
            guest_mem.write_u16(0x463, 0x03D4);
        }

        if ps2_irq {
            vm.set_irq_line(1, false).ok();
            vm.set_irq_line(1, true).ok();
        }
        if mouse_irq {
            vm.set_irq_line(12, false).ok();
            vm.set_irq_line(12, true).ok();
        }
        if uart_irq {
            vm.set_irq_line(4, false).ok();
            vm.set_irq_line(4, true).ok();
        }
        if ide14 {
            vm.set_irq_line(14, false).ok();
            vm.set_irq_line(14, true).ok();
        }
        if ide15 {
            vm.set_irq_line(15, false).ok();
            vm.set_irq_line(15, true).ok();
        }

        // Dormir hasta ahora + 1 ms. clock_nanosleep absoluto: si una señal
        // lo interrumpe (EINTR) se reintenta con el mismo deadline (ya
        // pasado → retorna al momento) y el bucle no acumula deriva.
        let deadline = ts_add_ms(mono_now(), 1);
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &deadline,
                std::ptr::null_mut(),
            )
        };
        if rc != 0 && rc != libc::EINTR {
            eprintln!("[VMM] clock_nanosleep falló: rc={}", rc);
            return;
        }
    }
}

/// Reset the vCPU to the power-on state (like a hardware reset via port 0xCF9).
///
/// Además de restaurar el estado del procesador, resetea los dispositivos
/// emulados (`bus.reset()`), reinicializa BDA/EBDA/banner VGA en la RAM del
/// guest y baja las líneas de IRQ del kernel: tras un reboot el guest debe
/// ver el mismo hardware limpio que al encender (tarea 5 del TODO).
fn reset_vcpu_to_post(
    vcpu: &kvm_ioctls::VcpuFd,
    vm: &kvm_ioctls::VmFd,
    bus: &mut DeviceBus,
    guest_mem: &mut [u8],
    high_mem: Option<&mut [u8]>,
    bios: Option<&[u8]>,
    vga_rom: Option<&[u8]>,
) {
    // 1. Interrumpir todos los hilos AP para que salgan de vcpu.run() y se aparquen
    RESET_REQUESTED.store(true, Ordering::SeqCst);
    if let Ok(tids) = AP_TIDS.lock() {
        for &tid in tids.iter() {
            if tid > 0 {
                unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR1); }
            }
        }
    }
    // Dar una pequeña ventana para que los hilos AP reciban la señal y se aparquen
    std::thread::sleep(std::time::Duration::from_millis(20));

    // 2. Limpiar eventos pendientes en vCPU (excepciones, interrupciones residuales o NMIs)
    let events = kvm_vcpu_events::default();
    vcpu.set_vcpu_events(&events).ok();

    // 3. Restaurar estado de registros de modo real completamente limpios
    let mut sregs = kvm_sregs::default();
    if IS_UEFI.load(Ordering::Relaxed) {
        sregs.cs.base = 0xFFFF_0000;
        sregs.cs.selector = 0xF000;
    } else {
        sregs.cs.base = (RESET_VECTOR_CS as u16 as u64) << 4;
        sregs.cs.selector = RESET_VECTOR_CS as u16;
    }
    sregs.cs.limit = 0xFFFF;
    sregs.cs.type_ = 11;
    sregs.cs.present = 1;
    sregs.cs.s = 1;
    sregs.cs.g = 0;
    sregs.cs.db = 0;
    sregs.cs.l = 0;
    sregs.cs.avl = 0;

    for seg in [&mut sregs.ds, &mut sregs.es, &mut sregs.fs, &mut sregs.gs, &mut sregs.ss] {
        seg.base = 0;
        seg.selector = 0;
        seg.limit = 0xFFFF;
        seg.type_ = 3;
        seg.present = 1;
        seg.s = 1;
        seg.g = 0;
        seg.db = 0;
        seg.l = 0;
        seg.avl = 0;
    }
    sregs.tr.base = 0;
    sregs.tr.selector = 0;
    sregs.tr.limit = 0xFFFF;
    sregs.tr.type_ = 11;
    sregs.tr.present = 1;
    sregs.tr.s = 0;

    sregs.ldt.base = 0;
    sregs.ldt.selector = 0;
    sregs.ldt.limit = 0xFFFF;
    sregs.ldt.type_ = 2;
    sregs.ldt.present = 1;
    sregs.ldt.s = 0;

    sregs.gdt.base = 0;
    sregs.gdt.limit = 0xFFFF;
    sregs.idt.base = 0;
    sregs.idt.limit = 0xFFFF;

    sregs.cr0 = 0x6000_0010;
    sregs.cr2 = 0;
    sregs.cr3 = 0;
    sregs.cr4 = 0;
    sregs.cr8 = 0;
    sregs.efer = 0;
    sregs.apic_base = 0xFEE0_0900;
    sregs.interrupt_bitmap = [0; 4];

    if let Err(e) = vcpu.set_sregs(&sregs) {
        eprintln!("[VMM] ERROR al restaurar sregs en reset: {}", e);
    }

    let mut regs = kvm_regs::default();
    regs.rip = RESET_VECTOR_RIP;
    regs.rflags = 0x2;
    if let Err(e) = vcpu.set_regs(&regs) {
        eprintln!("[VMM] ERROR al restaurar regs en reset: {}", e);
    }

    // Asegurar que vCPU 0 esté en estado RUNNABLE
    vcpu.set_mp_state(kvm_mp_state {
        mp_state: kvm_bindings::KVM_MP_STATE_RUNNABLE,
    }).ok();

    // Reset de FPU a valores por defecto
    vcpu.set_fpu(&kvm_fpu::default()).ok();

    // Reset de dispositivos (CD-ROM/ATAPI, PIT/PIC/PS2, PCI, VGA, CMOS...)
    // y de las áreas de memoria que el BIOS espera limpias.
    bus.reset();
    bus.legacy_irq.enable_default_pit();
    init_bios_data_area(guest_mem);

    // Restaurar copias intactas de BIOS ROM y VGA ROM
    if let Some(b) = bios {
        let load_addr = bios_load_addr(b.len());
        let load_off = load_addr as usize;
        if load_off + b.len() <= guest_mem.len() {
            guest_mem[load_off..load_off + b.len()].copy_from_slice(b);
        }
        if let Some(hm) = high_mem {
            const HIGH_MEM_ADDR: u64 = 0xE000_0000u64;
            let flash_offset_in_high = (0xFFFC_0000u64.saturating_sub(HIGH_MEM_ADDR)) as usize;
            if flash_offset_in_high + b.len() <= hm.len() {
                hm[flash_offset_in_high..flash_offset_in_high + b.len()].copy_from_slice(b);
            }
        }
    }
    if let Some(rom) = vga_rom {
        let vga_load_off = 0xC0000usize;
        if vga_load_off + rom.len() <= guest_mem.len() {
            guest_mem[vga_load_off..vga_load_off + rom.len()].copy_from_slice(rom);
        }
    }

    // Bajar las líneas de IRQ del kernel para no arrastrar flancos del ciclo
    // anterior (SeaBIOS reprograma el PIC en el POST de todas formas).
    vm.set_irq_line(0, false).ok();
    vm.set_irq_line(1, false).ok();
    vm.set_irq_line(8, false).ok();
    vm.set_irq_line(4, false).ok();
    vm.set_irq_line(14, false).ok();
    vm.set_irq_line(15, false).ok();
    vm.set_irq_line(11, false).ok();
    vm.set_irq_line(10, false).ok();
    vm.set_irq_line(9, false).ok();
    vm.set_irq_line(5, false).ok();
    // Las líneas PCI del irqchip acaban de quedar bajas: el cache de
    // `DeviceBus` debe reflejarlo (si quedara "alta", sync_pci_irqs no
    // volvería a subir esa línea jamás).
    bus.clear_pci_irq_levels();
    RESET_REQUESTED.store(false, Ordering::SeqCst);
    eprintln!("[VMM] Reset completado exitosamente: vCPU 0 en POST (CS:IP = {:#x}:{:#x})", sregs.cs.selector, regs.rip);
}

/// Reserva una región de memoria anónima alineada a página y la pone a cero.
///
/// # Safety (invariante de por vida)
/// La región se filta deliberadamente (`'static mut`): KVM la mantiene registrada
/// como `userspace_addr` de un memory-region durante toda la vida del proceso, y
/// el hilo de display guarda un puntero crudo a ella. No debe existir ningún
/// `&mut` exclusivo que invalide esos usos compartidos.
fn mmap_zeroed_region(size: usize) -> &'static mut [u8] {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        eprintln!("[VMM] mmap de {} bytes falló", size);
        exit(1);
    }
    // MAP_ANONYMOUS en Linux entrega páginas puestas a cero bajo demanda por el kernel;
    // solo pre-tocamos el primer MiB (BDA/EBDA/VGA/BIOS real-mode) para arranque instantáneo
    // sin bloquear cuando el usuario asigna 4 GiB, 8 GiB o 16 GiB de RAM.
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
    let pre_touch = size.min(1024 * 1024);
    slice[..pre_touch].fill(0);
    slice
}

/// Ajusta las hojas CPUID de topología e identificador APIC inicial para que
/// cada vCPU reporte su propio `cpu_id` (0..num_cpus-1) en lugar del APIC ID
/// del núcleo físico del host donde se ejecutó `get_supported_cpuid`.
/// Ajusta las hojas CPUID de topología e identificador APIC inicial para que
/// cada vCPU reporte su propio `cpu_id` (0..num_cpus-1) en lugar del APIC ID
/// del núcleo físico del host donde se ejecutó `get_supported_cpuid`.
fn apply_vcpu_cpuid(vcpu: &kvm_ioctls::VcpuFd, base_cpuid: &kvm_bindings::CpuId, cpu_id: u32, num_cpus: u32) {
    let mut cpuid = base_cpuid.clone();
    for entry in cpuid.as_mut_slice().iter_mut() {
        match entry.function {
            0x01 => {
                entry.ebx = (entry.ebx & 0x0000_FFFF)
                    | ((num_cpus.max(1) & 0xFF) << 16)
                    | ((cpu_id & 0xFF) << 24);
                if num_cpus > 1 {
                    entry.edx |= 1 << 28; // HTT
                }
            }
            0x0B | 0x1F => {
                entry.edx = cpu_id; // x2APIC ID = cpu_id
            }
            0x8000_001E => {
                entry.eax = cpu_id; // Extended APIC ID (AMD) = cpu_id
            }
            _ => {}
        }
    }
    vcpu.set_cpuid2(&cpuid).expect("set_cpuid2 falló");
}

/// Convierte una dirección física del guest (GPA) a un offset dentro de `guest_mem`,
/// teniendo en cuenta el hueco PCI MMIO en 3.5 GB (0xE000_0000..0x1_0000_0000).
fn gpa_to_mem_offset(gpa: u64, guest_mem_len: usize) -> Option<usize> {
    let off = if gpa < 0xE000_0000 {
        gpa as usize
    } else if gpa >= 0x1_0000_0000 {
        (0xE000_0000 + (gpa - 0x1_0000_0000)) as usize
    } else {
        return None;
    };
    if off < guest_mem_len { Some(off) } else { None }
}

/// Traduce una dirección lineal/virtual del guest a GPA si la paginación está activa.
fn resolve_guest_gpa(vcpu: &kvm_ioctls::VcpuFd, cr0: u64, linear: u64) -> u64 {
    if (cr0 & 0x8000_0000) != 0 {
        if let Ok(tr) = vcpu.translate_gva(linear) {
            if tr.valid != 0 {
                return tr.physical_address;
            }
        }
    }
    linear
}

/// Instala un handler para las señales indicadas (wrapper de libc::signal).
fn install_signal_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    unsafe {
        libc::signal(sig, handler as *const () as libc::sighandler_t);
    }
}

/// Sincroniza las líneas de interrupción de todos los dispositivos PCI hacia la VM de KVM
/// implementando la semántica estándar de nivel compartida (wired-OR activo en bajo/alto).
///
/// Si múltiples dispositivos comparten la misma línea IRQ (ej. AHCI y VirtIO-Serial en IRQ 10,
/// o USB UHCI y VMMDev en IRQ 11), la línea permanece en alto mientras CUALQUIERA de ellos
/// la mantenga asserted. Esto previene que un dispositivo inactivo limpie la interrupción
/// recién levantada por otro dispositivo en el mismo bus.
fn sync_pci_irqs(
    b: &mut devices::DeviceBus,
    vm: &kvm_ioctls::VmFd,
    metrics: Option<&metrics::VmmMetrics>,
) {
    let irq_usb = b.usb_irq_line() as u32;
    let (assert_usb, pulse_usb) = b.poll_usb_irq();

    let irq_vs = b.virtio_serial_irq_line() as u32;
    let (assert_vs, pulse_vs) = b.poll_virtio_serial_irq();

    let irq_vn = b.virtio_net_irq_line() as u32;
    let (assert_vn, pulse_vn) = b.poll_virtio_net_irq();

    let irq_ahci = b.ahci_irq_line() as u32;
    let assert_ahci = b.is_ahci_irq_asserted();

    let irq_vmm = b.vmmdev_irq_line() as u32;
    let assert_vmm = b.is_vmmdev_irq_asserted();

    let irq_ac97 = b.ac97_irq_line() as u32;
    let assert_ac97 = b.is_ac97_irq_asserted();

    let irq_e1000 = b.e1000_irq_line() as u32;
    let assert_e1000 = b.is_e1000_irq_asserted();

    let mut lines = [irq_usb, irq_vs, irq_vn, irq_ahci, irq_vmm, irq_ac97, irq_e1000];
    lines.sort_unstable();

    let mut last = 0xFF;
    for &line in &lines {
        if line == last || line == 0 || line == 0xFF {
            continue;
        }
        last = line;

        let pulse = (line == irq_usb && pulse_usb)
            || (line == irq_vs && pulse_vs)
            || (line == irq_vn && pulse_vn);

        let asserted = (line == irq_usb && assert_usb)
            || (line == irq_vs && assert_vs)
            || (line == irq_vn && assert_vn)
            || (line == irq_ahci && assert_ahci)
            || (line == irq_vmm && assert_vmm)
            || (line == irq_ac97 && assert_ac97)
            || (line == irq_e1000 && assert_e1000);

        // El ioctl KVM_IRQ_LINE cuesta ~480 ns y esto corre tras CADA
        // VM-exit: solo lo emitimos cuando el nivel cambia de verdad.
        // `pci_irq_level` es el estado que ya está en el irqchip del kernel.
        let was_high = b.pci_irq_level(line as u8);
        if pulse {
            // Flanco fresco: bajar (si estaba alto) → subir. Un PIC
            // edge-triggered no reintenta sin transición de nivel.
            if was_high {
                vm.set_irq_line(line, false).ok();
            }
            vm.set_irq_line(line, true).ok();
            b.set_pci_irq_level(line as u8, true);
            if let Some(m) = metrics {
                m.record_irq(line as u8);
            }
        } else if asserted {
            if !was_high {
                vm.set_irq_line(line, true).ok();
                b.set_pci_irq_level(line as u8, true);
                if let Some(m) = metrics {
                    m.record_irq(line as u8);
                }
            }
        } else if was_high {
            vm.set_irq_line(line, false).ok();
            b.set_pci_irq_level(line as u8, false);
        }
    }
}

fn main() {
    env_logger::init();

    // El hilo principal ES el BSP: guardar su TID para que el hilo de display
    // pueda despertarlo con tgkill al cerrarse la ventana (tarea 17).
    BSP_TID.store(unsafe { libc::syscall(libc::SYS_gettid) } as i32, Ordering::Relaxed);

    let args: Vec<String> = std::env::args().collect();

    // ─── Opciones de Instalación Desatendida (CLI y Entorno) ───
    let mut unattended = false;
    let mut unattended_user = String::from("two55");
    let mut unattended_pass = String::from("two55");
    let mut unattended_host = String::from("two55-vm");
    let mut cli_cdrom: Option<PathBuf> = None;
    let mut cli_disk: Option<PathBuf> = None;
    let mut cli_vm_name: Option<String> = None;

    // Variables de entorno: TWO_FIVE_FIVE_UNATTENDED=1, TFF_UNATTENDED=1, etc.
    if let Ok(val) = get_vmm_env("UNATTENDED") {
        if val == "1" || val.eq_ignore_ascii_case("true") || val.eq_ignore_ascii_case("yes") {
            unattended = true;
        }
    }
    if let Ok(val) = get_vmm_env("UNATTENDED_USER") {
        unattended_user = val;
    }
    if let Ok(val) = get_vmm_env("UNATTENDED_PASS") {
        unattended_pass = val;
    }
    if let Ok(val) = get_vmm_env("UNATTENDED_HOST") {
        unattended_host = val;
    }

    let mut force_gui = false;
    let mut no_gui = std::env::var("MI_VMM_NO_GUI").is_ok();
    let mut cli_cpus: Option<u32> = None;
    let mut cli_ram_mb: Option<usize> = None;

    let mut positional_args: Vec<String> = Vec::new();
    let mut iter = args.iter().skip(1).peekable();
    while let Some(arg) = iter.next() {
        if arg == "-u" || arg == "--unattended" {
            unattended = true;
        } else if arg == "-g" || arg == "--gui" || arg == "--launcher" {
            force_gui = true;
        } else if arg == "--no-gui" {
            no_gui = true;
        } else if arg == "--cpus" {
            if let Some(val) = iter.next() {
                cli_cpus = val.parse::<u32>().ok();
            }
        } else if let Some(val) = arg.strip_prefix("--cpus=") {
            cli_cpus = val.parse::<u32>().ok();
        } else if arg == "--ram" {
            if let Some(val) = iter.next() {
                cli_ram_mb = val.parse::<usize>().ok();
            }
        } else if let Some(val) = arg.strip_prefix("--ram=") {
            cli_ram_mb = val.parse::<usize>().ok();
        } else if arg == "--create-vdi" {
            let Some(file_arg) = iter.next() else {
                eprintln!("Error: --create-vdi requiere <archivo.vdi> <tamano_GiB> [--fixed]");
                exit(1);
            };
            let Some(size_arg) = iter.next() else {
                eprintln!("Error: --create-vdi requiere <archivo.vdi> <tamano_GiB> [--fixed]");
                exit(1);
            };
            let size_gb: u64 = size_arg.parse().unwrap_or(20).max(1);
            let fixed = iter.peek().map(|s| s.as_str() == "--fixed").unwrap_or(false);
            if fixed {
                let _ = iter.next();
            }
            let bytes = size_gb * 1024 * 1024 * 1024;
            match vdisk::VirtualDisk::create_vdi(
                file_arg,
                bytes,
                fixed,
                "Created by Two Five Five VMM (VBox VDI 1.1)",
            ) {
                Ok(_) => {
                    println!(
                        "[VDISK] Disco VDI {} creado en '{}' ({} GiB)",
                        if fixed { "Fijo" } else { "Dinamico" },
                        file_arg,
                        size_gb
                    );
                    exit(0);
                }
                Err(e) => {
                    eprintln!("[VDISK] Error al crear VDI '{}': {}", file_arg, e);
                    exit(1);
                }
            }
        } else if arg == "--compact-vdi" {
            let Some(file_arg) = iter.next() else {
                eprintln!("Error: --compact-vdi requiere <archivo.vdi>");
                exit(1);
            };
            match vdisk::VirtualDisk::open(file_arg).and_then(|mut d| d.compact()) {
                Ok(saved) => {
                    println!(
                        "[VDISK] '{}' compactado ({:.2} MiB liberados).",
                        file_arg,
                        saved as f64 / (1024.0 * 1024.0)
                    );
                    exit(0);
                }
                Err(e) => {
                    eprintln!("[VDISK] Error al compactar '{}': {}", file_arg, e);
                    exit(1);
                }
            }
        } else if arg == "--disk-info" {
            let Some(file_arg) = iter.next() else {
                eprintln!("Error: --disk-info requiere <archivo.vdi|img>");
                exit(1);
            };
            match vdisk::VirtualDisk::probe_info(file_arg) {
                Ok(info) => {
                    println!("=== Informacion de Medio Virtual (VBox VDI / RAW) ===");
                    println!("Archivo:          {}", info.path.display());
                    println!("Formato:          {}", info.format.label());
                    println!(
                        "Capacidad Logica: {:.2} GiB ({} bytes)",
                        info.virtual_size as f64 / (1024.0 * 1024.0 * 1024.0),
                        info.virtual_size
                    );
                    println!(
                        "Tamano en Host:   {:.2} MiB ({} bytes)",
                        info.actual_size as f64 / (1024.0 * 1024.0),
                        info.actual_size
                    );
                    println!("Tamano de Bloque: {} bytes", info.block_size);
                    println!(
                        "Bloques:          {} asignados / {} totales",
                        info.blocks_allocated, info.blocks_total
                    );
                    println!("UUID Creacion:    {}", info.uuid_create);
                    println!("Comentario:       {}", info.comment);
                    exit(0);
                }
                Err(e) => {
                    eprintln!("[VDISK] Error al inspeccionar '{}': {}", file_arg, e);
                    exit(1);
                }
            }
        } else if arg == "--unattended-user" {
            if let Some(val) = iter.next() {
                unattended_user = val.clone();
            }
        } else if let Some(val) = arg.strip_prefix("--unattended-user=") {
            unattended_user = val.to_string();
        } else if arg == "--unattended-pass" {
            if let Some(val) = iter.next() {
                unattended_pass = val.clone();
            }
        } else if let Some(val) = arg.strip_prefix("--unattended-pass=") {
            unattended_pass = val.to_string();
        } else if arg == "--unattended-host" {
            if let Some(val) = iter.next() {
                unattended_host = val.clone();
            }
        } else if let Some(val) = arg.strip_prefix("--unattended-host=") {
            unattended_host = val.to_string();
        } else if arg == "--cdrom" || arg == "--iso" {
            if let Some(val) = iter.next() {
                cli_cdrom = Some(PathBuf::from(val));
            }
        } else if let Some(val) = arg.strip_prefix("--cdrom=") {
            cli_cdrom = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--iso=") {
            cli_cdrom = Some(PathBuf::from(val));
        } else if arg == "--disk" || arg == "--drive" || arg == "--hda" {
            if let Some(val) = iter.next() {
                cli_disk = Some(PathBuf::from(val));
            }
        } else if let Some(val) = arg.strip_prefix("--disk=") {
            cli_disk = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--drive=") {
            cli_disk = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--hda=") {
            cli_disk = Some(PathBuf::from(val));
        } else if arg == "--vm-name" || arg == "--startvm" {
            if let Some(val) = iter.next() {
                cli_vm_name = Some(val.clone());
                no_gui = true;
            }
        } else if let Some(val) = arg.strip_prefix("--vm-name=") {
            cli_vm_name = Some(val.to_string());
            no_gui = true;
        } else if let Some(val) = arg.strip_prefix("--startvm=") {
            cli_vm_name = Some(val.to_string());
            no_gui = true;
        } else if arg == "-h" || arg == "--help" {
            usage();
        } else if arg.starts_with("--") {
            // Otras banderas de configuración (ej. --no-iso, --no-cdrom, --tui, etc.)
        } else {
            positional_args.push(arg.clone());
        }
    }

    // Si se especificó una VM (--startvm o --vm-name), sincronizar con su perfil en vms.conf
    if let Some(ref name) = cli_vm_name {
        let isos = gui_launcher::scan_iso_files();
        let disks = gui_launcher::scan_disk_files();
        let profiles = gui_launcher::load_vm_profiles(&isos, &disks);
        if let Some(p) = profiles.into_iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
            if cli_cpus.is_none() {
                cli_cpus = Some(p.cpus);
            }
            if cli_ram_mb.is_none() {
                cli_ram_mb = Some(p.ram_mb);
            }
            if cli_cdrom.is_none() && p.iso_path.is_some() {
                cli_cdrom = p.iso_path.map(PathBuf::from);
            }
            if cli_disk.is_none() && p.disk_path.is_some() {
                cli_disk = p.disk_path.map(PathBuf::from);
            }
            unattended = unattended || p.unattended;
        }
    }

    let (bios_path_buf, mut iso_path_buf, mut disk_path_buf) = if positional_args.is_empty() {
        let bios = find_default_bios().unwrap_or_else(|| {
            eprintln!("[VMM] ERROR: No se especificó BIOS ni se encontró SeaBIOS en rutas estándar.");
            usage();
        });
        let iso = cli_cdrom.or_else(auto_detect_iso);
        let disk = cli_disk.or_else(auto_detect_disk);
        (bios, iso, disk)
    } else {
        let mut bios = None;
        let mut iso = cli_cdrom;
        let mut disk = cli_disk;

        for arg in &positional_args {
            if bios.is_none() && is_bios_file(arg) {
                bios = Some(PathBuf::from(arg));
            } else if disk.is_none() && is_disk_file(arg) {
                disk = Some(PathBuf::from(arg));
            } else if iso.is_none() && (arg.to_lowercase().ends_with(".iso") || resolve_iso_file(arg).is_some()) {
                iso = resolve_iso_file(arg).or_else(|| Some(PathBuf::from(arg)));
            } else if disk.is_none() && Path::new(arg).is_file() {
                disk = Some(PathBuf::from(arg));
            }
        }

        let bios = bios.unwrap_or_else(|| {
            find_default_bios().unwrap_or_else(|| {
                eprintln!("[VMM] ERROR: No se encontró SeaBIOS para arrancar la VM.");
                usage();
            })
        });

        // Si se especificó una ISO pero ningún disco, conectar el disco por defecto si existe
        let disk = disk.or_else(auto_detect_disk);

        (bios, iso, disk)
    };

    if args.iter().any(|a| a == "--no-iso" || a == "--no-cdrom") {
        iso_path_buf = None;
    }

    let verbose = std::env::var("MI_VMM_VERBOSE").is_ok();
    let mut num_cpus: u32 = cli_cpus
        .or_else(|| std::env::var("MI_VMM_CPUS").ok().and_then(|v| v.parse::<u32>().ok()))
        .unwrap_or(4)
        .clamp(1, 16);

    let mut ram_mb: usize = cli_ram_mb
        .or_else(|| std::env::var("MI_VMM_RAM").ok().and_then(|v| v.parse::<usize>().ok()))
        .unwrap_or(4096)
        .max(256);

    // ─── Mini GUI de Selección de Máquina, ISO, Discos VDI y Recursos ───
    let should_open_gui = force_gui
        || (!no_gui
            && positional_args.is_empty()
            && std::env::var("MI_VMM_SERIAL_IN").is_err());

    if should_open_gui {
        match gui_launcher::run_gui_launcher(
            iso_path_buf.clone(),
            disk_path_buf.clone(),
            num_cpus,
            ram_mb,
        ) {
            gui_launcher::GuiOutcome::Selected(sel) => {
                eprintln!("[GUI] Iniciando VM '{}'...", sel.vm_name);
                cli_vm_name = Some(sel.vm_name);
                num_cpus = sel.cpus.clamp(1, 16);
                ram_mb = sel.ram_mb.max(256);
                iso_path_buf = sel.iso_path;
                disk_path_buf = sel.disk_path;
                unattended = sel.unattended;
                // Breve pausa para que el servidor X11/Wayland cierre limpiamente la ventana
                // del lanzador y suelte la tecla Enter antes de abrir DisplayManager.
                std::thread::sleep(std::time::Duration::from_millis(120));
            }
            gui_launcher::GuiOutcome::Cancelled => {
                eprintln!("[GUI] Administrador de maquinas cerrado.");
                exit(0);
            }
            gui_launcher::GuiOutcome::Headless => {}
        }
    }

    let bios_path_str = bios_path_buf.to_string_lossy().to_string();
    let bios_path = &bios_path_str;
    let iso_path_str = iso_path_buf.as_ref().map(|p| p.to_string_lossy().to_string());
    let iso_path = iso_path_str.as_deref();
    let disk_path_str = disk_path_buf.as_ref().map(|p| p.to_string_lossy().to_string());
    let disk_path = disk_path_str.as_deref();

    eprintln!("[VMM] BIOS: {}", bios_path);
    if let Some(iso) = iso_path {
        eprintln!("[VMM] ISO:  {}", iso);
    } else {
        eprintln!("[VMM] ISO:  (ninguna)");
    }
    if let Some(disk) = disk_path {
        eprintln!("[VMM] DISK: {}", disk);
    }

    let kvm = Kvm::new().expect("No se pudo abrir /dev/kvm");
    // vm se comparte con el hilo pit-timer (tarea 16): kvm-ioctls 0.14 no
    // implementa Clone para VmFd, así que se envuelve en Arc (VmFd es
    // Send+Sync: contiene un File y un usize). Todos los métodos usan
    // &self, así que las llamadas funcionan por auto-deref.
    let vm = Arc::new(kvm.create_vm().expect("create_vm falló"));
    vm.create_irq_chip().expect("create_irq_chip falló");

    // ─── RAM principal (con split para hueco PCI a 3.5GB) ────────
    let guest_mem_size = ram_mb * 1024 * 1024;

    let guest_mem = mmap_zeroed_region(guest_mem_size);
    // Manija compartida con bounds-check (tarea 18): display, DebugCon y el
    // hilo de temporización la reciben en vez de punteros `*const u8` crudos.
    let guest_mem_handle: Arc<GuestMemory> =
        GuestMemory::arc(guest_mem.as_mut_ptr(), guest_mem.len());

    const RAM_BELOW_4G_LIMIT: u64 = 0xE000_0000;
    let ram_below_4g = (guest_mem_size as u64).min(RAM_BELOW_4G_LIMIT);
    unsafe {
        vm.set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: ram_below_4g,
            userspace_addr: guest_mem_handle.as_mut_ptr() as u64,
            flags: 0,
        })
        .expect("set_user_memory_region (RAM baja) falló");
    }

    if (guest_mem_size as u64) > ram_below_4g {
        let ram_above_4g = (guest_mem_size as u64) - ram_below_4g;
        unsafe {
            vm.set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                slot: 1,
                guest_phys_addr: 0x1_0000_0000,
                memory_size: ram_above_4g,
                userspace_addr: guest_mem_handle.as_mut_ptr() as u64 + ram_below_4g,
                flags: 0,
            })
            .expect("set_user_memory_region (RAM alta >4GB) falló");
        }
        eprintln!(
            "[VMM] RAM dividida: {} MiB baja (<3.5GB) + {} MiB alta (>4GB)",
            ram_below_4g / (1024 * 1024),
            ram_above_4g / (1024 * 1024)
        );
    }

    // ─── High memory: 512 MiB desde 0xE0000000 ─────────────────
    // (tarea 7) La ventana se registra TROCEADA: las páginas de MMIO del
    // LAPIC (0xFEE00000) y del IOAPIC (0xFEC00000) quedan SIN memslot para
    // que las atienda el irqchip del kernel (create_irq_chip). Con un
    // memslot de RAM encima, la EPT resolvía el acceso como memoria y los
    // accesos del guest (ICR, IOREDTBL…) caían en RAM tonta.
    const HIGH_MEM_SIZE: usize = 512 * 1024 * 1024;
    const HIGH_MEM_ADDR: u64 = 0xE000_0000u64;
    let high_mem = mmap_zeroed_region(HIGH_MEM_SIZE);
    for (i, (start, end)) in
        carve_reserved_holes(HIGH_MEM_ADDR, HIGH_MEM_SIZE as u64, &HIGH_MEM_HOLES)
            .into_iter()
            .enumerate()
    {
        let off = (start - HIGH_MEM_ADDR) as usize;
        unsafe {
            vm.set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
                slot: 2 + i as u32,
                guest_phys_addr: start,
                memory_size: end - start,
                userspace_addr: high_mem.as_ptr() as u64 + off as u64,
                flags: 0,
            })
            .expect("set_user_memory_region (high mem) falló");
        }
    }
    eprintln!(
        "[VMM] Huecos reservados en high_mem: PCI MMIO@0xFE000000 (12MB), LAPIC@0xFEE00000 e IOAPIC@0xFEC00000 (sin memslot de RAM)"
    );

    // ─── VRAM: 16 MiB en GPA 0xE8000000 (offset 128MB de high_mem) ───
    const VRAM_SIZE: usize = 16 * 1024 * 1024;
    let vram_ptr = unsafe { high_mem.as_mut_ptr().add(128 * 1024 * 1024) };

    // ─── Buscar VGA Option ROM para fw_cfg y C0000 ─────────────
    let vga_candidates = [
        "bios/vgabios-stdvga.bin",
        "vgabios-stdvga.bin",
        "/usr/share/seabios/vgabios-stdvga.bin",
        "/usr/share/seabios/vgabios-bochs-display.bin",
        "/usr/share/qemu/vgabios-stdvga.bin",
        "/usr/share/seabios/vgabios.bin",
        "/usr/share/qemu/vgabios.bin",
    ];
    let mut vga_rom_data: Option<Vec<u8>> = None;
    for vga_path in &vga_candidates {
        let mut path_to_try = PathBuf::from(vga_path);
        if !path_to_try.is_file() {
            if let Ok(exe) = std::env::current_exe() {
                if let Some(exe_dir) = exe.parent() {
                    let root_candidate = exe_dir.join("../..").join(vga_path);
                    if root_candidate.is_file() {
                        path_to_try = root_candidate;
                    }
                }
            }
        }
        if let Ok(mut f) = File::open(&path_to_try) {
            let mut vga_rom = Vec::new();
            if f.read_to_end(&mut vga_rom).is_ok() {
                eprintln!("[VMM] VGA Option ROM encontrado en {} ({} bytes)", path_to_try.display(), vga_rom.len());
                vga_rom_data = Some(vga_rom);
                break;
            }
        }
    }

    let (mut bus, vga_state) = match DeviceBus::new(
        iso_path,
        disk_path,
        vram_ptr,
        VRAM_SIZE,
        guest_mem_size as u64,
        num_cpus,
        high_mem.as_mut_ptr(),
        HIGH_MEM_ADDR,
        HIGH_MEM_SIZE,
        vga_rom_data.clone(),
    ) {
        Ok(res) => res,
        Err(e) => {
            eprintln!("[VMM] Error: {}", e);
            exit(1);
        }
    };
    // Connect DebugCon to guest memory for VGA text mirroring
    bus.debugcon.set_guest_mem(guest_mem_handle.clone());
    // Connect DeviceBus to guest memory for Bus Master DMA (BMDMA)
    bus.set_guest_mem(guest_mem_handle.clone());
    // Habilitar PIT canal 0 por defecto (Modo 3, 18.2 Hz) como en hardware PC real
    bus.legacy_irq.enable_default_pit();
    if !bus.debugcon.mirror_enabled() {
        eprintln!("[VMM] Espejo BIOS→0xB8000 desactivado (MI_VMM_MIRROR_BIOS=1 para activarlo)");
    }

    // ─── Instalación Desatendida (OEMDRV / CIDATA) ──────────────
    if unattended {
        bus.ahci.unattended = true;
        if let Some(ref mut cdrom) = bus.cdrom {
            cdrom.unattended = true;
        }
        if let Some(iso) = iso_path {
            let config = unattended::UnattendedConfig {
                username: unattended_user,
                password: unattended_pass,
                hostname: unattended_host,
                timezone: "UTC".to_string(),
            };
            match unattended::prepare_unattended_media(Path::new(iso), &config) {
                Ok(aux_path) => {
                    let path_str = aux_path.to_string_lossy().to_string();
                    bus.ahci.attach_aux_disk(&path_str);
                    eprintln!("[two-five-five] Instalación desatendida activada (OEMDRV montado)");
                }
                Err(e) => {
                    eprintln!("[two-five-five] Error preparando medio desatendido: {}", e);
                }
            }
        } else {
            eprintln!("[two-five-five] Advertencia: --unattended activado pero no hay ISO/CD-ROM disponible.");
        }
    }

    // ─── (tarea 6) Compartir el bus entre todos los vCPUs ──────
    // Los APs manejan sus propios exits de E/S contra los mismos
    // dispositivos; el Mutex serializa el acceso. (La condición Send de
    // DeviceBus se justifica en devices/mod.rs.)
    let bus = Arc::new(Mutex::new(bus));

    // ─── Serial-in: stdin del host → COM1 (RX) ──────────────────
    // Opcional (MI_VMM_SERIAL_IN=1): un hilo reenvía lo que se teclea en la
    // terminal a la cola RX del UART, de modo que console=ttyS0 sirva también
    // para escribir en el guest (GRUB menu, shell de Linux, etc.).
    if std::env::var("MI_VMM_SERIAL_IN").is_ok() {
        let bus_serial = Arc::clone(&bus);
        std::thread::Builder::new()
            .name("serial-in".to_string())
            .spawn(move || {
                use std::io::Read;
                let mut stdin = std::io::stdin().lock();
                let mut buf = [0u8; 64];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break, // EOF: terminal cerrada
                        Ok(n) => {
                            let mut b = bus_serial.lock().unwrap_or_else(|p| p.into_inner());
                            for &byte in &buf[..n] {
                                b.uart.push_rx(byte);
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn del hilo serial-in falló");
        eprintln!("[VMM] Serial-in activo: stdin del host → COM1 (RX del guest)");
    }

    // ─── Cargar firmware ────────────────────────────────────────
    let mut bios = Vec::new();
    let bios_io = File::open(bios_path)
        .and_then(|mut f| f.read_to_end(&mut bios));
    if let Err(e) = bios_io {
        eprintln!("[VMM] No se pudo leer '{}': {}", bios_path, e);
        exit(1);
    }
    // Soporte para Legacy BIOS (<= 512 KB) y UEFI / OVMF (hasta 16 MB)
    const BIOS_MAX: usize = 16 * 1024 * 1024;
    if bios.len() > BIOS_MAX {
        eprintln!(
            "[VMM] ERROR: '{}' tiene {} bytes (>{} KB). Excede el tamaño máximo de firmware.",
            bios_path, bios.len(), BIOS_MAX / 1024
        );
        exit(1);
    }
    let is_uefi = bios.len() > 512 * 1024
        || bios_path.to_lowercase().contains("ovmf")
        || bios_path.to_lowercase().contains(".fd");
    IS_UEFI.store(is_uefi, Ordering::Relaxed);

    if is_uefi {
        // Rediseño de mapeo de flash para UEFI / OVMF (Item 22):
        // OVMF se mapea justo debajo de 4 GiB (0x1_0000_0000 - len)
        let flash_addr = (0x1_0000_0000u64).saturating_sub(bios.len() as u64);
        if flash_addr >= HIGH_MEM_ADDR {
            let offset_in_high = (flash_addr - HIGH_MEM_ADDR) as usize;
            if offset_in_high + bios.len() <= high_mem.len() {
                high_mem[offset_in_high..offset_in_high + bios.len()].copy_from_slice(&bios);
                eprintln!("[VMM] Firmware UEFI/OVMF mapeado en {:#x}..{:#x} ({} MB)",
                    flash_addr, flash_addr + bios.len() as u64, bios.len() / (1024 * 1024));
            }
        }

        // Cargar o inicializar VarStore para UEFI (Item 22)
        let vars_path = std::env::var("MI_VMM_VARS").ok();
        let pflash = match vars_path {
            Some(ref p) if std::path::Path::new(p).exists() => {
                devices::pflash::ParallelFlash::load_file("OVMF_VARS", p, 64 * 1024, false)
                    .unwrap_or_else(|_| devices::pflash::ParallelFlash::new("OVMF_VARS", 512 * 1024, 64 * 1024, false))
            }
            _ => devices::pflash::ParallelFlash::new("OVMF_VARS", 512 * 1024, 64 * 1024, false),
        };
        bus.lock().unwrap_or_else(|p| p.into_inner()).set_pflash(pflash);
        eprintln!("[VMM] VarStore UEFI (pflash) inicializado.");
    } else {
        let load_addr = bios_load_addr(bios.len());
        let load_off = load_addr as usize;
        guest_mem[load_off..load_off + bios.len()].copy_from_slice(&bios);
        eprintln!("[VMM] BIOS Legacy cargado en {:#x} ({} bytes)", load_addr, bios.len());

        let rv_off = 0xFFFF0usize;
        eprintln!("[VMM] Reset vector @0xFFFF0: {:02x?}", &guest_mem[rv_off..rv_off + 16]);
        let ep_phys = 0xFE05B_usize;
        if ep_phys + 16 <= guest_mem.len() {
            eprintln!("[VMM] BIOS entry @phys {:#x}: {:02x?}", ep_phys, &guest_mem[ep_phys..ep_phys + 16]);
        }

        let flash_offset_in_high = (0xFFFC_0000u64 - HIGH_MEM_ADDR) as usize;
        high_mem[flash_offset_in_high..flash_offset_in_high + bios.len()].copy_from_slice(&bios);
        eprintln!("[VMM] Flash BIOS en {:#x}", 0xFFFC_0000u64);
    }

    // ─── Copiar VGA Option ROM en memoria física (0xC0000) ─────────
    if let Some(ref vga_rom) = vga_rom_data {
        let vga_load_off = 0xC0000usize;
        if vga_load_off + vga_rom.len() <= guest_mem.len() {
            guest_mem[vga_load_off..vga_load_off + vga_rom.len()].copy_from_slice(vga_rom);
            eprintln!("[VMM] VGA Option ROM copiado en 0xC0000 ({} bytes)", vga_rom.len());
        }
    } else {
        eprintln!("[VMM] Info: No se encontró archivo VGA ROM externo. Usando emulación gráfica integrada.");
    }

    init_bios_data_area(guest_mem);

    // ─── vCPU en modo real ─────────────────────────────────────
    let mut vcpu = vm.create_vcpu(0).expect("create_vcpu falló");
    let mut sregs: kvm_sregs = vcpu.get_sregs().expect("get_sregs falló");
    if IS_UEFI.load(Ordering::Relaxed) {
        sregs.cs.base = 0xFFFF_0000;
        sregs.cs.selector = 0xF000;
        eprintln!("[VMM] Vector de reset UEFI: CS.base=0xFFFF0000, RIP={:#x} (GPA {:#x})",
            RESET_VECTOR_RIP, 0xFFFF_0000 + RESET_VECTOR_RIP);
    } else {
        sregs.cs.base = (RESET_VECTOR_CS as u16 as u64) << 4;
        sregs.cs.selector = RESET_VECTOR_CS as u16;
    }
    sregs.cs.limit = 0xFFFF;
    sregs.cs.type_ = 11; sregs.cs.present = 1; sregs.cs.s = 1;
    for seg in [&mut sregs.ds, &mut sregs.es, &mut sregs.fs, &mut sregs.gs, &mut sregs.ss] {
        seg.base = 0; seg.selector = 0; seg.limit = 0xFFFF;
        seg.type_ = 3; seg.present = 1; seg.s = 1;
    }
    sregs.tr.type_ = 11; sregs.tr.present = 1;
    sregs.ldt.type_ = 2; sregs.ldt.present = 1;
    sregs.cr0 = 0x6000_0010;
    vcpu.set_sregs(&sregs).expect("set_sregs falló");

    let mut regs: kvm_regs = vcpu.get_regs().expect("get_regs falló");
    regs.rip = RESET_VECTOR_RIP;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs).expect("set_regs falló");

    // ─── APs (tarea 6: SMP) ──────────────────────────────────
    // Cada AP nace aparcado en KVM_MP_STATE_INIT_RECEIVED (esperando
    // SIPI) en su propio hilo. Solo ejecutará cuando el guest le entregue
    // INIT-SIPI por el LAPIC — manejado íntegramente por el irqchip del
    // kernel (create_irq_chip): SeaBIOS y Linux hacen su sondeo SIPI
    // estándar y detectan los APs sin tablas adicionales (el fw_cfg
    // NB_CPUS de FwCfg ya anuncia num_cpus).
    // El set_mp_state va ANTES del spawn: si el hilo llegara a correr sin
    // aparcar, el AP ejecutaría el vector de reset y duplicaría el BIOS.
    // ─── Métricas y profiling de VM-Exits (Item 24) ────────────
    let metrics = Arc::new(metrics::VmmMetrics::new());
    metrics.ram_bytes.store(guest_mem_size as u64, Ordering::Relaxed);
    metrics.high_mem_bytes.store(HIGH_MEM_SIZE as u64, Ordering::Relaxed);
    metrics.num_cpus.store(num_cpus, Ordering::Relaxed);
    metrics.max_cpus.store(16, Ordering::Relaxed);

    // Cargar snapshot previo si se solicitó (Item 24)
    if let Ok(snap_path) = std::env::var("MI_VMM_SNAPSHOT_LOAD") {
        eprintln!("[VMM] Cargando snapshot desde: {}", snap_path);
        snapshot::load_vm_snapshot(&snap_path, guest_mem, high_mem, &[&vcpu]).ok();
    }

    // ─── Configurar CPUID (FPU, SSE, MMX, etc. para Linux/Windows) ─────
    let cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .expect("get_supported_cpuid falló");
    apply_vcpu_cpuid(&vcpu, &cpuid, 0, num_cpus);

    let mut ap_handles = Vec::new();
    for cpu_id in 1..num_cpus {
        let ap = vm.create_vcpu(cpu_id as u64).expect("create_vcpu (AP) falló");
        apply_vcpu_cpuid(&ap, &cpuid, cpu_id, num_cpus);
        ap.set_mp_state(kvm_mp_state {
            mp_state: kvm_bindings::KVM_MP_STATE_INIT_RECEIVED,
        })
        .expect("set_mp_state (AP) falló");
        let bus_ap = Arc::clone(&bus);
        let metrics_ap = Arc::clone(&metrics);
        let vm_ap = Arc::clone(&vm);
        ap_handles.push(
            std::thread::Builder::new()
                .name(format!("vcpu-{cpu_id}"))
                .spawn(move || ap_vcpu_worker(cpu_id, ap, vm_ap, bus_ap, metrics_ap, verbose))
                .expect("spawn de hilo AP falló"),
        );
    }
    // Los hilos AP no se joinean: el proceso vive mientras viva el BSP.
    if num_cpus > 1 {
        eprintln!(
            "[VMM] SMP: {} vCPUs (BSP + {} APs en wait-for-SIPI; MI_VMM_CPUS=n para cambiar)",
            num_cpus,
            num_cpus - 1
        );
    }

    // Handle Ctrl+C / SIGTERM: dump VGA screen and diagnostics on exit
    install_signal_handler(libc::SIGTERM, shutdown_handler);
    install_signal_handler(libc::SIGINT, shutdown_handler);
    install_signal_handler(libc::SIGUSR1, sigusr1_handler);

    // ─── Display GUI Manager ───────────────────────────────────
    let kbd_queue: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
    // Cola del ratón host: (dx, dy, botones PS/2) desde la ventana minifb.
    let mouse_queue: Arc<Mutex<VecDeque<(i16, i16, u8)>>> = Arc::new(Mutex::new(VecDeque::new()));
    // Cola de la tableta gráfica USB: (x, y, botones, rueda) coordenadas absolutas (0..32767).
    let tablet_queue: Arc<Mutex<VecDeque<(u16, u16, u8, i8)>>> = Arc::new(Mutex::new(VecDeque::new()));
    // Solicitud de cambio de resolución dinámica desde la ventana minifb
    let resize_queue: Arc<Mutex<Option<(u32, u32)>>> = Arc::new(Mutex::new(None));
    
    // Inyección de ENTER (tarea 3): acelera el arranque de ISOLINUX/menu.c32 a los 3s, 5s, 7s, 9s y 11s
    // para tests y modo headless; activo en instalación desatendida o con MI_VMM_AUTO_ENTER=1.
    if unattended || std::env::var("MI_VMM_AUTO_ENTER").map(|v| v == "1").unwrap_or(false) {
        let auto_enter_kbd = Arc::clone(&kbd_queue);
        std::thread::spawn(move || {
            for wait_secs in [3, 2, 2, 2, 2] {
                std::thread::sleep(std::time::Duration::from_secs(wait_secs));
                if let Ok(mut q) = auto_enter_kbd.lock() {
                    q.push_back(0x1C);
                    q.push_back(0x9C);
                }
            }
        });
    }

    let vm_title = cli_vm_name.map(|name| format!("{} [Corriendo] — Oracle VM VirtualBox", name));
    let _display = display::DisplayManager::start(
        vga_state.clone(),
        guest_mem_handle.clone(),
        kbd_queue.clone(),
        mouse_queue.clone(),
        tablet_queue.clone(),
        resize_queue.clone(),
        Arc::clone(&metrics),
        vm_title,
    );

    // ─── Dashboard TUI interactivo ─────────────────────────────
    let force_tui = args.iter().any(|a| a == "--tui") || std::env::var("MI_VMM_TUI").is_ok();
    let no_tui = args.iter().any(|a| a == "--no-tui") || std::env::var("MI_VMM_NO_TUI").is_ok();
    let is_interactive = unsafe { libc::isatty(libc::STDIN_FILENO) != 0 && libc::isatty(libc::STDOUT_FILENO) != 0 };
    let use_tui = (force_tui || (!no_tui && is_interactive)) && std::env::var("MI_VMM_SERIAL_IN").is_err();

    let tui_running = Arc::new(AtomicBool::new(true));
    let tui_handle = if use_tui {
        let iso_name = iso_path.and_then(|p| Path::new(p).file_name()).map(|n| n.to_string_lossy().to_string());
        let disk_name = disk_path.and_then(|p| Path::new(p).file_name()).map(|n| n.to_string_lossy().to_string());
        tui::start_tui(
            Arc::clone(&metrics),
            vga_state.clone(),
            Arc::clone(&bus),
            iso_name,
            disk_name,
            Arc::clone(&tui_running),
        )
    } else {
        None
    };

    // ─── Hilo de temporización (tarea 16) ──────────────────────
    // Sustituye al setitimer(1ms)+SIGALRM: avanza el PIT, pulsa IRQ0,
    // mantiene el tick del BDA e inyecta el input host cada 1 ms con
    // clock_nanosleep absoluto (sin deriva), incluso con el guest en HLT.
    let irq0_flag: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let bda_tick_count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    std::thread::Builder::new()
        .name("pit-timer".to_string())
        .spawn({
            let bus_t = Arc::clone(&bus);
            let kbd_t = Arc::clone(&kbd_queue);
            let mouse_t = Arc::clone(&mouse_queue);
            let tablet_t = Arc::clone(&tablet_queue);
            let resize_t = Arc::clone(&resize_queue);
            let irq0_t = Arc::clone(&irq0_flag);
            let bda_t = Arc::clone(&bda_tick_count);
            let metrics_t = Arc::clone(&metrics);
            // vm (Arc<VmFd>) se clona ANTES del move (el BSP sigue usando vm).
            let vm_t = Arc::clone(&vm);
            let gm_t = Arc::clone(&guest_mem_handle);
            move || {
                pit_timer_thread(
                    vm_t,
                    bus_t,
                    gm_t,
                    kbd_t,
                    mouse_t,
                    tablet_t,
                    resize_t,
                    irq0_t,
                    bda_t,
                    metrics_t,
                )
            }
        })
        .expect("spawn del hilo pit-timer falló");

    // ─── Bucle VMM ──────────────────────────────────────────────
    // Acceso al DeviceBus compartido (BSP y APs). Tolerante al
    // envenenamiento del Mutex: si un AP paniquea con el candado puesto,
    // el BSP recupera el estado y sigue (into_inner).
    let bus_lock = || bus.lock().unwrap_or_else(|p| p.into_inner());
    eprintln!("[VMM] Arrancando VM...");
    let mut recent: VecDeque<String> = VecDeque::with_capacity(128);
    let mut total_exits: u64 = 0;
    let mut last_report = std::time::Instant::now();
    // State of IRQ lines currently raised in the kernel PIC (edge-triggered)
    let mut irq0_raised = false;
    // Nivel actual de la línea IRQ1 en el kernel PIC (la mantenemos en alto
    // mientras el 8042 tenga datos sin leer y la bajamos al drenarse).
    let mut irq1_line_high = false;
    // Estado actual de kvm_run.request_interrupt_window (tarea 4).
    let mut irq_window_requested = false;
    let mut total_reboots: u32 = 0;
    let mut last_post = 0u8;
    let mut last_cr0: u64 = 0x6000_0010; // Initial CR0 (real mode)
    let mut mode_transitions: u32 = 0;
    // I/O trace ring buffer: last 100 operations (is_write, port, value)
    let mut io_trace: VecDeque<(u8, u16, u8)> = VecDeque::with_capacity(100);
    // Último estado conocido de los dispositivos de puntero absoluto
    // (tableta USB, ratón del VMMDev) para registrar solo los cambios.
    let mut last_abs_state: Option<(bool, bool)> = None;

    loop {
        // ── Shutdown solicitado (SIGTERM/SIGINT) ──────────────────
        if SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            tui_running.store(false, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(150));
            eprintln!("[VMM] Shutdown solicitado — volcando estado final (exits={}, reboots={})...",
                total_exits, total_reboots);
            if let Ok(shot_path) = std::env::var("MI_VMM_SCREENSHOT") {
                if let Err(e) = display::save_ppm_screenshot(&vga_state, &guest_mem_handle, &shot_path) {
                    eprintln!("[VMM] Error al guardar captura en {}: {}", shot_path, e);
                } else {
                    eprintln!("[VMM] Captura guardada en {}", shot_path);
                }
            }
            dump_vga_text_screen(guest_mem);
            exit(0);
        }
        // ── Input / ACPI / reset: UNA sola adquisición del bus ─────
        // Este bloque corre en cada VM-exit: cada `bus_lock()` de más es un
        // mutex compartido con los APs y el hilo PIT. Antes había 5 locks
        // aquí; ahora se drenan todas las colas y latches bajo un guard.
        let (acpi_sleep, ps2_irq, mouse_irq, abs_ready, tablet_ready, vmm_abs, reset_from_device) = {
            let mut b = bus_lock();
            // ── Input desde la ventana gráfica ───────────────────
            if let Ok(mut q) = kbd_queue.try_lock() {
                while let Some(scancode) = q.pop_front() {
                    b.legacy_irq.inject_scancode(scancode);
                }
            }
            // Movimiento/botones del ratón host → ratón PS/2 del guest (IRQ12)
            if let Ok(mut q) = mouse_queue.try_lock() {
                while let Some((dx, dy, buttons)) = q.pop_front() {
                    b.legacy_irq.inject_mouse_delta(dx, dy, buttons);
                }
            }
            // Eventos de la tableta gráfica USB (coordenadas absolutas 0..32767)
            if let Ok(mut q) = tablet_queue.try_lock() {
                while let Some((x, y, buttons, wheel)) = q.pop_front() {
                    b.inject_tablet_event(x, y, buttons, wheel);
                }
            }
            (
                b.acpi_sleep_requested(),
                b.take_ps2_irq(),
                b.take_mouse_irq(),
                b.is_tablet_configured(),
                b.usb_tablet_configured(),
                b.vmmdev_abs_mouse_active(),
                b.take_reset_requested(),
            )
        };
        // ── Apagado limpio vía ACPI (SLP_EN en PM1a_CNT) ─────────
        // El guest (Linux) evaluó _S5 y escribió SLP_EN en 0x604; AcpiPm
        // lo detectó. Salimos igual que con SIGTERM/SIGINT: dump + exit.
        if acpi_sleep {
            tui_running.store(false, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(150));
            eprintln!("[VMM] ACPI S5 solicitado por el guest — apagado limpio (exits={}, reboots={})...",
                total_exits, total_reboots);
            dump_vga_text_screen(guest_mem);
            exit(0);
        }
        if ps2_irq {
            vm.set_irq_line(1, false).ok();
            vm.set_irq_line(1, true).ok();
            irq1_line_high = true;
            metrics.record_irq(1);
        }
        if mouse_irq {
            vm.set_irq_line(12, false).ok();
            vm.set_irq_line(12, true).ok();
            metrics.record_irq(12);
        }
        metrics.mouse_abs_available.store(abs_ready, Ordering::Relaxed);
        // Traza de estado del puntero absoluto: distingue "el guest usa la
        // tableta USB" de "Guest Additions usa el ratón del VMMDev" de "no hay
        // dispositivo absoluto (el host queda en PS/2 relativo)".
        if last_abs_state != Some((tablet_ready, vmm_abs)) {
            last_abs_state = Some((tablet_ready, vmm_abs));
            if tablet_ready || vmm_abs {
                tui::log(&format!(
                    "[MOUSE] Puntero absoluto disponible (tableta USB={}, VMMDev/Guest Additions={})",
                    if tablet_ready { "sí" } else { "no" },
                    if vmm_abs { "sí" } else { "no" }
                ));
            } else {
                tui::log("[MOUSE] Sin puntero absoluto: el guest no ha configurado la tableta USB ni el ratón del VMMDev (se usa PS/2 relativo)");
            }
        }

        // Comandos de control de la VM provenientes de la GUI (barra superior de VirtualBox)
        if ACPI_SHUTDOWN_REQUESTED.swap(false, Ordering::Relaxed) {
            bus_lock().trigger_power_button();
            tui::log("[GUI] Solicitud de apagado ACPI enviada al SO huésped.");
        }
        if RESET_REQUESTED.swap(false, Ordering::SeqCst) || reset_from_device {
            tui::log("[VMM] Reinicio de la VM solicitado (hardware reset/POST)...");
            if let Ok(mut q) = kbd_queue.lock() {
                q.clear();
            }
            if let Ok(mut q) = mouse_queue.lock() {
                q.clear();
            }
            reset_vcpu_to_post(
                &vcpu,
                &vm,
                &mut bus_lock(),
                guest_mem,
                Some(high_mem),
                Some(&bios),
                vga_rom_data.as_deref(),
            );
            total_reboots = 0;
            continue;
        }
        if CDROM_EJECT_REQUESTED.swap(false, Ordering::Relaxed) {
            bus_lock().eject_cdrom();
            tui::log("[GUI] Medio óptico CD-ROM expulsado.");
        }

        // ── Comandos interactivos del Dashboard TUI ──────────────
        if let Some(ref handle) = tui_handle {
            while let Some(cmd) = handle.pop_command() {
                match cmd {
                    tui::DashboardCommand::TogglePause => {
                        let cur = metrics.is_paused.load(Ordering::Relaxed);
                        metrics.is_paused.store(!cur, Ordering::Relaxed);
                        tui::log(if !cur { "[TUI] Máquina virtual pausada" } else { "[TUI] Máquina virtual reanudada" });
                    }
                    tui::DashboardCommand::AddCpu => {
                        let current_cpus = metrics.num_cpus.load(Ordering::Relaxed);
                        let max_cpus = metrics.max_cpus.load(Ordering::Relaxed);
                        if current_cpus < max_cpus {
                            let new_id = current_cpus;
                            if bus_lock().plug_cpu(new_id).is_ok() {
                                match vm.create_vcpu(new_id as u64) {
                                    Ok(new_vcpu) => {
                                        apply_vcpu_cpuid(&new_vcpu, &cpuid, new_id, max_cpus);
                                        let _ = new_vcpu.set_mp_state(kvm_mp_state {
                                            mp_state: kvm_bindings::KVM_MP_STATE_INIT_RECEIVED,
                                        });
                                        let bus_ap = Arc::clone(&bus);
                                        let metrics_ap = Arc::clone(&metrics);
                                        let vm_ap = Arc::clone(&vm);
                                        ap_handles.push(
                                            std::thread::Builder::new()
                                                .name(format!("vcpu-{new_id}"))
                                                .spawn(move || ap_vcpu_worker(new_id, new_vcpu, vm_ap, bus_ap, metrics_ap, verbose))
                                                .expect("spawn de hilo AP falló"),
                                        );
                                        metrics.num_cpus.fetch_add(1, Ordering::Relaxed);
                                        {
                                            let mut b = bus_lock();
                                            vm.set_irq_line(9, false).ok();
                                            vm.set_irq_line(9, true).ok();
                                            // IRQ9 es compartida con VirtIO-Net: el
                                            // cache debe reflejar el flanco.
                                            b.set_pci_irq_level(9, true);
                                        }
                                        metrics.record_irq(9);
                                        tui::log(format!("[TUI] vCPU #{} conectado en caliente con éxito (total: {})", new_id, current_cpus + 1));
                                    }
                                    Err(e) => {
                                        tui::log(format!("[TUI] Error al crear vCPU #{}: {}", new_id, e));
                                    }
                                }
                            }
                        } else {
                            tui::log("[TUI] Ya se alcanzó el número máximo de vCPUs (16).");
                        }
                    }
                    tui::DashboardCommand::RemoveCpu => {
                        let current_cpus = metrics.num_cpus.load(Ordering::Relaxed);
                        if current_cpus > 1 {
                            let target_id = current_cpus - 1;
                            if bus_lock().cpu_hotplug.unplug_cpu(target_id).is_ok() {
                                metrics.num_cpus.fetch_sub(1, Ordering::Relaxed);
                                {
                                    let mut b = bus_lock();
                                    vm.set_irq_line(9, false).ok();
                                    vm.set_irq_line(9, true).ok();
                                    // IRQ9 es compartida con VirtIO-Net: el
                                    // cache debe reflejar el flanco.
                                    b.set_pci_irq_level(9, true);
                                }
                                metrics.record_irq(9);
                                tui::log(format!("[TUI] vCPU #{} marcado para desconexión", target_id));
                            }
                        } else {
                            tui::log("[TUI] No se puede desconectar el BSP (CPU 0).");
                        }
                    }
                    tui::DashboardCommand::EjectCdrom => {
                        let is_inserted = bus_lock().is_cdrom_inserted();
                        if is_inserted {
                            bus_lock().eject_cdrom();
                            tui::log("[TUI] CD-ROM expulsado.");
                        } else if let Some(ref path) = iso_path {
                            let _ = bus_lock().insert_cdrom(path);
                            tui::log("[TUI] CD-ROM reinsertado.");
                        } else {
                            tui::log("[TUI] No hay ruta de ISO disponible para reinsertar.");
                        }
                    }
                    tui::DashboardCommand::CycleScale => {
                        let cur = metrics.display_scale.load(Ordering::Relaxed);
                        let next = if cur >= 3 { 1 } else { cur + 1 };
                        metrics.display_scale.store(next, Ordering::Relaxed);
                        tui::log(format!("[TUI] Escala cambiada a {}x", next));
                    }
                    tui::DashboardCommand::ShutdownAcpi => {
                        bus_lock().trigger_power_button();
                        tui::log("[TUI] Solicitud de apagado ACPI enviada al SO.");
                    }
                    tui::DashboardCommand::ResetVm => {
                        tui::log("[TUI] Reinicio manual solicitado desde el Dashboard.");
                        reset_vcpu_to_post(
                            &vcpu,
                            &vm,
                            &mut bus_lock(),
                            guest_mem,
                            Some(high_mem),
                            Some(&bios),
                            vga_rom_data.as_deref(),
                        );
                        total_reboots += 1;
                    }
                    tui::DashboardCommand::Quit => {
                        tui::log("[TUI] Salida solicitada.");
                        SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                    }
                }
            }
        }

        // ── Pausa de la VM ──────────────────────────────────────
        while metrics.is_paused.load(Ordering::Relaxed) && !SHUTDOWN_REQUESTED.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Some(ref handle) = tui_handle {
                while let Some(cmd) = handle.pop_command() {
                    match cmd {
                        tui::DashboardCommand::TogglePause => {
                            metrics.is_paused.store(false, Ordering::Relaxed);
                            tui::log("[TUI] Máquina virtual reanudada");
                        }
                        tui::DashboardCommand::Quit => {
                            SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            }
        }

        // ── Progreso periódico ──────────────────────────────────
        {
            let now = std::time::Instant::now();
            if now.duration_since(last_report) >= std::time::Duration::from_secs(5) {
                last_report = now;
                if tui::is_active() {
                    let post_last = bus_lock().post.last;
                    tui::log(format!("[VMM] exits={} (reboots={}) | POST=0x{:02X}", total_exits, total_reboots, post_last));
                } else if let Ok(r) = vcpu.get_regs() {
                    if let Ok(s) = vcpu.get_sregs() {
                        let linear = s.cs.base.wrapping_add(r.rip);
                        let phys = resolve_guest_gpa(&vcpu, s.cr0, linear);
                        let code_dump = if let Some(p) = gpa_to_mem_offset(phys, guest_mem.len()) {
                            if p + 16 <= guest_mem.len() {
                                format!("{:02x?}", &guest_mem[p..p+16])
                            } else { String::from("???") }
                        } else { String::from("???") };
                        // Dump stack top 32 bytes
                        let stack_lin = s.ss.base.wrapping_add(r.rsp);
                        let stack_phys = resolve_guest_gpa(&vcpu, s.cr0, stack_lin);
                        let stack_dump = if let Some(sp) = gpa_to_mem_offset(stack_phys, guest_mem.len()) {
                            if sp + 32 <= guest_mem.len() {
                                format!("{:02x?}", &guest_mem[sp..sp+32])
                            } else { String::from("???") }
                        } else { String::from("???") };
                        let (post_last, ps2_acc, pit_acc) = {
                            let b = bus_lock();
                            (b.post.last, b.legacy_irq.ps2_access_count, b.legacy_irq.pit_access_count)
                        };
                        eprintln!(
                            "[VMM] {} exits (reboots={}) | POST=0x{:02X} rip={:#x} (gpa={:#x}) cr0={:#x} rflags={:#x} idt={:#x}/{:#x}\n  code: {}\n  stack(S:{:04x} L:{:#x} P:{:#x}): {}",
                            total_exits, total_reboots, post_last, linear, phys, s.cr0, r.rflags, s.idt.base, s.idt.limit, code_dump,
                            s.ss.selector, stack_lin, stack_phys, stack_dump
                        );
                        eprintln!("  PS2 access={}, PIT access={}, mode_transitions={}",
                            ps2_acc,
                            pit_acc,
                            mode_transitions);
                        // Dump segment registers and BDA keyboard buffer
                        eprintln!("  DS={:04x} ES={:04x} FS={:04x} GS={:04x} SS={:04x} CS={:04x}",
                            s.ds.selector, s.es.selector, s.fs.selector, s.gs.selector, s.ss.selector, s.cs.selector);
                        let bda_tick_val = guest_mem[BDA_TICK_ADDR] as u32
                            | ((guest_mem[BDA_TICK_ADDR+1] as u32) << 8)
                            | ((guest_mem[BDA_TICK_ADDR+2] as u32) << 16)
                            | ((guest_mem[BDA_TICK_ADDR+3] as u32) << 24);
                        eprintln!("  BDA tick@0x46C={:08x} (written={}) kbd: head@0x41A={:04x} tail@0x41C={:04x}",
                            bda_tick_val, bda_tick_count.load(Ordering::Relaxed),
                            guest_mem[0x41A] as u16 | ((guest_mem[0x41B] as u16) << 8),
                            guest_mem[0x41C] as u16 | ((guest_mem[0x41D] as u16) << 8));
                        // Dump bytes at ES:0x1A and ES:0x1C (the addresses the loop reads)
                        let es_base = s.es.selector as usize * 16;
                        let es_1a = es_base + 0x1A;
                        let es_1c = es_base + 0x1C;
                        if es_1a + 2 <= guest_mem.len() && es_1c + 2 <= guest_mem.len() {
                            eprintln!("  ES:0x1A (phys={:#x})={:04x} ES:0x1C (phys={:#x})={:04x}",
                                es_1a, guest_mem[es_1a] as u16 | ((guest_mem[es_1a+1] as u16) << 8),
                                es_1c, guest_mem[es_1c] as u16 | ((guest_mem[es_1c+1] as u16) << 8));
                        }
                        // Dump 32 bytes from BDA keyboard buffer area (0x41E)
                        if 0x41E + 32 <= guest_mem.len() {
                            eprintln!("  BDA kbd buf (0x41E): {:02x?}", &guest_mem[0x41E..0x41E+32]);
                        }
                        // Top I/O ports: los contadores por puerto ya viven en
                        // `metrics` (atomics). Antes se duplicaban en dos arrays
                        // de 512 KB que se incrementaban en cada exit.
                        let top_ports: Vec<String> = metrics.top_ports(10).iter()
                            .map(|(p, ins, outs)| format!("0x{:X}:{}/{}", p, ins, outs))
                            .collect();
                        eprintln!("  Top I/O (IN/OUT): {}", top_ports.join(", "));
                        // Dump code at the return address (caller on stack)
                        if let Some(sp) = gpa_to_mem_offset(stack_phys, guest_mem.len()) {
                            if sp + 4 <= guest_mem.len() {
                                let ret_addr = guest_mem[sp] as u32
                                    | ((guest_mem[sp + 1] as u32) << 8)
                                    | ((guest_mem[sp + 2] as u32) << 16)
                                    | ((guest_mem[sp + 3] as u32) << 24);
                                let ra = ret_addr as usize;
                                if ra + 48 <= guest_mem.len() {
                                    eprintln!("  Caller at {:#x}: {:02x?}", ra, &guest_mem[ra..ra+48]);
                                }
                            }
                        }
                        // Dump IVT entries for key vectors
                        for vec_num in [0u8, 1, 6, 8, 9, 0x0E, 0x10, 0x13] {
                            let ivt_off = (vec_num as usize) * 4;
                            if ivt_off + 4 <= guest_mem.len() {
                                let offset = guest_mem[ivt_off] as u16 | ((guest_mem[ivt_off+1] as u16) << 8);
                                let segment = guest_mem[ivt_off+2] as u16 | ((guest_mem[ivt_off+3] as u16) << 8);
                                let handler_phys = ((segment as u32) << 4) + (offset as u32);
                                eprintln!("  IVT[{:02x}] = {:04x}:{:04x} (phys={:#x})", vec_num, segment, offset, handler_phys);
                            }
                        }
                        // Dump last 50 I/O trace entries
                        let trace_len = io_trace.len();
                        let start = if trace_len > 50 { trace_len - 50 } else { 0 };
                        let trace_entries: Vec<String> = io_trace.range(start..).map(|(w, p, v)| {
                            format!("{} 0x{:04X}={:02X}", if *w == 1 { "W" } else { "R" }, p, v)
                        }).collect();
                        eprintln!("  I/O trace (last {}): {}", trace_entries.len(), trace_entries.join(", "));
                    }
                }
            }
        }

        // ── IRQ0 pulsada por el hilo del PIT (tarea 16) ────────────
        // El marcador del BSP (irq0_raised) alimenta la lógica de
        // interrupt-window; el hilo de temporización la pulsa al haber
        // underflow del PIT, así que la reflejamos aquí.
        if irq0_flag.swap(false, Ordering::Relaxed) {
            irq0_raised = true;
        }

        // ── Detectar cambio de POST y ventana de interrupciones en UN solo lock ──
        let (cur_post, irq_pending) = {
            let b = bus_lock();
            let pending = irq0_raised
                || b.legacy_irq.ps2_has_data()
                || b.legacy_irq.mouse_has_data()
                || b.uart_irq_pending()
                || b.ide_irq_pending();
            (b.post.last, pending)
        };
        if cur_post != last_post {
            let r = vcpu.get_regs().unwrap_or_default();
            let s = vcpu.get_sregs().unwrap_or_default();
            let msg = format!("[VMM] POST: 0x{:02X} → 0x{:02X} (phys={:#x})",
                last_post, cur_post, s.cs.base + r.rip);
            tui::log(&msg);
            last_post = cur_post;
        }

        // ── Detectar cambio de CR0 (modo real ↔ protegido) ────
        // KVM manages CR0.PE (Protected Enable) and segment registers
        // via VMCS — no shadow tracking needed. We log transitions
        // for diagnostics only.
        if total_exits % 50000 == 0 {
            if !tui::is_active() {
                eprint!("{}", metrics.format_summary(1.0, total_exits.saturating_sub(50000)));
            }
            if let Ok(s) = vcpu.get_sregs() {
                if s.cr0 != last_cr0 {
                    mode_transitions += 1;
                    let pe_old = (last_cr0 & 1) != 0;
                    let pe_new = (s.cr0 & 1) != 0;
                    let r = vcpu.get_regs().unwrap_or_default();
                    let msg = format!("[VMM] CR0 change: {:#x} → {:#x} (PE {}→{}) at phys={:#x} gdt={:#x}/{:#x} idt={:#x}/{:#x}",
                        last_cr0, s.cr0, pe_old, pe_new,
                        s.cs.base + r.rip, s.gdt.base, s.gdt.limit, s.idt.base, s.idt.limit);
                    tui::log(&msg);
                    last_cr0 = s.cr0;
                }
            }
        }

        // ── Ventana de interrupciones (tarea 4) ────────────────────
        // Si hay una IRQ pendiente y el guest corre con IF=0, activamos
        // kvm_run.request_interrupt_window para que KVM nos saque con
        // KVM_EXIT_IRQ_WINDOW_OPEN en cuanto el guest abra la ventana
        // (STI/POPF/IRET). Con irqchip en el kernel el IRR pendiente no se
        // pierde, pero este aviso da entrega puntual en el mismo instante
        // en que el guest se vuelve interumpible.
        // SOLO con IF=0: con la ventana abierta KVM saldría en cada entrada
        {
            let want_window = if irq_pending {
                // IF = bit 9 de RFLAGS. unwrap_or(true): ante error de ioctl
                // no pedimos la ventana (comportamiento conservador previo).
                !vcpu.get_regs().map(|r| r.rflags & 0x200 != 0).unwrap_or(true)
            } else {
                false
            };
            if want_window != irq_window_requested {
                let req = if want_window { 1u8 } else { 0u8 };
                vcpu.get_kvm_run().request_interrupt_window = req;
                irq_window_requested = want_window;
            }
        }

        // ── Ejecutar guest ──────────────────────────────────────
        let bsp_run_start = std::time::Instant::now();
        let exit_reason = match vcpu.run() {
            Ok(r) => r,
            Err(e) if e.errno() == libc::EINTR => {
                // Señal (SIGINT/SIGTERM) interrumpió vcpu.run(). El PIT lo
                // gobierna el hilo dedicado (tarea 16); aquí solo entregamos
                // IRQ pendientes de dispositivos y reintentamos bajo un solo lock.
                let (ps2_irq, mouse_irq, uart_irq, (ide14, ide15)) = {
                    let mut b = bus_lock();
                    let ps2 = b.take_ps2_irq();
                    let mouse = b.take_mouse_irq();
                    let uart = b.take_uart_irq();
                    let ide = b.take_ide_irq();
                    sync_pci_irqs(&mut b, &vm, None);
                    (ps2, mouse, uart, ide)
                };
                if ps2_irq {
                    vm.set_irq_line(1, false).ok();
                    vm.set_irq_line(1, true).ok();
                }
                if mouse_irq {
                    vm.set_irq_line(12, false).ok();
                    vm.set_irq_line(12, true).ok();
                }
                if uart_irq {
                    vm.set_irq_line(4, false).ok();
                    vm.set_irq_line(4, true).ok();
                }
                if ide14 {
                    vm.set_irq_line(14, false).ok();
                    vm.set_irq_line(14, true).ok();
                }
                if ide15 {
                    vm.set_irq_line(15, false).ok();
                    vm.set_irq_line(15, true).ok();
                }
                continue;
            }
            Err(e) => { eprintln!("[VMM] vcpu.run() falló: {}", e); exit(1); }
        };
        let bsp_nanos = bsp_run_start.elapsed().as_nanos() as u64;
        metrics.record_vcpu_active(0, bsp_nanos);
        metrics.record_vcpu_exit(0);
        total_exits += 1;

       // NOTE: The PIT is only advanced by the dedicated pit-timer thread
        // (clock_nanosleep, tarea 16). We do NOT call pit_tick() here to
        // avoid double-counting: no está gobernado por el conteo de exits.

        // ── IRQs pendientes de dispositivos: un solo lock del bus ────
        // Antes eran 8 adquisiciones por exit (ps2/mouse/uart/ide/sync/smi/
        // hotplug/nivel de IRQ1). Se recogen todos los latches one-shot y la
        // sincronización PCI bajo el mismo guard.
        let (ps2_irq, mouse_irq, uart_irq, ide_irqs, smi, ps2_has_data) = {
            let mut b = bus_lock();
            let ps2 = b.take_ps2_irq();
            let mouse = b.take_mouse_irq();
            let uart = b.take_uart_irq();
            let ide = b.take_ide_irq();
            // Dispositivos PCI (UHCI, VirtIO, AHCI, VMMDev, AC'97, e1000):
            // sincronizar con lógica wired-OR (solo emite ioctl al cambiar).
            sync_pci_irqs(&mut b, &vm, Some(&metrics));
            let smi = b.take_smi();
            // CPU Hotplug (Item 23): SCI en IRQ9. Esta línea es compartida
            // con VirtIO-Net (PIRQA), así que el cache de niveles debe
            // reflejar el flanco para que sync_pci_irqs pueda bajarla luego.
            let sci = b.take_cpu_hotplug_sci();
            if sci {
                vm.set_irq_line(9, false).ok();
                vm.set_irq_line(9, true).ok();
                b.set_pci_irq_level(9, true);
            }
            let ps2_data = b.legacy_irq.ps2_has_data();
            (ps2, mouse, uart, ide, smi, ps2_data)
        };
        // Inject IRQ1 into the kernel PIC when the 8042 has undelivered
        // data. Lower→raise pulse: garantiza un flanco fresco por byte
        // pendiente (antes se subía la línea sin bajarla y, si ya estaba
        // alta, la interrupción se perdía silenciosamente).
        if ps2_irq {
            vm.set_irq_line(1, false).ok();
            vm.set_irq_line(1, true).ok();
            irq1_line_high = true;
            metrics.record_irq(1);
        }
        // Ratón PS/2 (IRQ12): flanco por cada byte pendiente del auxiliar.
        if mouse_irq {
            vm.set_irq_line(12, false).ok();
            vm.set_irq_line(12, true).ok();
            metrics.record_irq(12);
        }
        // UART 16550 (IRQ4): flanco por cada RX/THRE pendiente (tarea 12).
        if uart_irq {
            vm.set_irq_line(4, false).ok();
            vm.set_irq_line(4, true).ok();
            metrics.record_irq(4);
        }
        // Canales IDE (IRQ14 disco / IRQ15 CD-ROM ATAPI)
        if ide_irqs.0 {
            vm.set_irq_line(14, false).ok();
            vm.set_irq_line(14, true).ok();
            metrics.record_irq(14);
        }
        if ide_irqs.1 {
            vm.set_irq_line(15, false).ok();
            vm.set_irq_line(15, true).ok();
            metrics.record_irq(15);
        }
        // APM / SMI (Item 23): inyectar SMI al vCPU si hubo comando en 0xB2
        if smi {
            use std::os::unix::io::AsRawFd;
            const KVM_SMI: libc::c_ulong = 0xAEB7;
            unsafe { libc::ioctl(vcpu.as_raw_fd(), KVM_SMI); };
            if verbose { eprintln!("[VMM] SMI inyectado a vCPU 0 tras comando APM en 0xB2"); }
        }
        // (tarea 4) Disciplina de nivel para IRQ1: si el guest consumió
        // todo el output buffer, la línea puede bajar con seguridad — la
        // interrupción que la subió ya se entregó y atendió. Bajarla aquí
        // devuelve la línea a un estado limpio para el siguiente flanco.
        if !ps2_has_data && irq1_line_high {
            vm.set_irq_line(1, false).ok();
            irq1_line_high = false;
        }

        match exit_reason {
            VcpuExit::IoOut(port, data) => {
                metrics.record_io_out(port);
                let (handled, reset_from_dev) = {
                    let mut b = bus_lock();
                    let h = b.out(port, data);
                    let r = b.take_reset_requested();
                    (h, r)
                };
                if !handled {
                    eprintln!("[VMM] OUT no manejado: 0x{:X} <- {:02X?}", port, data);
                }
                // Hardware reset register (PCI/PIIX): 0xCF9
                // 0x02 = soft reset, 0x04 = hard reset, 0x06 = full reset
                if (port == 0xCF9 && data.iter().any(|&v| v & 0x04 != 0)) || reset_from_dev {
                    eprintln!("[VMM] Reset via puerto 0x{:X} (data={:02X?}) — reiniciando guest...", port, data);
                    if let Ok(mut q) = kbd_queue.lock() {
                        q.clear();
                    }
                    if let Ok(mut q) = mouse_queue.lock() {
                        q.clear();
                    }
                    reset_vcpu_to_post(
                        &vcpu,
                        &vm,
                        &mut bus_lock(),
                        guest_mem,
                        Some(high_mem),
                        Some(&bios),
                        vga_rom_data.as_deref(),
                    );
                    total_reboots = 0;
                    continue;
                }
                // (tarea 4) EOI del guest al PIC maestro (OCW2): la
                // interrupción ya se entregó y atendió, así que damos por
                // entregado el IRQ0 pendiente (el marcador solo alimenta
                // request_interrupt_window). La línea NO se baja aquí: un
                // EOI no descarta ticks coalescidos pendientes en el IRR.
                if port == 0x20
                    && (data[0] == 0x20 || ((data[0] & 0xF8) == 0x60 && (data[0] & 0x07) == 0))
                {
                    irq0_raised = false;
                }
                // Traza de I/O solo en modo verbose: estos dos push se
                // ejecutaban en CADA acceso a puerto aunque nadie leyera
                // la traza (solo se imprime en el volcado de diagnóstico).
                if verbose {
                    recent.push_back(format!("OUT 0x{:X} <- {:02X?}", port, data));
                    if recent.len() > 128 { recent.pop_front(); }
                    if io_trace.len() >= 100 { io_trace.pop_front(); }
                    io_trace.push_back((1, port, data[0]));
                }
            }
            VcpuExit::IoIn(port, data) => {
                metrics.record_io_in(port);
                match bus_lock().input(port, data.len()) {
                    Some(bytes) => {
                        for (i, b) in bytes.iter().take(data.len()).enumerate() {
                            data[i] = *b;
                        }
                    }
                    None => {
                        eprintln!("[VMM] IN no manejado: 0x{:X}", port);
                        data.fill(0xFF);
                    }
                }
                if verbose {
                    recent.push_back(format!("IN  0x{:X}={} ({} bytes)", port, data[0], data.len()));
                    if recent.len() > 128 { recent.pop_front(); }
                    if io_trace.len() >= 100 { io_trace.pop_front(); }
                    io_trace.push_back((0, port, data[0]));
                }
            }
            VcpuExit::Hlt => {
                metrics.record_hlt();
                // HLT es un evento normal (guest ocioso o STI;HLT), no un
                // error: los dos ioctls de registros + escritura a stderr
                // se hacían en cada halt y no aportan nada sin verbose.
                if verbose {
                    let r = vcpu.get_regs().unwrap_or_default();
                    let s = vcpu.get_sregs().unwrap_or_default();
                    eprintln!("[VMM] HLT — phys={:#x} rflags={:#x}", s.cs.base + r.rip, r.rflags);
                }
            }
            VcpuExit::Shutdown => {
                metrics.record_shutdown();
                eprintln!("[VMM] === TRIPLE FAULT ===");
                dump_crash_state(&vcpu, &guest_mem, &recent, total_exits);
                crash_reboot(
                    &vcpu,
                    &vm,
                    &mut bus_lock(),
                    guest_mem,
                    Some(high_mem),
                    Some(&bios),
                    vga_rom_data.as_deref(),
                    &mut total_reboots,
                );
                if total_reboots >= MAX_CRASH_REBOOTS { break; }
            }
            VcpuExit::IrqWindowOpen => {
                metrics.record_irq_window();
                irq0_raised = false;
                let (ps2_irq, mouse_irq, uart_irq, (ide14, ide15)) = {
                    let mut b = bus_lock();
                    (
                        b.take_ps2_irq(),
                        b.take_mouse_irq(),
                        b.take_uart_irq(),
                        b.take_ide_irq(),
                    )
                };
                if ps2_irq {
                    vm.set_irq_line(1, false).ok();
                    vm.set_irq_line(1, true).ok();
                    irq1_line_high = true;
                    metrics.record_irq(1);
                }
                if mouse_irq {
                    vm.set_irq_line(12, false).ok();
                    vm.set_irq_line(12, true).ok();
                    metrics.record_irq(12);
                }
                if uart_irq {
                    vm.set_irq_line(4, false).ok();
                    vm.set_irq_line(4, true).ok();
                    metrics.record_irq(4);
                }
                if ide14 {
                    vm.set_irq_line(14, false).ok();
                    vm.set_irq_line(14, true).ok();
                    metrics.record_irq(14);
                }
                if ide15 {
                    vm.set_irq_line(15, false).ok();
                    vm.set_irq_line(15, true).ok();
                    metrics.record_irq(15);
                }
            }
            VcpuExit::MmioWrite(addr, data) => {
                metrics.record_mmio_write();
                {
                    let mut b = bus_lock();
                    b.mmio_write(addr, data);
                    sync_pci_irqs(&mut b, &vm, Some(&metrics));
                }
                if verbose { eprintln!("[VMM] MMIO W {:#x}", addr); }
            }
            VcpuExit::MmioRead(addr, data) => {
                metrics.record_mmio_read();
                let bytes = {
                    let mut b = bus_lock();
                    let res = b.mmio_read(addr, data.len());
                    sync_pci_irqs(&mut b, &vm, Some(&metrics));
                    res
                };
                let count = bytes.len().min(data.len());
                data[..count].copy_from_slice(&bytes[..count]);
                if verbose { eprintln!("[VMM] MMIO R {:#x}", addr); }
            }
            VcpuExit::InternalError => {
                metrics.record_internal_error();
                let r = vcpu.get_regs().unwrap_or_default();
                let s = vcpu.get_sregs().unwrap_or_default();
                eprintln!("[VMM] KVM InternalError phys={:#x}", s.cs.base + r.rip);
                dump_crash_state(&vcpu, &guest_mem, &recent, total_exits);
                crash_reboot(
                    &vcpu,
                    &vm,
                    &mut bus_lock(),
                    guest_mem,
                    Some(high_mem),
                    Some(&bios),
                    vga_rom_data.as_deref(),
                    &mut total_reboots,
                );
                if total_reboots >= MAX_CRASH_REBOOTS { break; }
            }
            other => {
                metrics.record_other();
                let r = vcpu.get_regs().unwrap_or_default();
                let s = vcpu.get_sregs().unwrap_or_default();
                eprintln!("[VMM] Exit no manejado: {:?} phys={:#x}", other, s.cs.base + r.rip);
                break;
            }
        }
    }

    // ─── Cierre de VM: sincronización y guardado de snapshots (Item 22, 24) ───
    tui_running.store(false, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(150));
    eprintln!("\n{}", metrics.format_summary(1.0, 0));
    if let Some(pf) = bus.lock().unwrap_or_else(|p| p.into_inner()).pflash.as_mut() {
        pf.flush_to_disk().ok();
    }
    if let Ok(snap_path) = std::env::var("MI_VMM_SNAPSHOT_SAVE") {
        eprintln!("[VMM] Guardando snapshot en: {}", snap_path);
        snapshot::save_vm_snapshot(&snap_path, guest_mem, high_mem, &[&vcpu]).ok();
    }
}

/// Bucle de un vCPU AP (tarea 6: SMP).
///
/// El AP nace aparcado en KVM_MP_STATE_INIT_RECEIVED: `run()` se bloquea
/// en el kernel hasta que el guest le entrega INIT-SIPI por el LAPIC (el
/// irqchip del kernel lo maneja entero, incluido fijar CS:IP = vector<<4
/// en modo real). Desde ahí ejecuta el guest igual que el BSP pero con
/// una política de exits más simple:
///   - El temporizador (PIT/IRQ0/tick del BDA) lo gobierna el hilo dedicado
///     pit-timer (tarea 16): si una señal interrumpe este hilo (EINTR) se
///     reintenta sin tocar nada.
///   - Un triple fault en un AP solo "resetea" ese CPU: se re-aparca en
///     wait-for-SIPI y el siguiente INIT-SIPI del guest lo trae de vuelta
///     (en hardware real un triple fault de un AP no apaga la caja).
fn ap_vcpu_worker(
    cpu_id: u32,
    vcpu: kvm_ioctls::VcpuFd,
    vm: Arc<kvm_ioctls::VmFd>,
    bus: Arc<Mutex<DeviceBus>>,
    metrics: Arc<metrics::VmmMetrics>,
    verbose: bool,
) {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    if let Ok(mut tids) = AP_TIDS.lock() {
        tids.push(tid);
    }
    let mut total_exits: u64 = 0;
    let park = |vcpu: &kvm_ioctls::VcpuFd| {
        let events = kvm_vcpu_events::default();
        vcpu.set_vcpu_events(&events).ok();
        vcpu.set_mp_state(kvm_mp_state {
            mp_state: kvm_bindings::KVM_MP_STATE_INIT_RECEIVED,
        })
        .ok();
    };
    loop {
        if RESET_REQUESTED.load(Ordering::SeqCst) {
            park(&vcpu);
            while RESET_REQUESTED.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        while metrics.is_paused.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let run_start = std::time::Instant::now();
        let exit_reason = match vcpu.run() {
            Ok(r) => r,
            // Señal (SIGINT/SIGTERM/SIGUSR1): si es reset global, aparcar en wait-for-SIPI
            Err(e) if e.errno() == libc::EINTR => {
                if RESET_REQUESTED.load(Ordering::SeqCst) {
                    park(&vcpu);
                    while RESET_REQUESTED.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
                continue;
            }
            Err(e) => {
                eprintln!("[AP{}] vcpu.run() falló: {}", cpu_id, e);
                return;
            }
        };
        let run_nanos = run_start.elapsed().as_nanos() as u64;
        metrics.record_vcpu_active(cpu_id as usize, run_nanos);
        metrics.record_vcpu_exit(cpu_id as usize);
        total_exits += 1;
        match exit_reason {
            VcpuExit::IoOut(port, data) => {
                metrics.record_io_out(port);
                let (handled, kbd_irq, mouse_irq, ide_irqs, reset_req) = {
                    let mut b = bus.lock().unwrap_or_else(|p| p.into_inner());
                    let h = b.out(port, data);
                    let k = b.take_ps2_irq();
                    let m = b.take_mouse_irq();
                    let ide = b.take_ide_irq();
                    let r = (port == 0xCF9 && data.iter().any(|&v| v & 0x04 != 0)) || b.take_reset_requested();
                    sync_pci_irqs(&mut b, &vm, Some(&metrics));
                    (h, k, m, ide, r)
                };
                if reset_req {
                    eprintln!("[AP{}] Solicitud de reset de VM detectada en I/O (port={:#X})", cpu_id, port);
                    request_vm_reset();
                    park(&vcpu);
                    while RESET_REQUESTED.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    continue;
                }
                if kbd_irq {
                    vm.set_irq_line(1, false).ok();
                    vm.set_irq_line(1, true).ok();
                    metrics.record_irq(1);
                }
                if mouse_irq {
                    vm.set_irq_line(12, false).ok();
                    vm.set_irq_line(12, true).ok();
                    metrics.record_irq(12);
                }
                if ide_irqs.0 {
                    vm.set_irq_line(14, false).ok();
                    vm.set_irq_line(14, true).ok();
                    metrics.record_irq(14);
                }
                if ide_irqs.1 {
                    vm.set_irq_line(15, false).ok();
                    vm.set_irq_line(15, true).ok();
                    metrics.record_irq(15);
                }
                if !handled {
                    eprintln!("[AP{}] OUT no manejado: 0x{:X} <- {:02X?}", cpu_id, port, data);
                }
            }
            VcpuExit::IoIn(port, data) => {
                metrics.record_io_in(port);
                let (res, kbd_irq, mouse_irq, ide_irqs) = {
                    let mut b = bus.lock().unwrap_or_else(|p| p.into_inner());
                    let r = b.input(port, data.len());
                    let k = b.take_ps2_irq();
                    let m = b.take_mouse_irq();
                    let ide = b.take_ide_irq();
                    sync_pci_irqs(&mut b, &vm, Some(&metrics));
                    (r, k, m, ide)
                };
                if kbd_irq {
                    vm.set_irq_line(1, false).ok();
                    vm.set_irq_line(1, true).ok();
                    metrics.record_irq(1);
                }
                if mouse_irq {
                    vm.set_irq_line(12, false).ok();
                    vm.set_irq_line(12, true).ok();
                    metrics.record_irq(12);
                }
                if ide_irqs.0 {
                    vm.set_irq_line(14, false).ok();
                    vm.set_irq_line(14, true).ok();
                    metrics.record_irq(14);
                }
                if ide_irqs.1 {
                    vm.set_irq_line(15, false).ok();
                    vm.set_irq_line(15, true).ok();
                    metrics.record_irq(15);
                }
                match res {
                    Some(bytes) => {
                        for (i, b) in bytes.iter().take(data.len()).enumerate() {
                            data[i] = *b;
                        }
                    }
                    None => {
                        if verbose {
                            eprintln!("[AP{}] IN no manejado: 0x{:X}", cpu_id, port);
                        }
                        data.fill(0xFF);
                    }
                }
            }
            VcpuExit::Hlt => {
                metrics.record_hlt();
                // STI;HLT del guest (p. ej. el bucle de paro de los APs de
                // SeaBIOS). Con irqchip en el kernel, KVM bloquea dentro de
                // run() hasta una interrupción o SIPI: no hay nada que hacer.
                if verbose {
                    eprintln!("[AP{}] HLT (exits={})", cpu_id, total_exits);
                }
            }
            VcpuExit::Intr => {
                metrics.record_intr();
                // KVM_EXIT_INTR: ejecución interrumpida por evento interno.
                // Benigno: reintentar (aparcar aquí rompería el SMP).
            }
            VcpuExit::IrqWindowOpen => {
                metrics.record_irq_window();
                // La ventana de interrupciones solo la pide el BSP.
            }
            VcpuExit::Shutdown | VcpuExit::InternalError => {
                metrics.record_shutdown();
                eprintln!(
                    "[AP{}] triple fault/InternalError — AP solicita reset de VM y se aparca en wait-for-SIPI",
                    cpu_id
                );
                request_vm_reset();
                park(&vcpu);
                while RESET_REQUESTED.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            VcpuExit::MmioRead(addr, data) => {
                metrics.record_mmio_read();
                let bytes = {
                    let mut b = bus.lock().unwrap_or_else(|p| p.into_inner());
                    let res = b.mmio_read(addr, data.len());
                    sync_pci_irqs(&mut b, &vm, Some(&metrics));
                    res
                };
                let count = bytes.len().min(data.len());
                data[..count].copy_from_slice(&bytes[..count]);
            }
            VcpuExit::MmioWrite(addr, data) => {
                metrics.record_mmio_write();
                let mut b = bus.lock().unwrap_or_else(|p| p.into_inner());
                b.mmio_write(addr, data);
                sync_pci_irqs(&mut b, &vm, Some(&metrics));
            }
            other => {
                metrics.record_other();
                eprintln!("[AP{}] exit no manejado: {:?} — AP aparcado", cpu_id, other);
                park(&vcpu);
            }
        }
    }
}

fn dump_crash_state(
    vcpu: &kvm_ioctls::VcpuFd,
    guest_mem: &[u8],
    recent: &VecDeque<String>,
    total_exits: u64,
) {
    let sregs = vcpu.get_sregs().unwrap_or_default();
    let regs = vcpu.get_regs().unwrap_or_default();
    let linear = sregs.cs.base.wrapping_add(regs.rip);
    let phys = resolve_guest_gpa(vcpu, sregs.cr0, linear);
    eprintln!("[VMM] Crash: rip={:#x} cs={:#x} base={:#x} cr0={:#x} rflags={:#x}",
        regs.rip, sregs.cs.selector, sregs.cs.base, sregs.cr0, regs.rflags);
    eprintln!("  linear={:#x} gpa={:#x} idt={:#x}/{:#x} gdt={:#x}/{:#x} exits={}",
        linear, phys, sregs.idt.base, sregs.idt.limit, sregs.gdt.base, sregs.gdt.limit, total_exits);
    if let Some(p) = gpa_to_mem_offset(phys, guest_mem.len()) {
        if p + 32 <= guest_mem.len() {
            eprintln!("  Code: {:02x?}", &guest_mem[p..p + 32]);
        }
    }
    let ep = 0xFE05B_usize;
    if ep + 16 <= guest_mem.len() {
        eprintln!("  BIOS entry: {:02x?}", &guest_mem[ep..ep + 16]);
    }
    if !recent.is_empty() {
        eprintln!("  Last I/O:");
        for ev in recent.iter().rev().take(20).collect::<Vec<_>>().iter().rev() {
            eprintln!("    {}", ev);
        }
    }
}

// ─── Tests del layout de memslots (tarea 7) ────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    /// Layout real del high_mem: 512 MiB @0xE0000000 con los huecos del
    /// IOAPIC y del LAPIC → 3 regiones que encajan sin solaparse.
    #[test]
    fn carve_high_mem_layout() {
        let r = carve_reserved_holes(0xE000_0000, 0x2000_0000, &KERNEL_IRQCHIP_HOLES);
        assert_eq!(
            r,
            vec![
                (0xE000_0000, 0xFEC0_0000),
                (0xFEC0_1000, 0xFEE0_0000),
                (0xFEE0_1000, 0x1_0000_0000),
            ]
        );
        // Cobertura total: 512 MiB menos las dos páginas reservadas.
        let covered: u64 = r.iter().map(|(s, e)| e - s).sum();
        assert_eq!(covered, 0x2000_0000 - 2 * 0x1000);
    }

    #[test]
    fn carve_high_mem_with_pci_mmio_hole() {
        let r = carve_reserved_holes(0xE000_0000, 0x2000_0000, &HIGH_MEM_HOLES);
        assert_eq!(
            r,
            vec![
                (0xE000_0000, 0xFE00_0000),
                (0xFEC0_1000, 0xFEE0_0000),
                (0xFEE0_1000, 0x1_0000_0000),
            ]
        );
    }

    #[test]
    fn carve_sin_interseccion_no_recorta() {
        let r = carve_reserved_holes(0x0, 0x1000, &KERNEL_IRQCHIP_HOLES);
        assert_eq!(r, vec![(0x0, 0x1000)]);
    }

    #[test]
    fn carve_hueco_cubre_todo_el_rango() {
        let r = carve_reserved_holes(0xFEC0_0000, 0x1000, &KERNEL_IRQCHIP_HOLES);
        assert!(r.is_empty());
    }

    #[test]
    fn carve_huecos_en_los_extremos() {
        let holes = [(0x0000, 0x1000), (0x5000, 0x1000)];
        let r = carve_reserved_holes(0x0, 0x6000, &holes);
        assert_eq!(r, vec![(0x1000, 0x5000)]);
    }

    #[test]
    fn carve_regiones_consecutivas_no_solapan() {
        let r = carve_reserved_holes(0xE000_0000, 0x2000_0000, &KERNEL_IRQCHIP_HOLES);
        for w in r.windows(2) {
            assert!(w[0].1 <= w[1].0, "regiones solapadas: {:?}", w);
        }
    }

    #[test]
    fn test_unattended_env_variables() {
        std::env::set_var("TFF_UNATTENDED", "1");
        assert_eq!(get_vmm_env("UNATTENDED").unwrap(), "1");
        std::env::remove_var("TFF_UNATTENDED");

        std::env::set_var("TWO_FIVE_FIVE_UNATTENDED", "1");
        assert_eq!(get_vmm_env("UNATTENDED").unwrap(), "1");
        std::env::remove_var("TWO_FIVE_FIVE_UNATTENDED");

        std::env::set_var("TFF_UNATTENDED_USER", "custom_admin");
        assert_eq!(get_vmm_env("UNATTENDED_USER").unwrap(), "custom_admin");
        std::env::remove_var("TFF_UNATTENDED_USER");
    }

    #[test]
    fn test_unattended_config_defaults() {
        let cfg = unattended::UnattendedConfig::default();
        assert_eq!(cfg.username, "two55");
        assert_eq!(cfg.password, "two55");
        assert_eq!(cfg.hostname, "two55-vm");
        assert_eq!(cfg.timezone, "UTC");
    }

    #[test]
    fn test_vcpu_reset_to_real_mode() {
        let kvm = kvm_ioctls::Kvm::new().unwrap();
        let vm = kvm.create_vm().unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        let mut sregs = vcpu.get_sregs().unwrap();
        // Simular que Windows 7 estuvo en 64-bit Long Mode
        sregs.efer = 0x0000_0D01;
        sregs.cr0 = 0x8001_0031;
        sregs.cr4 = 0x0000_06F8;
        sregs.cr3 = 0x0010_0000;
        vcpu.set_sregs(&sregs).unwrap();

        // Ahora intentamos resetear tal como hacía el código anterior
        // (dejando efer y cr4 sucios pero forzando cr0 a 0x60000010)
        let mut dirty_sregs = sregs.clone();
        dirty_sregs.cr0 = 0x6000_0010;
        let res_dirty = vcpu.set_sregs(&dirty_sregs);
        println!("Resultado de set_sregs sucio: {:?}", res_dirty);

        // Ahora reseteo limpio completo
        let mut clean_sregs = kvm_sregs::default();
        clean_sregs.cs.base = 0xF0000;
        clean_sregs.cs.selector = 0xF000;
        clean_sregs.cs.limit = 0xFFFF;
        clean_sregs.cs.type_ = 11;
        clean_sregs.cs.present = 1;
        clean_sregs.cs.s = 1;
        for seg in [&mut clean_sregs.ds, &mut clean_sregs.es, &mut clean_sregs.fs, &mut clean_sregs.gs, &mut clean_sregs.ss] {
            seg.base = 0; seg.selector = 0; seg.limit = 0xFFFF;
            seg.type_ = 3; seg.present = 1; seg.s = 1;
        }
        clean_sregs.tr.type_ = 11; clean_sregs.tr.present = 1;
        clean_sregs.ldt.type_ = 2; clean_sregs.ldt.present = 1;
        clean_sregs.gdt.limit = 0xFFFF; clean_sregs.idt.limit = 0xFFFF;
        clean_sregs.cr0 = 0x6000_0010;
        clean_sregs.cr2 = 0; clean_sregs.cr3 = 0; clean_sregs.cr4 = 0;
        clean_sregs.efer = 0;
        clean_sregs.apic_base = 0xFEE0_0900;
        let res_clean = vcpu.set_sregs(&clean_sregs);
        println!("Resultado de set_sregs limpio: {:?}", res_clean);
        assert!(res_clean.is_ok());

        let events = kvm_bindings::kvm_vcpu_events::default();
        let res_events = vcpu.set_vcpu_events(&events);
        println!("Resultado de set_vcpu_events: {:?}", res_events);
        assert!(res_events.is_ok());
    }
}
