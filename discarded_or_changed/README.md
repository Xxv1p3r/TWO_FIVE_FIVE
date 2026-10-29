# Registro de Componentes Descartados, Optimizados y Modificados

Este directorio contiene las versiones anteriores del código y la justificación técnica de los cambios realizados para convertir a **Two Five Five (255)** en un clon 1:1 de **Oracle VM VirtualBox** en Rust.

---

## 1. GUI Launcher y Consumo Excesivo de Recursos (`gui_launcher_v0.rs`)
- **Problema descartado:** El bucle principal del launcher ejecutaba un refresco incondicional a 60 FPS mediante `window.update_with_buffer()` sin seguimiento de cambios (*dirty tracking*), lo que generaba un consumo residual constante de **15% a 30% de CPU en Linux en reposo**.
- **Solución implementada:** Se integró dirty tracking reactivo (`needs_redraw`). Si no hay movimiento de ratón, clics ni pulsaciones de teclado, el bucle duerme 20 ms (`thread::sleep`) y delega el sondeo de eventos a `window.update()`. El consumo en reposo bajó a **< 0.1% CPU**.
- **Diálogo de Configuración (`UIAdvancedSettingsDialogMachine`):**
  - La versión anterior solo desplegaba un boceto de la pestaña "Sistema" y usaba punteros crudos (`cat as *const _`) en los botones del selector lateral, provocando que hacer clic en las categorías (General, Almacenamiento, Pantalla, Audio, Red, USB) no tuviera ningún efecto.
  - Se completó la implementación réplica 1:1 de VirtualBox con navegación funcional entre las 7 pestañas, selector de árbol de almacenamiento IDE PIIX4 / SATA AHCI, y botones modales `[Aceptar]` y `[Cancelar]`.

---

## 2. Puntero del Ratón y Negociación Dinámica (`display_v0.rs`)
- **Problema descartado:** Se mantenía forzada la bandera `mouse_integration = true` desde el arranque. Durante la fase previa a la carga del controlador de Tableta USB (BIOS SeaBIOS, WinPE y primeras pantallas del instalador de Windows 7 / Linux), el huésped no ha activado los descriptores HID USB, por lo que enviar coordenadas absolutas a la cola de la tableta causaba que el puntero se congelara completamente en la pantalla de instalación.
- **Solución implementada:**
  - Se añadió la métrica atómica `mouse_abs_available`, sincronizada dinámicamente con `bus.is_tablet_configured()`.
  - Cuando el SO huésped aún no ha inicializado el tablet USB: el sistema opera en modo relativo PS/2 con captura al hacer clic (`[Ratón: PS/2 (Clic captura)]`) y liberación limpia al pulsar la tecla Host (Ctrl Derecho) o Escape (`[Ratón: CAPTURADO]`).
  - En cuanto Windows o Linux cargan el controlador USB HID, el sistema conmuta automáticamente a integración absoluta (`[Ratón: Integrado]`) sin requerir interacción manual del usuario.

---

## 3. Ventana Emergente de Cierre (`UIVMCloseDialog`)
- **Problema descartado:** Al cerrar la ventana o presionar `Host+Q`, el proceso se cerraba inmediatamente o forzaba la terminación sin preguntar al usuario, arriesgando pérdida de datos o corrupción en discos virtuales VDI no sincronizados.
- **Solución implementada:** Se programó el diálogo modal oficial de VirtualBox `UIVMCloseDialog`, con oscurecimiento del fondo y tres opciones de radio:
  1. *Guardar el estado de la máquina*
  2. *Enviar señal de apagado (ACPI)*
  3. *Apagar la máquina (Power Off)*
  Con navegación por teclado (Arriba/Abajo/Enter/Esc) y botones interactivos `[Aceptar]` y `[Cancelar]`.

---

## 4. Detección de Discos Virtuales en Instaladores de SO (IDE & AHCI)
- **PIIX4 IDE (8086:7111):**
  - El bus PCI tenía asignado el ID PIIX3 `8086:7010` sin disco conectado en el canal maestro primario (`vdisk: None`).
  - Se actualizó a PIIX4 `8086:7111` con `prog_if = 0x8A`, `revision = 0x01` y configuración de registros de temporización `IDETIM` (`0x40`/`0x42` = `0x8011`).
- **SATA AHCI (Intel 82801 8086:2829):**
  - Faltaba el registro Intel MAP (`0x90 = 0x40`) y PCS (`0x92 = 0x3F`), impidiendo que el controlador `msahci.sys` de Windows se enlazara al hardware.
  - Se implementó la emisión inicial del FIS D2H tras la habilitación de `PORT_CMD_FRE` (0x0010) para evitar timeouts en el handshake COMRESET.
  - Se reparó `generate_ata_identify` completando las palabras 54-58 con la geometría CHS real, capacidad LBA48 y los bits de `FLUSH CACHE` (palabras 82-83).
