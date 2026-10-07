#!/usr/bin/env python3
"""Stage 17F: a deterministic *camera-like* workload in `av1-cpu-bench dump` format.

There is no camera on the development machine, so this synthesises what makes camera video hard for
an encoder and easy encoders' benchmarks miss: a textured scene, a panning and shaking camera,
moving foreground objects and fresh per-frame sensor noise in every channel. It is SYNTHETIC - not
real camera footage - and is labelled that way in every report.

    make_camera_like.py OUT.i420 --width 1280 --height 720 --frames 150 --fps 15 [--noise 5]
"""
import argparse
import math
import random
import struct

from PIL import Image, ImageChops, ImageDraw, ImageFilter


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--width", type=int, default=1280)
    ap.add_argument("--height", type=int, default=720)
    ap.add_argument("--frames", type=int, default=150)
    ap.add_argument("--fps", type=int, default=15)
    ap.add_argument("--noise", type=int, default=5, help="peak sensor noise amplitude per channel")
    ap.add_argument("--seed", type=int, default=17)
    a = ap.parse_args()
    w, h = a.width, a.height
    rng = random.Random(a.seed)

    # The scene is larger than the frame so the camera can pan and shake across it: smooth
    # colour blobs plus sharp-edged shapes, so it has both low- and high-frequency detail.
    sw, sh = w + w // 4, h + h // 4
    small = Image.frombytes("RGB", (sw // 32, sh // 32), rng.randbytes((sw // 32) * (sh // 32) * 3))
    scene = small.resize((sw, sh), Image.BICUBIC).filter(ImageFilter.GaussianBlur(6))
    d = ImageDraw.Draw(scene)
    for _ in range(60):
        x, y = rng.randrange(sw), rng.randrange(sh)
        r = rng.randrange(20, 140)
        col = tuple(rng.randrange(256) for _ in range(3))
        if rng.random() < 0.5:
            d.rectangle([x, y, x + r, y + r // 2], fill=col)
        else:
            d.ellipse([x, y, x + r, y + r], outline=col, width=4)
    # Fine texture (foliage/fabric-like): high-frequency noise baked into the scene.
    tex = Image.frombytes("RGB", (sw, sh), rng.randbytes(sw * sh * 3)).point(lambda v: v // 8)
    scene = ImageChops.add(scene, tex, scale=1.0, offset=-16)

    objs = [(rng.randrange(w), rng.randrange(h), rng.uniform(-9, 9), rng.uniform(-6, 6),
             rng.randrange(40, 110), tuple(rng.randrange(40, 256) for _ in range(3))) for _ in range(4)]
    amp = a.noise
    with open(a.out, "wb") as f:
        f.write(b"I420" + struct.pack("<III", w, h, a.fps))
        for i in range(a.frames):
            # camera pan with hand-shake
            ox = int((sw - w) / 2 + (sw - w) / 2 * math.sin(i / 23.0) + 3 * math.sin(i * 1.7))
            oy = int((sh - h) / 2 + (sh - h) / 2 * math.sin(i / 31.0 + 1) + 3 * math.cos(i * 1.3))
            fr = scene.crop((ox, oy, ox + w, oy + h))
            dr = ImageDraw.Draw(fr)
            for k, (x0, y0, vx, vy, r, col) in enumerate(objs):
                x = (x0 + vx * i) % (w + r) - r // 2
                y = (y0 + vy * i) % (h + r) - r // 2
                dr.ellipse([x, y, x + r, y + r * 0.8], fill=col)
            # fresh sensor noise every frame, every channel
            noise = Image.frombytes("RGB", (w, h), rng.randbytes(w * h * 3)).point(lambda v: v * amp // 128)
            fr = ImageChops.add(fr, noise, scale=1.0, offset=-amp)
            y_, cb, cr = fr.convert("YCbCr").split()
            f.write(y_.tobytes())
            f.write(cb.resize((w // 2, h // 2), Image.BOX).tobytes())
            f.write(cr.resize((w // 2, h // 2), Image.BOX).tobytes())
    print(f"{a.out}: {w}x{h} {a.frames} frames @ {a.fps} fps, noise +-{amp}")


if __name__ == "__main__":
    main()
