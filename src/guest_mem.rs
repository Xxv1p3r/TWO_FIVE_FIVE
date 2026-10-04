//! Memoria física del guest: región mmap anónima registrada en KVM.
//!
//! Sustituye los punteros crudos `*const u8` que antes viajaban del bucle
//! VMM al display, al DebugCon y al hilo de temporización (tarea 18 del
//! TODO) por una manija con acceso acotado: todos los accesos host pasan
//! por métodos que comprueban `offset + len <= size` antes de tocar
//! memoria, eliminando el UB potencial de leer/escribir fuera de la región.
//!
//! Soporta esquemas x86-64 con hueco PCI en 3.5G (0xE000_0000) y memoria
//! extendida por encima de 4G (0x1_0000_0000).

use std::sync::Arc;

/// Manija a la memoria del guest (región mmap `'static` registrada en KVM).
///
/// La región la mapea `mmap_zeroed_region` con vida `'static`: KVM la usa
/// como `userspace_addr` mientras viva la VM y el guest escribe en ella de
/// forma concurrente (sin sincronización, igual que el hardware real). Por
/// eso `GuestMemory` no propaga exclusividad de acceso: se comparte por
/// `Arc`/`Clone` entre el bucle VMM, el display, los dispositivos y los
/// hilos auxiliares.
///
/// # Safety
/// El puntero interior apunta a la región mmap `'static`. Desde la API
/// pública no hay forma de salirse de los límites: cada método comprueba el
/// rango antes de tocar memoria y devuelve `None`/`false`/bytes copiados en
/// lugar de leer o escribir fuera de la región (UB).
#[derive(Clone, Debug)]
pub struct GuestMemory {
    ptr: *mut u8,
    size: usize,
}

// Safety: misma justificación que `DeviceBus`/`VgaState` — la región es
// 'static y el acceso mutable concurrente es semántica de hardware (el
// guest escribe sin avisar), no un préstamo de Rust que haya que invalidar.
unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    /// Envuelve una región mmap existente (tamaño en bytes).
    pub fn new(ptr: *mut u8, size: usize) -> Self {
        Self { ptr, size }
    }

    /// Tamaño de la región, en bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Dirección host de la región (para registrarla en KVM). No expone
    /// acceso a datos: para leer/escribir usar los métodos acotados.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Traduce una GPA (Guest Physical Address) a un offset dentro de la región mmap contigua del host.
    /// Para sistemas con más de 3.5 GiB de RAM, el rango entre 3.5G (0xE000_0000) y 4G (0x1_0000_0000)
    /// corresponde al hueco PCI/MMIO reservado; la RAM por encima de 4G se aloja contigua en el buffer host
    /// a partir del offset 3.5G.
    #[inline]
    pub fn gpa_to_host_offset(&self, gpa: usize) -> Option<usize> {
        const RAM_BELOW_4G_LIMIT: usize = 0xE000_0000;
        let ram_below_4g = self.size.min(RAM_BELOW_4G_LIMIT);
        if gpa < ram_below_4g {
            Some(gpa)
        } else if gpa >= 0x1_0000_0000 {
            let high_off = gpa - 0x1_0000_0000;
            let host_off = ram_below_4g.checked_add(high_off)?;
            if host_off < self.size {
                Some(host_off)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Lee un byte con bounds-check. `None` si `offset` está fuera de rango.
    #[inline]
    #[allow(dead_code)]
    pub fn read(&self, offset: usize) -> Option<u8> {
        let host_off = self.gpa_to_host_offset(offset)?;
        Some(unsafe { *self.ptr.add(host_off) })
    }

    /// Escribe un byte con bounds-check. Devuelve `false` (y no escribe) si
    /// `offset` está fuera de rango.
    #[inline]
    pub fn write(&self, offset: usize, val: u8) -> bool {
        if let Some(host_off) = self.gpa_to_host_offset(offset) {
            unsafe { *self.ptr.add(host_off) = val };
            true
        } else {
            false
        }
    }

    /// Lee un byte con bounds-check (0 si fuera de límites).
    #[inline]
    #[allow(dead_code)]
    pub fn read_u8(&self, offset: usize) -> u8 {
        self.read(offset).unwrap_or(0)
    }

    /// Lee un u16 little-endian con bounds-check (0 si fuera de límites).
    #[inline]
    pub fn read_u16(&self, offset: usize) -> u16 {
        if let (Some(h1), Some(h2)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 1)) {
            if h2 == h1 + 1 {
                unsafe {
                    let low = *self.ptr.add(h1) as u16;
                    let high = *self.ptr.add(h2) as u16;
                    low | (high << 8)
                }
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Escribe un byte con bounds-check. Devuelve false si fuera de límites.
    #[inline]
    pub fn write_u8(&self, offset: usize, val: u8) -> bool {
        self.write(offset, val)
    }

    /// Lee un u32 little-endian con bounds-check (0 si fuera de límites).
    #[inline]
    pub fn read_u32(&self, offset: usize) -> u32 {
        if let (Some(h0), Some(h3)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 3)) {
            if h3 == h0 + 3 {
                unsafe {
                    std::ptr::read_unaligned(self.ptr.add(h0) as *const u32).to_le()
                }
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Escribe un u16 little-endian con bounds-check. Devuelve false si fuera de límites.
    #[inline]
    pub fn write_u16(&self, offset: usize, val: u16) -> bool {
        if let (Some(h0), Some(h1)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 1)) {
            if h1 == h0 + 1 {
                unsafe {
                    *self.ptr.add(h0) = val as u8;
                    *self.ptr.add(h1) = (val >> 8) as u8;
                }
                return true;
            }
        }
        false
    }

    /// Escribe un u32 little-endian con bounds-check. Devuelve false si fuera de límites.
    #[inline]
    pub fn write_u32(&self, offset: usize, val: u32) -> bool {
        if let (Some(h0), Some(h3)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 3)) {
            if h3 == h0 + 3 {
                unsafe {
                    std::ptr::write_unaligned(self.ptr.add(h0) as *mut u32, val.to_le());
                }
                return true;
            }
        }
        false
    }

    /// Escribe u32 little-endian con `write_volatile` y bounds-check: el
    /// guest u otro hilo pueden leerlo sin sincronización (p. ej. el tick
    /// del BDA en 0x46C).
    pub fn write_u32_volatile(&self, offset: usize, val: u32) -> bool {
        if let (Some(h0), Some(h3)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 3)) {
            if h3 == h0 + 3 {
                unsafe {
                    std::ptr::write_volatile(self.ptr.add(h0) as *mut u32, val);
                }
                return true;
            }
        }
        false
    }

    /// Copia hasta `dst.len()` bytes desde `offset` a `dst` con bounds-check
    /// (recorta si el rango se sale de la región). Devuelve los bytes copiados.
    pub fn copy_from(&self, offset: usize, dst: &mut [u8]) -> usize {
        const RAM_BELOW_4G_LIMIT: usize = 0xE000_0000;
        let ram_below_4g = self.size.min(RAM_BELOW_4G_LIMIT);
        let (host_start, max_contiguous) = if offset < ram_below_4g {
            (offset, ram_below_4g - offset)
        } else if offset >= 0x1_0000_0000 {
            let high_off = offset - 0x1_0000_0000;
            let host_off = ram_below_4g + high_off;
            if host_off < self.size {
                (host_off, self.size - host_off)
            } else {
                return 0;
            }
        } else {
            return 0;
        };

        let len = dst.len().min(max_contiguous);
        if len > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(self.ptr.add(host_start), dst.as_mut_ptr(), len);
            }
        }
        len
    }

    /// Copia `src` a partir de `offset` con bounds-check. Devuelve `false`
    /// (y no copia nada) si el rango no cabe entero en la región.
    pub fn copy_to(&self, offset: usize, src: &[u8]) -> bool {
        const RAM_BELOW_4G_LIMIT: usize = 0xE000_0000;
        let ram_below_4g = self.size.min(RAM_BELOW_4G_LIMIT);
        let (host_start, max_contiguous) = if offset < ram_below_4g {
            (offset, ram_below_4g - offset)
        } else if offset >= 0x1_0000_0000 {
            let high_off = offset - 0x1_0000_0000;
            let host_off = ram_below_4g + high_off;
            if host_off < self.size {
                (host_off, self.size - host_off)
            } else {
                return false;
            }
        } else {
            return false;
        };

        if src.len() > max_contiguous {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(host_start), src.len());
        }
        true
    }

    /// Lee un u64 little-endian con bounds-check (0 si fuera de límites).
    #[inline]
    pub fn read_u64(&self, offset: usize) -> u64 {
        if let (Some(h0), Some(h7)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 7)) {
            if h7 == h0 + 7 {
                unsafe {
                    std::ptr::read_unaligned(self.ptr.add(h0) as *const u64).to_le()
                }
            } else {
                0
            }
        } else {
            0
        }
    }

    /// Escribe un u64 little-endian con bounds-check. Devuelve false si fuera de límites.
    #[inline]
    pub fn write_u64(&self, offset: usize, val: u64) -> bool {
        if let (Some(h0), Some(h7)) = (self.gpa_to_host_offset(offset), self.gpa_to_host_offset(offset + 7)) {
            if h7 == h0 + 7 {
                unsafe {
                    std::ptr::write_unaligned(self.ptr.add(h0) as *mut u64, val.to_le());
                }
                return true;
            }
        }
        false
    }

    /// Lee un bloque de bytes hacia un buffer.
    #[inline]
    pub fn read_bytes(&self, offset: u64, dst: &mut [u8]) -> Result<(), ()> {
        let n = self.copy_from(offset as usize, dst);
        if n == dst.len() { Ok(()) } else { Err(()) }
    }

    /// Escribe un bloque de bytes desde un buffer.
    #[inline]
    pub fn write_bytes(&self, offset: u64, src: &[u8]) -> Result<(), ()> {
        if self.copy_to(offset as usize, src) { Ok(()) } else { Err(()) }
    }
}

impl GuestMemory {
    /// Envuelve la región en un `Arc` para compartirla entre hilos.
    pub fn arc(ptr: *mut u8, size: usize) -> Arc<Self> {
        Arc::new(Self::new(ptr, size))
    }
}