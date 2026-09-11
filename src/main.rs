//! mi-vmm — Hipervisor Tipo-2 minimalista sobre KVM/Linux.

mod devices;
mod display;

use devices::DeviceBus;
use kvm_bindings::{kvm_mp_state, kvm_regs, kvm_sregs};
use kvm_ioctls::{Kvm, VcpuExit};
use std::fs::File;
use std::io::Read;
use std::collections::VecDeque;
use std::process::exit;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const GUEST_MEM_SIZE: usize = 256 * 1024 * 1024;
const RESET_VECTOR_CS: u64 = 0xF000;
const RESET_VECTOR_RIP: u64 = 0xFFF0;
/// Máximo de reboots provocados por crashes del guest (triple fault o
/// KVM InternalError) antes de rendirse. Evita un bucle infinito de
/// reinicios cuando el guest siempre crashea en el mismo punto.
const MAX_CRASH_REBOOTS: u32 = 5;

/// Resetea el vCPU al POST (equivalente a un reset por hardware tras
/// un crash del guest) e incrementa el contador de reboots.
fn crash_reboot(vcpu: &kvm_ioctls::VcpuFd, total_reboots: &mut u32) {
    *total_reboots += 1;
    eprintln!("[VMM] Crash del guest — reseteando VM (reboot {} de {})...",
        total_reboots, MAX_CRASH_REBOOTS);
    reset_vcpu_to_post(vcpu);
}

fn bios_load_addr(bios_len: usize) -> u64 {
    (0x0010_0000u64).saturating_sub(bios_len as u64)
}

fn usage() -> ! {
    eprintln!("Uso: mi-vmm <bios.bin> [imagen.iso]");
    exit(1);
}

/// Counter incremented by SIGALRM handler every 1ms.
/// Using AtomicU32 instead of AtomicBool so we don't lose ticks
/// when multiple alarms fire between loop iterations.
static ALARM_TICKS: AtomicU32 = AtomicU32::new(0);

/// Set by SIGTERM/SIGINT handler; checked in the vCPU loop to dump state and exit.
static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// BDA tick counter physical address (BIOS Data Area, offset 0x400 base + 0x6C).
const BDA_TICK_ADDR: usize = 0x46C;

extern "C" fn alarm_handler(_sig: libc::c_int) {
    // Async-signal-safe: solo incrementa un contador atómico.
    // El BDA tick (0x46C) se escribe desde el bucle vCPU (no aquí):
    // IRQ0 se inyecta al kernel PIC desde el loop y el handler INT 08h
    // de SeaBIOS incrementa 0x46C él mismo (evita avanzar al doble).
    ALARM_TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Arrange for SIGALRM to fire every `ms` milliseconds.
fn start_periodic_timer(ms: u32) {
    unsafe {
        // Set interval timer: 0 = first fire, interval = ms
        let mut itv = libc::itimerval {
            it_interval: libc::timeval { tv_sec: 0, tv_usec: (ms as i64) * 1000 },
            it_value: libc::timeval { tv_sec: 0, tv_usec: (ms as i64) * 1000 },
        };
        libc::setitimer(libc::ITIMER_REAL, &mut itv, std::ptr::null_mut());
    }
    install_signal_handler(libc::SIGALRM, alarm_handler);
}


/// Reset the vCPU to the power-on state (like a hardware reset via port 0xCF9).
fn reset_vcpu_to_post(vcpu: &kvm_ioctls::VcpuFd) {
    let mut sregs: kvm_sregs = vcpu.get_sregs().unwrap_or_default();
    sregs.cs.base = (RESET_VECTOR_CS as u16 as u32 as u64) << 4;
    sregs.cs.selector = RESET_VECTOR_CS as u16;
    sregs.cs.limit = 0xFFFF;
    sregs.cs.avl = 0;
    sregs.cs.db = 0;
    sregs.cs.l = 0;
    sregs.cs.g = 0;
    sregs.ds.base = 0; sregs.ds.selector = 0; sregs.ds.limit = 0xFFFF;
    sregs.es.base = 0; sregs.es.selector = 0; sregs.es.limit = 0xFFFF;
    sregs.fs.base = 0; sregs.fs.selector = 0; sregs.fs.limit = 0xFFFF;
    sregs.gs.base = 0; sregs.gs.selector = 0; sregs.gs.limit = 0xFFFF;
    sregs.ss.base = 0; sregs.ss.selector = 0; sregs.ss.limit = 0xFFFF;
    sregs.cr0 = 0x60000010;
    sregs.idt.base = 0; sregs.idt.limit = 0xFFFF;
    sregs.gdt.base = 0; sregs.gdt.limit = 0xFFFF;
    vcpu.set_sregs(&sregs).ok();
    let mut regs = kvm_regs::default();
    regs.rip = RESET_VECTOR_RIP;
    regs.rflags = 0x2;
    vcpu.set_regs(&regs).ok();
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
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
    for chunk in slice.chunks_mut(4096) {
        chunk.fill(0);
    }
    slice
}

/// Instala un handler para las señales indicadas (wrapper de libc::signal).
fn install_signal_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    unsafe {
        libc::signal(sig, handler as *const () as libc::sighandler_t);
    }
}

/// (tarea 7) Ventanas de MMIO que pertenecen al irqchip del kernel y NO
/// deben quedar cubiertas por ningún memslot de RAM.
///
/// Con `KVM_CREATE_IRQCHIP` el LAPIC (base 0xFEE00000) y el IOAPIC
/// (0xFEC00000) viven DENTRO de KVM. Si un memslot de RAM respalda esas
/// páginas, la EPT resuelve los accesos como memoria normal y el irqchip
/// del kernel jamás ve un registro: el guest no puede sondear CPUs
/// (ICR→INIT-SIPI) ni enrutar INTx (IOREDTBL). Estas ventanas se recortan
/// al registrar high_mem, y los antiguos "stubs falsos" se eliminaron:
/// eran exactamente lo que pisaba al irqchip real.
const KERNEL_IRQCHIP_HOLES: [(u64, u64); 2] = [
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

fn main() {
    env_logger::init();

        let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }
    let bios_path = &args[1];
    let iso_path = args.get(2).map(|s| s.as_str());
    let disk_path = args.get(3).map(|s| s.as_str());
    let verbose = std::env::var("MI_VMM_VERBOSE").is_ok();
    // (tarea 6) Número de vCPUs: 1 BSP + N-1 APs. El default es 2 (SMP
    // mínimo que ejercita el sondeo SIPI de SeaBIOS/Linux); MI_VMM_CPUS=n
    // lo cambia (MI_VMM_CPUS=1 recupera el comportamiento uniprocesador).
    let num_cpus: u32 = std::env::var("MI_VMM_CPUS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(2)
        .clamp(1, 8);

    let kvm = Kvm::new().expect("No se pudo abrir /dev/kvm");
    let vm = kvm.create_vm().expect("create_vm falló");
    // (tarea 7) Irqchip EN EL KERNEL: LAPIC por vCPU + IOAPIC + PIC 8259
    // viven dentro de KVM. Verificado: se crea aquí, ANTES de cualquier
    // create_vcpu (orden que KVM exige) y sus páginas de MMIO quedan
    // SIN memslot de RAM encima (ver high_mem más abajo).
    vm.create_irq_chip().expect("create_irq_chip falló");

    // ─── RAM principal: 256 MiB ────────────────────────────────
    let guest_mem = mmap_zeroed_region(GUEST_MEM_SIZE);
    unsafe {
        vm.set_user_memory_region(kvm_bindings::kvm_userspace_memory_region {
            slot: 0, guest_phys_addr: 0,
            memory_size: guest_mem.len() as u64,
            userspace_addr: guest_mem.as_ptr() as u64, flags: 0,
        }).expect("set_user_memory_region falló");
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
        carve_reserved_holes(HIGH_MEM_ADDR, HIGH_MEM_SIZE as u64, &KERNEL_IRQCHIP_HOLES)
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
        "[VMM] irqchip en kernel: LAPIC@0xFEE00000 e IOAPIC@0xFEC00000 reservados al kernel (sin memslot de RAM)"
    );

    // ─── VRAM: 16 MiB en GPA 0xE8000000 (offset 128MB de high_mem) ───
    const VRAM_SIZE: usize = 16 * 1024 * 1024;
    let vram_ptr = unsafe { high_mem.as_mut_ptr().add(128 * 1024 * 1024) };

        let (mut bus, vga_state) =
            match DeviceBus::new(iso_path, disk_path, vram_ptr, VRAM_SIZE, num_cpus) {
        Ok(res) => res,
        Err(e) => { eprintln!("[VMM] Error: {}", e); exit(1); }
    };
    // Connect DebugCon to guest memory for VGA text mirroring
    bus.debugcon.set_guest_mem(guest_mem.as_ptr());

    // ─── (tarea 6) Compartir el bus entre todos los vCPUs ──────
    // Los APs manejan sus propios exits de E/S contra los mismos
    // dispositivos; el Mutex serializa el acceso. (La condición Send de
    // DeviceBus se justifica en devices/mod.rs.)
    let bus = Arc::new(Mutex::new(bus));

    // ─── Cargar firmware ────────────────────────────────────────
    let mut bios = Vec::new();
    let bios_io = File::open(bios_path)
        .and_then(|mut f| f.read_to_end(&mut bios));
    if let Err(e) = bios_io {
        eprintln!("[VMM] No se pudo leer '{}': {}", bios_path, e);
        exit(1);
    }
    // Seguridad: el firmware se mapea en el tope de la ventana 0-1M (BIOS de 128/256 KB).
    // Si el archivo es más grande (p.ej. pasaron la ISO por accidente como primer
    // argumento), fallamos con un error claro en lugar de paniquear.
    const BIOS_MAX: usize = 512 * 1024;
    if bios.len() > BIOS_MAX {
        eprintln!(
            "[VMM] ERROR: '{}' tiene {} bytes (>{} KB).\n\
            \x20 Eso no es un firmware bios válido.\n\
            \x20 ¿Pasaste la ISO como primer argumento? Uso correcto:\n\
            \x20   ./run.sh [bios.bin] [imagen.iso]\n\
            \x20   ./target/release/mi-vmm /usr/share/seabios/bios-256k.bin /ruta/linuxmint.iso",
            bios_path, bios.len(), BIOS_MAX / 1024
        );
        exit(1);
    }
    let load_addr = bios_load_addr(bios.len());
    let load_off = load_addr as usize;
    guest_mem[load_off..load_off + bios.len()].copy_from_slice(&bios);
    eprintln!("[VMM] BIOS cargado en {:#x} ({} bytes)", load_addr, bios.len());

    let rv_off = 0xFFFF0usize;
    eprintln!("[VMM] Reset vector @0xFFFF0: {:02x?}", &guest_mem[rv_off..rv_off + 16]);
    let ep_phys = 0xFE05B_usize;
    if ep_phys + 16 <= guest_mem.len() {
        eprintln!("[VMM] BIOS entry @phys {:#x}: {:02x?}", ep_phys, &guest_mem[ep_phys..ep_phys + 16]);
    }

    let flash_offset_in_high = (0xFFFC_0000u64 - HIGH_MEM_ADDR) as usize;
    high_mem[flash_offset_in_high..flash_offset_in_high + bios.len()].copy_from_slice(&bios);
    eprintln!("[VMM] Flash BIOS en {:#x}", 0xFFFC_0000u64);

    // ─── Cargar VGA Option ROM (0xC0000) ─────────────────────────
    let vga_candidates = [
        "/usr/share/seabios/vgabios-stdvga.bin",
        "/usr/share/seabios/vgabios-bochs-display.bin",
        "/usr/share/qemu/vgabios-stdvga.bin",
        "/usr/share/seabios/vgabios.bin",
        "/usr/share/qemu/vgabios.bin",
    ];
    let mut vga_loaded = false;
    for vga_path in &vga_candidates {
        if let Ok(mut f) = File::open(vga_path) {
            let mut vga_rom = Vec::new();
            if f.read_to_end(&mut vga_rom).is_ok() {
                let vga_load_off = 0xC0000usize;
                if vga_load_off + vga_rom.len() <= guest_mem.len() {
                    guest_mem[vga_load_off..vga_load_off + vga_rom.len()].copy_from_slice(&vga_rom);
                    eprintln!("[VMM] VGA Option ROM cargado en 0xC0000 ({} bytes) desde {}", vga_rom.len(), vga_path);
                    vga_loaded = true;
                    break;
                }
            }
        }
    }
    if !vga_loaded {
        eprintln!("[VMM] Info: No se encontró archivo VGA ROM externo. Usando emulación gráfica integrada.");
    }

    // ─── LAPIC/IOAPIC (0xFEE00000 / 0xFEC00000) — tarea 7 ──────
    // Sin stubs: esas páginas quedaron sin memslot y el irqchip del kernel
    // responde con los registros REALES — APIC ID distinto por vCPU,
    // versión 0x11 con 24 IOREDENTRIES, ICR para INIT-SIPI (SMP), EOI,
    // LVT, IRR/ISR… Los bytes falsos que antes se preescribían en RAM
    // solo lograban tapar al irqchip real de KVM.

    // ─── VGA text buffer init (0xB8000) ─────────────────────────
    // Clear the VGA text buffer so SeaBIOS messages appear cleanly.
    {
        let vga_off = 0xB8000usize;
        let vga_end = vga_off + 80 * 25 * 2;
        if vga_end <= guest_mem.len() {
            guest_mem[vga_off..vga_end].fill(0x00);
            // Write a startup banner in the first row
            let banner = b"mi-vmm: waiting for BIOS...";
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
            guest_mem[ebda_off] = 0x01; // EBDA size in KB
            guest_mem[ebda_off + 0x30] = 0xC0;
            guest_mem[ebda_off + 0x31] = 0x9F;
        }

        eprintln!("[VMM] BDA inicializado: mem=640KB, kb_buf=head=tail=0x1E, EBDA=0x9FC0");
    }

    // ─── vCPU en modo real ─────────────────────────────────────
    let mut vcpu = vm.create_vcpu(0).expect("create_vcpu falló");
    let mut sregs: kvm_sregs = vcpu.get_sregs().expect("get_sregs falló");
    sregs.cs.base = (RESET_VECTOR_CS as u16 as u64) << 4;
    sregs.cs.selector = RESET_VECTOR_CS as u16;
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
    let mut ap_handles = Vec::new();
    for cpu_id in 1..num_cpus {
        let ap = vm.create_vcpu(cpu_id as u64).expect("create_vcpu (AP) falló");
        ap.set_mp_state(kvm_mp_state {
            mp_state: kvm_bindings::KVM_MP_STATE_INIT_RECEIVED,
        })
        .expect("set_mp_state (AP) falló");
        let bus_ap = Arc::clone(&bus);
        ap_handles.push(
            std::thread::Builder::new()
                .name(format!("vcpu-{cpu_id}"))
                .spawn(move || ap_vcpu_worker(cpu_id, ap, bus_ap, verbose))
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

    // ─── Start SIGALRM every 1ms for PIT timing ────────────────
    // This ensures PIT advances even when guest is stuck in CPU loop.
    start_periodic_timer(1);
    // Handle Ctrl+C / SIGTERM: dump VGA screen and diagnostics on exit
    install_signal_handler(libc::SIGTERM, shutdown_handler);
    install_signal_handler(libc::SIGINT, shutdown_handler);

    // ─── Expose guest memory to alarm handler for direct BDA tick ──

    // ─── Display GUI Manager ───────────────────────────────────
    let kbd_queue: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
    let _display = display::DisplayManager::start(
        vga_state,
        guest_mem.as_ptr(),
        kbd_queue.clone(),
    );

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
    // BDA tick counter emulation (18.2 Hz) — see EINTR handler below
    let mut ms_since_bda_tick: u32 = 0;
    let mut bda_ticks: u32 = 0;
    let mut total_reboots: u32 = 0;
    let mut last_post = 0u8;
    let mut last_cr0: u64 = 0x6000_0010; // Initial CR0 (real mode)
    let mut mode_transitions: u32 = 0;
    // I/O port access counters for diagnostics (Box to avoid 1MB on stack)
    let mut port_read_counts: Box<[u64; 65536]> = Box::new([0u64; 65536]);
    let mut port_write_counts: Box<[u64; 65536]> = Box::new([0u64; 65536]);
    // I/O trace ring buffer: last 100 operations (is_write, port, value)
    let mut io_trace: VecDeque<(u8, u16, u8)> = VecDeque::with_capacity(100);

    loop {
        // ── Shutdown solicitado (SIGTERM/SIGINT) ──────────────────
        if SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("[VMM] Shutdown solicitado — volcando estado final (exits={}, reboots={})...",
                total_exits, total_reboots);
            dump_vga_text_screen(guest_mem);
            exit(0);
        }
        // ── Input desde la ventana gráfica ───────────────────────
        if let Ok(mut q) = kbd_queue.try_lock() {
            while let Some(scancode) = q.pop_front() {
                bus_lock().legacy_irq.inject_scancode(scancode);
            }
        }

      // ── Si SIGALRM fired, advance PIT by the number of ms elapsed ──
        // Using fetch_swap on a counter so we don't lose ticks when
        // multiple alarms fire between loop iterations.
        // BDA tick counter (0x46C) must be incremented at 18.2 Hz so
        // SeaBIOS's wait_ms() polling loop can unblock.
        let alarm_ticks = ALARM_TICKS.swap(0, Ordering::Relaxed);
        if alarm_ticks > 0 {
            // Advance PIT channel 0 for accuracy (SeaBIOS may read it).
            // (tarea 4) Si hay underflow hay que pulsar IRQ0 AQUÍ también:
            // antes solo se pulsaba en la rama EINTR y un tick contado
            // mientras no estábamos dentro de vcpu.run() se quedaba sin
            // interrupción.
            if bus_lock().legacy_irq.pit_advance_ticks(alarm_ticks * 1193) {
                vm.set_irq_line(0, false).ok();
                vm.set_irq_line(0, true).ok();
                irq0_raised = true;
            }
            // Increment BDA tick counter at 18.2 Hz (1 tick every ~55ms)
            ms_since_bda_tick += alarm_ticks;
            while ms_since_bda_tick >= 55 {
                ms_since_bda_tick -= 55;
                bda_ticks = bda_ticks.wrapping_add(1);
                let tick_ptr = unsafe { guest_mem.as_ptr().add(BDA_TICK_ADDR) } as *mut u32;
                unsafe { std::ptr::write_volatile(tick_ptr, bda_ticks); }
            }
            // (tarea 4) Ya NO se baja IRQ0 incondicionalmente aquí: eso
            // hacía pic_set_irq1() limpiar el IRR de un tick pendiente no
            // entregado (guest con IF=0) y el tick se perdía.
        }

        // ── Progreso periódico ──────────────────────────────────
        {
            let now = std::time::Instant::now();
            if now.duration_since(last_report) >= std::time::Duration::from_secs(5) {
                last_report = now;
                if let Ok(r) = vcpu.get_regs() {
                    if let Ok(s) = vcpu.get_sregs() {
                        let phys = s.cs.base + r.rip;
                        let p = phys as usize;
                        let code_dump = if p + 16 <= guest_mem.len() {
                            format!("{:02x?}", &guest_mem[p..p+16])
                        } else { String::from("???") };
                        // Dump stack top 32 bytes (SS:ESP in real mode)
                        let esp = r.rsp as usize;
                        let ss_base = s.ss.base as usize;
                        let stack_phys = ss_base + esp;
                        let stack_dump = if stack_phys + 32 <= guest_mem.len() {
                            format!("{:02x?}", &guest_mem[stack_phys..stack_phys+32])
                        } else { String::from("???") };
                        eprintln!(
                            "[VMM] {} exits (reboots={}) | POST=0x{:02X} phys={:#x} cr0={:#x} rflags={:#x} idt={:#x}/{:#x}\n  code: {}\n  stack(S:{:04x} P:{:#x}): {}",
                            total_exits, total_reboots, bus_lock().post.last, phys, s.cr0, r.rflags, s.idt.base, s.idt.limit, code_dump,
                            s.ss.selector, stack_phys, stack_dump
                        );
                        eprintln!("  PS2 access={}, PIT access={}, mode_transitions={}",
                            bus_lock().legacy_irq.ps2_access_count,
                            bus_lock().legacy_irq.pit_access_count,
                            mode_transitions);
                        // Dump segment registers and BDA keyboard buffer
                        eprintln!("  DS={:04x} ES={:04x} FS={:04x} GS={:04x} SS={:04x} CS={:04x}",
                            s.ds.selector, s.es.selector, s.fs.selector, s.gs.selector, s.ss.selector, s.cs.selector);
                        let bda_tick_val = guest_mem[BDA_TICK_ADDR] as u32
                            | ((guest_mem[BDA_TICK_ADDR+1] as u32) << 8)
                            | ((guest_mem[BDA_TICK_ADDR+2] as u32) << 16)
                            | ((guest_mem[BDA_TICK_ADDR+3] as u32) << 24);
                        eprintln!("  BDA tick@0x46C={:08x} (written={}) kbd: head@0x41A={:04x} tail@0x41C={:04x}",
                            bda_tick_val, bda_ticks,
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
                        // Top I/O ports by access count
                        let mut ports: Vec<(u16, u64)> = Vec::new();
                        for p in 0u16..=65534u16 {
                            let total = port_read_counts[p as usize] + port_write_counts[p as usize];
                            if total > 0 { ports.push((p, total)); }
                        }
                        ports.sort_by(|a, b| b.1.cmp(&a.1));
                        let top_ports: Vec<String> = ports.iter().take(10)
                            .map(|(p, c)| format!("0x{:X}:{}", p, c))
                            .collect();
                        eprintln!("  Top I/O: {}", top_ports.join(", "));
                        // Dump code at the return address (caller on stack)
                        if stack_phys + 4 <= guest_mem.len() {
                            let ret_addr = guest_mem[stack_phys] as u32
                                | ((guest_mem[stack_phys + 1] as u32) << 8)
                                | ((guest_mem[stack_phys + 2] as u32) << 16)
                                | ((guest_mem[stack_phys + 3] as u32) << 24);
                            let ra = ret_addr as usize;
                            if ra + 48 <= guest_mem.len() {
                                eprintln!("  Caller at {:#x}: {:02x?}", ra, &guest_mem[ra..ra+48]);
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

        // ── Detectar cambio de POST ─────────────────────────────
        let cur_post = bus_lock().post.last;
        if cur_post != last_post {
            let r = vcpu.get_regs().unwrap_or_default();
            let s = vcpu.get_sregs().unwrap_or_default();
            eprintln!("[VMM] POST: 0x{:02X} → 0x{:02X} (phys={:#x})",
                last_post, cur_post, s.cs.base + r.rip);
            last_post = cur_post;
        }

        // ── Detectar cambio de CR0 (modo real ↔ protegido) ────
        // KVM manages CR0.PE (Protected Enable) and segment registers
        // via VMCS — no shadow tracking needed. We log transitions
        // for diagnostics only.
        if total_exits % 50000 == 0 {
            if let Ok(s) = vcpu.get_sregs() {
                if s.cr0 != last_cr0 {
                    mode_transitions += 1;
                    let pe_old = (last_cr0 & 1) != 0;
                    let pe_new = (s.cr0 & 1) != 0;
                    let r = vcpu.get_regs().unwrap_or_default();
                    eprintln!("[VMM] CR0 change: {:#x} → {:#x} (PE {}→{}) at phys={:#x} gdt={:#x}/{:#x} idt={:#x}/{:#x}",
                        last_cr0, s.cr0, pe_old, pe_new,
                        s.cs.base + r.rip, s.gdt.base, s.gdt.limit, s.idt.base, s.idt.limit);
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
        // (tormenta de IrqWindowOpen) y el guest nunca ejecutaría.
        {
            let irq_pending = irq0_raised || bus_lock().legacy_irq.ps2_has_data();
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
        let exit_reason = match vcpu.run() {
            Ok(r) => r,
            Err(e) if e.errno() == libc::EINTR => {
                // SIGALRM interrupted vcpu.run(). Tick PIT and inject IRQ0.
                // BDA tick (0x46C) is incremented by SeaBIOS's INT 08h handler
                // once the IRQ0 is delivered — we must NOT write it ourselves.
                let alarm_ticks = ALARM_TICKS.swap(0, Ordering::Relaxed);
                if alarm_ticks > 0 {
                    // BDA tick counter (0x46C): SeaBIOS's wait loops depend on
                    // it. The PIT drives IRQ0 → INT 08h → tick increment, but
                    // SeaBIOS may never program the PIT (it uses the PM Timer
                    // instead). To keep the tick counter alive in all cases,
                    // increment it here at the hardware rate (18.2065 Hz =
                    // one tick every ~54.9 ms of PIT ticks at 1.193 MHz).
                    ms_since_bda_tick += alarm_ticks;
                    while ms_since_bda_tick >= 55 {
                        ms_since_bda_tick -= 55;
                        bda_ticks = bda_ticks.wrapping_add(1);
                        let tick_ptr = unsafe { guest_mem.as_ptr().add(BDA_TICK_ADDR) } as *mut u32;
                        unsafe { std::ptr::write_volatile(tick_ptr, bda_ticks); }
                    }
                    let irq0_fired = bus_lock().legacy_irq.pit_advance_ticks(alarm_ticks * 1193);
                    if irq0_fired {
                        // Pulse IRQ0 into the kernel PIC. La bajada es
                        // segura: el raise inmediato vuelve a fijar IRR,
                        // así que un tick pendiente no entregado nunca se
                        // pierde aunque el guest tenga IF=0.
                        vm.set_irq_line(0, false).ok();
                        vm.set_irq_line(0, true).ok();
                        irq0_raised = true;
                    }
                    // (tarea 4) Ya NO bajamos la línea cuando no hay
                    // underflow nuevo: eso limpiaba el IRR de un tick
                    // pendiente sin entregar y el tick se perdía.
                }
                // Also inject IRQ1 if the 8042 has undelivered data.
                // SIEMPRE pulsamos (bajar→subir) para generar un flanco
                // fresco aunque la línea ya estuviera alta: un PIC
                // edge-triggered no reintenta sin flanco de subida.
                if bus_lock().take_ps2_irq() {
                    vm.set_irq_line(1, false).ok();
                    vm.set_irq_line(1, true).ok();
                }
                continue;
            }
            Err(e) => { eprintln!("[VMM] vcpu.run() falló: {}", e); exit(1); }
        };
        total_exits += 1;

       // NOTE: PIT is only advanced via SIGALRM (alarm_ticks above).
        // We do NOT call pit_tick() here to avoid double-counting.
        // The PIT is driven by the real-time SIGALRM, not by exit counting.

        // Inject IRQ1 into the kernel PIC when the 8042 has undelivered
        // data. Lower→raise pulse: garantiza un flanco fresco por byte
        // pendiente (antes se subía la línea sin bajarla y, si ya estaba
        // alta, la interrupción se perdía silenciosamente).
        if bus_lock().take_ps2_irq() {
            vm.set_irq_line(1, false).ok();
            vm.set_irq_line(1, true).ok();
            irq1_line_high = true;
        }
        // (tarea 4) Disciplina de nivel para IRQ1: si el guest consumió
        // todo el output buffer, la línea puede bajar con seguridad — la
        // interrupción que la subió ya se entregó y atendió. Bajarla aquí
        // devuelve la línea a un estado limpio para el siguiente flanco.
        if !bus_lock().legacy_irq.ps2_has_data() && irq1_line_high {
            vm.set_irq_line(1, false).ok();
            irq1_line_high = false;
        }

        match exit_reason {
            VcpuExit::IoOut(port, data) => {
                port_write_counts[port as usize] += 1;
                if !bus_lock().out(port, data) {
                    eprintln!("[VMM] OUT no manejado: 0x{:X} <- {:02X?}", port, data);
                }
                // Hardware reset register (PCI/PIIX): 0xCF9
                // 0x02 = soft reset, 0x04 = hard reset, 0x06 = full reset
                if port == 0xCF9 && data.iter().any(|&v| v & 0x04 != 0) {
                    eprintln!("[VMM] Reset via puerto 0xCF9 (data={:02X?}) — reiniciando guest...", data);
                    reset_vcpu_to_post(&vcpu);
                    total_reboots += 1;
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
                if verbose {
                    recent.push_back(format!("OUT 0x{:X} <- {:02X?}", port, data));
                    if recent.len() > 128 { recent.pop_front(); }
                }
                if io_trace.len() >= 100 { io_trace.pop_front(); }
                io_trace.push_back((1, port, data[0]));
            }
            VcpuExit::IoIn(port, data) => {
                port_read_counts[port as usize] += 1;
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
                }
                if io_trace.len() >= 100 { io_trace.pop_front(); }
                io_trace.push_back((0, port, data[0]));
            }
            VcpuExit::Hlt => {
                let r = vcpu.get_regs().unwrap_or_default();
                let s = vcpu.get_sregs().unwrap_or_default();
                eprintln!("[VMM] HLT — phys={:#x} rflags={:#x}", s.cs.base + r.rip, r.rflags);
            }
            VcpuExit::Shutdown => {
                eprintln!("[VMM] === TRIPLE FAULT ===");
                dump_crash_state(&vcpu, &guest_mem, &recent, total_exits);
                crash_reboot(&vcpu, &mut total_reboots);
                if total_reboots >= MAX_CRASH_REBOOTS { break; }
            }
            VcpuExit::IrqWindowOpen => {
                // (tarea 4) El guest abrió la ventana (IF=1) con
                // request_interrupt_window activo. Con irqchip en el kernel,
                // KVM inyecta él mismo los IRR pendientes en la próxima
                // entrada a la VM: no hay que inyectar nada a mano (el
                // antiguo código consultaba el PIC emulado de usuariospace,
                // que nunca tenía IRQ0/IRQ1 marcados → siempre no-op).
                // Solo queda consumir pulsos nuevos del 8042 y dar por
                // entregado el marcador de IRQ0.
                irq0_raised = false;
                if bus_lock().take_ps2_irq() {
                    vm.set_irq_line(1, false).ok();
                    vm.set_irq_line(1, true).ok();
                    irq1_line_high = true;
                }
            }
            VcpuExit::MmioWrite(addr, _data) => {
                if verbose { eprintln!("[VMM] MMIO W {:#x}", addr); }
            }
            VcpuExit::MmioRead(addr, data) => {
                if verbose { eprintln!("[VMM] MMIO R {:#x}", addr); }
                data.fill(0xFF);
            }
            VcpuExit::InternalError => {
                // KVM no pudo emular una instrucción (p. ej. INT en modo
                // protegido con estado indefinido tras un probe fallido).
                // En hardware real esto equivaldría a un fault del CPU:
                // reseteamos el guest (como un 0xCF9) en vez de matar la VM.
                let r = vcpu.get_regs().unwrap_or_default();
                let s = vcpu.get_sregs().unwrap_or_default();
                eprintln!("[VMM] KVM InternalError phys={:#x}", s.cs.base + r.rip);
                dump_crash_state(&vcpu, &guest_mem, &recent, total_exits);
                crash_reboot(&vcpu, &mut total_reboots);
                if total_reboots >= MAX_CRASH_REBOOTS { break; }
            }
            other => {
                let r = vcpu.get_regs().unwrap_or_default();
                let s = vcpu.get_sregs().unwrap_or_default();
                eprintln!("[VMM] Exit no manejado: {:?} phys={:#x}", other, s.cs.base + r.rip);
                break;
            }
        }
    }
}

/// Bucle de un vCPU AP (tarea 6: SMP).
///
/// El AP nace aparcado en KVM_MP_STATE_INIT_RECEIVED: `run()` se bloquea
/// en el kernel hasta que el guest le entrega INIT-SIPI por el LAPIC (el
/// irqchip del kernel lo maneja entero, incluido fijar CS:IP = vector<<4
/// en modo real). Desde ahí ejecuta el guest igual que el BSP pero con
/// una política de exits más simple:
///   - El temporizador (PIT/IRQ0/tick del BDA) lo gobierna SOLO el BSP:
///     si el SIGALRM interrumpe este hilo (EINTR) se reintenta sin tocar
///     nada.
///   - Un triple fault en un AP solo "resetea" ese CPU: se re-aparca en
///     wait-for-SIPI y el siguiente INIT-SIPI del guest lo trae de vuelta
///     (en hardware real un triple fault de un AP no apaga la caja).
fn ap_vcpu_worker(
    cpu_id: u32,
    vcpu: kvm_ioctls::VcpuFd,
    bus: Arc<Mutex<DeviceBus>>,
    verbose: bool,
) {
    let mut total_exits: u64 = 0;
    let park = |vcpu: &kvm_ioctls::VcpuFd| {
        vcpu.set_mp_state(kvm_mp_state {
            mp_state: kvm_bindings::KVM_MP_STATE_INIT_RECEIVED,
        })
        .ok();
    };
    loop {
        let exit_reason = match vcpu.run() {
            Ok(r) => r,
            // Señal (SIGALRM/SIGINT/SIGTERM): el temporizador lo gobierna
            // el BSP; aquí solo reintentamos.
            Err(e) if e.errno() == libc::EINTR => continue,
            Err(e) => {
                eprintln!("[AP{}] vcpu.run() falló: {}", cpu_id, e);
                return;
            }
        };
        total_exits += 1;
        match exit_reason {
            VcpuExit::IoOut(port, data) => {
                let handled =
                    bus.lock().unwrap_or_else(|p| p.into_inner()).out(port, data);
                if !handled {
                    eprintln!("[AP{}] OUT no manejado: 0x{:X} <- {:02X?}", cpu_id, port, data);
                }
            }
            VcpuExit::IoIn(port, data) => {
                match bus.lock().unwrap_or_else(|p| p.into_inner()).input(port, data.len()) {
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
                // STI;HLT del guest (p. ej. el bucle de paro de los APs de
                // SeaBIOS). Con irqchip en el kernel, KVM bloquea dentro de
                // run() hasta una interrupción o SIPI: no hay nada que hacer.
                if verbose {
                    eprintln!("[AP{}] HLT (exits={})", cpu_id, total_exits);
                }
            }
            VcpuExit::Intr => {
                // KVM_EXIT_INTR: ejecución interrumpida por evento interno.
                // Benigno: reintentar (aparcar aquí rompería el SMP).
            }
            VcpuExit::IrqWindowOpen => {
                // La ventana de interrupciones solo la pide el BSP.
            }
            VcpuExit::Shutdown | VcpuExit::InternalError => {
                eprintln!(
                    "[AP{}] triple fault/InternalError — AP re-aparcado en wait-for-SIPI",
                    cpu_id
                );
                park(&vcpu);
            }
            VcpuExit::MmioRead(_addr, data) => {
                data.fill(0xFF);
            }
            VcpuExit::MmioWrite(_addr, _data) => {
                // MMIO sin enrutar (tarea 1): misma política que el BSP.
            }
            other => {
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
    let phys = sregs.cs.base + regs.rip;
    eprintln!("[VMM] Crash: rip={:#x} cs={:#x} base={:#x} cr0={:#x} rflags={:#x}",
        regs.rip, sregs.cs.selector, sregs.cs.base, sregs.cr0, regs.rflags);
    eprintln!("  phys={:#x} idt={:#x}/{:#x} gdt={:#x}/{:#x} exits={}",
        phys, sregs.idt.base, sregs.idt.limit, sregs.gdt.base, sregs.gdt.limit, total_exits);
    let p = phys as usize;
    if p + 32 <= guest_mem.len() {
        eprintln!("  Code: {:02x?}", &guest_mem[p..p + 32]);
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
}
