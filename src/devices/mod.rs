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
pub mod usb_tablet;
pub mod virtio_serial;
pub mod virtio_net;
pub mod bmdma;
// Librería de helpers INT 13h (AH=41h/42h/08h/02h): se consume desde sus
// tests y queda lista para un futuro dispatch directo del VMM, de ahí que
// el binario no la use todavía (mismo criterio que los helpers de vga.rs).
#[allow(dead_code)]
pub mod bios_int13h;
pub mod pflash;
pub mod cpu_hotplug;
pub mod ahci;
pub mod vmmdev;
pub mod ac97;

use bmdma::BmdmaController;
use fw_cfg::FwCfg;
use pic_pit::LegacyInterrupts;
use legacy::{A20Gate, AcpiPm, ApmSmiDevice, CmosRtc, DebugCon, FloppyStub, I8237Dma, PlatformStubs, PostCode};
use vga::VgaDevice;
use usb_uhci::UsbUhci;
use virtio_serial::{VirtioSerialDevice, VirtioSerialState};
use virtio_net::{VirtioNetDevice, VirtioNetState};
use vmmdev::{VmmDevDevice, VmmDevState};
use ac97::{Ac97Device, Ac97State};
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
    pub virtio_serial: VirtioSerialDevice,
    pub virtio_serial_state: std::sync::Arc<std::sync::Mutex<VirtioSerialState>>,
    pub virtio_net: VirtioNetDevice,
    pub virtio_net_state: std::sync::Arc<std::sync::Mutex<VirtioNetState>>,
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
    pub dma: I8237Dma,
    pub apm: ApmSmiDevice,
    pub cpu_hotplug: CpuHotplugController,
    pub pflash: Option<ParallelFlash>,
    pub bmdma: BmdmaController,
    pub ahci: ahci::AhciController,
    pub vmmdev: VmmDevDevice,
    pub vmmdev_state: std::sync::Arc<std::sync::Mutex<VmmDevState>>,
    pub ac97: Ac97Device,
    pub ac97_state: std::sync::Arc<std::sync::Mutex<Ac97State>>,
    pub guest_mem: Option<std::sync::Arc<crate::guest_mem::GuestMemory>>,
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
        // El disco duro principal se conecta exclusivamente al controlador SATA AHCI (Puerto 0)
        // para evitar dispositivos duplicados (/dev/sda vs /dev/sdb) y conflictos de concurrencia.
        let primary_ide = cdrom::PrimaryIde::new();
        let (vga_device, vga_state) = VgaDevice::new(vram_ptr, vram_size);
        let usb = UsbUhci::new();
        let virtio_serial_state = std::sync::Arc::new(std::sync::Mutex::new(VirtioSerialState::new()));
        let virtio_serial = VirtioSerialDevice::new(virtio_serial_state.clone());
        let virtio_net_state = std::sync::Arc::new(std::sync::Mutex::new(VirtioNetState::new()));
        let virtio_net = VirtioNetDevice::new(virtio_net_state.clone());
        let mut pci = pci::PciBus::with_legacy_ide();
        pci.connect_usb(usb.state.clone());
        pci.connect_virtio_serial(virtio_serial_state.clone());
        pci.connect_virtio_net(virtio_net_state.clone());
        let vmmdev_state = std::sync::Arc::new(std::sync::Mutex::new(VmmDevState::new()));
        let vmmdev = VmmDevDevice::new(vmmdev_state.clone());
        pci.connect_vmmdev(vmmdev_state.clone());
        let ac97_state = std::sync::Arc::new(std::sync::Mutex::new(Ac97State::new()));
        let ac97 = Ac97Device::new(ac97_state.clone());
        pci.connect_ac97(ac97_state.clone());
        // Tablas ACPI (RSDP/RSDT/FADT/DSDT/MADT/FACS) expuestas por fw_cfg
        // con el interface estándar de QEMU: SeaBIOS las instala en RAM y el
        // guest (Linux) encuentra el RSDP en FSEG → apagado limpio vía _S5.
        let acpi_files = acpi::build_acpi_files(num_cpus);
        let cpu_hotplug = CpuHotplugController::new(num_cpus, 16);
        let ahci = ahci::AhciController::with_files(disk_path, iso_path);
        Ok((
            Self {
                uart: uart::Uart16550::new(),
                primary_ide,
                cdrom,
                pci,
                usb,
                virtio_serial,
                virtio_serial_state,
                virtio_net,
                virtio_net_state,
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
                dma: I8237Dma::new(),
                apm: ApmSmiDevice::new(),
                cpu_hotplug,
                pflash: None,
                bmdma: BmdmaController::new(),
                ahci,
                vmmdev,
                vmmdev_state,
                ac97,
                ac97_state,
                guest_mem: None,
                high_mem_ptr,
                high_mem_gpa,
                high_mem_size,
                unknown_mmio_events: 0,
            },
            vga_state,
        ))
    }

    /// Asigna la memoria física del guest para transferencias DMA y diagnósticos
    pub fn set_guest_mem(&mut self, mem: std::sync::Arc<crate::guest_mem::GuestMemory>) {
        self.guest_mem = Some(mem);
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
        self.bmdma.reset();
        self.ahci.reset();
        self.vmmdev_state.lock().unwrap().reset();
        self.ac97_state.lock().unwrap().reset();
        self.usb.reset();
        self.virtio_serial_state.lock().unwrap().reset();
        self.virtio_net_state.lock().unwrap().reset();
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
        self.dma.reset();
        self.apm.reset();
        self.cpu_hotplug.reset(1);
        if let Some(pf) = self.pflash.as_mut() {
            pf.mode = pflash::PFlashMode::ReadArray;
            pf.status = pflash::ParallelFlash::STATUS_READY;
        }
        eprintln!("[VMM] Dispositivos reiniciados (UART, IDE/ATAPI, PCI, USB, VirtIO-Serial, VirtIO-Net, VMMDev, AC'97, DMA, PIT/PIC/PS2, VGA, CMOS, ACPI, APM, CPU-Hotplug...)");
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
                if let Some(ref mem) = self.guest_mem {
                    self.bmdma.execute_secondary_dma(cd, mem);
                }
                return true;
            }
        }
        if self.vmmdev.matches_port(port) {
            self.vmmdev.write(port, data, self.guest_mem.as_deref());
            return true;
        }
        if self.ac97.matches_port(port) {
            self.ac97.write(port, data, self.guest_mem.as_deref());
            return true;
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
                if let Some(base) = self.pci.last_ide_bmdma_bar_write.take() {
                    self.bmdma.set_iobase(base);
                }
                if let Some(base) = self.pci.last_ahci_bar_write.take() {
                    self.ahci.bar5 = base;
                }
                if let Some(base) = self.pci.last_vmmdev_io_bar_write.take() {
                    self.vmmdev.set_iobase(base);
                }
                if let Some(base) = self.pci.last_vmmdev_mmio_bar_write.take() {
                    self.vmmdev.set_mmio_base(base);
                }
                if let Some(base) = self.pci.last_ac97_nam_bar_write.take() {
                    self.ac97.set_nambar(base);
                }
                if let Some(base) = self.pci.last_ac97_nabm_bar_write.take() {
                    self.ac97.set_nabmbar(base);
                }
            },
            bmdma => {
                if let Some(ref mem) = self.guest_mem {
                    self.bmdma.execute_primary_dma(&mut self.primary_ide, mem);
                    if let Some(ref mut cd) = self.cdrom {
                        self.bmdma.execute_secondary_dma(cd, mem);
                    }
                }
            },
            dma,
            legacy_irq,
            primary_ide => {
                if let Some(ref mem) = self.guest_mem {
                    self.bmdma.execute_primary_dma(&mut self.primary_ide, mem);
                }
            },
            floppy,
            vga,
            usb,
            virtio_serial,
            virtio_net => {
                if let Some(mem) = self.guest_mem.clone() {
                    self.step_virtio_net(&mem);
                }
            },
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
        if self.ahci.bar5 != 0 && addr >= self.ahci.bar5 as u64 && addr < (self.ahci.bar5 as u64) + 0x1000 {
            let off = addr - self.ahci.bar5 as u64;
            self.ahci.write(off, data, self.guest_mem.as_deref());
            return;
        }
        if self.vmmdev.mmio_write(addr, data) {
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
        if self.ahci.bar5 != 0 && addr >= self.ahci.bar5 as u64 && addr < (self.ahci.bar5 as u64) + 0x1000 {
            let off = addr - self.ahci.bar5 as u64;
            return self.ahci.read(off, size);
        }
        if let Some(bytes) = self.vmmdev.mmio_read(addr, size) {
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
        if self.vmmdev.matches_port(port) {
            return Some(self.vmmdev.read(port, count));
        }
        if self.ac97.matches_port(port) {
            return Some(self.ac97.read(port, count));
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
            bmdma,
            dma,
            legacy_irq,
            primary_ide,
            floppy,
            vga,
            usb,
            virtio_serial,
            virtio_net,
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

    /// Comprueba si hay interrupciones pendientes de los canales IDE:
    /// devuelve (irq14_primary, irq15_secondary).
    pub fn take_ide_irq(&mut self) -> (bool, bool) {
        let irq14 = self.primary_ide.take_irq();
        let irq15 = self.cdrom.as_mut().map(|c| c.take_irq()).unwrap_or(false);
        (irq14, irq15)
    }

    /// Nivel actual de interrupción pendiente en los canales IDE.
    pub fn ide_irq_pending(&self) -> bool {
        self.primary_ide.irq_pending || self.cdrom.as_ref().map(|c| c.irq_pending()).unwrap_or(false)
    }

    // ─── USB UHCI & Tablet ──────────────────────────────────────────

    /// Ejecuta un tick del scheduler UHCI (Frame List, QHs y TDs).
    pub fn step_usb(&mut self, mem: &crate::guest_mem::GuestMemory) {
        self.usb.step(mem);
    }

    /// Comprueba si la línea de interrupción del USB UHCI está activa.
    pub fn is_usb_irq_asserted(&self) -> bool {
        self.usb.is_irq_asserted()
    }

    /// Extrae el flag de pulso de interrupción fresca del USB UHCI.
    pub fn take_usb_irq_pulse(&mut self) -> bool {
        self.usb.take_irq_pulse()
    }

    /// Devuelve la línea IRQ asignada al USB UHCI en el bus PCI (dev 1:2).
    pub fn usb_irq_line(&self) -> u8 {
        self.pci.usb_irq_line()
    }

    /// Inyecta un evento en la tableta USB (X, Y en 0..32767, botones, rueda).
    pub fn inject_tablet_event(&mut self, x: u16, y: u16, buttons: u8, wheel: i8) {
        self.usb.inject_tablet_event(x, y, buttons, wheel);
    }

    // ─── VirtIO Serial & SPICE Dynamic Resolution ───────────────────

    /// Avanza las virtqueues del VirtIO-Serial y despacha mensajes de resolución.
    pub fn step_virtio_serial(&mut self, mem: &crate::guest_mem::GuestMemory) {
        self.virtio_serial_state.lock().unwrap().step(mem);
    }

    /// Extrae el flag de pulso de interrupción de VirtIO-Serial.
    pub fn take_virtio_serial_irq_pulse(&mut self) -> bool {
        self.virtio_serial_state.lock().unwrap().take_irq_pulse()
    }

    /// Devuelve la línea IRQ asignada a VirtIO-Serial (dev 3:0).
    pub fn virtio_serial_irq_line(&self) -> u8 {
        self.pci.virtio_serial_irq_line()
    }

    /// Comprueba si la línea de interrupción de VirtIO-Serial está activa.
    pub fn is_virtio_serial_irq_asserted(&self) -> bool {
        self.virtio_serial_state.lock().unwrap().isr_status != 0
    }

    /// Solicita un cambio de resolución dinámica a VirtIO-Serial.
    pub fn request_resolution(&mut self, width: u32, height: u32) {
        self.virtio_serial_state.lock().unwrap().request_resolution(width, height);
    }

    // ─── VirtIO-Net & Red Integrada (Slirp / Modo Usuario / TAP) ────

    /// Avanza las virtqueues del VirtIO-Net (procesa TX y despacha tramas RX pendientes).
    pub fn step_virtio_net(&mut self, mem: &crate::guest_mem::GuestMemory) {
        self.virtio_net_state.lock().unwrap().step(mem);
    }

    /// Extrae el flag de pulso de interrupción de VirtIO-Net.
    pub fn take_virtio_net_irq_pulse(&mut self) -> bool {
        self.virtio_net_state.lock().unwrap().take_irq_pulse()
    }

    /// Comprueba si la línea de interrupción de VirtIO-Net está activa.
    pub fn is_virtio_net_irq_asserted(&self) -> bool {
        self.virtio_net_state.lock().unwrap().isr_status != 0
    }

    /// Devuelve la línea IRQ asignada a VirtIO-Net en el bus PCI (dev 4:0).
    pub fn virtio_net_irq_line(&self) -> u8 {
        self.pci.virtio_net_irq_line()
    }

    // ─── SATA AHCI Controller (dev 5:0) ─────────────────────────────

    /// Devuelve la línea IRQ asignada al AHCI en el bus PCI (dev 5:0).
    pub fn ahci_irq_line(&self) -> u8 {
        self.pci.ahci_irq_line()
    }

    /// Comprueba si la línea de interrupción del AHCI está activa.
    pub fn is_ahci_irq_asserted(&self) -> bool {
        self.ahci.is_irq_asserted()
    }

    // ─── VirtualBox VMMDev (dev 6:0) ─────────────────────────────────

    /// Devuelve la línea IRQ asignada al VMMDev en el bus PCI (dev 6:0).
    pub fn vmmdev_irq_line(&self) -> u8 {
        self.pci.vmmdev_irq_line()
    }

    /// Comprueba si la línea de interrupción del VMMDev está activa.
    pub fn is_vmmdev_irq_asserted(&self) -> bool {
        self.vmmdev_state.lock().unwrap().irq_asserted
    }

    // ─── Intel 82801AA AC'97 Audio (dev 7:0) ─────────────────────────

    /// Devuelve la línea IRQ asignada al AC'97 en el bus PCI (dev 7:0).
    pub fn ac97_irq_line(&self) -> u8 {
        self.pci.ac97_irq_line()
    }

    /// Comprueba si la línea de interrupción del AC'97 está activa.
    pub fn is_ac97_irq_asserted(&self) -> bool {
        self.ac97_state.lock().unwrap().irq_asserted
    }

    /// True si el guest pidió apagado limpio vía ACPI: escribió SLP_EN en
    /// PM1a_CNT (0x604) tras evaluar _S5. El VMM lo consulta para salir.
    pub fn acpi_sleep_requested(&self) -> bool {
        self.acpi_pm.sleep_requested()
    }

    /// Expulsa el CD-ROM en caliente.
    pub fn eject_cdrom(&mut self) {
        if let Some(cd) = self.cdrom.as_mut() {
            cd.eject();
        }
    }

    /// Inserta una nueva imagen ISO en caliente.
    pub fn insert_cdrom(&mut self, path: &str) -> Result<u64, Box<dyn std::error::Error>> {
        if let Some(cd) = self.cdrom.as_mut() {
            cd.insert(path)
        } else {
            let cd = cdrom::CdRom::new(path)?;
            let size = cd.iso_size;
            self.cdrom = Some(cd);
            Ok(size)
        }
    }

    pub fn is_cdrom_inserted(&self) -> bool {
        self.cdrom.as_ref().map(|c| c.is_inserted()).unwrap_or(false)
    }

    pub fn cdrom_sectors_read(&self) -> u64 {
        self.cdrom.as_ref().map(|c| c.sectors_read_total).unwrap_or(0)
    }

    pub fn disk_sectors_read(&self) -> u64 {
        self.primary_ide.sectors_read_total
    }

    pub fn disk_sectors_written(&self) -> u64 {
        self.primary_ide.sectors_written_total
    }

    /// Dispara el evento del botón de encendido ACPI (PWRBTN).
    pub fn trigger_power_button(&mut self) -> bool {
        self.acpi_pm.trigger_power_button()
    }

    /// Conecta un vCPU en caliente vía el controlador ACPI.
    pub fn plug_cpu(&mut self, cpu_id: u32) -> Result<bool, &'static str> {
        self.cpu_hotplug.plug_cpu(cpu_id)
    }
}
