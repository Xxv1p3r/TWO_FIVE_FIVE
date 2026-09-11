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

# ─── Main ─────────────────────────────────────────────────────────
echo ""
info "═══════════════════════════════════════════════"
info "  mi-vmm — Hipervisor Tipo-2 minimalista"
info "═══════════════════════════════════════════════"
echo ""

check_kvm
check_binary

BIOS=$(find_bios "${1:-}")
ISO="${2:-}"
DISK="${3:-}"

# Tolerancia: si el primer argumento es una ISO, el usuario la puso en la
# posicion del BIOS por accidente. Tómalo como ISO y deja que se autodetecte.
case "${1:-}" in
    *.iso|*.ISO)
        warn "El 1er argumento parece una ISO; lo redirijo a 'ISO' y busco el BIOS."
        ISO="${1}"
        BIOS=$(find_bios "")
        ;;
esac

echo ""
info "BIOS: $BIOS"
if [ -n "$ISO" ]; then
    [ -f "$ISO" ] || error "ISO no encontrado: $ISO"
    info "ISO:  $ISO"
else
    warn "Sin ISO — solo BIOS"
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
