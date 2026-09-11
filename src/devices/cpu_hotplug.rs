//! Controlador ACPI de CPU Hotplug (Item 23).
//!
//! Emula la interfaz estándar de QEMU/ACPI para conexión y desconexión en caliente de vCPUs:
//!   - Puerto 0xCD8 (write): Selección de CPU ID.
//!   - Puerto 0xCD8 (read) : Estado del CPU seleccionado (presente, habilitado, pendiente).
//!   - Puerto 0xCD9 (write): Comandos de inserción/expulsión (_EJ0).

use super::IoDevice;

pub const CPU_HOTPLUG_BASE: u16 = 0x0CD8;
pub const CPU_HOTPLUG_END: u16 = 0x0CDB;

pub const CPU_STATUS_PRESENT: u8 = 0x01;
pub const CPU_STATUS_ENABLED: u8 = 0x02;
pub const CPU_STATUS_INSERTING: u8 = 0x04;
#[allow(dead_code)]
pub const CPU_STATUS_REMOVING: u8 = 0x08;

#[allow(dead_code)]
pub struct CpuHotplugController {
    pub max_cpus: u32,
    pub selected_cpu: u32,
    pub cpu_status: Vec<u8>,
    pub sci_pending: bool,
}

impl CpuHotplugController {
    pub fn new(boot_cpus: u32, max_cpus: u32) -> Self {
        let max_cpus = max_cpus.max(boot_cpus).max(1);
        let mut cpu_status = vec![0u8; max_cpus as usize];

        // Los vCPUs de arranque nacen presentes y habilitados
        for i in 0..boot_cpus.min(max_cpus) as usize {
            cpu_status[i] = CPU_STATUS_PRESENT | CPU_STATUS_ENABLED;
        }

        Self {
            max_cpus,
            selected_cpu: 0,
            cpu_status,
            sci_pending: false,
        }
    }

    /// Añade un vCPU en caliente durante la ejecución del guest.
    /// Devuelve true si el CPU se insertó y requiere señalización SCI (IRQ9).
    #[allow(dead_code)]
    pub fn plug_cpu(&mut self, cpu_id: u32) -> Result<bool, &'static str> {
        if cpu_id >= self.max_cpus {
            return Err("CPU ID excede el máximo permitido");
        }
        let idx = cpu_id as usize;
        if (self.cpu_status[idx] & CPU_STATUS_PRESENT) != 0 {
            return Err("El CPU ya se encuentra conectado");
        }

        self.cpu_status[idx] = CPU_STATUS_PRESENT | CPU_STATUS_ENABLED | CPU_STATUS_INSERTING;
        self.sci_pending = true;
        eprintln!("[CPU-HOTPLUG] vCPU #{} conectado en caliente (SCI pendiente)", cpu_id);
        Ok(true)
    }

    /// Desconecta un vCPU en caliente.
    #[allow(dead_code)]
    pub fn unplug_cpu(&mut self, cpu_id: u32) -> Result<bool, &'static str> {
        if cpu_id == 0 {
            return Err("No se puede desconectar el BSP (CPU 0)");
        }
        if cpu_id >= self.max_cpus {
            return Err("CPU ID excede el máximo permitido");
        }
        let idx = cpu_id as usize;
        if (self.cpu_status[idx] & CPU_STATUS_PRESENT) == 0 {
            return Err("El CPU no está presente");
        }

        self.cpu_status[idx] |= CPU_STATUS_REMOVING;
        self.sci_pending = true;
        eprintln!("[CPU-HOTPLUG] vCPU #{} marcado para desconexión (SCI pendiente)", cpu_id);
        Ok(true)
    }

    /// Consume y limpia el indicador de interrupción SCI pendiente para ACPI.
    pub fn take_sci(&mut self) -> bool {
        let p = self.sci_pending;
        self.sci_pending = false;
        p
    }

    pub fn reset(&mut self, boot_cpus: u32) {
        for i in 0..self.cpu_status.len() {
            if (i as u32) < boot_cpus {
                self.cpu_status[i] = CPU_STATUS_PRESENT | CPU_STATUS_ENABLED;
            } else {
                self.cpu_status[i] = 0;
            }
        }
        self.selected_cpu = 0;
        self.sci_pending = false;
    }
}

impl IoDevice for CpuHotplugController {
    fn matches_port(&self, port: u16) -> bool {
        (CPU_HOTPLUG_BASE..=CPU_HOTPLUG_END).contains(&port)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        match port {
            CPU_HOTPLUG_BASE => {
                // Selector de CPU (u32 LE o u8)
                let mut sel = 0u32;
                for (i, &b) in data.iter().take(4).enumerate() {
                    sel |= (b as u32) << (i * 8);
                }
                self.selected_cpu = sel;
            }
            0x0CD9 => {
                // Registro de comando / acknowledge de eventos
                let cmd = data[0];
                let idx = self.selected_cpu as usize;
                if idx < self.cpu_status.len() {
                    if (cmd & 0x01) != 0 {
                        // Limpiar flag de inserción
                        self.cpu_status[idx] &= !CPU_STATUS_INSERTING;
                    }
                    if (cmd & 0x02) != 0 {
                        // Confirmar expulsión (_EJ0)
                        self.cpu_status[idx] = 0;
                    }
                }
            }
            _ => {}
        }
    }

    fn read(&mut self, port: u16, count: usize) -> Vec<u8> {
        let mut res = vec![0u8; count];
        match port {
            CPU_HOTPLUG_BASE => {
                // Devuelve el status del CPU actualmente seleccionado
                let idx = self.selected_cpu as usize;
                if idx < self.cpu_status.len() {
                    res[0] = self.cpu_status[idx];
                } else {
                    res[0] = 0x00;
                }
            }
            0x0CD9 => {
                // Flags de presencia global
                res[0] = if self.sci_pending { 0x01 } else { 0x00 };
            }
            _ => {}
        }
        res
    }
}
