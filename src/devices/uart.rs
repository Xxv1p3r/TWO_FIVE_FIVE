//! Emulación de UART 16550 (COM1) — Consola serial del guest.
//! Puertos base: 0x3F8 (datos), 0x3FD (estado).
//! Es el dispositivo más simple y el primero que deben dominar.
//!
//! Implementación completa del registro set (tarea 12 del TODO):
//!   - Loopback (bit 4 del MCR): THR → RBR interno y MSR refleja MCR.
//!   - LSR con DR/THRE/TEMT + bits de error pegajosos (OE/PE/FE/BI)
//!     que se limpian al leer el LSR.
//!   - IIR real (identificación de interrupciones, 0x01 = sin IRQ) con
//!     prioridades LSR > RX > THRE y bits de FIFO habilitado.
//!   - MSR (estado del módem, con deltas) y SCR (scratch).
//!   - IRQ4 real: la línea se pulsa desde el bucle VMM cuando hay RX o
//!     THRE pendiente con el bit correspondiente del IER habilitado.

use super::IoDevice;

/// Registros UART 16550 relativos al puerto base 0x3F8
const REG_RBR: u16 = 0x00; // Receiver Buffer Register (offset 0) → IN data desde aquí
const REG_THR: u16 = 0x00; // Transmitter Holding Register (offset 0) → OUT data aquí
const REG_IER: u16 = 0x01; // Interrupt Enable Register (offset 1)
const REG_IIR: u16 = 0x02; // Interrupt Identification (offset 2, solo read)
const REG_FCR: u16 = 0x02; // FIFO Control (offset 2, solo write)
const REG_LCR: u16 = 0x03; // Line Control (offset 3)
const REG_MCR: u16 = 0x04; // Modem Control (offset 4)
const REG_LSR: u16 = 0x05; // Line Status (offset 5, solo read)
const REG_MSR: u16 = 0x06; // Modem Status (offset 6, solo read)
const REG_SCR: u16 = 0x07; // Scratch (offset 7)

// ─── Bits del Line Status Register ─────────────────────────────────
const LSR_DATA_READY: u8 = 1 << 0; // Hay dato en el RBR
const LSR_OVERRUN: u8 = 1 << 1;    // Overrun error (pegajoso, se limpia al leer LSR)
// Bits de error que nunca se generan en esta emulación (el enlace es
// perfecto): se documentan para completar el registro, como en el 16550 real.
#[allow(dead_code)]
const LSR_PARITY: u8 = 1 << 2;  // Parity error (pegajoso)
#[allow(dead_code)]
const LSR_FRAMING: u8 = 1 << 3; // Framing error (pegajoso)
#[allow(dead_code)]
const LSR_BREAK: u8 = 1 << 4;   // Break interrupt (pegajoso)
const LSR_THRE: u8 = 1 << 5;    // Transmitter Holding Register Empty
const LSR_TEMT: u8 = 1 << 6;    // Transmitter Empty (THR + shift register)
#[allow(dead_code)]
const LSR_FIFO_ERR: u8 = 1 << 7; // Error en RX FIFO (sin FIFO real: siempre 0)

// ─── Bits del Interrupt Enable Register ────────────────────────────
const IER_ERBFI: u8 = 1 << 0; // Received Data Available → IRQ4
const IER_ETBEI: u8 = 1 << 1; // Transmitter Holding Register Empty → IRQ4
const IER_ELSI: u8 = 1 << 2;  // Receiver Line Status (errores) → IRQ4
const IER_EDSSI: u8 = 1 << 3; // Modem Status (deltas MSR) → IRQ4

// ─── Bits del Interrupt Identification Register ────────────────────
const IIR_NO_INT: u8 = 0x01; // bit 0 = 1: sin interrupción pendiente
const IIR_ID_MS: u8 = 0x00;  // bits 3:1 = 000: modem status
const IIR_ID_THRE: u8 = 0x02; // bits 3:1 = 001: THRE
const IIR_ID_RX: u8 = 0x04;  // bits 3:1 = 010: dato recibido disponible
const IIR_ID_LS: u8 = 0x06;  // bits 3:1 = 110: line status / RX timeout
const IIR_FIFO_EN: u8 = 0xC0; // bits 7:6 = 11: FIFO habilitada (16550A)

// ─── Bits del Modem Control Register ───────────────────────────────
const MCR_DTR: u8 = 1 << 0;
const MCR_RTS: u8 = 1 << 1;
const MCR_OUT1: u8 = 1 << 2;
const MCR_OUT2: u8 = 1 << 3;
const MCR_LOOP: u8 = 1 << 4; // Loopback: THR→RBR, MSR←MCR

// ─── Bits del Modem Status Register ────────────────────────────────
const MSR_DCTS: u8 = 1 << 0; // Delta CTS
const MSR_DDSR: u8 = 1 << 1; // Delta DSR
// Trailing edge of RI: solo se fija en transiciones RI 1→0; en esta
// emulación el RI nunca cambia fuera de loopback, así que no se usa.
#[allow(dead_code)]
const MSR_TERI: u8 = 1 << 2;
const MSR_DDCD: u8 = 1 << 3; // Delta DCD
const MSR_CTS: u8 = 1 << 4;
const MSR_DSR: u8 = 1 << 5;
const MSR_RI: u8 = 1 << 6;
const MSR_DCD: u8 = 1 << 7;

/// Estado del módem fuera de loopback: modem "listo" (DCD+DSR+CTS
/// afirmados, como un null modem). Evita que drivers que esperan
/// DCD/CTS se bloqueen durante el arranque.
const MSR_DEFAULT: u8 = MSR_DCD | MSR_DSR | MSR_CTS;

pub struct Uart16550 {
    /// Puerta base de COM1
    pub base_port: u16,
    /// Dato recibido del "host" pendiente de lectura por el guest.
    rx_byte: Option<u8>,
    /// Divisor del baud rate (DLAB), solo para que el guest lo configure.
    divisor_latch: u16,
    dlab: bool,
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    msr: u8,
    scr: u8,
    /// Bits de error pegajosos del LSR (OE/PE/FE/BI): se limpian al leer LSR.
    lsr_errors: u8,
    /// THRE interrupt source pendiente (se arma al escribir THR con
    /// IER.ETBEI y se limpia al leer el IIR).
    thre_int_pending: bool,
    /// One-shot latch de IRQ4 consumido por el bucle VMM (patrón PS/2).
    irq_latch: bool,
}

impl Uart16550 {
    pub fn new() -> Self {
        Self {
            base_port: 0x3F8,
            rx_byte: None,
            divisor_latch: 0,
            dlab: false,
            ier: 0,
            fcr: 0,
            lcr: 0,
            mcr: 0,
            msr: MSR_DEFAULT,
            scr: 0,
            lsr_errors: 0,
            thre_int_pending: false,
            irq_latch: false,
        }
    }

    /// Encola un byte para que el guest lo lea (simula llegada por el cable).
    /// Marca IRQ4 pendiente si el guest habilitó la interrupción de RX
    /// (IER bit 0). Si el RBR ya estaba ocupado, el byte nuevo reemplaza
    /// al anterior y se fija el bit de overrun del LSR.
    pub fn push_rx(&mut self, byte: u8) {
        if self.rx_byte.is_some() {
            self.lsr_errors |= LSR_OVERRUN;
        }
        self.rx_byte = Some(byte);
        if self.ier & IER_ERBFI != 0 {
            self.irq_latch = true;
        }
    }

    /// One-shot latch de IRQ4 para el bucle VMM: devuelve true si hay que
    /// pulsar la línea 4 del PIC del kernel y lo consume.
    pub fn take_irq(&mut self) -> bool {
        if self.irq_latch {
            self.irq_latch = false;
            true
        } else {
            false
        }
    }

    /// Nivel actual de IRQ4 (para request_interrupt_window): hay una fuente
    /// de interrupción activa y habilitada en el IER.
    pub fn irq_pending(&self) -> bool {
        (self.rx_byte.is_some() && self.ier & IER_ERBFI != 0)
            || (self.thre_int_pending && self.ier & IER_ETBEI != 0)
            || (self.lsr_errors != 0 && self.ier & IER_ELSI != 0)
            || (self.msr & 0x0F != 0 && self.ier & IER_EDSSI != 0)
    }

    /// Identificación de interrupción (bits 3:1) con prioridad real 16550:
    /// line status > RX data > THRE > modem status. bit 0 = 1 si no hay nada.
    fn iir(&self) -> u8 {
        let iir = if self.fcr & 0x01 != 0 { IIR_FIFO_EN } else { 0 };
        if self.lsr_errors != 0 && self.ier & IER_ELSI != 0 {
            iir | IIR_ID_LS
        } else if self.rx_byte.is_some() && self.ier & IER_ERBFI != 0 {
            iir | IIR_ID_RX
        } else if self.thre_int_pending && self.ier & IER_ETBEI != 0 {
            iir | IIR_ID_THRE
        } else if self.msr & 0x0F != 0 && self.ier & IER_EDSSI != 0 {
            iir | IIR_ID_MS
        } else {
            iir | IIR_NO_INT
        }
    }

    /// Reset del UART (equivalente a un reset del chip 16550).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn matches(&self, port: u16) -> bool {
        port >= self.base_port && port <= self.base_port + 7
    }
}

impl Default for Uart16550 {
    fn default() -> Self {
        Self::new()
    }
}

impl IoDevice for Uart16550 {
    fn matches_port(&self, port: u16) -> bool {
        self.matches(port)
    }

    fn write(&mut self, port: u16, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let offset = port - self.base_port;
        let val = data[0];

        // DLAB activo: offsets 0 y 1 acceden al divisor de baudios.
        if self.dlab && (offset == 0 || offset == 1) {
            let shift = if offset == 0 { 0 } else { 8 };
            self.divisor_latch =
                (self.divisor_latch & !(0xFF << shift)) | ((val as u16) << shift);
            return;
        }

        match offset {
            REG_THR => {
                if self.mcr & MCR_LOOP != 0 {
                    // Loopback: el byte se "recibe" internamente (THR → RBR).
                    if self.rx_byte.is_some() {
                        self.lsr_errors |= LSR_OVERRUN;
                    }
                    self.rx_byte = Some(val);
                    if self.ier & IER_ERBFI != 0 {
                        self.irq_latch = true;
                    }
                } else {
                    // Transmitir: escribimos el byte a stdout (consola serial del guest).
                    use std::io::Write;
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(&[val]);
                    let _ = stdout.flush();
                }
                // Transmisión instantánea → THRE vuelve a 1 al momento y,
                // si el guest habilitó la interrupción de THRE (IER bit 1),
                // se arma el flanco (el driver escribe el siguiente byte).
                self.thre_int_pending = self.ier & IER_ETBEI != 0;
                if self.thre_int_pending {
                    self.irq_latch = true;
                }
            }
            REG_IER => self.ier = val & 0x0F,
            REG_FCR => {
                self.fcr = val;
                // Sin FIFO real: conservamos un solo byte (semántica 16550).
                // bit 2 (clear RX FIFO): descartar el byte pendiente.
                if val & 0x04 != 0 {
                    self.rx_byte = None;
                }
            }
            REG_LCR => {
                self.lcr = val;
                self.dlab = val & 0x80 != 0;
            }
            REG_MCR => {
                self.mcr = val;
                if val & MCR_LOOP != 0 {
                    // Loopback: MSR refleja MCR (DCD←OUT2, RI←OUT1,
                    // DSR←DTR, CTS←RTS) y los deltas señalan cambios.
                    let mut mapped = 0u8;
                    if val & MCR_DTR != 0 { mapped |= MSR_DSR; }
                    if val & MCR_RTS != 0 { mapped |= MSR_CTS; }
                    if val & MCR_OUT1 != 0 { mapped |= MSR_RI; }
                    if val & MCR_OUT2 != 0 { mapped |= MSR_DCD; }
                    let changed = (self.msr ^ mapped) & 0xF0;
                    let mut deltas = 0u8;
                    if changed & MSR_CTS != 0 { deltas |= MSR_DCTS; }
                    if changed & MSR_DSR != 0 { deltas |= MSR_DDSR; }
                    if changed & MSR_DCD != 0 { deltas |= MSR_DDCD; }
                    self.msr = mapped | deltas;
                    if changed != 0 && self.ier & IER_EDSSI != 0 {
                        self.irq_latch = true;
                    }
                } else {
                    self.msr = MSR_DEFAULT;
                }
            }
            REG_SCR => self.scr = val,
            _ => {}
        }
    }

    fn read(&mut self, port: u16, _count: usize) -> Vec<u8> {
        let offset = port - self.base_port;

        // DLAB activo: lectura del divisor.
        if self.dlab && (offset == 0 || offset == 1) {
            let shift = if offset == 0 { 0 } else { 8 };
            return vec![(self.divisor_latch >> shift) as u8];
        }

        match offset {
            REG_RBR => {
                let b = self.rx_byte.take().unwrap_or(0);
                vec![b]
            }
            REG_IER => vec![self.ier],
            REG_IIR => {
                let iir = self.iir();
                // Leer el IIR limpia la fuente THRE (la RX se limpia al
                // leer el RBR y la de line status al leer el LSR).
                if iir & 0x0F == IIR_ID_THRE {
                    self.thre_int_pending = false;
                }
                vec![iir]
            }
            REG_LCR => vec![self.lcr],
            REG_MCR => vec![self.mcr],
            REG_LSR => {
                // THRE/TEMT siempre (transmisión instantánea); DR si hay RX.
                let mut lsr = LSR_THRE | LSR_TEMT;
                if self.rx_byte.is_some() {
                    lsr |= LSR_DATA_READY;
                }
                lsr |= self.lsr_errors;
                // Leer LSR limpia los bits de error pegajosos.
                self.lsr_errors = 0;
                vec![lsr]
            }
            REG_MSR => {
                let msr = self.msr;
                // Leer MSR limpia los bits delta.
                self.msr &= !0x0F;
                vec![msr]
            }
            REG_SCR => vec![self.scr],
            _ => vec![0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsr_thre_always_set() {
        let mut uart = Uart16550::new();
        let lsr = uart.read(0x3F8 + REG_LSR, 1)[0];
        assert_eq!(lsr & LSR_THRE, LSR_THRE);
        assert_eq!(lsr & LSR_TEMT, LSR_TEMT);
        assert_eq!(lsr & LSR_DATA_READY, 0);
    }

    #[test]
    fn rx_byte_delivered_once() {
        let mut uart = Uart16550::new();
        uart.push_rx(b'A');
        assert_eq!(uart.read(0x3F8, 1)[0], b'A');
        // Tras leer, DATA_READY baja y el dato es 0.
        let lsr = uart.read(0x3F8 + REG_LSR, 1)[0];
        assert_eq!(lsr & LSR_DATA_READY, 0);
        assert_eq!(uart.read(0x3F8, 1)[0], 0);
    }

    #[test]
    fn dlab_divisor_roundtrip() {
        let mut uart = Uart16550::new();
        uart.write(0x3F8 + REG_LCR, &[0x80]); // DLAB on
        uart.write(0x3F8, &[0x0C]); // divisor low = 12
        uart.write(0x3F8 + 1, &[0x00]); // divisor high = 0
        assert_eq!(uart.read(0x3F8, 1)[0], 0x0C);
        uart.write(0x3F8 + REG_LCR, &[0x03]); // DLAB off
        assert_eq!(uart.read(0x3F8 + REG_LCR, 1)[0], 0x03);
    }

    // ─── IIR / interrupciones (IRQ4) ─────────────────────────────

    #[test]
    fn iir_no_int_by_default() {
        let mut uart = Uart16550::new();
        // Sin FIFO: 0x01 = no interrupt. Con FIFO habilitada: 0xC1.
        assert_eq!(uart.read(0x3F8 + REG_IIR, 1)[0] & 0x0F, IIR_NO_INT);
        uart.write(0x3F8 + REG_FCR, &[0x01]);
        assert_eq!(uart.read(0x3F8 + REG_IIR, 1)[0], IIR_FIFO_EN | IIR_NO_INT);
    }

    #[test]
    fn rx_irq4_armed_only_when_ier_enabled() {
        let mut uart = Uart16550::new();
        // Sin IER.ERBFI: byte legible por polling, sin IRQ4.
        uart.push_rx(b'X');
        assert!(!uart.take_irq());
        assert!(!uart.irq_pending());
        assert_eq!(uart.read(0x3F8, 1)[0], b'X');

        // Con IER.ERBFI: latch armado y pulsado una sola vez.
        uart.write(0x3F8 + REG_IER, &[IER_ERBFI]);
        uart.push_rx(b'Y');
        assert!(uart.irq_pending());
        assert!(uart.take_irq());
        assert!(!uart.take_irq());
        // El IIR reporta RX data (ID 0x04) mientras el RBR tenga dato.
        let iir = uart.read(0x3F8 + REG_IIR, 1)[0];
        assert_eq!(iir & 0x0F, IIR_ID_RX);
        // Leer el RBR limpia la fuente.
        assert_eq!(uart.read(0x3F8, 1)[0], b'Y');
        assert!(!uart.irq_pending());
        assert_eq!(uart.read(0x3F8 + REG_IIR, 1)[0] & 0x0F, IIR_NO_INT);
    }

    #[test]
    fn thre_irq4_armed_on_thr_write() {
        let mut uart = Uart16550::new();
        uart.write(0x3F8 + REG_IER, &[IER_ETBEI]);
        assert!(!uart.take_irq());
        uart.write(0x3F8 + REG_THR, &[b'Z']);
        assert!(uart.take_irq());
        // Leer el IIR con ID THRE limpia la fuente.
        let iir = uart.read(0x3F8 + REG_IIR, 1)[0];
        assert_eq!(iir & 0x0F, IIR_ID_THRE);
        assert!(!uart.irq_pending());
    }

    // ─── Loopback ────────────────────────────────────────────────

    #[test]
    fn loopback_thr_routes_to_rbr() {
        let mut uart = Uart16550::new();
        uart.write(0x3F8 + REG_MCR, &[MCR_LOOP]);
        uart.write(0x3F8 + REG_THR, &[b'Q']);
        // El byte no sale por stdout: queda en el RBR interno.
        assert_eq!(uart.read(0x3F8, 1)[0], b'Q');
        assert_eq!(uart.read(0x3F8 + REG_LSR, 1)[0] & LSR_DATA_READY, 0);
    }

    #[test]
    fn loopback_msr_reflects_mcr() {
        let mut uart = Uart16550::new();
        // DTR+RTS afirmados + loopback → MSR = DSR|CTS (+ deltas la 1ª vez).
        uart.write(0x3F8 + REG_MCR, &[MCR_LOOP | MCR_DTR | MCR_RTS]);
        let msr = uart.read(0x3F8 + REG_MSR, 1)[0];
        assert_eq!(msr & 0xF0, MSR_DSR | MSR_CTS);
        // Los deltas se limpian tras la lectura.
        assert_eq!(uart.read(0x3F8 + REG_MSR, 1)[0] & 0x0F, 0);
        // Fuera de loopback: estado null-modem por defecto.
        uart.write(0x3F8 + REG_MCR, &[0]);
        assert_eq!(uart.read(0x3F8 + REG_MSR, 1)[0] & 0xF0, MSR_DEFAULT & 0xF0);
    }

    // ─── Errores de línea y misc ─────────────────────────────────

    #[test]
    fn overrun_sets_sticky_lsr_bit() {
        let mut uart = Uart16550::new();
        uart.push_rx(b'A');
        uart.push_rx(b'B'); // RBR ocupado → overrun, B reemplaza a A
        let lsr = uart.read(0x3F8 + REG_LSR, 1)[0];
        assert_ne!(lsr & LSR_OVERRUN, 0, "OE debe estar fijado");
        // Leer LSR limpia el bit pegajoso.
        assert_eq!(uart.read(0x3F8 + REG_LSR, 1)[0] & LSR_OVERRUN, 0);
        assert_eq!(uart.read(0x3F8, 1)[0], b'B');
    }

    #[test]
    fn scratch_register_roundtrip() {
        let mut uart = Uart16550::new();
        uart.write(0x3F8 + REG_SCR, &[0x5A]);
        assert_eq!(uart.read(0x3F8 + REG_SCR, 1)[0], 0x5A);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut uart = Uart16550::new();
        uart.write(0x3F8 + REG_IER, &[IER_ERBFI]);
        uart.push_rx(b'A');
        uart.write(0x3F8 + REG_MCR, &[MCR_LOOP]);
        uart.write(0x3F8 + REG_THR, &[b'B']);
        assert!(uart.irq_pending());
        uart.reset();
        assert!(!uart.irq_pending());
        assert!(!uart.take_irq());
        assert_eq!(uart.read(0x3F8 + REG_IIR, 1)[0] & 0x0F, IIR_NO_INT);
    }
}