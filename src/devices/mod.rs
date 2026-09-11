//! Módulo de dispositivos emulados del VMM.
//! Cada dispositivo maneja un rango de puertos I/O del guest.

pub mod uart;
pub mod cdrom;
pub mod pci;
pub mod legacy;
pub mod fw_cfg;
pub mod acpi;
pub mod pic_pit;
pub mod vga;
pub mod font;
pub mod usb_uhci;
// Librería de helpers INT 13h (AH=41h/42h/08h/02h): se consume desde sus
// tests y queda lista para un futuro dispatch directo del VMM, de ahí que
// el binario no la use todavía (mismo criterio que los helpers de vga.rs).
#[allow(dead_code)]
pub mod bios_int13h;
pub mod pflash;
pub mod cpu_hotplug;

use fw_cfg::FwCfg;
use pic_pit::LegacyInterrupts;
use legacy::{A20Gate, AcpiPm, ApmSmiDevice, CmosRtc, DebugCon, FloppyStub, PlatformStubs, PostCode};
use vga::VgaDevice;
use usb_uhci::UsbUhci;
use cpu_hotplug::CpuHotplugController;
use pflash::ParallelFlash;

use std::fmt;

/// Error de dispositivo
#[allow(dead_code)]
#[derive(Debug)]
pub enum DeviceError {
    Io(std::io::Error),
    UnknownPort(u16),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::Io(e) => write!(f, "IO error: {}", e),
            DeviceError::UnknownPort(p) => write!(f, "Puerto I/O no manejado: 0x{:X}", p),
        }
    }
}

impl std::error::Error for DeviceError {}

/// Trait que todos los dispositivos de I/O implementan.
pub trait IoDevice {
    fn matches_port(&self, port: u16) -> bool;
    fn write(&mut self, port: u16, data: &[u8]);
    fn read(&mut self, port: u16, count: usize) -> Vec<u8>;
}

/// Genera el despacho secuencial de OUT: consulta `matches_port` de cada
/// dispositivo en orden y devuelve `true` del método envolvente al primero
/// que maneja el puerto. Acepta un bloque opcional `=> { ... }` tras un
/// campo para ejecutar hooks post-escritura (p. ej. el de PCI/ACPI).
macro_rules! io_dispatch_out {
    ($self:expr, $port:expr, $data:expr; $($field:ident $(=> $hook:block)?),+ $(,)?) => {{
        $(
            if $self.$field.matches_port($port) {
                $self.$field.write($port, $data);
                $($hook)?
                return true;
            }
        )+
        false
    }};
}

/// Genera el despacho secuencial de IN: análogo a `io_dispatch_out!` para
/// lecturas, devolviendo `Some(datos)` del primer dispositivo que maneja
/// el puerto, o `None` si ninguno lo hace.
macro_rules! io_dispatch_in {
    ($self:expr, $port:expr, $count:expr; $($field:ident),+ $(,)?) => {{
        $(
            if $self.$field.matches_port($port) {
                return Some($self.$field.read($port, $count));
            }
        )+
        None
    }};
}

/// Bus de dispositivos: despacha accesos I/O al dispositivo correcto.
pub struct DeviceBus {
    pub uart: uart::Uart16550,
    pub primary_ide: cdrom::PrimaryIde,
    pub cdrom: Option<cdrom::CdRom>,
    pub pci: pci::PciBus,
    pub usb: UsbUhci,
    pub debugcon: DebugCon,
    pub post: PostCode,
    pub cmos: CmosRtc,
    pub fw_cfg: FwCfg,
    pub legacy_irq: LegacyInterrupts,
    pub a20: A20Gate,
    pub acpi_pm: AcpiPm,
    pub vga: VgaDevice,
    pub floppy: FloppyStub,
    pub platform: PlatformStubs,
    pub apm: ApmSmiDevice,
    pub cpu_hotplug: CpuHotplugController,
    pub pflash: Option<ParallelFlash>,
    // (tarea 1) Ventana de high RAM de KVM (slot 2): necesaria para
    // re-apuntar el framebuffer cuando el guest asigna el BAR0 VGA dentro
    // de RAM respaldada (esos accesos no generan exits MMIO).
    high_mem_ptr: *mut u8,
    high_mem_gpa: u64,
    high_mem_size: usize,
    /// (tarea 1) Contador de exits MMIO no enrutados (log limitado).
    unknown_mmio_events: u64,
}

// Safety (tarea 6): DeviceBus se comparte entre los hilos de todos los
// vCPUs vía `Arc<Mutex<DeviceBus>>`. El único puntero crudo que queda es
// `vram_ptr` (VgaState), que apunta a una región mmap filtrada
// deliberadamente con vida 'static (ver `mmap_zeroed_region` en main.rs);
// la memoria del guest viaja como `Arc<GuestMemory>` (Send+Sync, acceso
// acotado — tarea 18) y todo acceso mutable queda serializado por el
// Mutex. Mismo criterio que el `unsafe impl Send for VgaState` de vga.rs.
unsafe impl Send for DeviceBus {}

impl DeviceBus {
    pub fn new(
        iso_path: Option<&str>,
        disk_path: Option<&str>,
        vram_ptr: *mut u8,
        vram_size: usize,
        ram_size: u64,
        num_cpus: u32,
        high_mem_ptr: *mut u8,
        high_mem_gpa: u64,
        high_mem_size: usize,
        vga_rom: Option<Vec<u8>>,
    ) -> Result<(Self, std::sync::Arc<std::sync::Mutex<vga::VgaState>>), Box<dyn std::error::Error>> {
        // `num_cpus` (tarea 6): se anuncia al guest por fw_cfg
        // (FW_CFG_NB_CPUS/FW_CFG_MAX_CPUS) para que SeaBIOS acote su
        // sondeo SIPI y construya la tabla MP con el nº real de vCPUs.
        let cdrom = match iso_path {
            Some(p) => Some(cdrom::CdRom::new(p)?),
            None => Some(cdrom::CdRom::stub()), // stub: responde "no media"
        };
        let primary_ide = match disk_path {
            Some(p) => cdrom::PrimaryIde::with_disk(p)?,
            None => cdrom::PrimaryIde::new(),
        };
        let (vga_device, vga_state) = VgaDevice::new(vram_ptr, vram_size);
        let usb = UsbUhci::new();
        let mut pci = pci::PciBus::with_legacy_ide();
        pci.connect_usb(usb.state.clone());
        // Tablas ACPI (RSDP/RSDT/FADT/DSDT/MADT/FACS) expuestas por fw_cfg
        // con el interface estándar de QEMU: SeaBIOS las instala en RAM y el
        // guest (Linux) encuentra el RSDP en FSEG → apagado limpio vía _S5.
        let acpi_files = acpi::build_acpi_files(num_cpus);
        let cpu_hotplug = CpuHotplugController::new(num_cpus, 16);
        Ok((
            Self {
                uart: uart::Uart16550::new(),
                primary_ide,
                cdrom,
                pci,
                usb,
                debugcon: DebugCon::new(),
                post: PostCode::new(),
                cmos: CmosRtc::with_ram_size(ram_size),
                fw_cfg: FwCfg::new(ram_size, num_cpus, Some(acpi_files), vga_rom),
                legacy_irq: LegacyInterrupts::new(),
                a20: A20Gate::new(),
                acpi_pm: AcpiPm::new(),
                vga: vga_device,
                floppy: FloppyStub::new(),
                platform: PlatformStubs::new(),
                apm: ApmSmiDevice::new(),
                cpu_hotplug,
                pflash: None,
                high_mem_ptr,
                high_mem_gpa,
                high_mem_size,
                unknown_mmio_events: 0,
            },
            vga_state,
        ))
    }

    /// Reset de todo el hardware emulado (equivalente a un power-on reset
    /// del chipset): cada dispositivo vuelve a su estado de arranque.
    /// La RAM del guest y el firmware NO se tocan aquí (eso lo hace el
    /// llamador con `init_bios_data_area`).
    pub fn reset(&mut self) {
        self.uart.reset();
        self.primary_ide.reset();
        if let Some(cd) = self.cdrom.as_mut() {
            cd.reset();
        }
        self.pci.reset();
        self.usb.reset();
        self.debugcon.reset();
        self.post.reset();
        self.cmos.reset();
        self.fw_cfg.reset();
        self.legacy_irq.reset();
        self.a20.reset();
        self.acpi_pm.reset();
        self.vga.reset();
        self.floppy.reset();
        self.platform.reset();
        self.apm.reset();
        self.cpu_hotplug.reset(1);
        if let Some(pf) = self.pflash.as_mut() {
            pf.mode = pflash::PFlashMode::ReadArray;
            pf.status = pflash::ParallelFlash::STATUS_READY;
        }
        eprintln!("[VMM] Dispositivos reiniciados (UART, IDE/ATAPI, PCI, USB, PIT/PIC/PS2, VGA, CMOS, ACPI, APM, CPU-Hotplug...)");
    }

    /// Despacha un OUT del guest. Devuelve true si algún dispositivo lo manejó.
    ///
    /// La lista de dispositivos vive en UN solo sitio (`io_dispatch_out!`), la
    /// misma que usa `input()` vía `io_dispatch_in!`, evitando que las dos
    /// cadenas de `matches_port` se desincronicen al añadir dispositivos.
    /// `cdrom` es Option y se comprueba aparte (no se solapa con otros puertos).
    pub fn out(&mut self, port: u16, data: &[u8]) -> bool {
        if let Some(cd) = self.cdrom.as_mut() {
            if cd.matches_port(port) {
                cd.write(port, data);
                return true;
            }
        }
        io_dispatch_out!(self, port, data;
            debugcon,
            post,
            a20,
            acpi_pm,
            cmos,
            fw_cfg,
            uart,
            pci => {
                // Check for PIIX3 ACPI PM config writes and update AcpiPm
                if let Some((reg, val)) = self.pci.last_acpi_config_write.take() {
                    self.acpi_pm.update_pci_config(reg, val);
                }
                // (tarea 1) Asignaciones de BARs VGA detectadas en el config
                // space: registrarlas y (si caen en RAM respaldada del slot 2)
                // re-apuntar el framebuffer del renderizador.
                if let Some(base) = self.pci.last_vga_lfb_bar_write.take() {
                    self.apply_vga_bar_assignment(base, true);
                }
                if let Some(base) = self.pci.last_vga_mmio_bar_write.take() {
                    self.apply_vga_bar_assignment(base, false);
                }
            },
            legacy_irq,
            primary_ide,
            floppy,
            vga,
            usb,
            platform,
            apm,
            cpu_hotplug,
        )
    }

    // ─── MMIO (tarea 1 & 22) ───────────────────────────────────

    /// (tarea 1) Aplica la asignación de un BAR VGA detectada en el config
    /// space. Si el BAR0 (framebuffer) cae dentro de la ventana de high RAM
    /// (slot 2 de KVM), los accesos del guest son RAM normal y NO generan
    /// exits MMIO: re-apuntamos el puntero host del framebuffer para que el
    /// renderizador (display.rs) lea la zona donde el guest escribe de verdad.
    /// Si cae fuera, los accesos llegarán como exits MmioRead/MmioWrite y los
    /// atiende `VgaDevice::mmio_read/mmio_write`.
    fn apply_vga_bar_assignment(&mut self, base: u32, is_lfb: bool) {
        if !is_lfb {
            self.vga.set_mmio_bar(base);
            return;
        }
        self.vga.set_lfb_bar(base);
        let vram_size = { self.vga.state.lock().unwrap().vram_size } as u64;
        let gpa = (base & 0xFFFF_FFF0) as u64;
        if base != 0
            && gpa >= self.high_mem_gpa
            && gpa + vram_size <= self.high_mem_gpa + self.high_mem_size as u64
        {
            let off = (gpa - self.high_mem_gpa) as usize;
            let ptr = unsafe { self.high_mem_ptr.add(off) };
            self.vga.set_vram_host_ptr(ptr);
            eprintln!(
                "[VGA] Framebuffer RAM-backed: vram re-apuntado a high_mem+{:#x} (GPA {:#x})",
                off, gpa
            );
        }
    }

    /// (tarea 1 & 22) Despacha una escritura MMIO del guest (exit MmioWrite).
    /// Enruta a VGA (BAR2 dispi + BAR0 framebuffer) y PFlash (UEFI/OVMF VarStore).
    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        if self.vga.mmio_write(addr, data) {
            return;
        }
        if let Some(pf) = self.pflash.as_mut() {
            let flash_offset = addr.saturating_sub(0xFFC0_0000);
            if flash_offset < pf.data.len() as u64 {
                pf.write(flash_offset as usize, data);
                return;
            }
        }
        self.unknown_mmio_events += 1;
        if self.unknown_mmio_events <= 16 {
            eprintln!(
                "[VMM] MMIO W no enrutado: {:#x} <- {:02x?} (no enrutados: {})",
                addr, data, self.unknown_mmio_events
            );
        }
    }

    /// (tarea 1 & 22) Despacha una lectura MMIO del guest (exit MmioRead).
    /// Devuelve exactamente `size` bytes (0xFF si nadie atiende la dirección).
    pub fn mmio_read(&mut self, addr: u64, size: usize) -> Vec<u8> {
        if let Some(bytes) = self.vga.mmio_read(addr, size) {
            return bytes;
        }
        if let Some(pf) = self.pflash.as_ref() {
            let flash_offset = addr.saturating_sub(0xFFC0_0000);
            if flash_offset < pf.data.len() as u64 {
                let mut bytes = pf.read(flash_offset as usize, size);
                bytes.resize(size, 0xFF);
                return bytes;
            }
        }
        self.unknown_mmio_events += 1;
        if self.unknown_mmio_events <= 16 {
            eprintln!(
                "[VMM] MMIO R no enrutado: {:#x} (no enrutados: {})",
                addr, self.unknown_mmio_events
            );
        }
        vec![0xFF; size]
    }

    /// Despacha un IN del guest.
    pub fn input(&mut self, port: u16, count: usize) -> Option<Vec<u8>> {
        if let Some(cd) = self.cdrom.as_mut() {
            if cd.matches_port(port) {
                return Some(cd.read(port, count));
            }
        }
        io_dispatch_in!(self, port, count;
            debugcon,
            post,
            a20,
            acpi_pm,
            cmos,
            fw_cfg,
            uart,
            pci,
            legacy_irq,
            primary_ide,
            floppy,
            vga,
            usb,
            platform,
            apm,
            cpu_hotplug,
        )
    }

    /// Verifica si se solicitó una interrupción SMM / SMI por puerto 0xB2 (Item 23).
    pub fn take_smi(&mut self) -> bool {
        self.apm.take_smi()
    }

    /// Verifica si hay un evento SCI de conexión de vCPU pendiente (Item 23).
    pub fn take_cpu_hotplug_sci(&mut self) -> bool {
        self.cpu_hotplug.take_sci()
    }

    /// Asigna una instancia emulada de flash paralela CFI / VarStore (Item 22).
    pub fn set_pflash(&mut self, pflash: ParallelFlash) {
        self.pflash = Some(pflash);
    }

    // ─── Interrupciones ─────────────────────────────────────────────
    #[allow(dead_code)]
    pub fn pit_tick(&mut self) -> bool {
        if self.legacy_irq.pit_tick() {
            self.legacy_irq.raise_irq(0);
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn pit_advance_ticks(&mut self, ticks: u32) -> bool {
        if self.legacy_irq.pit_advance_ticks(ticks) {
            self.legacy_irq.raise_irq(0);
            true
        } else {
            false
        }
    }

    // (tarea 4) Sombras del PIC de usuariospace sin llamador desde que el
    // handler de IrqWindowOpen pasó a confiar en el kernel irqchip.
    #[allow(dead_code)]
    pub fn pending_irq(&self) -> Option<u8> {
        self.legacy_irq.pending_irq()
    }

    #[allow(dead_code)]
    pub fn acknowledge_irq(&mut self, vector: u8) {
        self.legacy_irq.acknowledge_irq(vector);
    }

    #[allow(dead_code)]
    pub fn lower_irq(&mut self, line: u8) {
        self.legacy_irq.lower_irq(line);
    }

    /// Check if PS/2 has data that needs IRQ1 injected into the kernel PIC.
    pub fn take_ps2_irq(&mut self) -> bool {
        self.legacy_irq.take_irq1_pending()
    }

    /// Check if the PS/2 mouse has data that needs IRQ12 injected into the
    /// kernel PIC (flanco en la línea 12 del esclavo).
    pub fn take_mouse_irq(&mut self) -> bool {
        self.legacy_irq.take_irq12_pending()
    }

    /// UART 16550: IRQ4 pendiente de inyección al kernel PIC (flanco en la
    /// línea 4). One-shot: consumido por el bucle VMM (tarea 12).
    pub fn take_uart_irq(&mut self) -> bool {
        self.uart.take_irq()
    }

    /// Nivel actual de IRQ4 del UART (RX/THRE/error con su bit del IER
    /// habilitado): lo usa request_interrupt_window cuando IF=0.
    pub fn uart_irq_pending(&self) -> bool {
        self.uart.irq_pending()
    }

    /// True si el guest pidió apagado limpio vía ACPI: escribió SLP_EN en
    /// PM1a_CNT (0x604) tras evaluar _S5. El VMM lo consulta para salir.
    pub fn acpi_sleep_requested(&self) -> bool {
        self.acpi_pm.sleep_requested()
    }
}
