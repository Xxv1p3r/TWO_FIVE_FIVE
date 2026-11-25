#!/usr/bin/env bash
# ─── Two Five Five (255) launcher ──────────────────────────────────
# Usage: ./run.sh [bios.bin] [image.iso] [disk.img]
#
# If no BIOS is specified, tries common SeaBIOS paths.
# If no ISO is specified, boots without CD-ROM.
# If no disk is specified, boots without hard disk.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$SCRIPT_DIR/target/release/two-five-five" ]; then
    BINARY="$SCRIPT_DIR/target/release/two-five-five"
elif [ -f "$SCRIPT_DIR/target/release/mi-vmm" ]; then
    BINARY="$SCRIPT_DIR/target/release/mi-vmm"
else
    BINARY="$SCRIPT_DIR/target/release/two-five-five"
fi

# ─── Colors ────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[VMM]${NC} $*"; }
warn()  { echo -e "${YELLOW}[VMM]${NC} $*"; }
error() { echo -e "${RED}[VMM]${NC} $*"; exit 1; }

# ─── Check KVM ────────────────────────────────────────────────────
check_kvm() {
    if [ ! -e /dev/kvm ]; then
        error "KVM no disponible. Instala KVM:\n  Debian/Ubuntu: sudo apt install qemu-kvm\n  Fedora:        sudo dnf install qemu-kvm\n  Arch:          sudo pacman -S qemu-full"
    fi

    if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
        warn "Sin permisos R/W en /dev/kvm para el usuario actual."
        warn "Ejecuta: sudo usermod -aG kvm \$USER && newgrp kvm"
    fi

    info "KVM disponible: $(ls -la /dev/kvm 2>/dev/null | awk '{print $1, $3, $4}' || echo '/dev/kvm')"
}

# ─── Check binary ─────────────────────────────────────────────────
check_binary() {
    if [ ! -x "$BINARY" ]; then
        warn "Binario no encontrado. Compilando..."
        if ! command -v cargo >/dev/null 2>&1; then
            if [ -f "$HOME/.cargo/env" ]; then
                # shellcheck disable=SC1091
                . "$HOME/.cargo/env"
            elif [ -d "$HOME/.cargo/bin" ]; then
                export PATH="$HOME/.cargo/bin:$PATH"
            fi
        fi
        if ! command -v cargo >/dev/null 2>&1; then
            error "Rust/Cargo no esta instalado o no esta en PATH.\n  Instala Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        fi
        if ! (cd "$SCRIPT_DIR" && cargo build --release); then
            error "Compilacion fallo. En Debian/Ubuntu instala dependencias:\n  sudo apt install build-essential pkg-config libx11-dev libxkbcommon-dev libwayland-dev"
        fi
    fi
    info "Binario: $BINARY ($(du -h "$BINARY" 2>/dev/null | cut -f1 || echo '?'))"
}

# ─── Find BIOS ────────────────────────────────────────────────────
find_bios() {
    local bios="$1"

    if [ -n "$bios" ]; then
        [ -f "$bios" ] || error "BIOS no encontrado: $bios"
        echo "$bios"
        return
    fi

    # Common SeaBIOS paths
    local candidates=(
        "$SCRIPT_DIR/bios/bios-256k.bin"
        "$SCRIPT_DIR/bios-256k.bin"
        "bios/bios-256k.bin"
        "bios-256k.bin"
        "/usr/share/seabios/bios-256k.bin"
        "/usr/share/seabios/bios-128k.bin"
        "/usr/share/qemu/bios-256k.bin"
        "/usr/share/qemu/bios-128k.bin"
        "/usr/share/edk2/ovmf/OVMF_CODE.fd"
    )

    for path in "${candidates[@]}"; do
        if [ -f "$path" ]; then
            # A stderr: esta función se llama dentro de $( ) y su stdout
            # es el valor de retorno (la ruta del BIOS).
            info "BIOS encontrado: $path" >&2
            echo "$path"
            return
        fi
    done

    error "No se encontró BIOS. Instala SeaBIOS:\n  Debian/Ubuntu: sudo apt install seabios\n  Fedora:        sudo dnf install seabios\n  Arch:          sudo pacman -S seabios\n\nO especifica la ruta: ./run.sh /ruta/a/bios.bin"
}

# ─── Find ISO ─────────────────────────────────────────────────────
find_iso() {
    local target="${1:-}"

    if [ -n "$target" ]; then
        if [ -f "$target" ]; then
            echo "$target"
            return
        fi
        if [ -f "$SCRIPT_DIR/$target" ]; then
            echo "$SCRIPT_DIR/$target"
            return
        fi
        for ext in ".iso" "-current.iso"; do
            if [ -f "$target$ext" ]; then
                echo "$target$ext"
                return
            fi
            if [ -f "$SCRIPT_DIR/$target$ext" ]; then
                echo "$SCRIPT_DIR/$target$ext"
                return
            fi
        done
        local match
        match=$(find "$SCRIPT_DIR" "." -maxdepth 1 -iname "*$target*.iso" -print -quit 2>/dev/null || true)
        if [ -n "$match" ] && [ -f "$match" ]; then
            echo "$match"
            return
        fi
        error "ISO no encontrado para '$target'"
    fi

    local candidates=(
        "$SCRIPT_DIR/CorePlus-current.iso"
        "$SCRIPT_DIR/TinyCore-current.iso"
        "$SCRIPT_DIR/Core-current.iso"
        "CorePlus-current.iso"
        "TinyCore-current.iso"
        "Core-current.iso"
    )
    for c in "${candidates[@]}"; do
        if [ -f "$c" ]; then
            echo "$c"
            return
        fi
    done

    local any_iso
    any_iso=$(find "$SCRIPT_DIR" -maxdepth 1 -name "*.iso" -print -quit 2>/dev/null || true)
    if [ -n "$any_iso" ]; then
        echo "$any_iso"
        return
    fi

    echo ""
}

# ─── Main ─────────────────────────────────────────────────────────
for arg in "$@"; do
    if [ "$arg" = "-h" ] || [ "$arg" = "--help" ]; then
        cat <<'EOF'
Uso: ./run.sh [opciones] [bios.bin] [imagen.iso] [disco.vdi|disco.img]

Opciones:
  -g, --gui, --launcher         Abrir el Administrador Grafico de VMs
  --startvm, --vm-name <nombre> Iniciar una VM configurada en vms.conf
  --no-gui                      Omitir lanzador grafico inicial
  --tui / --no-tui              Forzar / desactivar dashboard TUI
  --efi, --uefi                 Arrancar con firmware UEFI (OVMF)
  --no-disk                     No adjuntar ni autodetectar disco duro
  --no-iso, --no-cdrom          Arrancar sin imagen ISO
  --cpus <N> / --ram <MiB>      Recursos de la VM
  -u, --unattended              Activar instalacion desatendida
  --net <nombre> / --hostfwd <h:g>    Red virtual y redireccion de puertos
  --agent-status / --bogo-only / --agent-prompt <texto>  Agente IA Ollama (bogo Tailscale)
  --create-vdi / --compact-vdi / --disk-info   Utilidades de disco VDI
EOF
        exit 0
    fi
done

echo ""
info "═══════════════════════════════════════════════"
info "  Two Five Five (255) — Hipervisor Tipo-2"
info "═══════════════════════════════════════════════"
echo ""

NEEDS_KVM=1
for arg in "$@"; do
    if [ "$arg" = "--create-vdi" ] || [ "$arg" = "--compact-vdi" ] || [ "$arg" = "--disk-info" ] || \
       [ "$arg" = "--agent-status" ] || [[ "$arg" == --agent-prompt* ]]; then
        NEEDS_KVM=0
        break
    fi
done

check_binary
if [ "$NEEDS_KVM" -eq 1 ]; then
    check_kvm
fi

FLAGS=()
BIOS=""
ISO=""
DISK=""
NO_DISK=0
NO_ISO=0
NO_GUI_ENV="${TWO_FIVE_FIVE_NO_GUI:-${TFF_NO_GUI:-${MI_VMM_NO_GUI:-}}}"

# Si se ejecuta sin argumentos y hay servidor gráfico disponible, abrir el Mini GUI
if [ $# -eq 0 ] && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; } && [ -z "$NO_GUI_ENV" ]; then
    exec "$BINARY" --gui
fi

# Si se solicita modo GUI, startvm o utilidades VDI directamente, delegar al binario
for arg in "$@"; do
    if [ "$arg" = "-g" ] || [ "$arg" = "--gui" ] || [ "$arg" = "--launcher" ] || \
       [ "$arg" = "--create-vdi" ] || [ "$arg" = "--compact-vdi" ] || [ "$arg" = "--disk-info" ] || \
       [ "$arg" = "--agent-status" ] || [ "$arg" = "--bogo-only" ] || [[ "$arg" == --agent-prompt* ]] || \
       [[ "$arg" == --startvm* ]] || [[ "$arg" == --vm-name* ]]; then
        exec "$BINARY" "$@"
    fi
done

# Categorizar los argumentos pasados (Flags, BIOS, ISO o DISK)
expect_val=0
for arg in "$@"; do
    [ -z "$arg" ] && continue
    if [ "$expect_val" -eq 1 ]; then
        FLAGS+=("$arg")
        expect_val=0
        continue
    fi
    case "$arg" in
        --cpus|--ram|--iso|--cdrom|--disk|--drive|--hda|--unattended-user|--unattended-pass|--unattended-host|--net|--net-mode|--net-subnet|--tap|--mac|--hostfwd|--ollama-model|--ollama-url)
            FLAGS+=("$arg")
            expect_val=1
            continue
            ;;
        --no-disk)
            NO_DISK=1
            FLAGS+=("$arg")
            continue
            ;;
        --no-iso|--no-cdrom)
            NO_ISO=1
            FLAGS+=("$arg")
            continue
            ;;
        -u|--unattended|--unattended-*|--cpus=*|--ram=*|--iso=*|--cdrom=*|--disk=*|--drive=*|--hda=*|--net=*|--net-mode=*|--net-subnet=*|--tap=*|--mac=*|--hostfwd=*|--ollama-model=*|--ollama-url=*|--no-gui|--tui|--no-tui|--efi|--uefi)
            FLAGS+=("$arg")
            continue
            ;;
    esac
    if [ -z "$BIOS" ] && ([[ "$arg" == *.bin ]] || [[ "$arg" == *.fd ]] || [[ "$arg" == *seabios* ]] || [[ "$arg" == *ovmf* ]]); then
        BIOS=$(find_bios "$arg")
    elif [ -z "$DISK" ] && ([[ "$arg" == *.img ]] || [[ "$arg" == *.raw ]] || [[ "$arg" == *.vdi ]] || [[ "$arg" == *.qcow2 ]]); then
        [ -f "$arg" ] || error "Disco no encontrado: $arg"
        DISK="$arg"
    elif [ -z "$ISO" ]; then
        candidate_iso=$(find_iso "$arg" 2>/dev/null || true)
        if [ -n "$candidate_iso" ] && [ -f "$candidate_iso" ]; then
            ISO="$candidate_iso"
        elif [ -f "$arg" ]; then
            DISK="$arg"
        fi
    fi
done

if [ -z "$BIOS" ]; then
    BIOS=$(find_bios "")
fi

# Si no se pasó ni ISO ni DISK, buscar ISO disponible salvo --no-iso
if [ "$NO_ISO" -eq 0 ] && [ -z "$DISK" ] && [ -z "$ISO" ]; then
    ISO=$(find_iso "")
fi

# Si hay una ISO pero no hay disco asignado (y no se pidió --no-disk), asegurar disk.img
if [ "$NO_DISK" -eq 0 ] && [ -n "$ISO" ] && [ -z "$DISK" ]; then
    if [ -f "$SCRIPT_DIR/disk.img" ]; then
        DISK="$SCRIPT_DIR/disk.img"
        info "Disco duro virtual detectado: $DISK"
    else
        info "Creando disco duro virtual sparse de 20 GB (disk.img, ocupa 0 MB reales)..."
        truncate -s 20G "$SCRIPT_DIR/disk.img"
        DISK="$SCRIPT_DIR/disk.img"
    fi
fi

echo ""
info "BIOS: $BIOS"
if [ -n "$ISO" ]; then
    info "ISO:  $ISO"
else
    info "ISO:  (ninguna — arrancando directamente desde disco duro)"
fi
if [ -n "$DISK" ]; then
    [ -f "$DISK" ] || error "Disco no encontrado: $DISK"
    info "DISK: $DISK"
fi
echo ""

# Run the VMM
info "Arrancando VM..."
echo ""

# Build argument list (BIOS is always present; ISO/DISK only if provided)
ARGS=()
[ ${#FLAGS[@]} -gt 0 ] && ARGS+=("${FLAGS[@]}")
ARGS+=("$BIOS")
[ -n "$ISO" ]  && ARGS+=("$ISO")
[ -n "$DISK" ] && ARGS+=("$DISK")
exec "$BINARY" "${ARGS[@]}"
