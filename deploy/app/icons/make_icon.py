# Draws the Lumbridge Mail icon (original art): a blue rounded tile, a white envelope, a gold dot.
import struct, sys, io
from PIL import Image, ImageDraw

out = sys.argv[1]
S = 1024

def tile():
    big = 4 * S  # supersample
    k = big / S
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    # gradient
    grad = Image.new("RGBA", (big, big))
    top, bot = (0x4a, 0x8f, 0xe8), (0x1d, 0x57, 0xb0)
    gd = ImageDraw.Draw(grad)
    for y in range(big):
        t = y / (big - 1)
        c = tuple(int(top[i] + (bot[i] - top[i]) * t) for i in range(3)) + (255,)
        gd.line([(0, y), (big, y)], fill=c)
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle([64 * k, 64 * k, (S - 64) * k, (S - 64) * k], radius=200 * k, fill=255)
    img.paste(grad, (0, 0), mask)
    d = ImageDraw.Draw(img)
    # envelope
    d.rounded_rectangle([232 * k, 340 * k, 792 * k, 712 * k], radius=52 * k, fill=(255, 255, 255, 255))
    blue = (0x1d, 0x57, 0xb0, 255)
    w = int(40 * k)
    pts = [(272 * k, 392 * k), (512 * k, 562 * k), (752 * k, 392 * k)]
    d.line(pts, fill=blue, width=w, joint="curve")
    for p in (pts[0], pts[2]):
        d.ellipse([p[0] - w / 2, p[1] - w / 2, p[0] + w / 2, p[1] + w / 2], fill=blue)
    # gold dot (the assistant), with a white ring
    cx, cy, r = 772 * k, 332 * k, 86 * k
    d.ellipse([cx - r - 22 * k, cy - r - 22 * k, cx + r + 22 * k, cy + r + 22 * k], fill=(255, 255, 255, 255))
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=(0xf0, 0xb0, 0x4a, 255))
    return img.resize((S, S), Image.LANCZOS)

base = tile()
pngs = {}
for size in (16, 32, 48, 64, 128, 256, 512, 1024):
    im = base.resize((size, size), Image.LANCZOS)
    buf = io.BytesIO(); im.save(buf, "PNG"); pngs[size] = buf.getvalue()
for size in (32, 64, 128, 256, 512):
    open(f"{out}/lumbridge-mail-{size}.png", "wb").write(pngs[size])

# .icns: PNG payloads (macOS 10.7+ reads PNG for all these types).
entries = [(b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128), (b"ic08", 256), (b"ic09", 512),
           (b"ic10", 1024), (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512)]
body = b"".join(t + struct.pack(">I", 8 + len(pngs[s])) + pngs[s] for t, s in entries)
open(f"{out}/lumbridge-mail.icns", "wb").write(b"icns" + struct.pack(">I", 8 + len(body)) + body)

svg = '''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <!-- Lumbridge Mail app icon (original). Same drawing as deploy/app/icons/make_icon.py. -->
  <defs>
    <linearGradient id="g" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#4a8fe8"/>
      <stop offset="1" stop-color="#1d57b0"/>
    </linearGradient>
  </defs>
  <rect x="64" y="64" width="896" height="896" rx="200" fill="url(#g)"/>
  <rect x="232" y="340" width="560" height="372" rx="52" fill="#ffffff"/>
  <polyline points="272,392 512,562 752,392" fill="none" stroke="#1d57b0" stroke-width="40" stroke-linecap="round" stroke-linejoin="round"/>
  <circle cx="772" cy="332" r="108" fill="#ffffff"/>
  <circle cx="772" cy="332" r="86" fill="#f0b04a"/>
</svg>
'''
open(f"{out}/lumbridge-mail.svg", "w").write(svg)
print("ok")
