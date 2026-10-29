//! Emulación del Temporizador de Eventos de Alta Precisión (HPET - High Precision Event Timer).
//!
//! Especificación Intel IA-PC HPET (Revisión 1.0a).
//!
//! Mapeado en la dirección MMIO estándar `0xFED0_0000` (1 KiB = 0x400).
//! Es esencial para el arranque y estabilidad de kernels modernos (Windows 7/10/11 y Linux)
//! para evitar caídas por `CLOCK_WATCHDOG_TIMEOUT` y proveer un reloj monotónico de alta resolución.

use std::time::Instant;

pub const HPET_MMIO_BASE: u64 = 0xFED0_0000;
pub const HPET_MMIO_SIZE: u64 = 0x400; // 1 KiB

/// Periodo del reloj en femtosegundos (10 ns = 100_000_000 fs = 100 MHz de frecuencia).
pub const HPET_CLK_PERIOD_FS: u64 = 100_000_000;

// Registros de control general
pub const HPET_REG_GCAP_ID: usize = 0x000;
pub const HPET_REG_GEN_CONF: usize = 0x010;
pub const HPET_REG_GINTR_STA: usize = 0x020;
pub const HPET_REG_MAIN_CNT: usize = 0x0F0;

// Offsets dentro del bloque de cada temporizador (0x100 + n * 0x20)
pub const HPET_TN_CONF: usize = 0x00;
pub const HPET_TN_COMP: usize = 0x08;
pub const HPET_TN_FSB: usize = 0x10;

// Bits de configuración general (GEN_CONF)
pub const HPET_GEN_CONF_ENABLE: u64 = 1 << 0;
pub const HPET_GEN_CONF_LEG_RT: u64 = 1 << 1;

// Bits de configuración de cada temporizador (Tn_CONF)
pub const HPET_TN_INT_TYPE: u64 = 1 << 1;      // 0=Edge, 1=Level
pub const HPET_TN_ENABLE: u64 = 1 << 2;        // Interrupt Enable
pub const HPET_TN_PERIODIC: u64 = 1 << 3;      // Periodic mode
pub const HPET_TN_PER_CAP: u64 = 1 << 4;       // Periodic capable (RO)
pub const HPET_TN_64BIT_CAP: u64 = 1 << 5;     // 64-bit capable (RO)
pub const HPET_TN_SETVAL: u64 = 1 << 6;        // Value set
pub const HPET_TN_32MODE: u64 = 1 << 8;        // 32-bit mode
pub const HPET_TN_ROUTE_MASK: u64 = 0x1F << 9; // Interrupt Route Mask (bits 9..13)
pub const HPET_TN_FSB_EN: u64 = 1 << 14;       // FSB interrupt enable
pub const HPET_TN_FSB_CAP: u64 = 1 << 15;      // FSB delivery capable

pub const NUM_TIMERS: usize = 3;

#[derive(Debug, Clone)]
pub struct HpetTimer {
    pub config: u64,
    pub comparator: u64,
    pub period: u64,
    pub fsb_route: u64,
    pub val_set: bool,
    pub fired: bool,
}

impl HpetTimer {
    pub fn new(index: usize) -> Self {
        let route_cap: u64 = match index {
            0 => (1 << 2) | (1 << 0), // Puede enrutar a IRQ0 o IRQ2
            1 => 1 << 8,            // Enruta a IRQ8
            _ => (1 << 11) | (1 << 12),
        };
        let default_irq: u64 = match index {
            0 => 2,
            1 => 8,
            _ => 11,
        };
        // Periodic capable (bit 4), 64-bit capable (bit 5) y FSB capaz (bit 15)
        let config = HPET_TN_PER_CAP
            | HPET_TN_64BIT_CAP
            | HPET_TN_FSB_CAP
            | ((default_irq & 0x1F) << 9)
            | (route_cap << 32);

        Self {
            config,
            comparator: 0xFFFF_FFFF_FFFF_FFFF,
            period: 0,
            fsb_route: 0,
            val_set: false,
            fired: false,
        }
    }

    #[inline]
    pub fn is_periodic(&self) -> bool {
        (self.config & HPET_TN_PERIODIC) != 0
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        (self.config & HPET_TN_ENABLE) != 0
    }

    #[inline]
    pub fn route_irq(&self) -> u8 {
        ((self.config >> 9) & 0x1F) as u8
    }
}

pub struct HpetDevice {
    pub enabled: bool,
    pub legacy_route: bool,
    pub start_time: Instant,
    pub base_counter: u64,
    pub interrupt_status: u32,
    pub timers: [HpetTimer; NUM_TIMERS],
}

impl Default for HpetDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl HpetDevice {
    pub fn new() -> Self {
        Self {
            enabled: false,
            legacy_route: false,
            start_time: Instant::now(),
            base_counter: 0,
            interrupt_status: 0,
            timers: [
                HpetTimer::new(0),
                HpetTimer::new(1),
                HpetTimer::new(2),
            ],
        }
    }

    pub fn reset(&mut self) {
        self.enabled = false;
        self.legacy_route = false;
        self.start_time = Instant::now();
        self.base_counter = 0;
        self.interrupt_status = 0;
        for i in 0..NUM_TIMERS {
            self.timers[i] = HpetTimer::new(i);
        }
    }

    /// Retorna el valor actual del contador de 64 bits.
    /// Incrementa a razón de 100 MHz (1 tick cada 10 nanosegundos = 100_000_000 fs).
    pub fn read_counter(&self) -> u64 {
        if !self.enabled {
            return self.base_counter;
        }
        let elapsed_ns = self.start_time.elapsed().as_nanos() as u64;
        let ticks = elapsed_ns / 10;
        self.base_counter.wrapping_add(ticks)
    }

    pub fn write_counter(&mut self, val: u64) {
        self.base_counter = val;
        self.start_time = Instant::now();
    }

    pub fn advance_ticks(&mut self, ticks: u64) {
        let current = self.read_counter();
        self.base_counter = current.wrapping_add(ticks);
        if self.enabled {
            self.start_time = Instant::now();
        }
    }

    pub fn matches_address(&self, addr: u64) -> bool {
        addr >= HPET_MMIO_BASE && addr < (HPET_MMIO_BASE + HPET_MMIO_SIZE)
    }

    /// Lectura MMIO de 1, 2, 4 u 8 bytes
    pub fn read(&mut self, offset: u64, size: usize) -> Vec<u8> {
        if offset >= HPET_MMIO_SIZE || size == 0 {
            return vec![0xFF; size];
        }
        let reg_base = (offset & !7) as usize;
        let byte_off = (offset & 7) as usize;
        let val = self.read_reg(reg_base);
        let bytes = val.to_le_bytes();

        let mut out = vec![0u8; size];
        for i in 0..size {
            if byte_off + i < 8 {
                out[i] = bytes[byte_off + i];
            }
        }
        out
    }

    pub fn read_reg(&mut self, off: usize) -> u64 {
        match off {
            // General Capabilities & ID: 64-bit, 3 timers, legacy route capable, clk_period = 100_000_000 fs
            HPET_REG_GCAP_ID => {
                let num_tim_cap: u64 = (NUM_TIMERS as u64 - 1) << 8; // bits 8..12 (2 = 3 timers)
                let count_size_cap: u64 = 1 << 13;                  // bit 13 (64-bit)
                let leg_route_cap: u64 = 1 << 15;                   // bit 15 (legacy route capable)
                let vendor_id: u64 = 0x8086 << 16;                  // bits 16..31 (Intel)
                let period: u64 = HPET_CLK_PERIOD_FS << 32;          // bits 32..63 (100_000_000 fs)
                let rev_id: u64 = 0x01;
                period | vendor_id | leg_route_cap | count_size_cap | num_tim_cap | rev_id
            }

            HPET_REG_GEN_CONF => {
                let mut v = 0u64;
                if self.enabled {
                    v |= HPET_GEN_CONF_ENABLE;
                }
                if self.legacy_route {
                    v |= HPET_GEN_CONF_LEG_RT;
                }
                v
            }

            HPET_REG_GINTR_STA => self.interrupt_status as u64,

            HPET_REG_MAIN_CNT => self.read_counter(),

            0x100..=0x15F => {
                let t_idx = (off - 0x100) / 0x20;
                let reg_off = (off - 0x100) % 0x20;
                if t_idx < NUM_TIMERS {
                    let tmr = &self.timers[t_idx];
                    match reg_off {
                        HPET_TN_CONF => tmr.config,
                        HPET_TN_COMP => tmr.comparator,
                        HPET_TN_FSB => tmr.fsb_route,
                        _ => 0,
                    }
                } else {
                    0
                }
            }

            _ => 0,
        }
    }

    /// Escritura MMIO de 1, 2, 4 u 8 bytes
    pub fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= HPET_MMIO_SIZE || data.is_empty() {
            return;
        }
        let reg_base = (offset & !7) as usize;
        let byte_off = (offset & 7) as usize;
        let cur_val = self.read_reg(reg_base);
        let mut bytes = cur_val.to_le_bytes();

        for (i, &b) in data.iter().enumerate() {
            if byte_off + i < 8 {
                bytes[byte_off + i] = b;
            }
        }
        let new_val = u64::from_le_bytes(bytes);
        self.write_reg(reg_base, new_val);
    }

    pub fn write_reg(&mut self, off: usize, val: u64) {
        match off {
            HPET_REG_GEN_CONF => {
                let was_enabled = self.enabled;
                self.enabled = (val & HPET_GEN_CONF_ENABLE) != 0;
                self.legacy_route = (val & HPET_GEN_CONF_LEG_RT) != 0;
                if !was_enabled && self.enabled {
                    self.start_time = Instant::now();
                } else if was_enabled && !self.enabled {
                    self.base_counter = self.read_counter();
                }
            }

            HPET_REG_GINTR_STA => {
                // Write-1-to-clear
                self.interrupt_status &= !(val as u32);
            }

            HPET_REG_MAIN_CNT => {
                self.write_counter(val);
            }

            0x100..=0x15F => {
                let t_idx = (off - 0x100) / 0x20;
                let reg_off = (off - 0x100) % 0x20;
                if t_idx < NUM_TIMERS {
                    match reg_off {
                        HPET_TN_CONF => {
                            let tmr = &mut self.timers[t_idx];
                            // Preservar bits read-only
                            let ro_mask = HPET_TN_PER_CAP | HPET_TN_64BIT_CAP | HPET_TN_FSB_CAP | (0xFFFF_FFFFu64 << 32);
                            let ro_bits = tmr.config & ro_mask;
                            let rw_bits = val & !ro_mask;
                            tmr.config = ro_bits | rw_bits;
                            if (val & HPET_TN_SETVAL) != 0 {
                                tmr.val_set = true;
                            }
                        }

                        HPET_TN_COMP => {
                            let now_cnt = self.read_counter();
                            let tmr = &mut self.timers[t_idx];
                            if tmr.is_periodic() && tmr.val_set {
                                tmr.period = val;
                                tmr.comparator = now_cnt.wrapping_add(val);
                                tmr.val_set = false;
                            } else {
                                tmr.comparator = val;
                                if tmr.is_periodic() {
                                    tmr.period = val;
                                }
                            }
                            tmr.fired = false;
                        }

                        HPET_TN_FSB => {
                            self.timers[t_idx].fsb_route = val;
                        }

                        _ => {}
                    }
                }
            }

            _ => {}
        }
    }

    /// Retorna la línea IRQ asignada al temporizador según legacy routing
    pub fn get_timer_irq(&self, timer_idx: usize) -> u8 {
        if self.legacy_route {
            match timer_idx {
                0 => 0, // Legacy PIT replacement (IRQ0 o IRQ2)
                1 => 8, // Legacy RTC replacement (IRQ8)
                _ => self.timers[timer_idx].route_irq(),
            }
        } else {
            self.timers[timer_idx].route_irq()
        }
    }

    /// Evalúa los comparadores frente al contador principal y devuelve las líneas IRQ activadas.
    pub fn check_irqs(&mut self) -> Vec<u8> {
        let mut irqs = Vec::new();
        if !self.enabled {
            return irqs;
        }

        let cnt = self.read_counter();

        for i in 0..NUM_TIMERS {
            let (is_enabled, is_periodic, period, comparator, fired) = {
                let tmr = &self.timers[i];
                (tmr.is_enabled(), tmr.is_periodic(), tmr.period, tmr.comparator, tmr.fired)
            };

            if !is_enabled {
                continue;
            }

            if is_periodic && period > 0 {
                if cnt >= comparator {
                    let irq = self.get_timer_irq(i);
                    self.interrupt_status |= 1 << i;
                    irqs.push(irq);

                    let tmr = &mut self.timers[i];
                    while cnt >= tmr.comparator && tmr.period > 0 {
                        tmr.comparator = tmr.comparator.wrapping_add(tmr.period);
                    }
                }
            } else if cnt >= comparator && !fired {
                self.timers[i].fired = true;
                let irq = self.get_timer_irq(i);
                self.interrupt_status |= 1 << i;
                irqs.push(irq);
            }
        }

        irqs
    }

    /// Evalúa los comparadores y retorna máscara de bits de temporizadores disparados
    pub fn check_interrupts(&mut self) -> u32 {
        if !self.enabled {
            return 0;
        }

        let cnt = self.read_counter();
        let mut irq_mask = 0u32;

        for (i, tmr) in self.timers.iter_mut().enumerate() {
            if !tmr.is_enabled() {
                continue;
            }

            if cnt >= tmr.comparator {
                irq_mask |= 1 << i;
                self.interrupt_status |= 1 << i;

                if tmr.is_periodic() && tmr.period > 0 {
                    while cnt >= tmr.comparator {
                        tmr.comparator = tmr.comparator.wrapping_add(tmr.period);
                    }
                }
            }
        }

        irq_mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hpet_initial_capabilities() {
        let mut hpet = HpetDevice::new();
        let gcap = hpet.read_reg(HPET_REG_GCAP_ID);
        // Period debe ser 100_000_000 fs (10 ns)
        assert_eq!((gcap >> 32) as u64, HPET_CLK_PERIOD_FS);
        // Vendor ID 0x8086
        assert_eq!(((gcap >> 16) & 0xFFFF) as u16, 0x8086);
        // 3 timers: NUM_TIM_CAP = 2
        assert_eq!(((gcap >> 8) & 0x1F), 2);
        // 64-bit capable
        assert_ne!(gcap & (1 << 13), 0);
        // Legacy replacement route capable
        assert_ne!(gcap & (1 << 15), 0);
    }

    #[test]
    fn test_hpet_counter_enable_and_monotonicity() {
        let mut hpet = HpetDevice::new();
        assert_eq!(hpet.read_counter(), 0);

        hpet.write_reg(HPET_REG_GEN_CONF, HPET_GEN_CONF_ENABLE);
        assert!(hpet.enabled);

        std::thread::sleep(std::time::Duration::from_millis(5));
        let c1 = hpet.read_counter();
        assert!(c1 > 0, "Counter should increment when enabled");

        std::thread::sleep(std::time::Duration::from_millis(5));
        let c2 = hpet.read_counter();
        assert!(c2 > c1, "Counter must be monotonic");

        // Avance manual de ticks
        hpet.advance_ticks(5000);
        let c3 = hpet.read_counter();
        assert!(c3 >= c2 + 5000);
    }

    #[test]
    fn test_hpet_periodic_comparator_firing() {
        let mut hpet = HpetDevice::new();
        hpet.write_reg(HPET_REG_GEN_CONF, HPET_GEN_CONF_ENABLE);

        // Configurar Timer 0: habilitar interrupción periódica
        let t0_conf = HPET_TN_ENABLE | HPET_TN_PERIODIC | HPET_TN_SETVAL | (2 << 9);
        hpet.write_reg(0x100, t0_conf);
        hpet.write_reg(0x108, 1_000_000); // Periodo de 1_000_000 ticks (10 ms)

        // Simular avance del contador
        hpet.write_counter(500_000);
        let irqs = hpet.check_irqs();
        assert!(irqs.is_empty(), "No interrupt before comparator");

        hpet.write_counter(1_500_000);
        let irqs = hpet.check_irqs();
        assert_eq!(irqs, vec![2], "Timer 0 should have fired IRQ 2");
        assert!(hpet.timers[0].comparator > 1_500_000, "Comparator should have advanced");
    }

    #[test]
    fn test_hpet_legacy_replacement_routing() {
        let mut hpet = HpetDevice::new();

        // Configurar Timer 0 y Timer 1 con rutas explícitas
        // Timer 0 -> ruta 10
        let cfg0 = HPET_TN_ENABLE | (10 << 9);
        hpet.write_reg(0x100, cfg0);
        hpet.write_reg(0x108, 100);

        // Timer 1 -> ruta 11
        let cfg1 = HPET_TN_ENABLE | (11 << 9);
        hpet.write_reg(0x120, cfg1);
        hpet.write_reg(0x128, 100);

        // Caso 1: Legacy route desactivado -> IRQ 10 y 11
        hpet.write_reg(HPET_REG_GEN_CONF, HPET_GEN_CONF_ENABLE);
        hpet.write_counter(150);
        let irqs_non_leg = hpet.check_irqs();
        assert_eq!(irqs_non_leg, vec![10, 11]);

        // Reset y Caso 2: Legacy route activado -> Timer 0 a IRQ0, Timer 1 a IRQ8
        hpet.reset();
        hpet.write_reg(0x100, cfg0);
        hpet.write_reg(0x108, 100);
        hpet.write_reg(0x120, cfg1);
        hpet.write_reg(0x128, 100);

        hpet.write_reg(HPET_REG_GEN_CONF, HPET_GEN_CONF_ENABLE | HPET_GEN_CONF_LEG_RT);
        hpet.write_counter(150);
        let irqs_leg = hpet.check_irqs();
        assert_eq!(irqs_leg, vec![0, 8]);
    }

    #[test]
    fn test_hpet_sub_dword_mmio_access() {
        let mut hpet = HpetDevice::new();

        // Leer segunda mitad de GCAP_ID (offset 4, size 4)
        let bytes = hpet.read(0x004, 4);
        let upper = u32::from_le_bytes(bytes.try_into().unwrap());
        assert_eq!(upper as u64, HPET_CLK_PERIOD_FS);

        // Escribir GEN_CONF mediante escritura de 4 bytes en offset 0x010
        hpet.write(0x010, &1u32.to_le_bytes());
        assert!(hpet.enabled);

        // GINTR_STA W1C
        hpet.interrupt_status = 0x05;
        hpet.write(0x020, &1u32.to_le_bytes()); // Limpiar bit 0
        assert_eq!(hpet.interrupt_status, 0x04);
    }
}
