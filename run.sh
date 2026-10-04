#!/usr/bin/env bash
# ─── mi-vmm launcher ──────────────────────────────────────────────
# Usage: ./run.sh [bios.bin] [image.iso] [disk.img]
#
# If no BIOS is specified, tries common SeaBIOS paths.
# If no ISO is specified, boots without CD-ROM.
# If no disk is specified, boots without hard disk.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY="$SCRIPT_DIR/target/release/mi-vmm"

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

    if ! groups "$USER" 2>/dev/null | grep -qw "kvm"; then
        warn "Tu usuario no está en el grupo 'kvm'. Puede que necesites sudo."
        warn "Para evitarlo: sudo usermod -aG kvm \$USER (y vuelve a login)"
    fi

    info "KVM disponible: $(ls -la /dev/kvm 2>/dev/null | awk '{print $1, $3, $4}')"
}

# ─── Check binary ─────────────────────────────────────────────────
check_binary() {
    if [ ! -f "$BINARY" ]; then
        warn "Binario no encontrado. Compilando..."
        cd "$SCRIPT_DIR"
        cargo build --release || error "Compilación falló"
        cd - > /dev/null
    fi
    info "Binario: $BINARY ($(du -h "$BINARY" | cut -f1))"
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
        match=$(find "$SCRIPT_DIR" "." -maxdepth 1 -iname "*$target*.iso" 2>/dev/null | head -n 1)
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
    any_iso=$(find "$SCRIPT_DIR" -maxdepth 1 -name "*.iso" 2>/dev/null | head -n 1)
    if [ -n "$any_iso" ]; then
        echo "$any_iso"
        return
    fi

    echo ""
}

# ─── Main ─────────────────────────────────────────────────────────
echo ""
info "═══════════════════════════════════════════════"
info "  mi-vmm — Hipervisor Tipo-2 minimalista"
info "═══════════════════════════════════════════════"
echo ""

check_kvm
check_binary

ARG1="${1:-}"
ARG2="${2:-}"
ARG3="${3:-}"

BIOS=""
ISO=""
DISK=""

# Categorizar los argumentos pasados (BIOS, ISO o DISK)
for arg in "$ARG1" "$ARG2" "$ARG3"; do
    [ -z "$arg" ] && continue
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

# Si no se pasó ni ISO ni DISK, buscar ISO disponible
if [ -z "$DISK" ] && [ -z "$ISO" ]; then
    ISO=$(find_iso "")
fi

# Si hay una ISO pero no hay disco asignado, asegurar que exista un disco virtual (disk.img)
# para que el instalador de la VM siempre tenga un disco donde instalarse
if [ -n "$ISO" ] && [ -z "$DISK" ]; then
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
ARGS=("$BIOS")
[ -n "$ISO" ]  && ARGS+=("$ISO")
[ -n "$DISK" ] && ARGS+=("$DISK")
exec "$BINARY" "${ARGS[@]}"
