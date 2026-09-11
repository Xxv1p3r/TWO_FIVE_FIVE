//! PIC 8259 (0x20/0xA0), PIT 8254 (0x40-0x43), Puerto B (0x61),
//! controlador PS/2 (0x60/0x64), y DMA controller (0x00-0x1F).
//!
//! El PIT decrementa el contador basándose en tiempo real (Instant),
//! no por lectura. Esto hace que los delays de SeaBIOS funcionen a
//! velocidad real aunque cada KVM exit tome ~25μs.

use super::IoDevice;
use std::time::Instant;

// ─── PIC 8259 maestro/esclavo ──────────────────────────────────────
const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

// OCW2 bits
const OCW2_EOI: u8 = 0x20;

// ─── PIT 8254 ──────────────────────────────────────────────────────
const PIT_CH0: u16 = 0x40;
const PIT_CTRL: u16 = 0x43;

/// Frecuencia base del PIT en Hz.
const PIT_HZ: u32 = 1_193_182;



/// Un canal del PIT con decremento basado en tiempo real.
/// Usamos u32 internamente para evitar truncamiento al hacer 65536 as u16 = 0.
struct PitChannel {
    /// Valor de reload programado (16 bits visibles, pero guardamos u32 para
    /// poder representar 65536 como valor explícito cuando reload==0).
    reload: u32,
    /// Contador actual (u32 internamente; 0 = counter reached zero).
    counter: u32,
    /// Contador latched (freeze) para lectura consistente.
    latched_counter: Option<u32>,
    /// Modo de operación (0-5).
    mode: u8,
    /// True si opera en BCD.
    bcd: bool,
    /// Fase de lectura/escritura: byte bajo (false) o alto (true).
    read_high: bool,
    /// Modo RW: 0=latch, 1=low only, 2=high only, 3=low+high.
    rw_mode: u8,
    /// True si el canal está habilitado.
    enabled: bool,
    /// Momento del último tick.
    last_tick: Instant,
}

impl PitChannel {
    fn new() -> Self {
        Self {
            reload: 0,
            counter: 0,
            latched_counter: None,
            mode: 0,
            bcd: false,
            read_high: false,
            rw_mode: 0,
            enabled: false,
            last_tick: Instant::now(),
        }
    }

    /// Valor efectivo del reload (0 en PIT hardware = 65536).
    fn effective_reload(&self) -> u32 {
        if self.reload == 0 { 65536 } else { self.reload }
    }

    /// Retorna el valor actual del contador para lectura (16 bits).
    fn current(&self) -> u16 {
        let val = self.latched_counter.unwrap_or(self.counter);
        // Si counter == 65536, devolver 0 (hardware behavior: 0 = 65536)
        if val > 0xFFFF { 0 } else { val as u16 }
    }

    /// Decrementa el contador según el tiempo REAL transcurrido.
    /// Retorna true si hubo underflow (IRQ generada).
    #[allow(dead_code)]
    fn tick(&mut self) -> bool {
        if !self.enabled { return false; }
        let now = Instant::now();
        let elapsed_ns = now.duration_since(self.last_tick).as_nanos() as u64;
        self.last_tick = now;
        // Calcular cuántos ticks del PIT (1.193182 MHz) caben en el tiempo transcurrido.
        let ticks = ((elapsed_ns * PIT_HZ as u64) / 1_000_000_000) as u32;
        if ticks == 0 { return false; }

        let reload = self.effective_reload(); // 65536 si reload==0
        let auto_reload = matches!(self.mode, 2 | 3 | 5);

        // En modo one-shot (0, 4): si counter ya es 0, no hacer nada más.
        if !auto_reload && self.counter == 0 { return false; }

        // El valor activo del counter: si counter==0 y está habilitado,
        // significa 65536 (el caso reload=0 en hardware PIT).
        let effective = if self.counter == 0 { reload } else { self.counter };
        let new_val = effective.saturating_sub(ticks);

        if new_val == 0 {
            self.counter = 0;
            if auto_reload {
                self.counter = reload; // reload=0 → counter=0 → next tick starts from 65536
            }
            return true; // underflow → IRQ
        }
        self.counter = new_val;
        false
    }

    /// Avanza el counter por un número fijo de ticks (sin Instant).
    fn advance(&mut self, ticks: u32) -> bool {
        if !self.enabled { return false; }
        let reload = self.effective_reload();
        let auto_reload = matches!(self.mode, 2 | 3 | 5);
        if !auto_reload && self.counter == 0 { return false; }
        let effective = if self.counter == 0 { reload } else { self.counter };
        let new_val = effective.saturating_sub(ticks);
        if new_val == 0 {
            self.counter = 0;
            if auto_reload { self.counter = reload; }
            return true;
        }
        self.counter = new_val;
        false
    }

    #[allow(dead_code)]
    fn reset(&mut self) {
        self.reload = 0;
        self.counter = 0;
        self.latched_counter = None;
        self.mode = 0;
        self.bcd = false;
        self.read_high = false;
        self.rw_mode = 0;
        self.enabled = false;
        self.last_tick = Instant::now();
    }
}



// ─── Controlador PS/2 (8042) ───────────────────────────────────────
const PS2_DATA: u16 = 0x60;
const PS2_STATUS: u16 = 0x64;
const PS2_CMD: u16 = 0x64;

const PS2_OUTBUF_FULL: u8 = 1 << 0;
/// Bit 2: system flag — el self-test del 8042 pasó (POST OK).
const PS2_SYS_FLAG: u8 = 1 << 2;

/// Estado del 8042: comando previo que consume un byte de datos por 0x60.
/// Sin esta máquina de estados, un comando como 0x60 (write command byte)
/// recibía un ACK espurio que envenenaba el output buffer durante el
/// keyboard_init de SeaBIOS.
#[derive(Clone, Copy)]
enum Ps2Expect {
    /// Dato normal: va al dispositivo de teclado (responde ACK 0xFA).
    None,
    /// 0x60: escribir el command byte del controlador.
    CmdByte,
    /// 0xD1: escribir el output port.
    OutPort,
    /// 0xD2: escribir un byte directo al output buffer (genera IRQ1).
    KbOutBuf,
    /// 0xD3: escribir un byte al buffer auxiliar (ratón).
    AuxOut,
    /// 0xD4: escribir un byte al dispositivo auxiliar.
    AuxDev,
}

// ─── DMA Controller (8237) - stub ──────────────────────────────────
/// Puertos del DMA controller maestro (0x00-0x0F) y esclavo (0xC0-0xDF).
/// Plus puertos de manage page registers (0x81-0x83, 0x87).
fn is_dma_port(port: u16) -> bool {
    matches!(port,
        0x00..=0x0F |   // DMA maestro (includes 0x08, 0x0C)
        0x81..=0x83 |   // DMA page registers (ch 2,3,1)
        0x87 |           // DMA page register (ch 0)
        0xC0..=0xDF     // DMA esclavo (16-bit, includes 0xD0-0xDE)
    )
}

pub struct LegacyInterrupts {
    // PIC state
    pic1_irr: u8,
    pic1_isr: u8,
    pic1_mask: u8,
    pic1_icw2: u8,
    pic1_icw3: u8,
    pic1_icw_step: u8,
    pic1_read_reg: u8,
    pic1_auto_eoi: bool,

    pic2_irr: u8,
    pic2_isr: u8,
    pic2_mask: u8,
    pic2_icw2: u8,
    pic2_icw3: u8,
    pic2_icw_step: u8,
    pic2_read_reg: u8,
    pic2_auto_eoi: bool,

    // PIT
    pit: [PitChannel; 3],

    // PS/2
    ps2_out_buf: std::collections::VecDeque<u8>,
    ps2_cmd_byte: u8,
    /// Comando previo que consume el siguiente byte escrito en 0x60.
    ps2_expect: Ps2Expect,
    /// Bit 0 del command byte: OBF lleno genera IRQ1. Queda a true hasta
    /// que el guest programe el command byte por primera vez.
    kb_irq_enabled: bool,

    // Puerto B (0x61): speaker + gate PIT ch2
    port_b: u8,

    // Diagnostic: track if IRQ1 needs injection into kernel PIC
    irq1_pending: bool,
    // Diagnostic counters
    pub ps2_access_count: u32,
    pub pit_access_count: u32,
}

impl LegacyInterrupts {
    pub fn new() -> Self {
        Self {
            pic1_irr: 0,
            pic1_isr: 0,
            pic1_mask: 0xFF,
            pic1_icw2: 0x08,
            pic1_icw3: 0x04, // IRQ2 = cascade
            pic1_icw_step: 0,
            pic1_read_reg: 0,
            pic1_auto_eoi: false,

            pic2_irr: 0,
            pic2_isr: 0,
            pic2_mask: 0xFF,
            pic2_icw2: 0x70,
            pic2_icw3: 0x02, // connected to IRQ2 on master
            pic2_icw_step: 0,
            pic2_read_reg: 0,
            pic2_auto_eoi: false,

            pit: [PitChannel::new(), PitChannel::new(), PitChannel::new()],

            ps2_out_buf: std::collections::VecDeque::new(),
            ps2_cmd_byte: 0x01,
            ps2_expect: Ps2Expect::None,
            kb_irq_enabled: true,
            port_b: 0,

            irq1_pending: false,
            ps2_access_count: 0,
            pit_access_count: 0,
        }
    }

    /// Returns true if IRQ1 needs to be injected into the kernel PIC.
    /// After calling this, the flag is cleared.
    pub fn take_irq1_pending(&mut self) -> bool {
        if self.irq1_pending {
            self.irq1_pending = false;
            true
        } else {
            false
        }
    }

    /// Inyecta un scancode del host en el output buffer del 8042.
    /// El handler INT 09h del guest lo leerá por el puerto 0x60.
    pub fn inject_scancode(&mut self, scancode: u8) {
        self.push_out_buf(scancode);
    }

    /// Hay bytes sin leer en el output buffer del 8042.
    /// API de diagnóstico/tests; el bucle VMM usa `take_irq1_pending()`.
    #[allow(dead_code)]
    pub fn ps2_has_data(&self) -> bool {
        !self.ps2_out_buf.is_empty()
    }

    /// Encola un byte en el output buffer y marca IRQ1 como pendiente
    /// (si el command byte tiene habilitada la interrupción del teclado).
    /// Tope de 16 bytes: si el guest no lee, se descartan los más viejos
    /// para no crecer sin límite con KeyRepeat del host.
    fn push_out_buf(&mut self, val: u8) {
        if self.ps2_out_buf.len() >= 16 {
            self.ps2_out_buf.pop_front();
        }
        self.ps2_out_buf.push_back(val);
        if self.kb_irq_enabled {
            self.irq1_pending = true;
        }
    }

    /// Levanta la línea IRQ `line` (0-15) en el PIC.
    #[allow(dead_code)]
    pub fn raise_irq(&mut self, line: u8) {
        match line {
            0..=7 => { self.pic1_irr |= 1 << line; }
            8..=15 => {
                self.pic2_irr |= 1 << (line - 8);
                self.pic1_irr |= 1 << 2; // cascada
            }
            _ => {}
        }
    }

    /// Baja la línea IRQ `line`.
    #[allow(dead_code)]
    pub fn lower_irq(&mut self, line: u8) {
        match line {
            0..=7 => { self.pic1_irr &= !(1 << line); }
            8..=15 => {
                self.pic2_irr &= !(1 << (line - 8));
                if self.pic2_irr == 0 && self.pic2_isr == 0 {
                    self.pic1_irr &= !(1 << 2);
                }
            }
            _ => {}
        }
    }

    /// Retorna Some(vector) si hay un IRQ pendiente no enmascarado.
    ///
    /// (tarea 4) Sombra del PIC en userspace: el kernel irqchip gestiona la
    /// entrega real, así que estos métodos ya no tienen llamador en el bucle
    /// VMM. Se conservan como estado de diagnóstico (ICW/OCW/EOI/mascaras
    /// que escribe el guest) y para un futuro modo userspace-irqchip.
    #[allow(dead_code)]
    pub fn pending_irq(&self) -> Option<u8> {
        if let Some(vec) = self.pending_pic_irq(&self.pic2_irr, &self.pic2_isr, &self.pic2_mask) {
            return Some(vec);
        }
        self.pending_pic_irq(&self.pic1_irr, &self.pic1_isr, &self.pic1_mask)
    }

    #[allow(dead_code)]
    fn pending_pic_irq(&self, irr: &u8, isr: &u8, mask: &u8) -> Option<u8> {
        let pending = *irr & !*mask;
        if pending == 0 { return None; }
        for bit in 0..8u8 {
            let mask_bit = 1u8 << bit;
            if *isr & mask_bit != 0 { return None; }
            if pending & mask_bit != 0 { return Some(bit); }
        }
        None
    }

    fn pic_eoi(&mut self, is_master: bool, irq_num: u8) {
        if is_master {
            if self.pic1_auto_eoi { self.pic1_isr = 0; }
            else { self.pic1_isr &= !(1 << irq_num); }
        } else {
            if self.pic2_auto_eoi { self.pic2_isr = 0; }
            else { self.pic2_isr &= !(1 << irq_num); }
        }
    }

    #[allow(dead_code)]
    pub fn irq_vector(&self, irq: u8) -> u8 { self.pic1_icw2 + irq }
    #[allow(dead_code)]
    pub fn irq_vector_slave(&self, irq: u8) -> u8 { self.pic2_icw2 + irq }

    /// Tickea TODOS los canales del PIT usando tiempo real.
    #[allow(dead_code)]
    pub fn pit_tick(&mut self) -> bool {
        let irq0 = self.pit[0].tick();
        self.pit[1].tick();
        self.pit[2].tick();
        irq0
    }

    /// Avanza el PIT por un número fijo de ticks (sin usar Instant).
    /// Útil para cuando el guest está stuck sin exits y queremos
    /// simular tiempo real transcurrido.
    pub fn pit_advance_ticks(&mut self, ticks: u32) -> bool {
        let mut irq = false;
        for ch in &mut self.pit {
            if ch.advance(ticks) {
                irq = true;
            }
        }
        irq
    }

    fn pic_read(&mut self, port: u16) -> u8 {
        match port {
            PIC1_CMD => if self.pic1_read_reg == 1 { self.pic1_isr } else { self.pic1_irr },
            PIC2_CMD => if self.pic2_read_reg == 1 { self.pic2_isr } else { self.pic2_irr },
            PIC1_DATA => self.pic1_mask,
            PIC2_DATA => self.pic2_mask,
            _ => 0,
        }
    }

    fn pic_write(&mut self, port: u16, val: u8) {
        match port {
            PIC1_CMD => {
                if val & 0x10 != 0 {
                    self.pic1_icw_step = 1;
                    self.pic1_irr = 0;
                    self.pic1_isr = 0;
                    self.pic1_mask = 0xFF;
                    self.pic1_auto_eoi = false;
                } else if val & 0x08 == 0 {
                    if val & OCW2_EOI != 0 {
                        if val & 0x40 != 0 {
                            self.pic_eoi(true, val & 0x07);
                        } else {
                            for bit in 0..8u8 {
                                if self.pic1_isr & (1 << bit) != 0 {
                                    self.pic1_isr &= !(1 << bit);
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    if val & 0x02 != 0 { self.pic1_read_reg = val & 0x01; }
                }
            }
            PIC2_CMD => {
                if val & 0x10 != 0 {
                    self.pic2_icw_step = 1;
                    self.pic2_irr = 0;
                    self.pic2_isr = 0;
                    self.pic2_mask = 0xFF;
                } else if val & 0x08 != 0 {
                    if val & 0x02 != 0 { self.pic2_read_reg = val & 0x01; }
                }
            }
            PIC1_DATA => {
                match self.pic1_icw_step {
                    1 => { self.pic1_icw2 = val & 0xF8; self.pic1_icw_step = 2; }
                    2 => { self.pic1_icw3 = val; self.pic1_icw_step = 3; }
                    3 => { self.pic1_auto_eoi = val & 0x02 != 0; self.pic1_icw_step = 0; }
                    _ => self.pic1_mask = val,
                }
            }
            PIC2_DATA => {
                match self.pic2_icw_step {
                    1 => { self.pic2_icw2 = val & 0xF8; self.pic2_icw_step = 2; }
                    2 => { self.pic2_icw3 = val; self.pic2_icw_step = 3; }
                    3 => { self.pic2_auto_eoi = val & 0x02 != 0; self.pic2_icw_step = 0; }
                    _ => self.pic2_mask = val,
                }
            }
            _ => {}
        }
    }

    // ─── PIT ────────────────────────────────────────────────────────
    fn pit_read(&mut self, port: u16) -> u8 {
        let ch = (port - PIT_CH0) as usize;
        if ch >= 3 { return 0; }
        let cur = self.pit[ch].current();
        match self.pit[ch].rw_mode {
            3 => {
                let b = if self.pit[ch].read_high { (cur >> 8) as u8 } else { cur as u8 };
                self.pit[ch].read_high = !self.pit[ch].read_high;
                if !self.pit[ch].read_high { self.pit[ch].latched_counter = None; }
                b
            }
            1 => cur as u8,
            2 => (cur >> 8) as u8,
            _ => cur as u8,
        }
    }

    fn pit_write(&mut self, port: u16, val: u8) {
        let ch = (port - PIT_CH0) as usize;
        if ch >= 3 { return; }

        match self.pit[ch].rw_mode {
            3 => {
                if self.pit[ch].read_high {
                    self.pit[ch].reload = (self.pit[ch].reload & 0x00FF) | ((val as u32) << 8);
                    self.pit[ch].counter = self.pit[ch].reload;
                    self.pit[ch].enabled = true;
                    self.pit[ch].last_tick = Instant::now();
                } else {
                    self.pit[ch].reload = (self.pit[ch].reload & 0xFF00) | val as u32;
                }
                self.pit[ch].read_high = !self.pit[ch].read_high;
            }
            1 => {
                self.pit[ch].reload = val as u32;
                self.pit[ch].counter = self.pit[ch].reload;
                self.pit[ch].enabled = true;
                self.pit[ch].last_tick = Instant::now();
            }
            2 => {
                self.pit[ch].reload = (val as u32) << 8;
                self.pit[ch].counter = self.pit[ch].reload;
                self.pit[ch].enabled = true;
                self.pit[ch].last_tick = Instant::now();
            }
            _ => {} // rw_mode == 0: latch command
        }
    }

    fn pit_write_ctrl(&mut self, val: u8) {
        let ch = ((val >> 6) & 0x3) as usize;
        let rw = (val >> 4) & 0x3;

        // Read-back command (8254): bits 7-6 = 11
        if ch >= 3 {
            let latch_count = val & 0x20 == 0;
            for i in 0..3u8 {
                if val & (1 << i) != 0 {
                    if latch_count {
                        self.pit[i as usize].latched_counter = Some(self.pit[i as usize].counter);
                    }
                }
            }
            return;
        }

        self.pit[ch].bcd = val & 1 != 0;
        self.pit[ch].mode = (val >> 1) & 0x7;

        if rw == 0 {
            // Comando latch: congelar counter.
            self.pit[ch].latched_counter = Some(self.pit[ch].counter);
            self.pit[ch].read_high = false;
            return;
        }

        self.pit[ch].rw_mode = rw;
        self.pit[ch].read_high = false;
    }

    // ─── PS/2 ───────────────────────────────────────────────────────
    fn ps2_read(&mut self, port: u16) -> u8 {
        self.ps2_access_count += 1;
        match port {
            PS2_DATA => {
                let val = self.ps2_out_buf.pop_front().unwrap_or(0xFF);
                if self.ps2_out_buf.is_empty() {
                    // Último byte consumido: dar por entregada la IRQ1.
                    self.irq1_pending = false;
                } else {
                    // Quedan bytes: re-armar el flanco para que el bucle VMM
                    // vuelva a pulsar IRQ1 y el guest lea el siguiente.
                    // Sin esto, la segunda tecla encadenada se pierde: la
                    // línea ya está alta y un edge-triggered PIC no genera
                    // una nueva interrupción sin un flanco de bajada.
                    self.irq1_pending = self.kb_irq_enabled;
                }
                val
            }
            PS2_STATUS => {
                // bit0 OBF: hay dato listo para leer.
                // bit1 IBF: 0 — el controlador consume los escritos del host
                //   síncronamente, así que SIEMPRE se puede escribir. Antes
                //   se devolvía fijo a 1 y cada i8042_wait_write de SeaBIOS
                //   se quedaba esperando hasta el timeout.
                // bit2 system flag: self-test OK.
                let mut st = PS2_SYS_FLAG;
                if !self.ps2_out_buf.is_empty() { st |= PS2_OUTBUF_FULL; }
                st
            }
            _ => 0,
        }
    }

    fn ps2_write(&mut self, port: u16, val: u8) {
        match port {
            PS2_CMD => self.ps2_command(val),
            PS2_DATA => match self.ps2_expect {
                Ps2Expect::CmdByte => {
                    // Command byte del controlador: bit0 habilita IRQ1 (OBF).
                    self.ps2_cmd_byte = val;
                    self.kb_irq_enabled = val & 0x01 != 0;
                    self.ps2_expect = Ps2Expect::None;
                }
                Ps2Expect::OutPort => {
                    // El bit1 (A20) lo gestiona el A20Gate por el puerto 0x92.
                    self.ps2_expect = Ps2Expect::None;
                }
                Ps2Expect::KbOutBuf => {
                    // 0xD2: byte directo al output buffer (simula tecla).
                    self.push_out_buf(val);
                    self.ps2_expect = Ps2Expect::None;
                }
                Ps2Expect::AuxOut | Ps2Expect::AuxDev => {
                    // Ratón: sin emulación de dispositivo, se ignora.
                    self.ps2_expect = Ps2Expect::None;
                }
                Ps2Expect::None => {
                    // Dato directo al dispositivo de teclado (p. ej. 0xF4
                    // enable scanning): el dispositivo responde ACK (0xFA),
                    // que SeaBIOS espera en keyboard_init.
                    self.push_out_buf(0xFA);
                }
            },
            _ => {}
        }
    }

    /// Procesa un comando escrito en el puerto 0x64 del 8042.
    fn ps2_command(&mut self, val: u8) {
        match val {
            // Comandos que consumen un byte de datos por el puerto 0x60:
            // (NO generan respuesta: el ACK de antes envenenaba el buffer)
            0x60 => self.ps2_expect = Ps2Expect::CmdByte,  // write command byte
            0xD1 => self.ps2_expect = Ps2Expect::OutPort,  // write output port
            0xD2 => self.ps2_expect = Ps2Expect::KbOutBuf, // write kb output buf
            0xD3 => self.ps2_expect = Ps2Expect::AuxOut,   // write aux output buf
            0xD4 => self.ps2_expect = Ps2Expect::AuxDev,   // write aux device
            // Comandos con respuesta inmediata en el output buffer:
            0x20 => self.push_out_buf(self.ps2_cmd_byte),  // read command byte
            0xAA => self.push_out_buf(0x55), // self-test OK
            0xAB => self.push_out_buf(0x00), // test primer puerto OK
            0xA9 => self.push_out_buf(0x00), // test segundo puerto OK
            0xD0 => self.push_out_buf(0x00), // read output port
            0xC0 | 0xC1 | 0xC2 | 0xE0 => self.push_out_buf(0x00), // read input/info
            // Enable/disable de puertos y defaults: sin respuesta.
            0xAE | 0xAD | 0xA7 | 0xA8 | 0xF6 => {}
            // Pulse output port (0xF0-0xF3) y comandos desconocidos: sin
            // respuesta. Responder ACK a lo desconocido es tan incorrecto
            // como peligroso: el BIOS lee bytes que no esperaba.
            _ => {}
        }
    }

    /// Marca un IRQ como "en servicio" dado el vector.
    #[allow(dead_code)]
    pub fn acknowledge_irq(&mut self, vector: u8) {
        let pic1_base = self.pic1_icw2;
        let pic2_base = self.pic2_icw2;
        if vector >= pic1_base && vector < pic1_base + 8 {
            let irq = vector - pic1_base;
            self.pic1_isr |= 1 << irq;
            self.pic1_irr &= !(1 << irq);
        } else if vector >= pic2_base && vector < pic2_base + 8 {
            let irq = vector - pic2_base;
            self.pic2_isr |= 1 << irq;
            self.pic2_irr &= !(1 << irq);
            self.pic1_isr |= 1 << 2;
            self.pic1_irr &= !(1 << 2);
        }
    }

    #[allow(dead_code)]
    pub fn ch0_hz(&self) -> u32 {
        let r = self.pit[0].effective_reload();
        PIT_HZ / r as u32
    }
}

impl Default for LegacyInterrupts {
    fn default() -> Self { Self::new() }
}

impl IoDevice for LegacyInterrupts {
    fn matches_port(&self, port: u16) -> bool {
        matches!(
            port,
            PIC1_CMD | PIC1_DATA | PIC2_CMD | PIC2_DATA
                | PIT_CH0..=PIT_CTRL
                | 0x61
                | PS2_DATA | PS2_STATUS
        ) || is_dma_port(port)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() { return; }
        let val = data[0];
        match port {
            PIC1_CMD | PIC1_DATA | PIC2_CMD | PIC2_DATA => self.pic_write(port, val),
            PIT_CH0..=0x42 => self.pit_write(port, val),
            PIT_CTRL => self.pit_write_ctrl(val),
            0x61 => self.port_b = val,
            PS2_DATA | PS2_STATUS => self.ps2_write(port, val),
            _ => {} // DMA ports: silently ignore writes
        }
    }

    fn read(&mut self, port: u16, _count: usize) -> Vec<u8> {
        match port {
            PIC1_CMD | PIC1_DATA | PIC2_CMD | PIC2_DATA => vec![self.pic_read(port)],
            PIT_CH0..=0x42 => {
                self.pit_access_count += 1;
                vec![self.pit_read(port)]
            }
            PIT_CTRL => vec![0],
            0x61 => vec![self.port_b & 0x0F | 0x10],
            PS2_DATA | PS2_STATUS => vec![self.ps2_read(port)],
            _ => vec![0xFF], // DMA and unknown: return 0xFF (device not present)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(d: &mut LegacyInterrupts) -> u8 {
        d.read(PS2_STATUS, 1)[0]
    }

    #[test]
    fn status_ibf_clear_and_sysflag_set() {
        let mut d = LegacyInterrupts::new();
        let st = status(&mut d);
        // bit1 (IBF) = 0: el host puede escribir sin esperar timeouts.
        assert_eq!(st & 0x02, 0);
        assert_eq!(st & PS2_OUTBUF_FULL, 0);
        // bit2 system flag: POST OK.
        assert_eq!(st & PS2_SYS_FLAG, PS2_SYS_FLAG);
    }

    #[test]
    fn scancode_flow_irq1_edge_per_byte() {
        let mut d = LegacyInterrupts::new();
        d.inject_scancode(0x1C);
        assert!(d.take_irq1_pending());
        assert!(!d.take_irq1_pending()); // one-shot
        assert_eq!(status(&mut d) & PS2_OUTBUF_FULL, PS2_OUTBUF_FULL);
        assert_eq!(d.read(PS2_DATA, 1)[0], 0x1C);
        // buffer vacío → IRQ entregada
        assert!(!d.take_irq1_pending());
        assert_eq!(status(&mut d) & PS2_OUTBUF_FULL, 0);
    }

    #[test]
    fn queued_scancode_rearms_irq_after_partial_read() {
        let mut d = LegacyInterrupts::new();
        d.inject_scancode(0x1C);
        d.inject_scancode(0xF0);
        assert!(d.take_irq1_pending());
        // El guest lee el primero; queda otro → IRQ1 re-armada
        assert_eq!(d.read(PS2_DATA, 1)[0], 0x1C);
        assert!(d.take_irq1_pending());
        assert_eq!(d.read(PS2_DATA, 1)[0], 0xF0);
        assert!(!d.take_irq1_pending());
    }

    #[test]
    fn cmd_byte_write_produces_no_spurious_ack() {
        let mut d = LegacyInterrupts::new();
        d.write(PS2_CMD, &[0x60]); // write command byte
        d.write(PS2_DATA, &[0x65]); // bit0=1 → IRQ1 habilitada
        assert!(!d.ps2_has_data()); // sin ACK espurio
        assert!(!d.take_irq1_pending());
        // Y los scancodes siguen funcionando
        d.inject_scancode(0x2A);
        assert!(d.take_irq1_pending());
        assert_eq!(d.read(PS2_DATA, 1)[0], 0x2A);
    }

    #[test]
    fn cmd_byte_bit0_gates_keyboard_irq() {
        let mut d = LegacyInterrupts::new();
        d.write(PS2_CMD, &[0x60]);
        d.write(PS2_DATA, &[0x44]); // bit0=0 → sin IRQ1
        d.inject_scancode(0x1C);
        assert!(!d.take_irq1_pending());
        // El dato sigue legible por polling
        assert_eq!(d.read(PS2_DATA, 1)[0], 0x1C);
    }

    #[test]
    fn keyboard_data_gets_ack() {
        let mut d = LegacyInterrupts::new();
        d.write(PS2_DATA, &[0xF4]); // enable scanning → ACK
        assert!(d.take_irq1_pending());
        assert_eq!(d.read(PS2_DATA, 1)[0], 0xFA);
    }

    #[test]
    fn self_test_returns_0x55() {
        let mut d = LegacyInterrupts::new();
        d.write(PS2_CMD, &[0xAA]);
        assert_eq!(d.read(PS2_DATA, 1)[0], 0x55);
    }

    #[test]
    fn d2_writes_byte_directly_to_output_buffer() {
        let mut d = LegacyInterrupts::new();
        d.write(PS2_CMD, &[0xD2]);
        d.write(PS2_DATA, &[0x1C]);
        assert!(d.take_irq1_pending());
        assert_eq!(d.read(PS2_DATA, 1)[0], 0x1C);
    }

    #[test]
    fn output_buffer_capped_at_16() {
        let mut d = LegacyInterrupts::new();
        for i in 0..40u8 {
            d.inject_scancode(i);
        }
        assert_eq!(d.ps2_out_buf.len(), 16);
    }
}
