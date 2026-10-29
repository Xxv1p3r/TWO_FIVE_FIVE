//! Emulación del Controlador de Red Intel PRO/1000 MT Desktop (`82540EM` / `e1000`).
//!
//! Compatible con el driver nativo `e1000.sys` / `e100064w.sys` de Windows
//! (Windows XP, Windows 7, 8, 10, 11) y con el driver `e1000` de Linux.
//! Al tener controladores incluidos de fábrica en las ISOs oficiales de Windows,
//! permite tener conectividad de red out-of-the-box sin necesidad de drivers VirtIO.

use crate::guest_mem::GuestMemory;
use std::collections::VecDeque;
use std::sync::mpsc;

pub const E1000_VENDOR_ID: u16 = 0x8086;
pub const E1000_DEVICE_ID: u16 = 0x100E; // 82540EM Gigabit Ethernet Controller
pub const E1000_DEFAULT_MAC: [u8; 6] = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];

// Registros de control y estado de e1000 (offsets en BAR0 MMIO)
pub const REG_CTRL: usize = 0x0000;
pub const REG_STATUS: usize = 0x0008;
pub const REG_EECD: usize = 0x0010;
pub const REG_EERD: usize = 0x0014;
pub const REG_CTRL_EXT: usize = 0x0018;
pub const REG_MDIC: usize = 0x0020;
pub const REG_FCAL: usize = 0x0028;
pub const REG_FCAH: usize = 0x002C;
pub const REG_FCT: usize = 0x0030;
pub const REG_ICR: usize = 0x00C0;
pub const REG_ITR: usize = 0x00C4;
pub const REG_ICS: usize = 0x00C8;
pub const REG_IMS: usize = 0x00D0;
pub const REG_IMC: usize = 0x00D8;
pub const REG_RCTL: usize = 0x0100;
pub const REG_FCTTV: usize = 0x0170;
pub const REG_TCTL: usize = 0x0400;
pub const REG_TIPG: usize = 0x0410;
pub const REG_PBA: usize = 0x1000;
pub const REG_RDBAL: usize = 0x2800;
pub const REG_RDBAH: usize = 0x2804;
pub const REG_RDLEN: usize = 0x2808;
pub const REG_RDH: usize = 0x2810;
pub const REG_RDT: usize = 0x2818;
pub const REG_RDTR: usize = 0x2820;
pub const REG_RXDCTL: usize = 0x2828;
pub const REG_RADV: usize = 0x282C;
pub const REG_TDBAL: usize = 0x3800;
pub const REG_TDBAH: usize = 0x3804;
pub const REG_TDLEN: usize = 0x3808;
pub const REG_TDH: usize = 0x3810;
pub const REG_TDT: usize = 0x3818;
pub const REG_TIDV: usize = 0x3820;
pub const REG_TXDCTL: usize = 0x3828;
pub const REG_TADV: usize = 0x382C;
pub const REG_RXCSUM: usize = 0x5000;
pub const REG_MTA: usize = 0x5200; // 128 dwords (0x5200..0x5400)
pub const REG_RAL: usize = 0x5400;
pub const REG_RAH: usize = 0x5404;
pub const REG_MANC: usize = 0x5820;
pub const REG_SWSM: usize = 0x5B50;
pub const REG_FWSM: usize = 0x5B54;

// Banderas de STATUS (0x0008)
pub const STATUS_FD: u32 = 1 << 0;     // Full Duplex
pub const STATUS_LU: u32 = 1 << 1;     // Link Up
pub const STATUS_SPEED_1000: u32 = 2 << 6; // 1000 Mb/s
pub const STATUS_PCI66: u32 = 1 << 9;  // 66 MHz PCI
pub const STATUS_BUS64: u32 = 1 << 10; // 64-bit PCI bus

// Banderas de Interrupción (ICR / IMS / IMC)
pub const ICR_TXDW: u32 = 1 << 0; // Transmit Descriptor Written Back
pub const ICR_TXQE: u32 = 1 << 1; // Transmit Queue Empty
pub const ICR_LSC: u32 = 1 << 2;  // Link Status Change
pub const ICR_RXSEQ: u32 = 1 << 3; // Receive Sequence Error
pub const ICR_RXDMT0: u32 = 1 << 4; // Rx Desc Min Threshold
pub const ICR_RXO: u32 = 1 << 6;  // Receiver Overrun
pub const ICR_RXT0: u32 = 1 << 7; // Receiver Timer Interrupt

// Banderas de RCTL (0x0100)
pub const RCTL_EN: u32 = 1 << 1;      // Receiver Enable
pub const RCTL_SBP: u32 = 1 << 2;     // Store Bad Packets
pub const RCTL_UPE: u32 = 1 << 3;     // Unicast Promiscuous
pub const RCTL_MPE: u32 = 1 << 4;     // Multicast Promiscuous
pub const RCTL_BAM: u32 = 1 << 15;    // Broadcast Accept Mode
pub const RCTL_BSIZE_2048: u32 = 0 << 16;
pub const RCTL_BSIZE_1024: u32 = 1 << 16;
pub const RCTL_BSIZE_512: u32 = 2 << 16;
pub const RCTL_BSIZE_256: u32 = 3 << 16;

// Banderas de TCTL (0x0400)
pub const TCTL_EN: u32 = 1 << 1;      // Transmit Enable
pub const TCTL_PSP: u32 = 1 << 3;     // Pad Short Packets

// Banderas de Descriptor de Transmisión (Tx Desc)
pub const TXD_CMD_EOP: u8 = 1 << 0;   // End of Packet
pub const TXD_CMD_IFCS: u8 = 1 << 1;  // Insert FCS/CRC
pub const TXD_CMD_RS: u8 = 1 << 3;    // Report Status
pub const TXD_STAT_DD: u8 = 1 << 0;   // Descriptor Done

// Banderas de Descriptor de Recepción (Rx Desc)
pub const RXD_STAT_DD: u8 = 1 << 0;   // Descriptor Done
pub const RXD_STAT_EOP: u8 = 1 << 1;  // End of Packet

/// Plantilla EEPROM oficial de 64 palabras para Intel 82540EM (conforme a VirtualBox / QEMU).
pub const E1000_EEPROM_TEMPLATE: [u16; 64] = [
    0x0000, 0x0000, 0x0000, 0x0000, 0xffff, 0x0000, 0x0000, 0x0000,
    0x3000, 0x1000, 0x6403, 0x100e, 0x8086, 0x100e, 0x8086, 0x3040,
    0x0008, 0x2000, 0x7e14, 0x0048, 0x1000, 0x00d8, 0x0000, 0x2700,
    0x6cc9, 0x3150, 0x0722, 0x040b, 0x0984, 0x0000, 0xc000, 0x0706,
    0x1008, 0x0000, 0x0f04, 0x7fff, 0x4d01, 0xffff, 0xffff, 0xffff,
    0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
    0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
    0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0x0000,
];

/// Máquina de estados para emulación bit-banging Microwire (93C46 / 93C66) en EECD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EepromFsm {
    Idle,
    ReadOpcode(u16, u8),
    ReadData(u16, u8),
}

#[derive(Clone, Debug)]
pub struct EepromState {
    pub fsm: EepromFsm,
    pub eecd_val: u32,
    pub last_sk: bool,
    pub do_bit: bool,
}

impl Default for EepromState {
    fn default() -> Self {
        Self {
            fsm: EepromFsm::Idle,
            eecd_val: (1 << 8) | (1 << 9), // PRES | AUTORD
            last_sk: false,
            do_bit: false,
        }
    }
}

impl EepromState {
    pub fn write_eecd(&mut self, val: u32, eeprom: &[u16; 64]) {
        let sk = (val & (1 << 0)) != 0;
        let cs = (val & (1 << 1)) != 0;
        let di = (val & (1 << 2)) != 0;
        let req = (val & (1 << 6)) != 0;

        let mut next_val = (val & !((1 << 3) | (1 << 7))) | (1 << 8) | (1 << 9);
        if req {
            next_val |= 1 << 7; // GNT concedido inmediatamente
        }

        if !cs {
            self.fsm = EepromFsm::Idle;
            self.do_bit = false;
        } else if !self.last_sk && sk {
            // Flanco de subida de SK
            match self.fsm {
                EepromFsm::Idle => {
                    if di {
                        // Start bit recibido
                        self.fsm = EepromFsm::ReadOpcode(0, 0);
                    }
                }
                EepromFsm::ReadOpcode(acc, count) => {
                    let new_acc = (acc << 1) | if di { 1 } else { 0 };
                    let new_count = count + 1;
                    if new_count == 8 {
                        // 2 bits opcode + 6 bits de dirección
                        let opcode = (new_acc >> 6) & 0x03;
                        let addr = (new_acc & 0x3F) as usize;
                        if opcode == 2 {
                            // Operación READ
                            let data = eeprom[addr];
                            self.fsm = EepromFsm::ReadData(data, 16);
                            self.do_bit = false; // Dummy bit 0 antes de los datos
                        } else {
                            self.fsm = EepromFsm::Idle;
                        }
                    } else {
                        self.fsm = EepromFsm::ReadOpcode(new_acc, new_count);
                    }
                }
                EepromFsm::ReadData(data, bits_left) => {
                    if bits_left > 0 {
                        self.do_bit = ((data >> (bits_left - 1)) & 1) != 0;
                        self.fsm = EepromFsm::ReadData(data, bits_left - 1);
                    } else {
                        self.do_bit = false;
                        self.fsm = EepromFsm::Idle;
                    }
                }
            }
        }

        self.last_sk = sk;
        if self.do_bit {
            next_val |= 1 << 3; // DO
        }
        self.eecd_val = next_val;
    }

    pub fn read_eecd(&self) -> u32 {
        self.eecd_val | (1 << 8) | (1 << 9)
    }
}

pub struct E1000State {
    pub mmio_base: u32,
    pub iobase: u16,
    pub io_addr: u32, // Registro de índice para acceso I/O (BAR1)
    pub mac: [u8; 6],
    pub eeprom: [u16; 64],
    pub eeprom_state: EepromState,
    pub ctrl: u32,
    pub status: u32,
    pub eerd: u32,
    pub mdic: u32,
    pub phy_ctrl: u16,
    pub swsm: u32,
    pub rxcsum: u32,
    pub icr: u32,
    pub ims: u32,
    pub rctl: u32,
    pub tctl: u32,
    pub tipg: u32,

    // Anillo de recepción (RX)
    pub rdbal: u32,
    pub rdbah: u32,
    pub rdlen: u32,
    pub rdh: u32,
    pub rdt: u32,
    pub rdtr: u32,
    pub radv: u32,
    pub rxdctl: u32,

    // Anillo de transmisión (TX)
    pub tdbal: u32,
    pub tdbah: u32,
    pub tdlen: u32,
    pub tdh: u32,
    pub tdt: u32,
    pub tidv: u32,
    pub tadv: u32,
    pub txdctl: u32,

    pub mta: [u32; 128],
    pub ral: u32,
    pub rah: u32,
    pub irq_asserted: bool,
    pub tx_packets: VecDeque<Vec<u8>>,
    pub rx_queue: VecDeque<Vec<u8>>,

    // Conexión con el worker de red NAT / TAP
    pub tx_sender: Option<mpsc::Sender<Vec<u8>>>,
    pub rx_receiver: Option<mpsc::Receiver<Vec<u8>>>,
}

impl E1000State {
    pub fn new(mac: [u8; 6]) -> Self {
        let mut eeprom = E1000_EEPROM_TEMPLATE;
        // Palabras 0..2: Dirección MAC en orden little-endian
        eeprom[0] = (mac[0] as u16) | ((mac[1] as u16) << 8);
        eeprom[1] = (mac[2] as u16) | ((mac[3] as u16) << 8);
        eeprom[2] = (mac[4] as u16) | ((mac[5] as u16) << 8);

        // Calcular checksum de EEPROM (suma de words 0..63 debe ser 0xBABA)
        let mut sum: u16 = 0;
        for i in 0..63 {
            sum = sum.wrapping_add(eeprom[i]);
        }
        eeprom[63] = 0xBABA_u16.wrapping_sub(sum);

        let ral = (mac[0] as u32)
            | ((mac[1] as u32) << 8)
            | ((mac[2] as u32) << 16)
            | ((mac[3] as u32) << 24);
        let rah = (mac[4] as u32) | ((mac[5] as u32) << 8) | (1 << 31); // Address Valid

        // Inicializar canal de comunicación con el Worker de red
        let (tx_sender, tx_receiver) = mpsc::channel();
        let (rx_sender, rx_receiver) = mpsc::channel();
        crate::devices::virtio_net::spawn_net_worker(tx_receiver, rx_sender);

        Self {
            mmio_base: 0,
            iobase: 0,
            io_addr: 0,
            mac,
            eeprom,
            eeprom_state: EepromState::default(),
            ctrl: 0x0000_0241,
            status: STATUS_FD | STATUS_LU | STATUS_SPEED_1000 | STATUS_PCI66 | STATUS_BUS64,
            eerd: 0,
            mdic: 0x1800_0000 | (1 << 28), // MDIC Ready
            phy_ctrl: 0x1140, // Auto-negotiation enabled, 1000 Mbps, Full Duplex
            swsm: 0,
            rxcsum: 0,
            icr: 0,
            ims: 0,
            rctl: 0,
            tctl: 0,
            tipg: 0x0000_000A,
            rdbal: 0,
            rdbah: 0,
            rdlen: 0,
            rdh: 0,
            rdt: 0,
            rdtr: 0,
            radv: 0,
            rxdctl: 0,
            tdbal: 0,
            tdbah: 0,
            tdlen: 0,
            tdh: 0,
            tdt: 0,
            tidv: 0,
            tadv: 0,
            txdctl: 0,
            mta: [0u32; 128],
            ral,
            rah,
            irq_asserted: false,
            tx_packets: VecDeque::new(),
            rx_queue: VecDeque::new(),
            tx_sender: Some(tx_sender),
            rx_receiver: Some(rx_receiver),
        }
    }

    pub fn reset(&mut self) {
        self.ctrl = 0x0000_0241;
        self.status = STATUS_FD | STATUS_LU | STATUS_SPEED_1000 | STATUS_PCI66 | STATUS_BUS64;
        self.eerd = 0;
        self.mdic = 0x1800_0000 | (1 << 28);
        self.phy_ctrl = 0x1140;
        self.swsm = 0;
        self.rxcsum = 0;
        self.icr = 0;
        self.ims = 0;
        self.rctl = 0;
        self.tctl = 0;
        self.tipg = 0x0000_000A;
        self.rdbal = 0;
        self.rdbah = 0;
        self.rdlen = 0;
        self.rdh = 0;
        self.rdt = 0;
        self.rdtr = 0;
        self.radv = 0;
        self.rxdctl = 0;
        self.tdbal = 0;
        self.tdbah = 0;
        self.tdlen = 0;
        self.tdh = 0;
        self.tdt = 0;
        self.tidv = 0;
        self.tadv = 0;
        self.txdctl = 0;
        self.irq_asserted = false;
        self.tx_packets.clear();
        self.rx_queue.clear();
        self.eeprom_state = EepromState::default();
    }

    pub fn update_irq(&mut self) {
        let active = (self.icr & self.ims) != 0;
        self.irq_asserted = active;
    }

    pub fn read_phy_reg(&self, reg: usize) -> u16 {
        match reg {
            0 => self.phy_ctrl, // PHY Control (0x1140 default)
            1 => 0x796D,        // PHY Status (1000Base-T, Link Up, Auto-neg complete)
            2 => 0x0141,        // PHY Identifier 1 (OUI)
            3 => 0x0C20,        // PHY Identifier 2 (Model 88E1000)
            4 => 0x01E1,        // Auto-Negotiation Advertisement
            5 => 0x45E1,        // Auto-Negotiation Link Partner Ability
            9 => 0x0E00,        // 1000BASE-T Control
            10 => 0x3800,       // 1000BASE-T Status
            17 => 0xAC00,       // PHY Specific Status (1000M, Full Duplex, Link OK)
            _ => 0,
        }
    }

    pub fn read_reg(&mut self, off: usize) -> u32 {
        match off {
            REG_CTRL => self.ctrl,
            REG_STATUS => self.status,
            REG_EECD => self.eeprom_state.read_eecd(),
            REG_EERD => self.eerd,
            REG_CTRL_EXT => 0,
            REG_MDIC => self.mdic,
            REG_PBA => 0x0010_0030, // 48KB Rx, 16KB Tx
            REG_RXCSUM => self.rxcsum,
            REG_SWSM => {
                let v = self.swsm;
                self.swsm |= 1; // Bit 0 (SMBI) se activa en lectura según la especificación Intel
                v
            }
            REG_FWSM => 0,
            REG_ICR => {
                let val = self.icr;
                self.icr = 0;
                self.update_irq();
                val
            }
            REG_ITR => 0,
            REG_ICS => 0,
            REG_IMS => self.ims,
            REG_IMC => 0,
            REG_RCTL => self.rctl,
            REG_TCTL => self.tctl,
            REG_TIPG => self.tipg,
            REG_RDBAL => self.rdbal,
            REG_RDBAH => self.rdbah,
            REG_RDLEN => self.rdlen,
            REG_RDH => self.rdh,
            REG_RDT => self.rdt,
            REG_RDTR => self.rdtr,
            REG_RADV => self.radv,
            REG_RXDCTL => self.rxdctl,
            REG_TDBAL => self.tdbal,
            REG_TDBAH => self.tdbah,
            REG_TDLEN => self.tdlen,
            REG_TDH => self.tdh,
            REG_TDT => self.tdt,
            REG_TIDV => self.tidv,
            REG_TADV => self.tadv,
            REG_TXDCTL => self.txdctl,
            REG_RAL => self.ral,
            REG_RAH => self.rah,
            REG_MANC => 0x0000_1000,
            REG_FCAL | REG_FCAH | REG_FCT | REG_FCTTV => 0,
            0x4000..=0x40FC => 0, // Registros estadísticos
            0x5200..=0x53FC => {
                let idx = (off - 0x5200) / 4;
                if idx < 128 {
                    self.mta[idx]
                } else {
                    0
                }
            }
            0x5400..=0x547C => 0, // Filtros de direcciones adicionales
            _ => 0,
        }
    }

    pub fn write_reg(&mut self, off: usize, val: u32, mem: Option<&GuestMemory>) {
        match off {
            REG_CTRL => {
                self.ctrl = val;
                if (val & (1 << 26)) != 0 {
                    // Reset por software solicitado por el driver
                    self.reset();
                }
            }
            REG_STATUS => {} // Read-Only
            REG_EECD => {
                self.eeprom_state.write_eecd(val, &self.eeprom);
            }
            REG_EERD => {
                // Lectura de EEPROM: bits 15..8 dirección, bit 0 inicio
                if (val & 0x01) != 0 {
                    let addr = ((val >> 8) & 0x3F) as usize;
                    let data = self.eeprom[addr] as u32;
                    self.eerd = (data << 16) | (1 << 4) | (val & 0xFF00); // Data + Done bit (bit 4)
                } else {
                    self.eerd = val;
                }
            }
            REG_MDIC => {
                let op = (val >> 26) & 0x03;
                let reg = ((val >> 16) & 0x1F) as usize;
                let data_in = (val & 0xFFFF) as u16;
                if op == 1 {
                    // Write PHY register
                    if reg == 0 {
                        self.phy_ctrl = data_in;
                    }
                    self.mdic = (val & 0x0FFF_FFFF) | (1 << 28); // Ready
                } else if op == 2 {
                    // Read PHY register
                    let data_out = self.read_phy_reg(reg);
                    self.mdic = ((val & 0x0FFF_0000) | (data_out as u32)) | (1 << 28); // Ready
                } else {
                    self.mdic = val | (1 << 28) | (1 << 30); // Error + Ready
                }
            }
            REG_SWSM => self.swsm = val,
            REG_RXCSUM => self.rxcsum = val,
            REG_ICS => {
                self.icr |= val;
                self.update_irq();
            }
            REG_IMS => {
                self.ims |= val;
                self.update_irq();
            }
            REG_IMC => {
                self.ims &= !val;
                self.update_irq();
            }
            REG_RCTL => self.rctl = val,
            REG_TCTL => self.tctl = val,
            REG_TIPG => self.tipg = val,
            REG_RDBAL => self.rdbal = val & !0x0F,
            REG_RDBAH => self.rdbah = val,
            REG_RDLEN => self.rdlen = val & !0x7F,
            REG_RDH => self.rdh = val,
            REG_RDT => {
                self.rdt = val;
                if let Some(m) = mem {
                    self.process_rx(m);
                }
            }
            REG_RDTR => self.rdtr = val,
            REG_RADV => self.radv = val,
            REG_RXDCTL => self.rxdctl = val,
            REG_TDBAL => self.tdbal = val & !0x0F,
            REG_TDBAH => self.tdbah = val,
            REG_TDLEN => self.tdlen = val & !0x7F,
            REG_TDH => self.tdh = val,
            REG_TDT => {
                self.tdt = val;
                if let Some(m) = mem {
                    self.process_tx(m);
                }
            }
            REG_TIDV => self.tidv = val,
            REG_TADV => self.tadv = val,
            REG_TXDCTL => self.txdctl = val,
            REG_RAL => {
                self.ral = val;
                self.mac[0] = (val & 0xFF) as u8;
                self.mac[1] = ((val >> 8) & 0xFF) as u8;
                self.mac[2] = ((val >> 16) & 0xFF) as u8;
                self.mac[3] = ((val >> 24) & 0xFF) as u8;
            }
            REG_RAH => {
                self.rah = val;
                self.mac[4] = (val & 0xFF) as u8;
                self.mac[5] = ((val >> 8) & 0xFF) as u8;
            }
            0x5200..=0x53FC => {
                let idx = (off - 0x5200) / 4;
                if idx < 128 {
                    self.mta[idx] = val;
                }
            }
            _ => {}
        }
    }

    /// Procesa la cola de transmisión TX del guest
    pub fn process_tx(&mut self, mem: &GuestMemory) {
        if (self.tctl & TCTL_EN) == 0 || self.tdlen == 0 {
            return;
        }

        let desc_size = 16u64;
        let num_descs = (self.tdlen as u64) / desc_size;
        if num_descs == 0 {
            return;
        }

        let base_gpa = ((self.tdbah as u64) << 32) | (self.tdbal as u64);
        let mut cur_head = self.tdh as u64;
        let target_tail = self.tdt as u64;

        let mut packet_data = Vec::new();
        let mut any_written_back = false;

        while cur_head != target_tail {
            let desc_gpa = base_gpa + cur_head * desc_size;
            let mut desc_bytes = [0u8; 16];
            if mem.read_bytes(desc_gpa, &mut desc_bytes).is_err() {
                break;
            }

            let buf_addr = u64::from_le_bytes(desc_bytes[0..8].try_into().unwrap());
            let lower = u32::from_le_bytes(desc_bytes[8..12].try_into().unwrap());
            let cmd = ((lower >> 24) & 0xFF) as u8;

            let is_extended = (lower & (1 << 29)) != 0; // TXD_CMD_DEXT

            if is_extended {
                let is_data_desc = (lower & (1 << 20)) != 0; // DTYP = 1 (Data)
                if is_data_desc {
                    let len = (lower & 0x000F_FFFF) as usize;
                    if buf_addr != 0 && len > 0 {
                        let mut chunk = vec![0u8; len];
                        if mem.read_bytes(buf_addr, &mut chunk).is_ok() {
                            packet_data.extend_from_slice(&chunk);
                        }
                    }
                }
                // Si es un Descriptor de Contexto (DTYP = 0), no contiene payload
            } else {
                // Descriptor de formato Legacy
                let len = (lower & 0xFFFF) as usize;
                if buf_addr != 0 && len > 0 {
                    let mut chunk = vec![0u8; len];
                    if mem.read_bytes(buf_addr, &mut chunk).is_ok() {
                        packet_data.extend_from_slice(&chunk);
                    }
                }
            }

            // Marcar Descriptor Done (DD) si el descriptor solicitó Report Status (RS)
            if (cmd & TXD_CMD_RS) != 0 {
                desc_bytes[12] |= TXD_STAT_DD;
                let _ = mem.write_bytes(desc_gpa + 12, &[desc_bytes[12]]);
                any_written_back = true;
            }

            // Si es End of Packet (EOP) y hay datos acumulados, despachar la trama Ethernet completa
            if (cmd & TXD_CMD_EOP) != 0 && !packet_data.is_empty() {
                self.tx_packets.push_back(packet_data.clone());
                // Despachar a la red local (DHCP, ARP, ICMP) o al worker upstream
                if let Some(reply) = crate::devices::virtio_net::process_local_frame(&packet_data) {
                    self.receive_packet(reply, mem);
                } else if let Some(ref tx_tx) = self.tx_sender {
                    let _ = tx_tx.send(packet_data.clone());
                }
                packet_data.clear();
            }

            cur_head = (cur_head + 1) % num_descs;
        }

        self.tdh = cur_head as u32;

        if any_written_back || cur_head == target_tail {
            // Notificar al guest de la finalización de la transmisión
            self.icr |= ICR_TXDW | ICR_TXQE;
            self.update_irq();
        }
    }

    /// Encola un paquete Ethernet recibido para el guest
    pub fn receive_packet(&mut self, packet: Vec<u8>, mem: &GuestMemory) -> bool {
        self.rx_queue.push_back(packet);
        self.process_rx(mem)
    }

    /// Despacha paquetes pendientes hacia el anillo de recepción RX del guest
    pub fn process_rx(&mut self, mem: &GuestMemory) -> bool {
        if (self.rctl & RCTL_EN) == 0 || self.rdlen == 0 || self.rx_queue.is_empty() {
            return false;
        }

        let desc_size = 16u64;
        let num_descs = (self.rdlen as u64) / desc_size;
        if num_descs == 0 {
            return false;
        }

        let base_gpa = ((self.rdbah as u64) << 32) | (self.rdbal as u64);
        let mut cur_head = self.rdh as u64;
        let target_tail = self.rdt as u64;
        let mut delivered = false;

        let buf_size = match (self.rctl >> 16) & 0x03 {
            0 => 2048,
            1 => 1024,
            2 => 512,
            3 => 256,
            _ => 2048,
        };

        while cur_head != target_tail && !self.rx_queue.is_empty() {
            let pkt = self.rx_queue.pop_front().unwrap();
            let desc_gpa = base_gpa + cur_head * desc_size;
            let mut desc_bytes = [0u8; 16];
            if mem.read_bytes(desc_gpa, &mut desc_bytes).is_err() {
                self.rx_queue.push_front(pkt);
                break;
            }

            let buf_addr = u64::from_le_bytes(desc_bytes[0..8].try_into().unwrap());
            if buf_addr != 0 {
                let write_len = pkt.len().min(buf_size);
                let _ = mem.write_bytes(buf_addr, &pkt[..write_len]);

                // Actualizar descriptor RX: length, status (DD | EOP | IXSM | IPCS | TCPCS)
                let len_bytes = (write_len as u16).to_le_bytes();
                desc_bytes[8] = len_bytes[0];
                desc_bytes[9] = len_bytes[1];
                desc_bytes[10] = 0; // Checksum
                desc_bytes[11] = 0;
                // Status bits:
                // Bit 0: DD (Descriptor Done)
                // Bit 1: EOP (End of Packet)
                // Bit 2: IXSM (Ignore Checksum Indication)
                // Bit 5: TCPCS (TCP Checksum Calculated)
                // Bit 6: IPCS (IP Checksum Calculated)
                desc_bytes[12] = RXD_STAT_DD | RXD_STAT_EOP | (1 << 2) | (1 << 5) | (1 << 6);
                desc_bytes[13] = 0; // Errors: 0
                desc_bytes[14] = 0; // Special: 0
                desc_bytes[15] = 0;
                let _ = mem.write_bytes(desc_gpa + 8, &desc_bytes[8..16]);
            }

            cur_head = (cur_head + 1) % num_descs;
            delivered = true;
        }

        self.rdh = cur_head as u32;

        if delivered {
            self.icr |= ICR_RXT0;
            self.update_irq();
        }

        delivered
    }

    /// Avanza el e1000 recogiendo paquetes recibidos desde el host / NAT
    pub fn step(&mut self, mem: &GuestMemory) {
        if let Some(ref rx_rx) = self.rx_receiver {
            while let Ok(pkt) = rx_rx.try_recv() {
                self.rx_queue.push_back(pkt);
            }
        }
        if !self.rx_queue.is_empty() {
            self.process_rx(mem);
        }
    }
}

pub struct E1000Device {
    pub state: std::sync::Arc<std::sync::Mutex<E1000State>>,
    cached_iobase: u16,
    cached_mmio_base: u32,
}

impl E1000Device {
    pub fn new(state: std::sync::Arc<std::sync::Mutex<E1000State>>) -> Self {
        let (cached_iobase, cached_mmio_base) = {
            let s = state.lock().unwrap();
            (s.iobase, s.mmio_base)
        };
        Self {
            state,
            cached_iobase,
            cached_mmio_base,
        }
    }

    pub fn set_iobase(&mut self, base: u16) {
        self.cached_iobase = base;
        self.state.lock().unwrap().iobase = base;
    }

    pub fn set_mmio_base(&mut self, base: u32) {
        self.cached_mmio_base = base;
        self.state.lock().unwrap().mmio_base = base;
    }

    pub fn matches_mmio(&self, addr: u64) -> bool {
        let mmio_base = self.cached_mmio_base as u64;
        mmio_base != 0 && addr >= mmio_base && addr < (mmio_base + 0x20000)
    }

    pub fn mmio_read(&self, addr: u64, size: usize) -> Vec<u8> {
        let mut s = self.state.lock().unwrap();
        let off = (addr - s.mmio_base as u64) as usize;
        let val = s.read_reg(off);
        let bytes = val.to_le_bytes();
        bytes[..size.min(4)].to_vec()
    }

    pub fn mmio_write(&self, addr: u64, data: &[u8], mem: Option<&GuestMemory>) {
        let mut s = self.state.lock().unwrap();
        let off = (addr - s.mmio_base as u64) as usize;
        let mut bytes = [0u8; 4];
        let copy_len = data.len().min(4);
        bytes[..copy_len].copy_from_slice(&data[..copy_len]);
        let val = u32::from_le_bytes(bytes);
        s.write_reg(off, val, mem);
    }

    pub fn matches_port(&self, port: u16) -> bool {
        let iobase = self.cached_iobase;
        iobase != 0 && port >= iobase && port < iobase + 64
    }

    pub fn io_read(&self, port: u16, size: usize) -> Vec<u8> {
        let mut s = self.state.lock().unwrap();
        let iobase = s.iobase;
        let off = port.saturating_sub(iobase);
        match off {
            0..=3 => {
                let bytes = s.io_addr.to_le_bytes();
                let sub = (off & 3) as usize;
                bytes[sub..sub + size.min(4 - sub)].to_vec()
            }
            4..=7 => {
                let reg_off = s.io_addr as usize;
                let val = s.read_reg(reg_off);
                let bytes = val.to_le_bytes();
                let sub = (off & 3) as usize;
                bytes[sub..sub + size.min(4 - sub)].to_vec()
            }
            _ => vec![0xFF; size],
        }
    }

    pub fn io_write(&self, port: u16, data: &[u8], mem: Option<&GuestMemory>) {
        let mut s = self.state.lock().unwrap();
        let iobase = s.iobase;
        let off = port.saturating_sub(iobase);
        match off {
            0..=3 => {
                let mut bytes = s.io_addr.to_le_bytes();
                let sub = (off & 3) as usize;
                let copy_len = data.len().min(4 - sub);
                bytes[sub..sub + copy_len].copy_from_slice(&data[..copy_len]);
                s.io_addr = u32::from_le_bytes(bytes) & 0x1FFFF;
            }
            4..=7 => {
                let reg_off = s.io_addr as usize;
                let mut bytes = s.read_reg(reg_off).to_le_bytes();
                let sub = (off & 3) as usize;
                let copy_len = data.len().min(4 - sub);
                bytes[sub..sub + copy_len].copy_from_slice(&data[..copy_len]);
                let val = u32::from_le_bytes(bytes);
                s.write_reg(reg_off, val, mem);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_e1000_initial_status_and_mac() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let state = E1000State::new(mac);
        assert_eq!(state.status & STATUS_LU, STATUS_LU, "Link must be up");
        assert_eq!(state.status & STATUS_FD, STATUS_FD, "Must be full duplex");
        assert_eq!(state.mac, mac);
    }

    #[test]
    fn test_e1000_eeprom_checksum_validation() {
        let mac = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];
        let state = E1000State::new(mac);
        let mut sum: u16 = 0;
        for i in 0..64 {
            sum = sum.wrapping_add(state.eeprom[i]);
        }
        assert_eq!(sum, 0xBABA, "EEPROM 64-word sum must equal 0xBABA");
    }

    #[test]
    fn test_e1000_eerd_read_and_done() {
        let mac = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];
        let mut state = E1000State::new(mac);

        // Lectura de la palabra 0 del EEPROM (primeros 2 bytes de MAC)
        state.write_reg(REG_EERD, 1, None); // addr = 0, start = 1
        let eerd = state.read_reg(REG_EERD);
        assert_eq!(eerd & (1 << 4), (1 << 4), "Done bit must be set");
        let word0 = (eerd >> 16) as u16;
        let expected_word0 = (mac[0] as u16) | ((mac[1] as u16) << 8);
        assert_eq!(word0, expected_word0);
    }

    #[test]
    fn test_e1000_eecd_req_gnt_handshake() {
        let mac = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];
        let mut state = E1000State::new(mac);

        // Driver solicita acceso REQ (bit 6)
        state.write_reg(REG_EECD, 1 << 6, None);
        let eecd = state.read_reg(REG_EECD);
        assert_eq!(eecd & (1 << 7), 1 << 7, "GNT bit 7 must be asserted when REQ bit 6 is set");
        assert_eq!(eecd & (1 << 8), 1 << 8, "PRES bit 8 must be present");
        assert_eq!(eecd & (1 << 9), 1 << 9, "AUTORD bit 9 must be present");
    }

    #[test]
    fn test_e1000_mdic_phy_id_read() {
        let mac = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];
        let mut state = E1000State::new(mac);

        // Lectura de PHY reg 2 (PHY ID 1 = 0x0141)
        let mdic_cmd = (2 << 26) | (1 << 21) | (2 << 16);
        state.write_reg(REG_MDIC, mdic_cmd, None);
        let mdic_res = state.read_reg(REG_MDIC);
        assert_eq!(mdic_res & (1 << 28), (1 << 28), "MDIC Ready bit must be set");
        assert_eq!((mdic_res & 0xFFFF) as u16, 0x0141);
    }

    #[test]
    fn test_e1000_ims_and_interrupt_assert() {
        let mac = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
        let mut state = E1000State::new(mac);
        assert!(!state.irq_asserted);

        // Habilitar interrupciones TXDW
        state.write_reg(REG_IMS, ICR_TXDW, None);
        assert_eq!(state.ims, ICR_TXDW);

        // Disparar ICS
        state.write_reg(REG_ICS, ICR_TXDW, None);
        assert!(state.irq_asserted, "IRQ should be asserted when ICR matches IMS");

        // Leer ICR debe limpiar la interrupción
        let icr = state.read_reg(REG_ICR);
        assert_eq!(icr & ICR_TXDW, ICR_TXDW);
        assert!(!state.irq_asserted, "Reading ICR must clear interrupt line");
    }

    #[test]
    fn test_e1000_packet_transmission_extended_descriptors() {
        let mac = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];
        let mut state = E1000State::new(mac);

        let mut raw_mem = vec![0u8; 8192];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let tdesc_base = 0x1000u64;
        let payload_gpa = 0x1800u64;
        let test_payload = b"GET /index.html HTTP/1.1\r\nHost: 10.0.2.2\r\n\r\n";
        let _ = guest_mem.write_bytes(payload_gpa, test_payload);

        // 1. Descriptor de Contexto (DEXT = 1, DTYP = 0): no contiene datos
        let mut ctx_desc = [0u8; 16];
        let ctx_lower = (1 << 29) | ((TXD_CMD_RS as u32) << 24); // DEXT=1, DTYP=0, RS=1
        ctx_desc[8..12].copy_from_slice(&ctx_lower.to_le_bytes());
        let _ = guest_mem.write_bytes(tdesc_base, &ctx_desc);

        // 2. Descriptor de Datos Extendido (DEXT = 1, DTYP = 1): contiene el payload
        let mut data_desc = [0u8; 16];
        data_desc[0..8].copy_from_slice(&payload_gpa.to_le_bytes());
        let data_lower = (test_payload.len() as u32) | (1 << 20) | (1 << 29) | (((TXD_CMD_EOP | TXD_CMD_RS) as u32) << 24);
        data_desc[8..12].copy_from_slice(&data_lower.to_le_bytes());
        let _ = guest_mem.write_bytes(tdesc_base + 16, &data_desc);

        state.write_reg(REG_TCTL, TCTL_EN, None);
        state.write_reg(REG_TDBAL, tdesc_base as u32, None);
        state.write_reg(REG_TDLEN, 128, None);
        state.write_reg(REG_TDH, 0, None);

        // Escribir TDT = 2 para procesar ambos descriptores
        state.write_reg(REG_TDT, 2, Some(&guest_mem));

        // Debe haber transmitido exactamente 1 paquete con el payload correcto
        assert_eq!(state.tx_packets.len(), 1);
        assert_eq!(state.tx_packets[0], test_payload);

        // Verificar que ambos descriptores recibieron DD (Descriptor Done)
        let mut ctx_check = [0u8; 16];
        let _ = guest_mem.read_bytes(tdesc_base, &mut ctx_check);
        assert_eq!(ctx_check[12] & TXD_STAT_DD, TXD_STAT_DD, "Context desc must have DD set");

        let mut data_check = [0u8; 16];
        let _ = guest_mem.read_bytes(tdesc_base + 16, &mut data_check);
        assert_eq!(data_check[12] & TXD_STAT_DD, TXD_STAT_DD, "Data desc must have DD set");
    }

    #[test]
    fn test_e1000_rx_packet_delivery_and_offload_flags() {
        let mac = [0x08, 0x00, 0x27, 0x12, 0x34, 0x56];
        let mut state = E1000State::new(mac);

        let mut raw_mem = vec![0u8; 65536];
        let guest_mem = GuestMemory::new(raw_mem.as_mut_ptr(), raw_mem.len());

        let rdesc_base = 0x2000u64;
        let rx_buf_gpa = 0x2800u64;

        // Configurar descriptor de recepción en guest
        let mut rdesc = [0u8; 16];
        rdesc[0..8].copy_from_slice(&rx_buf_gpa.to_le_bytes());
        let _ = guest_mem.write_bytes(rdesc_base, &rdesc);

        state.write_reg(REG_RCTL, RCTL_EN | RCTL_BAM, None);
        state.write_reg(REG_RDBAL, rdesc_base as u32, None);
        state.write_reg(REG_RDLEN, 128, None);
        state.write_reg(REG_RDH, 0, None);
        state.write_reg(REG_RDT, 1, None);

        let fake_pkt = vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let ok = state.receive_packet(fake_pkt.clone(), &guest_mem);
        assert!(ok, "Packet delivery must succeed");

        // Verificar descriptor actualizado
        let mut updated_desc = [0u8; 16];
        let _ = guest_mem.read_bytes(rdesc_base, &mut updated_desc);
        let len = u16::from_le_bytes([updated_desc[8], updated_desc[9]]);
        assert_eq!(len as usize, fake_pkt.len());

        let status = updated_desc[12];
        assert_eq!(status & (RXD_STAT_DD | RXD_STAT_EOP), RXD_STAT_DD | RXD_STAT_EOP);
        // Offload flags (IXSM, IPCS, TCPCS)
        assert_ne!(status & (1 << 2), 0, "IXSM bit must be set");

        let errors = updated_desc[13];
        assert_eq!(errors, 0, "Errors byte must be 0");

        // Verificar payload en memoria del guest
        let mut read_back = vec![0u8; fake_pkt.len()];
        let _ = guest_mem.read_bytes(rx_buf_gpa, &mut read_back);
        assert_eq!(read_back, fake_pkt);
    }
}
