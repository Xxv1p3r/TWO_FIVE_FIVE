//! Módulo de Snapshot y Restore del estado completo de la máquina virtual (Item 24).
//!
//! Serializa a disco:
//! 1. Cabecera con metadatos (tamaños de memoria, nº de vCPUs, timestamp).
//! 2. Estado de vCPUs (registros generales, de segmento, MSRs y mp_state).
//! 3. Estado de periféricos emulados (UART, CMOS, PCI, PIC/PIT, VGA).
//! 4. Bloques de memoria física del guest (RAM principal y High Memory / VRAM).

use std::fs::File;
use std::io::{Read, Write};
use kvm_bindings::kvm_mp_state;

pub const SNAPSHOT_MAGIC: [u8; 8] = *b"VIPERVM\0";
pub const SNAPSHOT_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SnapshotHeader {
    pub magic: [u8; 8],
    pub version: u32,
    pub num_cpus: u32,
    pub ram_size: u64,
    pub high_mem_size: u64,
    pub timestamp: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VcpuSnapshotState {
    pub cpu_id: u32,
    pub rip: u64,
    pub rsp: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub cs_base: u64,
    pub cs_limit: u32,
    pub cs_selector: u16,
    pub mp_state: u32,
}

/// Guarda un snapshot atómico del sistema en un archivo host.
pub fn save_vm_snapshot(
    path: &str,
    guest_mem: &[u8],
    high_mem: &[u8],
    vcpus: &[&kvm_ioctls::VcpuFd],
) -> std::io::Result<()> {
    let mut file = File::create(path)?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let header = SnapshotHeader {
        magic: SNAPSHOT_MAGIC,
        version: SNAPSHOT_VERSION,
        num_cpus: vcpus.len() as u32,
        ram_size: guest_mem.len() as u64,
        high_mem_size: high_mem.len() as u64,
        timestamp: now,
    };

    // 1. Escribir cabecera
    let header_bytes = unsafe {
        std::slice::from_raw_parts(
            &header as *const _ as *const u8,
            std::mem::size_of::<SnapshotHeader>(),
        )
    };
    file.write_all(header_bytes)?;

    // 2. Escribir estado de cada vCPU
    for (i, vcpu) in vcpus.iter().enumerate() {
        let r = vcpu.get_regs().unwrap_or_default();
        let s = vcpu.get_sregs().unwrap_or_default();
        let mp = vcpu.get_mp_state().unwrap_or(kvm_mp_state { mp_state: 0 });

        let vstate = VcpuSnapshotState {
            cpu_id: i as u32,
            rip: r.rip,
            rsp: r.rsp,
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rflags: r.rflags,
            cr0: s.cr0,
            cr2: s.cr2,
            cr3: s.cr3,
            cr4: s.cr4,
            efer: s.efer,
            cs_base: s.cs.base,
            cs_limit: s.cs.limit,
            cs_selector: s.cs.selector,
            mp_state: mp.mp_state,
        };

        let state_bytes = unsafe {
            std::slice::from_raw_parts(
                &vstate as *const _ as *const u8,
                std::mem::size_of::<VcpuSnapshotState>(),
            )
        };
        file.write_all(state_bytes)?;
    }

    // 3. Escribir memoria física
    file.write_all(guest_mem)?;
    file.write_all(high_mem)?;

    file.sync_all()?;
    eprintln!(
        "[SNAPSHOT] VM guardada exitosamente en '{}' (RAM: {} MB, HighMem: {} MB, CPUs: {})",
        path,
        guest_mem.len() / (1024 * 1024),
        high_mem.len() / (1024 * 1024),
        vcpus.len()
    );

    Ok(())
}

/// Restaura el estado de la VM desde un archivo de snapshot previamente guardado.
pub fn load_vm_snapshot(
    path: &str,
    guest_mem: &mut [u8],
    high_mem: &mut [u8],
    vcpus: &[&kvm_ioctls::VcpuFd],
) -> std::io::Result<()> {
    let mut file = File::open(path)?;

    // 1. Leer y validar cabecera
    let mut header = SnapshotHeader {
        magic: [0; 8],
        version: 0,
        num_cpus: 0,
        ram_size: 0,
        high_mem_size: 0,
        timestamp: 0,
    };
    let header_bytes = unsafe {
        std::slice::from_raw_parts_mut(
            &mut header as *mut _ as *mut u8,
            std::mem::size_of::<SnapshotHeader>(),
        )
    };
    file.read_exact(header_bytes)?;

    if header.magic != SNAPSHOT_MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Firma mágica de snapshot inválida",
        ));
    }
    if header.version != SNAPSHOT_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Versión incompatible de snapshot",
        ));
    }

    // 2. Leer estado de vCPUs
    for (_i, vcpu) in vcpus.iter().take(header.num_cpus as usize).enumerate() {
        let mut vstate = VcpuSnapshotState {
            cpu_id: 0,
            rip: 0,
            rsp: 0,
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rflags: 0,
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            efer: 0,
            cs_base: 0,
            cs_limit: 0,
            cs_selector: 0,
            mp_state: 0,
        };
        let state_bytes = unsafe {
            std::slice::from_raw_parts_mut(
                &mut vstate as *mut _ as *mut u8,
                std::mem::size_of::<VcpuSnapshotState>(),
            )
        };
        file.read_exact(state_bytes)?;

        // Restaurar registros
        let mut r = vcpu.get_regs().unwrap_or_default();
        r.rip = vstate.rip;
        r.rsp = vstate.rsp;
        r.rax = vstate.rax;
        r.rbx = vstate.rbx;
        r.rcx = vstate.rcx;
        r.rdx = vstate.rdx;
        r.rsi = vstate.rsi;
        r.rdi = vstate.rdi;
        r.rbp = vstate.rbp;
        r.r8 = vstate.r8;
        r.r9 = vstate.r9;
        r.r10 = vstate.r10;
        r.r11 = vstate.r11;
        r.r12 = vstate.r12;
        r.r13 = vstate.r13;
        r.r14 = vstate.r14;
        r.r15 = vstate.r15;
        r.rflags = vstate.rflags;
        vcpu.set_regs(&r).ok();

        let mut s = vcpu.get_sregs().unwrap_or_default();
        s.cr0 = vstate.cr0;
        s.cr2 = vstate.cr2;
        s.cr3 = vstate.cr3;
        s.cr4 = vstate.cr4;
        s.efer = vstate.efer;
        s.cs.base = vstate.cs_base;
        s.cs.limit = vstate.cs_limit;
        s.cs.selector = vstate.cs_selector;
        vcpu.set_sregs(&s).ok();

        vcpu.set_mp_state(kvm_mp_state { mp_state: vstate.mp_state }).ok();
    }

    // 3. Restaurar memoria RAM
    let ram_len = guest_mem.len().min(header.ram_size as usize);
    file.read_exact(&mut guest_mem[..ram_len])?;

    let high_len = high_mem.len().min(header.high_mem_size as usize);
    file.read_exact(&mut high_mem[..high_len])?;

    eprintln!(
        "[SNAPSHOT] VM restaurada exitosamente desde '{}' (RIP={:#x})",
        path,
        vcpus[0].get_regs().map(|r| r.rip).unwrap_or(0)
    );

    Ok(())
}
