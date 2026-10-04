//! Emulación de un dispositivo USB Tablet (HID Absolute Pointer).
//!
//! Compatible con el estándar USB HID 1.11 y la especificación de QEMU USB Tablet
//! (Vendor ID 0x0627, Product ID 0x0001). Reporta coordenadas absolutas (0..32767)
//! y 3 botones, lo que permite la integración fluida del cursor (seamless cursor)
//! en cualquier distribución de Linux o sistema operativo con interfaz gráfica
//! sin necesidad de drivers adicionales.

use std::collections::VecDeque;

/// Descriptor de dispositivo (18 bytes)
pub const TABLET_DEVICE_DESC: [u8; 18] = [
    18,   // bLength
    0x01, // bDescriptorType: DEVICE
    0x10, 0x01, // bcdUSB: USB 1.10
    0x00, // bDeviceClass: Definido a nivel de interfaz
    0x00, // bDeviceSubClass
    0x00, // bDeviceProtocol
    0x08, // bMaxPacketSize0: 8 bytes
    0x27, 0x06, // idVendor: 0x0627 (QEMU USB)
    0x01, 0x00, // idProduct: 0x0001 (QEMU USB Tablet)
    0x00, 0x00, // bcdDevice: 0.00
    0x01, // iManufacturer: String 1 ("Antigravity")
    0x02, // iProduct: String 2 ("QEMU USB Tablet")
    0x03, // iSerialNumber: String 3 ("1")
    0x01, // bNumConfigurations: 1
];

/// Descriptor de configuración completo (34 bytes: Config + Interface + HID + Endpoint)
pub const TABLET_CONFIG_DESC: [u8; 34] = [
    // Configuration descriptor (9 bytes)
    0x09, // bLength
    0x02, // bDescriptorType: CONFIGURATION
    34, 0x00, // wTotalLength: 34 bytes
    0x01, // bNumInterfaces: 1
    0x01, // bConfigurationValue: 1
    0x00, // iConfiguration: None
    0xA0, // bmAttributes: Bus powered, Remote Wakeup
    50,   // bMaxPower: 100 mA (50 * 2mA)

    // Interface descriptor (9 bytes)
    0x09, // bLength
    0x04, // bDescriptorType: INTERFACE
    0x00, // bInterfaceNumber: 0
    0x00, // bAlternateSetting: 0
    0x01, // bNumEndpoints: 1
    0x03, // bInterfaceClass: HID (0x03)
    0x00, // bInterfaceSubClass: None
    0x00, // bInterfaceProtocol: None
    0x00, // iInterface: None

    // HID descriptor (9 bytes)
    0x09, // bLength
    0x21, // bDescriptorType: HID
    0x01, 0x01, // bcdHID: 1.01
    0x00, // bCountryCode: Not localized
    0x01, // bNumDescriptors: 1
    0x22, // bDescriptorType: REPORT
    74, 0x00, // wDescriptorLength: 74 bytes

    // Endpoint descriptor (7 bytes)
    0x07, // bLength
    0x05, // bDescriptorType: ENDPOINT
    0x81, // bEndpointAddress: EP 1 IN
    0x03, // bmAttributes: Interrupt (0x03)
    0x08, 0x00, // wMaxPacketSize: 8 bytes
    0x0A, // bInterval: 10 ms
];

/// Descriptor de reporte HID (74 bytes: Mouse absoluto 0..32767 con 3 botones y rueda)
pub const TABLET_REPORT_DESC: [u8; 74] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x03, //     Usage Maximum (3)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5) - padding
    0x81, 0x01, //     Input (Constant)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, // Logical Maximum (32767)
    0x35, 0x00, //     Physical Minimum (0)
    0x46, 0xFF, 0x7F, // Physical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x38, //     Usage (Wheel)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x35, 0x00, //     Physical Minimum (0)
    0x45, 0x00, //     Physical Maximum (0)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xC0,       //   End Collection
    0xC0,       // End Collection
];

/// Convierte una cadena de texto a un descriptor de string USB (UTF-16LE).
fn string_descriptor(s: &str) -> Vec<u8> {
    let utf16: Vec<u16> = s.encode_utf16().collect();
    let mut desc = vec![2 + (utf16.len() * 2) as u8, 0x03];
    for c in utf16 {
        desc.extend_from_slice(&c.to_le_bytes());
    }
    desc
}

/// Estado y lógica de emulación de la tableta USB.
#[derive(Debug, Clone)]
pub struct UsbTablet {
    pub address: u8,
    pub pending_address: u8,
    pub configured: bool,
    pub idle_rate: u8,
    pub protocol: u8,
    ctrl_response: Vec<u8>,
    ctrl_offset: usize,
    pub cur_x: u16,
    pub cur_y: u16,
    pub cur_buttons: u8,
    pub cur_wheel: i8,
    events: VecDeque<[u8; 6]>,
}

impl Default for UsbTablet {
    fn default() -> Self {
        Self::new()
    }
}

impl UsbTablet {
    pub fn new() -> Self {
        Self {
            address: 0,
            pending_address: 0,
            configured: false,
            idle_rate: 0,
            protocol: 1, // Report protocol por defecto
            ctrl_response: Vec::new(),
            ctrl_offset: 0,
            cur_x: 16384, // Centro por defecto
            cur_y: 16384,
            cur_buttons: 0,
            cur_wheel: 0,
            events: VecDeque::new(),
        }
    }

    /// Reinicia el dispositivo a su estado inicial tras un reset de puerto USB.
    pub fn reset(&mut self) {
        self.address = 0;
        self.pending_address = 0;
        self.configured = false;
        self.idle_rate = 0;
        self.protocol = 1;
        self.ctrl_response.clear();
        self.ctrl_offset = 0;
        self.events.clear();
    }

    /// Inyecta un evento de ratón absoluto (X, Y en 0..32767, botones en bits 0..2, rueda).
    pub fn inject_event(&mut self, x: u16, y: u16, buttons: u8, wheel: i8) {
        self.cur_x = x;
        self.cur_y = y;
        self.cur_buttons = buttons;
        self.cur_wheel = wheel;

        let report = [
            buttons & 0x07,
            (x & 0xFF) as u8,
            (x >> 8) as u8,
            (y & 0xFF) as u8,
            (y >> 8) as u8,
            wheel as u8,
        ];

        // Limitar la cola a 64 eventos para no saturar memoria
        if self.events.len() >= 64 {
            self.events.pop_front();
        }
        self.events.push_back(report);
    }

    /// Comprueba si hay eventos de entrada pendientes en la cola.
    pub fn has_pending_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// Procesa una petición SETUP en el Endpoint 0 (Control).
    pub fn handle_setup(&mut self, setup: &[u8]) -> bool {
        if setup.len() < 8 {
            return false;
        }

        let bm_request_type = setup[0];
        let b_request = setup[1];
        let w_value = u16::from_le_bytes([setup[2], setup[3]]);
        let _w_index = u16::from_le_bytes([setup[4], setup[5]]);
        let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;

        self.ctrl_response.clear();
        self.ctrl_offset = 0;

        // Tipo de petición: Standard (0x00), Class (0x20), Vendor (0x40)
        let req_type = (bm_request_type >> 5) & 0x03;

        match req_type {
            0 => {
                // Petición Estándar USB
                match b_request {
                    // GET_STATUS
                    0x00 => {
                        self.ctrl_response = vec![0x01, 0x00]; // Self-powered
                    }
                    // CLEAR_FEATURE / SET_FEATURE
                    0x01 | 0x03 => {
                        // Aceptado sin datos
                    }
                    // SET_ADDRESS
                    0x05 => {
                        self.pending_address = (w_value & 0x7F) as u8;
                    }
                    // GET_DESCRIPTOR
                    0x06 => {
                        let desc_type = (w_value >> 8) as u8;
                        let desc_index = (w_value & 0xFF) as u8;
                        match desc_type {
                            // DEVICE
                            0x01 => {
                                self.ctrl_response = TABLET_DEVICE_DESC.to_vec();
                            }
                            // CONFIGURATION
                            0x02 => {
                                self.ctrl_response = TABLET_CONFIG_DESC.to_vec();
                            }
                            // STRING
                            0x03 => {
                                match desc_index {
                                    0 => {
                                        // Language ID: English (US) 0x0409
                                        self.ctrl_response = vec![0x04, 0x03, 0x09, 0x04];
                                    }
                                    1 => {
                                        self.ctrl_response = string_descriptor("Antigravity");
                                    }
                                    2 => {
                                        self.ctrl_response = string_descriptor("QEMU USB Tablet");
                                    }
                                    3 => {
                                        self.ctrl_response = string_descriptor("1");
                                    }
                                    _ => {
                                        self.ctrl_response = string_descriptor("");
                                    }
                                }
                            }
                            // HID
                            0x21 => {
                                self.ctrl_response = TABLET_CONFIG_DESC[18..27].to_vec();
                            }
                            // REPORT
                            0x22 => {
                                self.ctrl_response = TABLET_REPORT_DESC.to_vec();
                            }
                            _ => {}
                        }
                    }
                    // GET_CONFIGURATION
                    0x08 => {
                        self.ctrl_response = vec![if self.configured { 1 } else { 0 }];
                    }
                    // SET_CONFIGURATION
                    0x09 => {
                        self.configured = (w_value & 0xFF) == 1;
                    }
                    // GET_INTERFACE
                    0x0A => {
                        self.ctrl_response = vec![0x00];
                    }
                    // SET_INTERFACE
                    0x0B => {
                        // Interfaz 0 aceptada
                    }
                    _ => return false,
                }
            }
            1 => {
                // Petición de Clase HID
                match b_request {
                    // GET_REPORT
                    0x01 => {
                        let report = [
                            self.cur_buttons & 0x07,
                            (self.cur_x & 0xFF) as u8,
                            (self.cur_x >> 8) as u8,
                            (self.cur_y & 0xFF) as u8,
                            (self.cur_y >> 8) as u8,
                            self.cur_wheel as u8,
                        ];
                        self.ctrl_response = report.to_vec();
                    }
                    // GET_IDLE
                    0x02 => {
                        self.ctrl_response = vec![self.idle_rate];
                    }
                    // GET_PROTOCOL
                    0x03 => {
                        self.ctrl_response = vec![self.protocol];
                    }
                    // SET_REPORT
                    0x09 => {
                        // Aceptado
                    }
                    // SET_IDLE
                    0x0A => {
                        self.idle_rate = (w_value >> 8) as u8;
                    }
                    // SET_PROTOCOL
                    0x0B => {
                        self.protocol = (w_value & 0xFF) as u8;
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }

        // Truncar la respuesta si el host pidió menos bytes que el descriptor completo
        if self.ctrl_response.len() > w_length {
            self.ctrl_response.truncate(w_length);
        }

        true
    }

    /// Procesa una transferencia IN en el endpoint especificado.
    /// Devuelve los bytes a transferir al host, o None si no hay datos (NAK).
    pub fn handle_in(&mut self, endpoint: u8, max_len: usize) -> Option<Vec<u8>> {
        match endpoint {
            // Endpoint 0: Fase de datos o estado de control
            0 => {
                if self.ctrl_offset >= self.ctrl_response.len() {
                    if self.pending_address != 0 {
                        self.address = self.pending_address;
                        self.pending_address = 0;
                    }
                    // Paquete vacío (ZLP) o fin de transferencia
                    return Some(Vec::new());
                }
                let remaining = self.ctrl_response.len() - self.ctrl_offset;
                let chunk_len = remaining.min(max_len);
                let chunk = self.ctrl_response[self.ctrl_offset..self.ctrl_offset + chunk_len].to_vec();
                self.ctrl_offset += chunk_len;
                if self.ctrl_offset >= self.ctrl_response.len() && self.pending_address != 0 {
                    self.address = self.pending_address;
                    self.pending_address = 0;
                }
                Some(chunk)
            }
            // Endpoint 1: Endpoint de interrupción para reportes de entrada
            1 => {
                if let Some(report) = self.events.pop_front() {
                    let len = report.len().min(max_len);
                    Some(report[..len].to_vec())
                } else {
                    None // NAK: no hay nuevos eventos pendientes
                }
            }
            _ => None,
        }
    }

    /// Procesa una transferencia OUT en el endpoint especificado.
    pub fn handle_out(&mut self, endpoint: u8, _data: &[u8]) -> bool {
        match endpoint {
            0 => true, // Aceptar ACK de fase de estado
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tablet_descriptors() {
        let mut tablet = UsbTablet::new();

        // 1. Get Device Descriptor
        let setup_dev = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];
        assert!(tablet.handle_setup(&setup_dev));
        let data = tablet.handle_in(0, 18).unwrap();
        assert_eq!(data.len(), 18);
        assert_eq!(data[0], 18); // bLength
        assert_eq!(data[1], 0x01); // DEVICE

        // 2. Set Address
        let setup_addr = [0x00, 0x05, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(tablet.handle_setup(&setup_addr));
        // Status stage IN:
        tablet.handle_in(0, 0);
        assert_eq!(tablet.address, 5);

        // 3. Get Report Descriptor
        let setup_rep = [0x81, 0x06, 0x00, 0x22, 0x00, 0x00, 74, 0x00];
        assert!(tablet.handle_setup(&setup_rep));
        let data = tablet.handle_in(0, 74).unwrap();
        assert_eq!(data.len(), 74);
        assert_eq!(data[0], 0x05); // Usage Page
    }

    #[test]
    fn test_tablet_event_injection() {
        let mut tablet = UsbTablet::new();
        assert!(!tablet.has_pending_events());
        assert!(tablet.handle_in(1, 8).is_none()); // NAK initially

        // Inyectar evento: X=1000, Y=2000, Botón Izquierdo (1)
        tablet.inject_event(1000, 2000, 1, 0);
        assert!(tablet.has_pending_events());

        let report = tablet.handle_in(1, 8).unwrap();
        assert_eq!(report.len(), 6);
        assert_eq!(report[0], 1); // Botón izquierdo
        assert_eq!(u16::from_le_bytes([report[1], report[2]]), 1000);
        assert_eq!(u16::from_le_bytes([report[3], report[4]]), 2000);
        assert_eq!(report[5], 0);

        // Cola vacía de nuevo
        assert!(!tablet.has_pending_events());
        assert!(tablet.handle_in(1, 8).is_none());
    }
}
