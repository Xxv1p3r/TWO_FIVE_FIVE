//! Memoria física del guest: región mmap anónima registrada en KVM.
//!
//! Sustituye los punteros crudos `*const u8` que antes viajaban del bucle
//! VMM al display, al DebugCon y al hilo de temporización (tarea 18 del
//! TODO) por una manija con acceso acotado: todos los accesos host pasan
//! por métodos que comprueban `offset + len <= size` antes de tocar
//! memoria, eliminando el UB potencial de leer/escribir fuera de la región.

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

    /// Lee un byte con bounds-check. `None` si `offset` está fuera de rango.
    #[inline]
    #[allow(dead_code)]
    pub fn read(&self, offset: usize) -> Option<u8> {
        if offset >= self.size {
            return None;
        }
        Some(unsafe { *self.ptr.add(offset) })
    }

    /// Escribe un byte con bounds-check. Devuelve `false` (y no escribe) si
    /// `offset` está fuera de rango.
    #[inline]
    pub fn write(&self, offset: usize, val: u8) -> bool {
        if offset >= self.size {
            return false;
        }
        unsafe { *self.ptr.add(offset) = val };
        true
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
        if offset + 2 <= self.size {
            unsafe {
                let low = *self.ptr.add(offset) as u16;
                let high = *self.ptr.add(offset + 1) as u16;
                low | (high << 8)
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

    /// Escribe un u16 little-endian con bounds-check. Devuelve false si fuera de límites.
    #[inline]
    pub fn write_u16(&self, offset: usize, val: u16) -> bool {
        if offset + 2 <= self.size {
            unsafe {
                *self.ptr.add(offset) = val as u8;
                *self.ptr.add(offset + 1) = (val >> 8) as u8;
            }
            true
        } else {
            false
        }
    }

    /// Escribe u32 little-endian con `write_volatile` y bounds-check: el
    /// guest u otro hilo pueden leerlo sin sincronización (p. ej. el tick
    /// del BDA en 0x46C).
    pub fn write_u32_volatile(&self, offset: usize, val: u32) -> bool {
        if offset + 4 > self.size {
            return false;
        }
        unsafe {
            std::ptr::write_volatile(self.ptr.add(offset) as *mut u32, val);
        }
        true
    }

    /// Copia hasta `dst.len()` bytes desde `offset` a `dst` con bounds-check
    /// (recorta si el rango se sale de la región). Devuelve los bytes copiados.
    pub fn copy_from(&self, offset: usize, dst: &mut [u8]) -> usize {
        let len = dst.len().min(self.size.saturating_sub(offset));
        if len > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(self.ptr.add(offset), dst.as_mut_ptr(), len);
            }
        }
        len
    }

    /// Copia `src` a partir de `offset` con bounds-check. Devuelve `false`
    /// (y no copia nada) si el rango no cabe entero en la región.
    pub fn copy_to(&self, offset: usize, src: &[u8]) -> bool {
        if offset + src.len() > self.size {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(offset), src.len());
        }
        true
    }
}

impl GuestMemory {
    /// Envuelve la región en un `Arc` para compartirla entre hilos.
    pub fn arc(ptr: *mut u8, size: usize) -> Arc<Self> {
        Arc::new(Self::new(ptr, size))
    }
}