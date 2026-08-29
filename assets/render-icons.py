#!/usr/bin/env python3
"""Generate the scurry icon set.

Committed as a script rather than as opaque binaries so the icons can be
regenerated and reviewed.

Two different things are drawn here:

  icon-*.png  the application icon: a light glyph on a dark rounded square,
              which is what shows in the Dock, Finder and a launcher. Sizes are
              the ones png2icns accepts (16/32/48/128/256/512); 64 is not a
              valid icns member and is deliberately absent.

  tray.png    the menu bar icon: the glyph alone, opaque, on a transparent
              background, with no plate behind it. macOS treats this as a
              template image and reads only the alpha channel, tinting it black
              or white to match the menu bar -- so it follows light and dark
              mode without shipping two files. A background square would be
              filled in solid and look nothing like the other menu bar icons.
              Drawn at 44px for a 22pt slot on a 2x display.
"""
import zlib, struct, math, pathlib

OUT = pathlib.Path(__file__).parent

def png(path, w, h, px):
    raw = b"".join(b"\x00" + bytes(px[y*w*4:(y+1)*w*4]) for y in range(h))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t+d) & 0xFFFFFFFF)
    path.write_bytes(b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b""))

def render(s, plate=True, glyph=(0xf5, 0xf6, 0xf8, 255), fill=1.0):
    px = bytearray(s*s*4)
    r = s*0.22
    def put(x, y, c):
        i = (y*s+x)*4
        px[i:i+4] = bytes(c)
    if plate:
        for y in range(s):
            for x in range(s):
                dx = max(r-x, 0, x-(s-1-r)); dy = max(r-y, 0, y-(s-1-r))
                cov = min(max(0.5 - (math.hypot(dx,dy)-r), 0.0), 1.0)
                if cov > 0:
                    put(x, y, (0x1c, 0x1f, 0x26, int(255*cov)))
    def tri(p,q,rr,c):
        sign = lambda a,b,cc: (a[0]-cc[0])*(b[1]-cc[1])-(b[0]-cc[0])*(a[1]-cc[1])
        for y in range(max(int(min(p[1],q[1],rr[1])),0), min(int(max(p[1],q[1],rr[1]))+1, s)):
            for x in range(max(int(min(p[0],q[0],rr[0])),0), min(int(max(p[0],q[0],rr[0]))+1, s)):
                pt=(x+0.5,y+0.5); d1,d2,d3 = sign(pt,p,q), sign(pt,q,rr), sign(pt,rr,p)
                if not (((d1<0)or(d2<0)or(d3<0)) and ((d1>0)or(d2>0)or(d3>0))):
                    put(x,y,c)
    # Without a plate the glyph carries the whole icon, so it is drawn larger
    # and centred rather than insetting for a background.
    c = 0.5 - (0.5 * fill)
    w = lambda f: (c + f * fill) * s
    tri((w(.30),w(.10)), (w(.30),w(.78)), (w(.70),w(.46)), glyph)
    tri((w(.30),w(.78)), (w(.44),w(.62)), (w(.56),w(.88)), glyph)
    return px

for size in (16, 32, 48, 128, 256, 512):
    png(OUT / f"icon-{size}.png", size, size, render(size, plate=True, fill=0.78))

# Opaque black glyph, transparent everywhere else. The colour is irrelevant on
# macOS -- only alpha is read from a template image -- but black is what a
# non-templating panel should draw on a light background.
png(OUT / "tray.png", 44, 44, render(44, plate=False, glyph=(0, 0, 0, 255), fill=0.92))
print("wrote icon-{16,32,48,128,256,512}.png and tray.png (44px template)")
