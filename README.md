# mi-vmm

Emulador / hipervisor Tipo-2 minimalista construido sobre **KVM** y **Rust**.

Arranca SeaBIOS y un guest Linux desde un CD-ROM ATAPI virtual sin necesidad de QEMU.

## Compilación

```bash
cargo build --release
```

## Ejecución

```bash
./target/release/mi-vmm /usr/share/seabios/bios-256k.bin [imagen.iso] [disco.img]
# ó
./run.sh
```

- Primer argumento: firmware BIOS (SeaBIOS, coreboot, etc.)
- Segundo argumento (opcional): imagen ISO para el CD-ROM virtual
- Tercer argumento (opcional): imagen de disco duro raw (.img)

## Test

```bash
cargo test
```

Actualmente: **20/20 tests pasan**.

## Arquitectura

```
┌─────────────────────────────────────────────────┐
│  mi-vmm (Rust + KVM)                            │
│                                                 │
│  ┌──────────┐  ┌──────────┐  ┌───────────────┐  │
│  │ VGA Text │  │ DebugCon │  │ CD-ROM ATAPI  │  │
│  │ Display  │  │ (0x402)  │  │ (0x170-0x177) │  │
│  └────┬─────┘  └────┬─────┘  └───────┬───────┘  │
│       │              │                │          │
│  ┌────┴──────────────┴────────────────┴───────┐  │
│  │              DeviceBus                     │  │
│  │  UART · PCI · USB · PM Timer · CMOS · ... │  │
│  └────────────────────┬───────────────────────┘  │
│                       │ KVM vmexit               │
│  ┌────────────────────┴───────────────────────┐  │
│  │              VMM Loop                      │  │
│  │  I/O dispatch · IRQ injection · BDA tick   │  │
│  └────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
         │ KVM /dev/kvm
         ▼
┌────────────────────────┐
│  Guest (SeaBIOS + OS)  │
│  256 MiB RAM + 512 Hi  │
└────────────────────────┘
```

## Dispositivos emulados

| Puerto | Dispositivo | Estado |
|--------|-------------|--------|
| 0x0402 | DebugCon → VGA mirror | ✅ |
| 0x0060/0x0064 | i8042 PS/2 | ✅ |
| 0x0070/0x0071 | CMOS/RTC | ✅ |
| 0x0040-0x0043 | PIT 8254 | ✅ |
| 0x00A0/0x00A0 | PIC 8259 (dual) | ✅ |
| 0x01F0-0x1F7 | Primary IDE (ATA disk) | ✅ |
| 0x0170-0x177 | Secondary IDE (CD-ROM ATAPI) | ✅ |
| 0x0376/0x3F6 | Alternate status | ✅ |
| 0x02F8-0x3FF | UART 16550 + COM stubs | ✅ |
| 0x0378/0x37A | LPT stubs | ✅ |
| 0x03B0-0x3DF | VGA (Bochs VBE) | ✅ |
| 0x0510-0x0511 | fw_cfg (QEMU) | ✅ |
| 0x0CF9 | Hardware reset | ✅ |
| 0x0800/0x808 | Port 92 (A20) | ✅ |
| 0xB000-0xB007 | ACPI PM Timer | ✅ |
| PCI config | i440FX + PIIX3 + VGA + USB | ✅ |
| 0xFEE00000 | Local APIC stub | ✅ |
| 0xFEC00000 | IOAPIC stub | ✅ |

## Estado actual del boot

```
✅ Reset vector → SeaBIOS POST
✅ PCI scan (6 dispositivos: i440FX, PIIX3, USB, VGA)
✅ SeaBIOS version banner en VGA
✅ ATA controllers detectados (primary + secondary)
✅ CD-ROM ATAPI detectado (IDENTIFY PACKET + SCSI INQUIRY)
✅ Boot sector leído del CD-ROM (ATAPI PACKET → SCSI READ 10)
✅ "Booting from DVD/CD..." → "Booting from 0000:7c00"
✅ ISOLINUX/GRUB loader ejecutándose desde 0x7C00
✅ Lectura multisectorial masiva (~98 MB de kernel + initrd)
✅ CR0 transitions tracked (real ↔ protected mode)
⚠️  ISOLINUX carga kernel/initrd pero no muestra splash (text mode VGA)
```

### Progreso detallado del boot

1. **POST** — SeaBIOS inicializa PCI, detecta dispositivos
2. **VGA ROM** — Option ROM ejecutado, modo texto activado
3. **ATA detection** — Primary (disk stub) + Secondary (CD-ROM)
4. **Boot order** — Floppy (falla) → DVD/CD (éxito)
5. **Boot sector** — SCSI READ(10) LBA=0 → 0x7C00
6. **ISOLINUX** — Lee kernel (`vmlinuz`) y initrd desde el ISO
7. **Multisector reads** — 32 sectores × 2048 bytes por operación
8. **Kernel loading** — ~98 MB leídos del CD-ROM

### Próximos pasos

- VGA framebuffer para graphics mode (ISOLINUX/GRUB splash)
- Soporte para Linux kernel mode (protected mode 32/64-bit completo)
- INT 13h extensions handler para bootloaders que las requieran

## INT 13h Handler Module

El módulo `bios_int13h` proporciona funciones para manejar interrupciones BIOS INT 13h:

- **AH=41h** — Check LBA Extensions (BX=0x55AA → CF=0, BX=0xAA55)
- **AH=42h** — Extended Read via DAP (Disk Address Packet → SCSI READ(10))
- **AH=08h** — Get Drive Parameters (BL=0x05 ATAPI CD-ROM)
- **DAP parsing** — Structura DiskAddressPacket de 16 bytes
- **LBA translation** — Conversión INT 13h (512B) → CD-ROM (2048B)
- **SCSI CDB builder** — build_scsi_read10_cdb()

## Cronología de correcciones

| Fix | Problema | Solución |
|-----|----------|----------|
| ATA status bits | ST_DRDY/ST_DRQ en bits incorrectos | Corregidos a 0x40/0x08 (matching SeaBIOS ata.h) |
| DebugCon → VGA | BIOS messages solo iban a serial | DebugCon escribe a 0xB8000 además de stderr |
| BDA tick counter | wait_ms() no avanzaba | Incremento a 18.2 Hz en main loop |
| CdRom stub | Secondary IDE no respondía sin ISO | `CdRom::stub()` creado siempre |
| LPT/COM stubs | SeaBIOS no detectaba puertos | PlatformStubs con data latch y LSR correcto |
| USB BAR sizing | pci_enable_iobar fallaba | Se salta detección durante sizing |
| ATAPI BSY phase | SeaBIOS nunca veía DRQ clear | `pending_packet` / `pending_identify` flags |
| CDB write handler | Solo procesaba 2 bytes de KVM | Ahora consume todos los bytes del buffer |
| Multi-sector read | Lecturas de 32 sectores | SCSI READ(10) con transfer_len variable |
| INT 13h module | Sin helpers para boot extensions | bios_int13h.rs con DAP parsing y SCSI CDB builder |
| CR0 tracking | Sin visibility de mode transitions | Log cada 50K exits con GDT/IDT info |
