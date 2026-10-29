#!/usr/bin/env python3
"""
Generador de assets gráficos personalizados para el hipervisor Two Five Five (255).
Procesa el logotipo oficial del gato 255 (removiendo el fondo blanco exterior con
anti-aliasing limpio y preservando los brillos blancos de los ojos) y genera todos
los iconos, banners de asistentes y marcos de monitor en `assets/`.
"""

import math
import os
from collections import deque
from PIL import Image, ImageDraw, ImageFilter

BASE_DIR = "/home/v1p3r/Escritorio/255_ver_0.80"
ASSETS_DIR = os.path.join(BASE_DIR, "assets")
OSE_DIR = os.path.join(ASSETS_DIR, "OSE")
UPLOADED_LOGO = "/home/v1p3r/.gemini/antigravity-cli/brain/d819138d-a86e-43a7-a9f7-a9acb458065e/.user_uploaded/uploaded_media_1793226783098.png"

os.makedirs(ASSETS_DIR, exist_ok=True)
os.makedirs(OSE_DIR, exist_ok=True)


def extract_clean_cat_logo(src_path: str) -> Image.Image:
    """
    Elimina únicamente el fondo blanco exterior mediante flood-fill desde los bordes,
    conservando intactos los reflejos blancos dentro de las pupilas, el iris amarillo
    y el pelaje negro, eliminando el halo blanco en los bordes anti-aliased.
    """
    im = Image.open(src_path).convert("RGBA")
    w, h = im.size
    px = im.load()

    visited = bytearray(w * h)
    q = deque()

    # Semillas en los 4 bordes de la imagen
    for x in range(w):
        q.append((x, 0))
        q.append((x, h - 1))
        visited[x] = 1
        visited[(h - 1) * w + x] = 1
    for y in range(h):
        q.append((0, y))
        q.append((w - 1, y))
        visited[y * w] = 1
        visited[y * w + (w - 1)] = 1

    # Flood-fill que se detiene en el contorno negro del gato
    while q:
        x, y = q.popleft()
        r, g, b, a = px[x, y]
        lum = (r + g + b) // 3
        # Solo expandir si el píxel no es parte del trazo negro profundo
        if lum > 35:
            for nx, ny in ((x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)):
                if 0 <= nx < w and 0 <= ny < h:
                    idx = ny * w + nx
                    if not visited[idx]:
                        nr, ng, nb, _ = px[nx, ny]
                        # El exterior es acromático (gris/blanco); evitar entrar en zonas amarillas por seguridad
                        if (nr + ng + nb) // 3 > 18 and abs(nr - nb) < 40:
                            visited[idx] = 1
                            q.append((nx, ny))

    # Convertir píxeles visitados del exterior en transparente / borde negro anti-aliased puro
    min_x, min_y, max_x, max_y = w, h, 0, 0
    for y in range(h):
        row_off = y * w
        for x in range(w):
            if visited[row_off + x]:
                r, g, b, _ = px[x, y]
                lum = (r + g + b) // 3
                if lum >= 242:
                    px[x, y] = (0, 0, 0, 0)
                else:
                    # Des-premultiplicar contra blanco para que no quede halo claro sobre fondos oscuros
                    alpha = int((242 - lum) * 255 / 242)
                    px[x, y] = (10, 10, 12, max(0, min(255, alpha)))
            else:
                if x < min_x:
                    min_x = x
                if x > max_x:
                    max_x = x
                if y < min_y:
                    min_y = y
                if y > max_y:
                    max_y = y

    # Recortar al bounding box del gato y centrar en un lienzo cuadrado con margen limpio
    cropped = im.crop((min_x, min_y, max_x + 1, max_y + 1))
    cw, ch = cropped.size
    side = int(max(cw, ch) * 1.06)
    square = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    ox = (side - cw) // 2
    oy = (side - ch) // 2
    square.paste(cropped, (ox, oy), cropped)
    return square


def save_png(im: Image.Image, rel_path: str):
    full_path = os.path.join(ASSETS_DIR, rel_path)
    os.makedirs(os.path.dirname(full_path), exist_ok=True)
    im.convert("RGBA").save(full_path, format="PNG", optimize=False)


def draw_rounded_rect(draw: ImageDraw.ImageDraw, box, radius, fill=None, outline=None, width=1):
    draw.rounded_rectangle(box, radius=radius, fill=fill, outline=outline, width=width)


def make_welcome_200(cat_sq: Image.Image) -> Image.Image:
    """Crea el emblema principal de 200x200 para la pantalla de bienvenida."""
    im = Image.new("RGBA", (200, 200), (0, 0, 0, 0))
    draw = ImageDraw.Draw(im)
    # Disco de fondo oscuro con doble aro dorado/ámbar estilo 255
    draw.ellipse((8, 8, 191, 191), fill=(20, 23, 29, 245), outline=(245, 196, 0, 255), width=3)
    draw.ellipse((15, 15, 184, 184), outline=(70, 80, 95, 180), width=1)
    cat_156 = cat_sq.resize((156, 156), Image.Resampling.LANCZOS)
    im.alpha_composite(cat_156, (22, 22))
    return im


def make_wizard_banner(cat_sq: Image.Image, mode: str) -> Image.Image:
    """
    Genera los banners laterales de 145x290 para los asistentes:
    - 'welcome': Nuevo VM Wizard
    - 'harddisk': Nuevo Disco Virtual VDI Wizard
    - 'clone': Clonar VM Wizard
    """
    w, h = 145, 290
    im = Image.new("RGBA", (w, h), (18, 21, 27, 255))
    px = im.load()

    # Gradiente vertical carbono oscuro + patrón sutil de rejilla tecnológica
    for y in range(h):
        t = y / float(h - 1)
        r = int(16 + 12 * (1.0 - t))
        g = int(19 + 14 * (1.0 - t))
        b = int(26 + 20 * (1.0 - t))
        for x in range(w):
            if (x % 16 == 0 or y % 16 == 0) and ((x + y) % 4 == 0):
                px[x, y] = (min(255, r + 10), min(255, g + 12), min(255, b + 15), 255)
            else:
                px[x, y] = (r, g, b, 255)

    draw = ImageDraw.Draw(im)
    # Borde exterior y línea lateral dorada
    draw.rectangle((0, 0, w - 1, h - 1), outline=(52, 60, 74, 255), width=1)
    draw.line((0, 0, 0, h - 1), fill=(245, 196, 0, 255), width=3)
    draw.line((12, 28, w - 12, 28), fill=(245, 196, 0, 180), width=1)

    # Cabecera "255" estilizada
    draw.text((14, 10), "TWO FIVE FIVE", fill=(245, 196, 0, 255))

    # Halo detrás del gato
    draw.ellipse((20, 42, 125, 147), fill=(28, 33, 43, 255), outline=(245, 196, 0, 140), width=2)
    cat_100 = cat_sq.resize((104, 104), Image.Resampling.LANCZOS)
    im.alpha_composite(cat_100, (21, 43))

    # Ilustración inferior específica del asistente
    if mode == "welcome":
        # Cubo/nodo de hipervisor + líneas de circuito doradas
        draw.rounded_rectangle((22, 168, 122, 232), radius=8, fill=(26, 31, 40, 255), outline=(245, 196, 0, 210), width=2)
        draw.text((38, 182), "VM NODE", fill=(240, 244, 250, 255))
        draw.text((35, 202), "x86_64 KVM", fill=(245, 196, 0, 255))
        draw.line((72, 148, 72, 168), fill=(245, 196, 0, 200), width=2)
    elif mode == "harddisk":
        # Ilustración de disco VDI de alta velocidad con platos dorados
        draw.rounded_rectangle((28, 164, 116, 238), radius=10, fill=(28, 34, 44, 255), outline=(140, 155, 175, 255), width=2)
        draw.ellipse((42, 174, 102, 228), fill=(18, 22, 29, 255), outline=(245, 196, 0, 230), width=2)
        draw.ellipse((64, 194, 80, 208), fill=(245, 196, 0, 255))
        draw.polygon([(72, 201), (95, 222), (88, 226)], fill=(220, 225, 235, 255))
    else:
        # Ilustración de clonación (dos nodos sincronizados)
        draw.rounded_rectangle((20, 168, 78, 220), radius=6, fill=(26, 31, 40, 255), outline=(130, 150, 175, 255), width=2)
        draw.rounded_rectangle((66, 182, 124, 234), radius=6, fill=(32, 38, 50, 255), outline=(245, 196, 0, 240), width=2)
        draw.text((80, 202), "255", fill=(245, 196, 0, 255))

    # Pie del banner
    draw.line((12, 256, w - 12, 256), fill=(60, 70, 86, 200), width=1)
    draw.text((26, 266), "255 HYPERVISOR", fill=(170, 182, 198, 255))
    return im


def make_monitor_preview(cat_sq: Image.Image, empty_logo: bool) -> Image.Image:
    """
    Genera el marco de monitor 16:9 de 242x155 (`preview_full_16to9_242x155px.png`
    y `preview_empty_16to9_242x155px.png`) con estética Two Five Five (255).
    """
    w, h = 242, 155
    im = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    draw = ImageDraw.Draw(im)

    # Sombra y peana metálica inferior
    draw.ellipse((74, 143, 168, 153), fill=(30, 35, 42, 220), outline=(85, 95, 110, 255), width=1)
    draw.polygon([(106, 128), (136, 128), (144, 147), (98, 147)], fill=(55, 62, 74, 255), outline=(90, 100, 115, 255))

    # Carcasa exterior del monitor
    draw.rounded_rectangle((6, 4, 235, 131), radius=7, fill=(28, 32, 39, 255), outline=(92, 102, 118, 255), width=2)
    # Bisel interior fino
    draw.rounded_rectangle((9, 7, 232, 120), radius=4, fill=(14, 17, 22, 255), outline=(45, 52, 64, 255), width=1)

    # Pantalla interna 16:9 (12, 10) -> (229, 117)
    for sy in range(10, 117):
        t = (sy - 10) / 107.0
        r = int(17 + 10 * (1.0 - t))
        g = int(21 + 12 * (1.0 - t))
        b = int(29 + 16 * (1.0 - t))
        draw.line((12, sy, 229, sy), fill=(r, g, b, 255))

    if empty_logo:
        # En preview_empty mostramos el gato 255 centrado a todo color
        cat_72 = cat_sq.resize((76, 76), Image.Resampling.LANCZOS)
        im.alpha_composite(cat_72, (83, 24))
    else:
        # En preview_full ponemos una marca de agua sutil en esquina para no tapar el icono de SO ni el texto
        cat_wm = cat_sq.resize((36, 36), Image.Resampling.LANCZOS)
        # Atenuar opacidad de la marca de agua
        r, g, b, a = cat_wm.split()
        a = a.point(lambda v: int(v * 0.28))
        cat_wm = Image.merge("RGBA", (r, g, b, a))
        im.alpha_composite(cat_wm, (190, 14))

    # Badge "255" en el bisel inferior + LED ámbar de encendido
    draw.text((112, 121), "255", fill=(245, 196, 0, 255))
    draw.ellipse((220, 123, 225, 128), fill=(245, 196, 0, 255))
    return im


def make_os_icon(kind: str) -> Image.Image:
    """Genera iconos de sistemas operativos de 32x32 nítidos y distintivos."""
    s = 32
    im = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    draw = ImageDraw.Draw(im)

    palettes = {
        "win11": ((0, 120, 212), (0, 180, 255), "W11"),
        "win10": ((0, 103, 184), (40, 160, 245), "W10"),
        "win81": ((0, 138, 230), (80, 190, 255), "W8"),
        "win8": ((0, 138, 230), (80, 190, 255), "W8"),
        "win7": ((22, 115, 185), (76, 175, 80), "W7"),
        "winvista": ((18, 105, 155), (80, 200, 180), "WV"),
        "winxp": ((35, 95, 200), (235, 110, 35), "XP"),
        "win2k": ((45, 85, 155), (140, 170, 210), "2K"),
        "win_other": ((40, 90, 160), (100, 150, 220), "WIN"),
        "ubuntu": ((233, 84, 32), (255, 145, 70), "UBU"),
        "debian": ((215, 10, 83), (245, 90, 130), "DEB"),
        "archlinux": ((23, 147, 209), (80, 195, 245), "ARC"),
        "fedora": ((41, 65, 114), (81, 162, 218), "FED"),
        "redhat": ((204, 0, 0), (245, 80, 80), "RHEL"),
        "opensuse": ((115, 186, 37), (165, 225, 85), "SUSE"),
        "gentoo": ((84, 72, 122), (155, 135, 205), "GEN"),
        "linux26": ((42, 48, 60), (245, 196, 0), "LNX"),
        "linux": ((42, 48, 60), (245, 196, 0), "LNX"),
        "freebsd": ((171, 43, 40), (235, 95, 70), "BSD"),
        "macosx": ((55, 65, 81), (180, 195, 215), "MAC"),
        "dos": ((35, 38, 45), (16, 185, 129), "DOS"),
        "other": ((75, 85, 99), (245, 196, 0), "VM"),
    }
    c1, c2, tag = palettes.get(kind, palettes["other"])

    # Fondo redondeado con borde brillante
    draw.rounded_rectangle((2, 2, 29, 29), radius=6, fill=(*c1, 255), outline=(*c2, 255), width=2)

    if kind.startswith("win"):
        # Ventana de 4 paneles estilo Windows
        draw.rectangle((7, 7, 14, 14), fill=(255, 255, 255, 240))
        draw.rectangle((17, 7, 24, 14), fill=(255, 255, 255, 240))
        draw.rectangle((7, 17, 14, 24), fill=(255, 255, 255, 240))
        draw.rectangle((17, 17, 24, 24), fill=(255, 255, 255, 240))
        if kind == "win7":
            draw.rectangle((7, 7, 14, 14), fill=(242, 80, 34, 255))
            draw.rectangle((17, 7, 24, 14), fill=(127, 186, 0, 255))
            draw.rectangle((7, 17, 14, 24), fill=(0, 164, 239, 255))
            draw.rectangle((17, 17, 24, 24), fill=(255, 185, 0, 255))
    elif kind in ("ubuntu", "debian", "fedora", "redhat", "opensuse", "gentoo"):
        # Círculo concéntrico de distribución Linux
        draw.ellipse((8, 8, 23, 23), outline=(255, 255, 255, 245), width=2)
        draw.ellipse((13, 13, 18, 18), fill=(*c2, 255))
    elif kind == "archlinux":
        # Pico estilizado de Arch
        draw.polygon([(16, 6), (7, 25), (13, 25), (16, 17), (19, 25), (25, 25)], fill=(255, 255, 255, 245))
    elif kind in ("linux26", "linux"):
        # Emblema de núcleo Linux / pingüino estilizado
        draw.ellipse((9, 6, 22, 25), fill=(20, 22, 26, 255), outline=(245, 196, 0, 255), width=1)
        draw.ellipse((12, 12, 19, 22), fill=(245, 247, 250, 255))
        draw.polygon([(14, 10), (18, 10), (16, 13)], fill=(245, 196, 0, 255))
    elif kind == "freebsd":
        draw.ellipse((8, 9, 23, 24), fill=(220, 38, 38, 255), outline=(255, 255, 255, 220), width=1)
        draw.polygon([(8, 11), (5, 5), (12, 9)], fill=(255, 200, 120, 255))
        draw.polygon([(23, 11), (26, 5), (19, 9)], fill=(255, 200, 120, 255))
    elif kind == "dos":
        draw.text((5, 10), "C:\\>", fill=(16, 185, 129, 255))
    else:
        tx = 16 - len(tag) * 3
        draw.text((max(4, tx), 11), tag, fill=(255, 255, 255, 255))

    return im


def make_icon(kind: str, size: int, cat_sq: Image.Image) -> Image.Image:
    """Genera iconos vectoriales limpios de 32px, 24px, 16px o 10px."""
    im = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(im)

    # Paleta Two Five Five (255): carbón, oro, esmeralda, azul técnico, carmesí
    DARK = (30, 35, 44, 255)
    SLATE = (70, 82, 98, 255)
    LIGHT = (230, 236, 245, 255)
    GOLD = (245, 196, 0, 255)
    GREEN = (34, 197, 94, 255)
    GREEN_DARK = (21, 128, 61, 255)
    BLUE = (56, 189, 248, 255)
    BLUE_DARK = (3, 105, 161, 255)
    RED = (239, 68, 68, 255)
    AMBER = (245, 158, 11, 255)

    m = max(1, size // 10)
    c = size // 2

    def add_badge_plus():
        r = max(4, size // 3)
        bx, by = size - r - 1, size - r - 1
        draw.ellipse((bx - r, by - r, bx + r, by + r), fill=GREEN, outline=(255, 255, 255, 240), width=1)
        w = max(1, size // 16)
        d = max(2, r - 2)
        draw.line((bx - d, by, bx + d, by), fill=(255, 255, 255, 255), width=w)
        draw.line((bx, by - d, bx, by + d), fill=(255, 255, 255, 255), width=w)

    def add_badge_minus():
        r = max(4, size // 3)
        bx, by = size - r - 1, size - r - 1
        draw.ellipse((bx - r, by - r, bx + r, by + r), fill=RED, outline=(255, 255, 255, 240), width=1)
        d = max(2, r - 2)
        draw.line((bx - d, by, bx + d, by), fill=(255, 255, 255, 255), width=max(1, size // 16))

    if kind == "vm_new":
        # Gato 255 + estrella/plus dorado-verde de creación
        cat_s = cat_sq.resize((size - 2, size - 2), Image.Resampling.LANCZOS)
        im.alpha_composite(cat_s, (1, 1))
        add_badge_plus()
    elif kind == "vm_add":
        draw.rounded_rectangle((m, m + 3, size - m - 1, size - m - 1), radius=3, fill=DARK, outline=GOLD, width=max(1, size // 16))
        draw.rectangle((m + 2, m, c, m + 4), fill=GOLD)
        add_badge_plus()
    elif kind == "vm_clone":
        draw.rounded_rectangle((m, m, size - m - 5, size - m - 5), radius=3, fill=SLATE, outline=LIGHT, width=1)
        draw.rounded_rectangle((m + 5, m + 5, size - m - 1, size - m - 1), radius=3, fill=DARK, outline=GOLD, width=max(1, size // 16))
    elif kind in ("vm_settings", "global_settings", "hd_modify"):
        # Engranaje metálico/dorado
        r_out = size // 2 - m
        r_in = max(3, size // 5)
        for i in range(8):
            ang = i * (math.pi / 4.0)
            tx = int(c + math.cos(ang) * (r_out - 1))
            ty = int(c + math.sin(ang) * (r_out - 1))
            tr = max(2, size // 9)
            draw.ellipse((tx - tr, ty - tr, tx + tr, ty + tr), fill=GOLD if kind != "global_settings" else BLUE)
        draw.ellipse((c - r_out + 2, c - r_out + 2, c + r_out - 2, c + r_out - 2), fill=DARK, outline=GOLD if kind != "global_settings" else BLUE, width=max(1, size // 14))
        draw.ellipse((c - r_in, c - r_in, c + r_in, c + r_in), fill=GOLD if kind != "global_settings" else LIGHT)
    elif kind in ("vm_start", "state_running"):
        draw.ellipse((m, m, size - m - 1, size - m - 1), fill=DARK, outline=GREEN, width=max(1, size // 14))
        p1 = (size * 38 // 100, size * 28 // 100)
        p2 = (size * 38 // 100, size * 72 // 100)
        p3 = (size * 74 // 100, c)
        draw.polygon([p1, p2, p3], fill=GREEN)
    elif kind in ("vm_pause", "state_paused"):
        draw.ellipse((m, m, size - m - 1, size - m - 1), fill=DARK, outline=AMBER, width=max(1, size // 14))
        bw = max(2, size // 7)
        draw.rectangle((c - bw - 2, size // 4, c - 2, size * 3 // 4), fill=AMBER)
        draw.rectangle((c + 2, size // 4, c + bw + 2, size * 3 // 4), fill=AMBER)
    elif kind in ("vm_poweroff", "state_powered_off"):
        col = RED if kind == "vm_poweroff" else (148, 163, 184, 255)
        draw.ellipse((m, m, size - m - 1, size - m - 1), fill=DARK, outline=col, width=max(1, size // 14))
        draw.line((c, m + 2, c, c), fill=GOLD if kind == "vm_poweroff" else col, width=max(2, size // 8))
    elif kind == "state_saved":
        draw.rounded_rectangle((m, m, size - m - 1, size - m - 1), radius=3, fill=DARK, outline=BLUE, width=max(1, size // 14))
        draw.rectangle((m + 3, m + 2, size - m - 4, c), fill=BLUE)
    elif kind == "state_aborted":
        draw.polygon([(c, m), (size - m - 1, size - m - 1), (m, size - m - 1)], fill=RED, outline=LIGHT)
        draw.line((c, m + 4, c, size - m - 5), fill=(255, 255, 255, 255), width=2)
    elif kind in ("vm_reset", "refresh"):
        draw.ellipse((m, m, size - m - 1, size - m - 1), fill=DARK, outline=GOLD, width=max(2, size // 10))
        draw.polygon([(size - m - 2, c - 4), (size - m + 2, c + 2), (size - m - 6, c + 2)], fill=GOLD)
    elif kind in ("vm_discard", "hd_release", "cd_unmount"):
        draw.rounded_rectangle((m, m, size - m - 1, size - m - 1), radius=4, fill=DARK, outline=AMBER, width=max(1, size // 14))
        draw.polygon([(c, size // 4), (size * 3 // 4, c + 2), (size // 4, c + 2)], fill=AMBER)
        draw.rectangle((size // 4, c + 4, size * 3 // 4, c + 7), fill=LIGHT)
    elif kind in ("vm_delete", "hd_remove"):
        draw.rounded_rectangle((m + 2, m + 4, size - m - 3, size - m - 1), radius=2, fill=DARK, outline=RED, width=max(1, size // 14))
        draw.line((m, m + 3, size - m - 1, m + 3), fill=RED, width=max(2, size // 10))
        draw.line((c - 2, m + 7, c - 2, size - m - 4), fill=LIGHT, width=1)
        draw.line((c + 2, m + 7, c + 2, size - m - 4), fill=LIGHT, width=1)
    elif kind in ("hd", "hd_create", "hd_add", "media_manager"):
        # Unidad de disco duro / VDI estilizada
        draw.rounded_rectangle((m + 1, m, size - m - 2, size - m - 1), radius=3, fill=DARK, outline=SLATE, width=max(1, size // 16))
        r = max(3, (size - 2 * m) // 3)
        draw.ellipse((c - r, c - r - 1, c + r, c + r - 1), fill=(18, 22, 28, 255), outline=GOLD, width=max(1, size // 16))
        draw.ellipse((c - 2, c - 3, c + 2, c + 1), fill=GOLD)
        if kind in ("hd_create", "hd_add"):
            add_badge_plus()
    elif kind in ("cd", "cd_add"):
        # Disco óptico ISO con reflejo dorado/azul
        draw.ellipse((m, m, size - m - 1, size - m - 1), fill=DARK, outline=BLUE, width=max(1, size // 14))
        r_in = max(2, size // 6)
        draw.ellipse((c - r_in, c - r_in, c + r_in, c + r_in), fill=(0, 0, 0, 0), outline=GOLD, width=1)
        if kind == "cd_add":
            add_badge_plus()
    elif kind == "fd":
        draw.rounded_rectangle((m, m, size - m - 1, size - m - 1), radius=2, fill=DARK, outline=GOLD, width=1)
        draw.rectangle((m + 3, m + 1, size - m - 4, c - 1), fill=LIGHT)
    elif kind in ("machine", "machine_details_manager", "vrdp", "interface"):
        # Monitor / Máquina
        draw.rounded_rectangle((m, m + 1, size - m - 1, size - m - 4), radius=2, fill=DARK, outline=GOLD if kind == "machine" else BLUE, width=max(1, size // 14))
        draw.line((c - 4, size - m - 1, c + 4, size - m - 1), fill=LIGHT, width=max(1, size // 12))
        draw.line((c, size - m - 4, c, size - m - 1), fill=LIGHT, width=max(1, size // 12))
    elif kind in ("chipset", "cpu"):
        # Microprocesador / Chipset con pines dorados
        for p in range(m + 3, size - m - 2, max(3, size // 6)):
            draw.line((p, m, p, size - m - 1), fill=GOLD, width=1)
            draw.line((m, p, size - m - 1, p), fill=GOLD, width=1)
        draw.rounded_rectangle((m + 2, m + 2, size - m - 3, size - m - 3), radius=2, fill=DARK, outline=GOLD, width=max(1, size // 16))
        draw.rectangle((c - 2, c - 2, c + 2, c + 2), fill=GOLD)
    elif kind == "ram":
        draw.rounded_rectangle((m, c - 4, size - m - 1, c + 4), radius=2, fill=DARK, outline=GREEN, width=1)
        for x in range(m + 3, size - m - 3, 4):
            draw.rectangle((x, c - 2, x + 2, c + 2), fill=GOLD)
    elif kind == "sound":
        draw.polygon([(m + 2, c - 3), (c - 1, c - 3), (c + 4, m + 2), (c + 4, size - m - 3), (c - 1, c + 3), (m + 2, c + 3)], fill=GOLD)
        draw.arc((c + 1, m + 3, size - m, size - m - 3), start=-60, end=60, fill=BLUE, width=max(1, size // 14))
    elif kind == "nw":
        # Red Ethernet / Nodos conectados
        draw.ellipse((c - 3, m + 1, c + 3, m + 7), fill=GOLD)
        draw.ellipse((m + 1, size - m - 7, m + 7, size - m - 1), fill=BLUE)
        draw.ellipse((size - m - 7, size - m - 7, size - m - 1, size - m - 1), fill=BLUE)
        draw.line((c, m + 4, m + 4, size - m - 4), fill=LIGHT, width=max(1, size // 16))
        draw.line((c, m + 4, size - m - 4, size - m - 4), fill=LIGHT, width=max(1, size // 16))
        draw.line((m + 4, size - m - 4, size - m - 4, size - m - 4), fill=LIGHT, width=max(1, size // 16))
    elif kind == "usb":
        draw.line((c, m + 2, c, size - m - 2), fill=GOLD, width=max(1, size // 12))
        draw.ellipse((c - 3, size - m - 5, c + 3, size - m - 1), fill=GOLD)
        draw.polygon([(c, m + 1), (c - 3, m + 5), (c + 3, m + 5)], fill=GOLD)
        draw.line((c, c + 2, c - 5, c - 2), fill=BLUE, width=1)
        draw.line((c, c + 4, c + 5, c), fill=BLUE, width=1)
    elif kind == "serial_port":
        draw.rounded_rectangle((m, c - 4, size - m - 1, c + 4), radius=2, fill=DARK, outline=GOLD, width=1)
        for x in range(m + 3, size - m - 2, 3):
            draw.point((x, c - 1), fill=GOLD)
            draw.point((x, c + 1), fill=GOLD)
    elif kind == "sf":
        draw.rounded_rectangle((m, m + 3, size - m - 1, size - m - 2), radius=2, fill=DARK, outline=GOLD, width=max(1, size // 16))
        draw.rectangle((m + 1, m + 1, c, m + 4), fill=GOLD)
    elif kind in ("vm_show_logs", "description"):
        draw.rounded_rectangle((m + 2, m, size - m - 2, size - m - 1), radius=2, fill=DARK, outline=GOLD, width=1)
        for y in range(m + 4, size - m - 3, 3):
            draw.line((m + 5, y, size - m - 5, y), fill=LIGHT, width=1)
    elif kind == "snapshot_manager":
        draw.rounded_rectangle((m, m + 3, size - m - 1, size - m - 2), radius=3, fill=DARK, outline=GOLD, width=1)
        draw.ellipse((c - 4, c - 3, c + 4, c + 5), fill=BLUE, outline=LIGHT, width=1)
    elif kind == "performance_monitor":
        draw.rounded_rectangle((m, m, size - m - 1, size - m - 1), radius=3, fill=DARK, outline=GOLD, width=1)
        draw.line((m + 2, size - m - 4, c - 2, c, c + 2, c + 3, size - m - 3, m + 4), fill=GREEN, width=2)
    elif kind in ("tools_global", "tools_menu"):
        cat_s = cat_sq.resize((size - 2, size - 2), Image.Resampling.LANCZOS)
        im.alpha_composite(cat_s, (1, 1))
    elif kind in ("ok", "status_check"):
        draw.ellipse((1, 1, size - 2, size - 2), fill=DARK, outline=GREEN, width=1)
        draw.line((4, c, c - 1, size - 5, size - 4, 4), fill=GREEN, width=2)
    elif kind in ("cancel", "close", "status_error"):
        draw.ellipse((1, 1, size - 2, size - 2), fill=DARK, outline=RED, width=1)
        draw.line((4, 4, size - 5, size - 5), fill=RED, width=2)
        draw.line((size - 5, 4, 4, size - 5), fill=RED, width=2)
    elif kind == "arrow_down":
        draw.polygon([(1, 3), (size - 2, 3), (c, size - 2)], fill=GOLD)
    elif kind == "arrow_right":
        draw.polygon([(3, 1), (3, size - 2), (size - 2, c)], fill=GOLD)
    else:
        cat_s = cat_sq.resize((size, size), Image.Resampling.LANCZOS)
        im.alpha_composite(cat_s, (0, 0))

    return im


def main():
    print("[255 Assets] Extrayendo logo oficial del gato 255 sin fondo blanco...")
    cat_sq = extract_clean_cat_logo(UPLOADED_LOGO)

    # 1. Guardar todas las variantes de branding del gato 255
    save_png(cat_sq.resize((512, 512), Image.Resampling.LANCZOS), "logo_255.png")
    save_png(cat_sq.resize((200, 200), Image.Resampling.LANCZOS), "logo_200px.png")
    save_png(cat_sq.resize((64, 64), Image.Resampling.LANCZOS), "logo_64px.png")
    save_png(cat_sq.resize((48, 48), Image.Resampling.LANCZOS), "logo_48px.png")
    save_png(cat_sq.resize((32, 32), Image.Resampling.LANCZOS), "logo_32px.png")
    save_png(cat_sq.resize((24, 24), Image.Resampling.LANCZOS), "logo_24px.png")
    save_png(cat_sq.resize((16, 16), Image.Resampling.LANCZOS), "logo_16px.png")

    # Claves de compatibilidad de branding
    save_png(make_welcome_200(cat_sq), "welcome_200px.png")
    save_png(cat_sq.resize((48, 48), Image.Resampling.LANCZOS), "virtualbox.png")
    save_png(cat_sq.resize((48, 48), Image.Resampling.LANCZOS), "OSE/VirtualBox_48px.png")
    save_png(cat_sq.resize((32, 32), Image.Resampling.LANCZOS), "OSE/VirtualBox_32px.png")
    save_png(cat_sq.resize((16, 16), Image.Resampling.LANCZOS), "OSE/VirtualBox_16px.png")

    # 2. Banners laterales de Wizards (145x290)
    save_png(make_wizard_banner(cat_sq, "welcome"), "wizard_new_welcome.png")
    save_png(make_wizard_banner(cat_sq, "harddisk"), "wizard_new_harddisk.png")
    save_png(make_wizard_banner(cat_sq, "clone"), "wizard_clone.png")

    # 3. Marcos de monitor de Vista Previa (242x155)
    save_png(make_monitor_preview(cat_sq, empty_logo=False), "preview_full_16to9_242x155px.png")
    save_png(make_monitor_preview(cat_sq, empty_logo=True), "preview_empty_16to9_242x155px.png")

    # 4. Iconos de Sistemas Operativos (32x32)
    os_kinds = [
        "win11", "win10", "win81", "win8", "win7", "winvista", "winxp", "win2k", "win_other",
        "ubuntu", "debian", "archlinux", "fedora", "redhat", "opensuse", "gentoo", "linux26",
        "linux", "freebsd", "macosx", "dos", "other"
    ]
    for k in os_kinds:
        save_png(make_os_icon(k), f"os_{k}.png")

    # 5. Todos los iconos de toolbar, estados, categorías y acciones
    icon_specs = [
        ("tools_global", 32),
        ("tools_menu", 24),
        ("vm_new", 32), ("vm_new", 16),
        ("vm_add", 32),
        ("vm_clone", 32),
        ("vm_settings", 32), ("vm_settings", 16),
        ("vm_discard", 32),
        ("vm_delete", 32), ("vm_delete", 16),
        ("vm_start", 32), ("vm_start", 16),
        ("vm_pause", 16),
        ("vm_reset", 16),
        ("vm_poweroff", 16),
        ("global_settings", 32),
        ("machine_details_manager", 24), ("machine_details_manager", 16),
        ("snapshot_manager", 24), ("snapshot_manager", 16),
        ("vm_show_logs", 24), ("vm_show_logs", 16),
        ("performance_monitor", 24),
        ("media_manager", 32), ("media_manager", 24), ("media_manager", 16),
        ("state_powered_off", 16),
        ("state_running", 16),
        ("state_saved", 16),
        ("state_paused", 16),
        ("state_aborted", 16),
        ("machine", 16), ("machine", 32),
        ("chipset", 16), ("chipset", 32),
        ("vrdp", 16), ("vrdp", 32),
        ("hd", 16), ("hd", 32),
        ("cd", 16),
        ("fd", 16),
        ("sound", 16), ("sound", 32),
        ("nw", 16), ("nw", 32),
        ("serial_port", 16), ("serial_port", 32),
        ("usb", 16), ("usb", 32),
        ("sf", 16), ("sf", 32),
        ("interface", 16), ("interface", 32),
        ("description", 16),
        ("cpu", 16),
        ("ram", 16),
        ("hd_create", 32), ("hd_create", 16),
        ("hd_add", 32), ("hd_add", 16),
        ("hd_modify", 32), ("hd_modify", 16),
        ("hd_remove", 32), ("hd_remove", 16),
        ("hd_release", 32),
        ("cd_add", 32), ("cd_add", 16),
        ("cd_unmount", 16),
        ("refresh", 32), ("refresh", 16),
        ("ok", 16),
        ("cancel", 16),
        ("close", 16),
        ("status_check", 16),
        ("status_error", 16),
        ("arrow_down", 10),
        ("arrow_right", 10),
    ]
    for kind, sz in icon_specs:
        save_png(make_icon(kind, sz, cat_sq), f"{kind}_{sz}px.png")

    print(f"[255 Assets] Generados exitosamente en {ASSETS_DIR}")


if __name__ == "__main__":
    main()
