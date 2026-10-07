#!/usr/bin/env python3
"""Stage 17F: tables from `results17f.jsonl` (and Stage 17E's `results.jsonl` for the rav1e rows on
the 17E workloads). Measurement only.

    WORK=/path/to/dir python3 analyze_stage17f.py
"""
import json
import math
import os

WORK = os.environ["WORK"]


def load(name):
    p = os.path.join(WORK, name)
    out = {}
    if os.path.exists(p):
        for line in open(p):
            if line.strip():
                r = json.loads(line)
                out[r["tag"]] = r
    return out


R17F, R17E = load("results17f.jsonl"), load("results.jsonl")
NAMES = {"s720": "synthetic 1280x720", "s1080": "synthetic 1920x1080", "f720": "screencast 1280x678",
         "f1080": "screencast 1888x998", "c720": "camera-like 1280x720 (SYNTHETIC)",
         "c1080": "camera-like 1920x1080 (SYNTHETIC)"}
FPS0 = 15


def m(tag, src=None):
    for R in ([src] if src else [R17F, R17E]):
        r = R.get(tag)
        if r and r.get("metrics") and "error" not in r["metrics"]:
            return r["metrics"]
    return None


def q(tag):
    for R in (R17F, R17E):
        r = R.get(tag)
        if r and r.get("quality") and "error" not in r["quality"]:
            return r["quality"]
    return None


def kbps(tag):
    x = q(tag)
    return x["bytes"] * 8 / (x["frames"] / FPS0) / 1000 if x else None


def f(v, d=1):
    return "n/a" if v is None else f"{v:.{d}f}"


print("=" * 118)
print("1. LIBRARY WRAPPER, unpaced, all 150 frames, keyframe forced at frame 0 and every 5 (the worker's policy)")
print("   latency = time inside SvtEncoder::encode per frame (submit -> its packet), reported separately for key / inter frames")
print("=" * 118)
print(f"{'workload':34s} {'fps':>6s} {'CPU%':>5s} {'RSS':>4s} {'kbps':>6s} | {'inter p50':>9s} {'p95':>6s} {'max':>6s} | {'key p50':>8s} {'p95':>6s} {'max':>6s} | {'PSNR':>5s} {'SSIM':>6s} {'contract'}")
for wl, name in NAMES.items():
    t = f"{wl}.lib.crf35"
    x, qq = m(t), q(t)
    if not x:
        continue
    i, k = x["lat_inter_avg_p50_p95_max_ms"], x["lat_key_avg_p50_p95_max_ms"]
    ok = ""
    if qq:
        ok = (f"td {qq['temporal_delimiter_first']}/{qq['frames']} seq {qq['key_units_with_sequence_header']}/{qq['key_units']} "
              f"grp {qq['groups_decodable_independently']}/{qq['groups']} flagmis {qq['key_flag_vs_bitstream_mismatches']}")
    print(f"{name:34s} {x['encode_fps']:>6} {x['cpu_percent_total']:>5} {x['rss_mb']:>4} {x['bitrate_kbps']:>6} | "
          f"{i[1]:>9} {i[2]:>6} {i[3]:>6} | {k[1]:>8} {k[2]:>6} {k[3]:>6} | "
          f"{f(qq['psnr_y_mean_db'],1) if qq else 'n/a':>5s} {f(qq['ssim_y_mean'],4) if qq else 'n/a':>6s} {ok}  req-mismatch {x['keyframe_request_mismatches']} gaps {x['keyframe_gaps_frames']}")

print("\n" + "=" * 118)
print("2. CAMERA-LIKE CONTENT (synthetic: texture + pan/shake + moving objects + per-frame sensor noise): SVT library vs rav1e production")
print("=" * 118)
for wl in ("c720", "c1080"):
    print(f"\n-- {NAMES[wl]}")
    rows = []
    for t in sorted(R17F):
        if t.startswith(f"{wl}.") and (".rav1e.q" in t or ".lib.crf" in t) and "paced" not in t and q(t):
            rows.append((kbps(t), q(t)["psnr_y_mean_db"], q(t)["ssim_y_mean"], t))
    for b, p, s, t in sorted(rows, key=lambda r: (r[3].split(".")[1], r[0])):
        print(f"   {t.split('.', 1)[1]:14s} {b:9.0f} kbps   PSNR {p:6.2f} dB   SSIM {s:.4f}")
    base = q(f"{wl}.rav1e.q60")
    if base:
        bb, pp = kbps(f"{wl}.rav1e.q60"), base["psnr_y_mean_db"]
        lib_pts = sorted((r[1], r[0]) for r in rows if ".lib." in r[3])  # (psnr, kbps)
        at = None
        for (p0, b0), (p1, b1) in zip(lib_pts, lib_pts[1:]):
            if p0 <= pp <= p1 and p1 > p0:
                at = math.exp(math.log(b0) + (pp - p0) / (p1 - p0) * (math.log(b1) - math.log(b0)))
        print(f"   ==> rav1e production: {bb:.0f} kbps @ {pp:.2f} dB; SVT bitrate at the same PSNR: "
              f"{f(at,0)} kbps" + (f" ({100*at/bb:.0f}% of rav1e)" if at else " (outside SVT's measured range)"))
    a, b_ = m(f"{wl}.rav1e.q60"), m(f"{wl}.lib.crf35")
    if a and b_:
        rf = a["frames_encoded"] / a["run_seconds"]
        print(f"   capacity: rav1e {rf:.1f} fps @ {a['cpu_percent_steady']}% CPU, {a['rss_mb']} MB   |   "
              f"SVT lib {b_['encode_fps']} fps @ {b_['cpu_percent_total']}% CPU, {b_['rss_mb']} MB")

print("\n" + "=" * 118)
print("3. REAL-TIME (paced 30 s, looped source, synchronous submit->packet per frame). sustained = frames encoded ~ frames offered")
print("=" * 118)
print(f"{'workload':34s} {'fps':>3s} {'enc':>6s} {'out fps':>8s} {'inter p50':>9s} {'p95':>6s} {'key p50':>8s} {'p95':>6s} {'max':>6s} {'CPU%':>5s} {'RSS':>4s} {'kbps':>6s}  verdict")
for fps in (15, 30):
    for wl, name in NAMES.items():
        for enc, tag in (("SVT lib", f"{wl}.paced{fps}.lib"), ("rav1e", f"{wl}.paced{fps}.rav1e")):
            x = m(tag)
            if not x:
                continue
            if enc == "SVT lib":
                i, k = x["lat_inter_avg_p50_p95_max_ms"], x["lat_key_avg_p50_p95_max_ms"]
                out = x["frames_encoded"] / x["run_seconds"]
                # the loop sleeps to the next frame slot, so falling behind shows as fewer frames than slots
                slots = x["run_seconds"] * fps
                ok = x["frames_encoded"] >= 0.97 * slots
                print(f"{name:34s} {fps:>3d} {enc:>6s} {out:>8.1f} {i[1]:>9} {i[2]:>6} {k[1]:>8} {k[2]:>6} {k[3]:>6} {x['cpu_percent_total']:>5} {x['rss_mb']:>4} {x['bitrate_kbps']:>6}  {'sustained' if ok else 'NOT sustained'}")
            else:
                drops = x["dropped_encoder_queue"] + x.get("dropped_source", 0)
                out = x["frames_encoded"] / x["run_seconds"]
                ok = drops == 0
                print(f"{name:34s} {fps:>3d} {enc:>6s} {out:>8.1f} {'':>9s} {'':>6s} {'':>8s} {x['enc_latency_p95_ms']:>6} {x['enc_latency_max_ms']:>6} {x['cpu_percent_steady']:>5} {x['rss_mb']:>4} {x['bitrate_kbps']:>6}  {'sustained' if ok else f'NOT sustained ({drops} dropped)'}")
