//! Módulo de dispositivos emulados del VMM.
//! Cada dispositivo maneja un rango de puertos I/O del guest.

pub mod uart;
pub mod cdrom;
pub mod pci;
pub mod legacy;
pub mod fw_cfg;
pub mod pic_pit;
pub mod vga;
pub mod font;
pub mod usb_uhci;
pub mod bios_int13h;


use fw_cfg::FwCfg;
use pic_pit::LegacyInterrupts;
use legacy::{A20Gate, AcpiPm, CmosRtc, DebugCon, FloppyStub, PlatformStubs, PostCode};
use vga::VgaDevice;
use usb_uhci::UsbUhci;

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
}

// Safety (tarea 6): DeviceBus se comparte entre los hilos de todos los
// vCPUs vía `Arc<Mutex<DeviceBus>>`. Contiene punteros crudos (`vram_ptr`
// vía VgaState y `guest_mem` en DebugCon) que apuntan a regiones mmap
// filtradas deliberadamente con vida 'static (ver `mmap_zeroed_region` en
// main.rs): no existe ningún préstamo exclusivo que invalidar y todo
// acceso mutable queda serializado por el Mutex. Mismo criterio que el
// `unsafe impl Send for VgaState` de vga.rs.
unsafe impl Send for DeviceBus {}

impl DeviceBus {
    pub fn new(
        iso_path: Option<&str>,
        disk_path: Option<&str>,
        vram_ptr: *mut u8,
        vram_size: usize,
        num_cpus: u32,
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
        Ok((
            Self {
                uart: uart::Uart16550::new(),
                primary_ide,
                cdrom,
                pci,
                usb,
                debugcon: DebugCon::new(),
                post: PostCode::new(),
                cmos: CmosRtc::new(),
                fw_cfg: FwCfg::new(256 * 1024 * 1024, num_cpus),
                legacy_irq: LegacyInterrupts::new(),
                a20: A20Gate::new(),
                acpi_pm: AcpiPm::new(),
                vga: vga_device,
                floppy: FloppyStub::new(),
                platform: PlatformStubs::new(),
            },
            vga_state,
        ))
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
            },
            legacy_irq,
            primary_ide,
            floppy,
            vga,
            usb,
            platform,
        )
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
        )
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
}
