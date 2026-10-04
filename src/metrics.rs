//! Módulo de telemetría, métricas de VM-Exits y profiling de puertos I/O (Item 24).
//!
//! Rastrea en tiempo real:
//! - Total de salidas desglosadas por tipo (I/O, MMIO, HLT, IRQ Window, etc.).
//! - Frecuencia de acceso por puerto I/O (hot ports).
//! - Inyecciones de interrupciones (IRQ0, IRQ1, IRQ4, IRQ12).
//! - Formateo estructurado para salida periódica y TUI en consola.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

pub struct VmmMetrics {
    pub exits_total: AtomicU64,
    pub exits_io_in: AtomicU64,
    pub exits_io_out: AtomicU64,
    pub exits_mmio_read: AtomicU64,
    pub exits_mmio_write: AtomicU64,
    pub exits_hlt: AtomicU64,
    pub exits_irq_window: AtomicU64,
    pub exits_intr: AtomicU64,
    pub exits_shutdown: AtomicU64,
    pub exits_internal_error: AtomicU64,
    pub exits_other: AtomicU64,

    // Conteo individual por puerto I/O (64k puertos)
    pub port_in_counts: Box<[AtomicU64; 65536]>,
    pub port_out_counts: Box<[AtomicU64; 65536]>,

    // Interrupciones inyectadas por línea (0-15)
    pub irq_counts: [AtomicU64; 16],

    // Telemetría por vCPU (soporta hasta 16 vCPUs)
    pub vcpu_exits: [AtomicU64; 16],
    pub vcpu_active_nanos: [AtomicU64; 16],
    pub vcpu_usage_pct: [AtomicU32; 16],
    pub num_cpus: AtomicU32,
    pub max_cpus: AtomicU32,

    // Memoria
    pub ram_bytes: AtomicU64,
    pub high_mem_bytes: AtomicU64,

    // Almacenamiento
    pub cdrom_sectors_read: AtomicU64,
    pub disk_sectors_read: AtomicU64,
    pub disk_sectors_written: AtomicU64,

    // Estado de VM
    pub is_paused: AtomicBool,

    // Pantalla / Resolución
    pub display_width: AtomicU32,
    pub display_height: AtomicU32,
    pub display_bpp: AtomicU32,
    pub display_fps: AtomicU32,
    pub display_scale: AtomicU32,
    pub is_vbe: AtomicBool,
    pub lfb_gpa: AtomicU64,
}

impl Default for VmmMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl VmmMetrics {
    pub fn new() -> Self {
        let mut in_vec = Vec::with_capacity(65536);
        let mut out_vec = Vec::with_capacity(65536);
        for _ in 0..65536 {
            in_vec.push(AtomicU64::new(0));
            out_vec.push(AtomicU64::new(0));
        }

        let boxed_in: Box<[AtomicU64; 65536]> = match in_vec.into_boxed_slice().try_into() {
            Ok(b) => b,
            Err(_) => unreachable!(),
        };
        let boxed_out: Box<[AtomicU64; 65536]> = match out_vec.into_boxed_slice().try_into() {
            Ok(b) => b,
            Err(_) => unreachable!(),
        };

        const INIT_ZERO_64: AtomicU64 = AtomicU64::new(0);
        const INIT_ZERO_32: AtomicU32 = AtomicU32::new(0);
        Self {
            exits_total: AtomicU64::new(0),
            exits_io_in: AtomicU64::new(0),
            exits_io_out: AtomicU64::new(0),
            exits_mmio_read: AtomicU64::new(0),
            exits_mmio_write: AtomicU64::new(0),
            exits_hlt: AtomicU64::new(0),
            exits_irq_window: AtomicU64::new(0),
            exits_intr: AtomicU64::new(0),
            exits_shutdown: AtomicU64::new(0),
            exits_internal_error: AtomicU64::new(0),
            exits_other: AtomicU64::new(0),
            port_in_counts: boxed_in,
            port_out_counts: boxed_out,
            irq_counts: [INIT_ZERO_64; 16],
            vcpu_exits: [INIT_ZERO_64; 16],
            vcpu_active_nanos: [INIT_ZERO_64; 16],
            vcpu_usage_pct: [INIT_ZERO_32; 16],
            num_cpus: AtomicU32::new(1),
            max_cpus: AtomicU32::new(8),
            ram_bytes: AtomicU64::new(0),
            high_mem_bytes: AtomicU64::new(0),
            cdrom_sectors_read: AtomicU64::new(0),
            disk_sectors_read: AtomicU64::new(0),
            disk_sectors_written: AtomicU64::new(0),
            is_paused: AtomicBool::new(false),
            display_width: AtomicU32::new(640),
            display_height: AtomicU32::new(480),
            display_bpp: AtomicU32::new(32),
            display_fps: AtomicU32::new(60),
            display_scale: AtomicU32::new(2),
            is_vbe: AtomicBool::new(false),
            lfb_gpa: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn record_io_in(&self, port: u16) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_io_in.fetch_add(1, Ordering::Relaxed);
        self.port_in_counts[port as usize].fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_io_out(&self, port: u16) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_io_out.fetch_add(1, Ordering::Relaxed);
        self.port_out_counts[port as usize].fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_mmio_read(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_mmio_read.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_mmio_write(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_mmio_write.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_hlt(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_hlt.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_irq_window(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_irq_window.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_intr(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_intr.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_shutdown(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_shutdown.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_internal_error(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_internal_error.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_other(&self) {
        self.exits_total.fetch_add(1, Ordering::Relaxed);
        self.exits_other.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_irq(&self, line: u8) {
        if (line as usize) < self.irq_counts.len() {
            self.irq_counts[line as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_vcpu_exit(&self, cpu_id: usize) {
        if cpu_id < self.vcpu_exits.len() {
            self.vcpu_exits[cpu_id].fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_vcpu_active(&self, cpu_id: usize, nanos: u64) {
        if cpu_id < self.vcpu_active_nanos.len() {
            self.vcpu_active_nanos[cpu_id].fetch_add(nanos, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn set_vcpu_usage(&self, cpu_id: usize, pct: u32) {
        if cpu_id < self.vcpu_usage_pct.len() {
            self.vcpu_usage_pct[cpu_id].store(pct.min(100), Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn record_cdrom_read(&self, sectors: u64) {
        self.cdrom_sectors_read.fetch_add(sectors, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_disk_read(&self, sectors: u64) {
        self.disk_sectors_read.fetch_add(sectors, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_disk_write(&self, sectors: u64) {
        self.disk_sectors_written.fetch_add(sectors, Ordering::Relaxed);
    }

    /// Obtiene los N puertos I/O con mayor tráfico acumulado (IN + OUT).
    pub fn top_ports(&self, top_n: usize) -> Vec<(u16, u64, u64)> {
        let mut active: Vec<(u16, u64, u64)> = Vec::new();
        for port in 0..=65535u16 {
            let ins = self.port_in_counts[port as usize].load(Ordering::Relaxed);
            let outs = self.port_out_counts[port as usize].load(Ordering::Relaxed);
            let sum = ins + outs;
            if sum > 0 {
                active.push((port, ins, outs));
            }
        }
        active.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
        active.truncate(top_n);
        active
    }

    /// Nombre descriptivo del dispositivo asignado a un puerto conocido.
    pub fn port_name(port: u16) -> &'static str {
        match port {
            0x20 | 0x21 => "PIC Master (8259A)",
            0x40..=0x43 => "PIT Timer (8254)",
            0x60 | 0x64 => "PS/2 Kbd/Mouse (8042)",
            0x70 | 0x71 => "RTC / CMOS (MC146818)",
            0x80 => "POST Code Port",
            0x92 => "Fast A20 Gate / Reset",
            0xA0 | 0xA1 => "PIC Slave (8259A)",
            0xB2 | 0xB3 => "APM Control / SMI Trigger",
            0x1CE | 0x1CF => "Bochs/QEMU VBE Registers",
            0x1F0..=0x1F7 | 0x3F6 => "Primary IDE / ATAPI (CD-ROM)",
            0x170..=0x177 | 0x376 => "Secondary IDE",
            0x3B4..=0x3B5 | 0x3D4..=0x3D5 => "VGA CRTC Registers",
            0x3C0..=0x3C9 | 0x3DA => "VGA Sequencer/DAC/Status",
            0x3F8..=0x3FF => "COM1 UART (16550A)",
            0x2F8..=0x2FF => "COM2 UART (16550A)",
            0x402 => "QEMU DebugCon Port",
            0x510 | 0x511 => "QEMU fw_cfg (I/O)",
            0x600..=0x60B => "PIIX4 ACPI Power Management",
            0x0CD8..=0x0CDF => "ACPI CPU Hotplug Interface",
            0xCF8 | 0xCFC => "PCI Configuration Space (0xCF8/CFC)",
            0xCF9 => "PCI Reset Control Register",
            _ => "Dispositivo I/O desconocido",
        }
    }

    /// Genera un panel de texto estructurado estilo TUI para log periódico.
    pub fn format_summary(&self, delta_seconds: f64, prev_total: u64) -> String {
        let total = self.exits_total.load(Ordering::Relaxed);
        let hlt = self.exits_hlt.load(Ordering::Relaxed);
        let io_in = self.exits_io_in.load(Ordering::Relaxed);
        let io_out = self.exits_io_out.load(Ordering::Relaxed);
        let mm_r = self.exits_mmio_read.load(Ordering::Relaxed);
        let mm_w = self.exits_mmio_write.load(Ordering::Relaxed);
        let irq_w = self.exits_irq_window.load(Ordering::Relaxed);

        let delta_exits = total.saturating_sub(prev_total);
        let rate = if delta_seconds > 0.0 {
            delta_exits as f64 / delta_seconds
        } else {
            0.0
        };

        let pct = |v: u64| {
            if total > 0 {
                (v as f64 / total as f64) * 100.0
            } else {
                0.0
            }
        };

        let mut out = String::new();
        out.push_str(&format!(
            "[METRICS] Exits: {} (+{}/s) | HLT: {:.1}% | I/O: {:.1}% (IN: {}, OUT: {}) | MMIO: {:.1}% | IRQ-win: {}\n",
            total,
            rate as u64,
            pct(hlt),
            pct(io_in + io_out),
            io_in,
            io_out,
            pct(mm_r + mm_w),
            irq_w
        ));

        let top = self.top_ports(5);
        if !top.is_empty() {
            out.push_str("          Top Hot Ports:\n");
            for (p, ins, outs) in top {
                out.push_str(&format!(
                    "            - 0x{:04X} (IN: {:6}, OUT: {:6}) -> {}\n",
                    p, ins, outs, Self::port_name(p)
                ));
            }
        }

        let irq0 = self.irq_counts[0].load(Ordering::Relaxed);
        let irq1 = self.irq_counts[1].load(Ordering::Relaxed);
        let irq4 = self.irq_counts[4].load(Ordering::Relaxed);
        let irq12 = self.irq_counts[12].load(Ordering::Relaxed);
        out.push_str(&format!(
            "          IRQs inyectadas: PIT(IRQ0)={} | KBD(IRQ1)={} | UART(IRQ4)={} | MOUSE(IRQ12)={}\n",
            irq0, irq1, irq4, irq12
        ));

        out
    }

    /// Formatea un snapshot de métricas como JSON estructurado.
    #[allow(dead_code)]
    pub fn to_json(&self) -> String {
        let total = self.exits_total.load(Ordering::Relaxed);
        let hlt = self.exits_hlt.load(Ordering::Relaxed);
        let io_in = self.exits_io_in.load(Ordering::Relaxed);
        let io_out = self.exits_io_out.load(Ordering::Relaxed);
        let mm_r = self.exits_mmio_read.load(Ordering::Relaxed);
        let mm_w = self.exits_mmio_write.load(Ordering::Relaxed);
        let irq_w = self.exits_irq_window.load(Ordering::Relaxed);

        let top = self.top_ports(5);
        let top_json: Vec<String> = top
            .iter()
            .map(|(p, ins, outs)| {
                format!(
                    r#"{{"port":"0x{:04X}","device":"{}","in":{},"out":{}}}"#,
                    p,
                    Self::port_name(*p),
                    ins,
                    outs
                )
            })
            .collect();

        format!(
            r#"{{"exits_total":{},"hlt":{},"io_in":{},"io_out":{},"mmio_r":{},"mmio_w":{},"irq_window":{},"top_ports":[{}]}}"#,
            total,
            hlt,
            io_in,
            io_out,
            mm_r,
            mm_w,
            irq_w,
            top_json.join(",")
        )
    }
}

#[allow(dead_code)]
pub type MetricsHandle = Arc<VmmMetrics>;
