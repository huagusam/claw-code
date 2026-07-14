import math
from PIL import Image, ImageDraw

ORANGE = (255, 107, 53, 255)      # #FF6B35
ORANGE_HI = (255, 159, 28, 255)   # #FF9F1C
ORANGE_DK = (214, 78, 30, 255)    # darker outline
GOLD = (255, 209, 102, 255)       # #FFD166
DARK = (40, 30, 25, 255)
WHITE = (255, 255, 255, 255)


def draw_crab(size):
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    s = size / 256.0  # scale factor

    def S(v):
        return int(round(v * s))

    # --- claws (arms) first so body overlaps their base ---
    claw_l = (S(46), S(104), S(86), S(150))   # x0,y0,x1,y1
    claw_r = (S(170), S(104), S(210), S(150))
    for c in (claw_l, claw_r):
        d.ellipse(c, fill=ORANGE, outline=ORANGE_DK, width=max(1, S(3)))
    # pincer notch: cut a small dark/transparent circle at claw tip
    for cx, cy in ((S(54), S(112)), (S(202), S(112))):
        d.ellipse([cx - S(9), cy - S(9), cx + S(9), cy + S(9)],
                  fill=(0, 0, 0, 0), outline=ORANGE_DK, width=max(1, S(2)))

    # --- legs (bottom) ---
    leg_y = S(178)
    for dx in (S(40), S(70), S(100)):
        for side in (-1, 1):
            x0 = S(128) + side * dx
            x1 = x0 + side * S(22)
            d.line([(x0, leg_y), (x1, leg_y + S(26))],
                   fill=ORANGE_DK, width=max(1, S(5)))

    # --- body ---
    body = [S(70), S(86), S(186), S(186)]
    d.ellipse(body, fill=ORANGE, outline=ORANGE_DK, width=max(1, S(4)))
    # top highlight
    d.ellipse([S(86), S(96), S(170), S(140)], fill=ORANGE_HI)
    # lower shade
    d.ellipse([S(82), S(150), S(174), S(182)], fill=ORANGE)

    # --- eyes ---
    for ex in (S(108), S(148)):
        ey = S(120)
        d.ellipse([ex - S(15), ey - S(15), ex + S(15), ey + S(15)],
                  fill=WHITE, outline=ORANGE_DK, width=max(1, S(2)))
        d.ellipse([ex - S(7), ey - S(7), ex + S(7), ey + S(7)], fill=DARK)

    # --- antennae ---
    for ex in (S(108), S(148)):
        d.line([(ex, S(96)), (ex + (S(6) if ex < S(128) else -S(6)), S(74))],
               fill=ORANGE_DK, width=max(1, S(3)))
        d.ellipse([ex + (S(2) if ex < S(128) else -S(10)),
                   S(70), ex + (S(10) if ex < S(128) else -S(2)), S(78)],
                  fill=GOLD)

    return img


sizes = [16, 32, 48, 64, 128, 256]
frames = [draw_crab(sz) for sz in sizes]
out = r"C:\Users\Incredible\Code\claw-code\rust\crates\rusty-claude-cli\assets\icons\claw-code.ico"
import struct

# Assemble a multi-resolution .ico manually using STANDARD BMP (uncompressed
# 32bpp BGRA) frames. The Windows resource compiler (RC.EXE / windres) used by
# embed-resource rejects PNG-compressed ICO frames, so we must use BMP.
entries = []
blob = bytearray()
offset = 6 + 16 * len(sizes)
for sz in sizes:
    img = draw_crab(sz).convert("RGBA")
    px = img.load()
    xor = bytearray()
    for y in range(sz - 1, -1, -1):  # bottom-up
        for x in range(sz):
            r, g, b, a = px[x, y]
            xor += bytes((b, g, r, a))
    mask_row = ((sz + 31) // 32) * 4
    and_mask = bytearray(mask_row * sz)  # all-zero (alpha channel carries transparency)
    bih = struct.pack("<IiiHHIIiiII", 40, sz, 2 * sz, 1, 32, 0,
                      len(xor) + len(and_mask), 0, 0, 0, 0)
    frame = bih + xor + and_mask
    w = 0 if sz >= 256 else sz
    entries.append((w, len(frame), offset))
    blob += frame
    offset += len(frame)

icondir = struct.pack("<HHH", 0, 1, len(sizes))
for (w, ln, off) in entries:
    icondir += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, ln, off)
icondir += bytes(blob)

with open(out, "wb") as f:
    f.write(icondir)
print("wrote", out, "sizes", sizes)
