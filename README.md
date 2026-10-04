# TWO FIVE FIVE (255)

Hipervisor Tipo-2 minimalista de alto rendimiento sobre **KVM** y **Rust**.

**Two Five Five (255)** arranca firmwares estándar (SeaBIOS, coreboot) y sistemas operativos modernos (Linux x86_64, distros Live como Kali Linux, Linux Mint, Alpine, TinyCore) directamente sobre KVM sin dependencias pesadas como QEMU.

---

## Características Principales

- **Arquitectura de Memoria $\ge 4\text{ GB}$**:
  - Particionado de memoria KVM respetando la arquitectura PC y el hueco PCI MMIO (3.5 GB – 4 GB).
  - RAM baja mapeada en GPA `0x0000_0000` (hasta 3584 MiB) y RAM alta en GPA `0x1_0000_0000`.
  - Tablas E820 y CMOS RTC con soporte para memorias superiores a 4 GB.

- **Multiprocesamiento Simétrico (SMP)**:
  - Soporte multi-core nativo (por defecto 4 vCPUs: 1 BSP + 3 APs) con sondeo SIPI y tablas ACPI MADT enlazadas al RSDT.

- **Paridad con Oracle VirtualBox**:
  - **AHCI SATA (0x8086:0x2829)**: Controlador ICH-8M completo con máquina de estados COMRESET, conteo LBA48 (65,536 sectores), interrupciones NCQ Set Device Bits FIS (0xA1) y emulación ATAPI CD-ROM SATA.
  - **VMMDev (0x80EE:0xCAFE)**: Soporte de eventos de host en `u32HostEvents`, Fast IRQ Ack sin consumo espurio de eventos y banderas HGCM.
  - **Audio AC'97 (0x8086:0x2415)**: Códec ICH con registros extendidos EAID/EACS (`0x0809` y `0x0009` con VRM/VRA), control W1C en status y lecturas NAM protegidas.
  - **BMDMA (Bus Master DMA IDE)**: Transferencias PRD DMA directas de alta velocidad.

- **Subsistemas y Dispositivos Adicionales**:
  - **Gráficos Bochs VBE**: Aceleración VBE con soporte `VBE_DISPI_GETCAPS` (hasta 2560×1600), Linear Framebuffer (LFB) y modos nativos del kernel Linux (`bochs-drm` a 1024×768@32bpp).
  - **Red VirtIO (`virtio-net`)**: Stack de red en userspace integrado con DHCP (10.0.2.15), Gateway (10.0.2.2) y proxy DNS nativo, además de soporte para interfaces TAP de Linux.
  - **VirtIO Serial (`virtio-serial`)**: Canal bidireccional guest-host para comunicación e integración con agentes.
  - **USB UHCI + Tableta HID**: Entrada de cursor absoluto USB para integración sin captura forzada del ratón.
  - **Reloj PIT de Alta Precisión**: Hilo independiente sincronizado con `CLOCK_MONOTONIC` (`clock_nanosleep` absoluto) que avanza el PIT y el BDA tick (18.2 Hz) sin deriva acumulada.

- **Interfaz Dual**:
  - **Frontend Gráfico**: Ventana nativa en el host (`minifb`) con escalado dinámico por interpolación nearest-neighbor y soporte de teclado en español.
  - **Dashboard TUI interactivo**: Panel estilo `btop`/`htop` en terminal con telemetría en tiempo real (uso de CPUs, salidas KVM/s, E/S de almacenamiento, resolución y visor de logs).

---

## Compilación

Requiere Rust 1.75+ y Linux con soporte para KVM (`/dev/kvm`).

```bash
cargo build --release
```

El binario compilado se generará en `target/release/two-five-five`.

---

## Ejecución

Puedes usar el script automatizado [`run.sh`](file:///home/v1p3r/Escritorio/255_ver_0.80/run.sh):

```bash
# Arranque automático (detecta SeaBIOS y cualquier ISO disponible)
./run.sh

# Arranque explícito con BIOS, ISO y Disco Duro virtual
./run.sh bios/bios-256k.bin kali.iso disk.img
```

O ejecutar directamente el binario:

```bash
./target/release/two-five-five bios/bios-256k.bin imagen.iso disco.img
```

### Variables de Entorno de Configuración

| Variable | Descripción | Valor por Defecto |
|----------|-------------|-------------------|
| `TWO_FIVE_FIVE_RAM` (o `TFF_RAM`) | Memoria RAM en MiB | `4096` |
| `TWO_FIVE_FIVE_CPUS` (o `TFF_CPUS`) | Número de núcleos vCPU | `4` |
| `TWO_FIVE_FIVE_NO_TUI` | Desactiva el dashboard TUI interactivo | Desactivado |
| `TWO_FIVE_FIVE_TAP` | Nombre de la interfaz TAP de red | No configurada (usa stack DHCP interno) |
| `TWO_FIVE_FIVE_AUTO_ENTER` | Inyección automática de Enter para bootloaders | `1` (activo) |
| `TWO_FIVE_FIVE_VERBOSE` | Muestra logs detallados de depuración de dispositivos | Desactivado |

*(Nota: Los prefijos heredados `MI_VMM_*` continúan siendo compatibles).*

---

## Tests

El proyecto cuenta con una amplia suite de pruebas que valida cada componente contra especificaciones reales de hardware y comportamiento de VirtualBox:

```bash
cargo test
```

Actualmente: **149 tests pasando (0 fallos)**.

---

## Licencia

Desarrollado por **v1p3r y equipo** como un hipervisor Tipo-2 de investigación y desarrollo de alto rendimiento.
