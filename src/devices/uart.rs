//! Emulación de UART 16550 (COM1) — Consola serial del guest.
//! Puertos base: 0x3F8 (datos), 0x3FD (estado).
//! Es el dispositivo más simple y el primero que deben dominar.

use super::IoDevice;

/// Registros UART 16550 relativos al puerto base 0x3F8
const REG_THR: u16 = 0x00; // Transmitter Holding Register (offset 0) → OUT data aquí
const REG_RBR: u16 = 0x00; // Receiver Buffer Register (offset 0) → IN data desde aquí
const REG_IER: u16 = 0x01; // Interrupt Enable Register (offset 1)
const REG_FCR: u16 = 0x02; // FIFO Control (offset 2, solo write)
const REG_LCR: u16 = 0x03; // Line Control (offset 3)
const REG_MCR: u16 = 0x04; // Modem Control (offset 4)
const REG_LSR: u16 = 0x05; // Line Status (offset 5, solo read) → bit 5 = THRE

// ─── Bits del Line Status Register ─────────────────────────────────
const LSR_DATA_READY: u8 = 1 << 0; // Hay dato en el RBR
const LSR_THRE: u8 = 1 << 5; // Transmitter Holding Register Empty

pub struct Uart16550 {
    /// Puerta base de COM1
    pub base_port: u16,
    /// Dato recibido del "host" pendiente de lectura por el guest.
    rx_byte: Option<u8>,
    /// Divisor del baud rate (DLAB), solo para que el guest lo configure.
    divisor_latch: u16,
    dlab: bool,
    ier: u8,
    lcr: u8,
    mcr: u8,
}

impl Uart16550 {
    pub fn new() -> Self {
        Self {
            base_port: 0x3F8,
            rx_byte: None,
            divisor_latch: 0,
            dlab: false,
            ier: 0,
            lcr: 0,
            mcr: 0,
        }
    }

    /// Encola un byte para que el guest lo lea (simula llegada por el cable).
    #[allow(dead_code)]
    pub fn push_rx(&mut self, byte: u8) {
        self.rx_byte = Some(byte);
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
                // Transmitir: escribimos el byte a stdout (consola serial del guest).
                use std::io::Write;
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(&[val]);
                let _ = stdout.flush();
            }
            REG_IER => self.ier = val,
            REG_FCR => {} // FIFO: no-op en nuestra emulación
            REG_LCR => {
                self.lcr = val;
                self.dlab = val & 0x80 != 0;
            }
            REG_MCR => self.mcr = val,
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
            REG_LSR => {
                // THRE siempre (podemos transmitir ya); DATA_READY si hay byte RX.
                let mut lsr = LSR_THRE;
                if self.rx_byte.is_some() {
                    lsr |= LSR_DATA_READY;
                }
                vec![lsr]
            }
            REG_LCR => vec![self.lcr],
            REG_MCR => vec![self.mcr],
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
}