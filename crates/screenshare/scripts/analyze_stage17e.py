#!/usr/bin/env python3
"""Stage 17E: turn `results.jsonl` into the comparison tables (measurement only).

    WORK=/path/to/dir python3 analyze_stage17e.py

Matched-bitrate / matched-quality points are read off each encoder's own rate-distortion curve
(log-bitrate vs quality, linear interpolation between the two neighbouring measured points; no
extrapolation: a point outside the measured range is reported as n/a).
"""
import json
import math
import os

WORK = os.environ["WORK"]
R = {}
for line in open(os.path.join(WORK, "results.jsonl")):
    if line.strip():
        r = json.loads(line)
        R[r["tag"]] = r
WL = {"s720": "synthetic 1280x720", "s1080": "synthetic 1920x1080",
      "f720": "screencast 1280x678", "f1080": "screencast 1888x998"}
FPS0 = 15  # the dumps are 15 fps; bitrate = bytes * 8 / (frames / 15)


def m(tag):
    r = R.get(tag)
    return r["metrics"] if r and "error" not in r["metrics"] else None


def q(tag):
    r = R.get(tag)
    return r.get("quality") if r and r.get("quality") and "error" not in r["quality"] else None


def kbps(tag):
    x = q(tag)
    return x["bytes"] * 8 / (x["frames"] / FPS0) / 1000 if x else None


def curve(prefix, wl, key):
    pts = []
    for t in R:
        if t.startswith(f"{wl}.{prefix}") and q(t) and "cbr" not in t and "lp4" not in t \
                and "tune0" not in t and ".p12" not in t and ".p8" not in t:
            pts.append((kbps(t), q(t)[key], t))
    return sorted(pts)


def interp_at_quality(pts, target):  # bitrate at a quality
    pts = sorted(pts, key=lambda p: p[1])
    for (b0, y0, _), (b1, y1, _) in zip(pts, pts[1:]):
        if y0 <= target <= y1 and y1 > y0:
            f = (target - y0) / (y1 - y0)
            return math.exp(math.log(b0) + f * (math.log(b1) - math.log(b0)))
    return None


def interp_at_bitrate(pts, target):  # quality at a bitrate
    pts = sorted(pts)
    for (b0, y0, _), (b1, y1, _) in zip(pts, pts[1:]):
        if b0 <= target <= b1 and b1 > b0:
            f = (math.log(target) - math.log(b0)) / (math.log(b1) - math.log(b0))
            return y0 + f * (y1 - y0)
    return None


def fmt(v, d=1):
    return "n/a" if v is None else f"{v:.{d}f}"


print("=" * 100)
print("1. UNPAUSED CAPACITY (all 150 frames, no pacing, lossless queue; 15 fps time base)")
print("=" * 100)
cfgs = [("rav1e prod (q60,4t,4thr)", "{wl}.rav1e.q60"),
        ("svt p10 crf35 lp=auto", "{wl}.svt.p10.crf35.lpauto"),
        ("svt p10 crf35 lp=4", "{wl}.svt.p10.crf35.lp4"),
        ("svt p12 crf35 lp=auto", "{wl}.svt.p12.crf35.lpauto"),
        ("svt p12 crf35 lp=4", "{wl}.svt.p12.crf35.lp4"),
        ("svt p8  crf35 lp=auto", "{wl}.svt.p8.crf35.lpauto")]
for wl, name in WL.items():
    print(f"\n-- {name}")
    print(f"{'config':26s} {'fps':>7s} {'CPU%':>5s} {'cpu-s/fr':>8s} {'RSS':>5s} {'kbps':>7s} {'keyB':>7s} {'intB':>6s} {'PSNRy':>6s} {'SSIMy':>7s}")
    for label, pat in cfgs:
        t = pat.format(wl=wl)
        x, qq = m(t), q(t)
        if not x:
            continue
        fps = x.get("encode_fps") if x.get("encoder", "").startswith("rav1e") else x.get("overall_fps")
        # rav1e's encode_fps is its submit rate; for a like-for-like number use frames / run time.
        if x.get("encoder", "").startswith("rav1e"):
            fps = x["frames_encoded"] / x["run_seconds"]
        print(f"{label:26s} {fmt(fps):>7s} {x['cpu_percent_steady']:>5} {x['cpu_percent_steady']/100/fps:>8.3f} {x['rss_mb']:>5} "
              f"{fmt(kbps(t),0):>7s} {x['avg_keyframe_bytes']:>7} {x['avg_interframe_bytes']:>6} "
              f"{fmt(qq['psnr_y_mean_db'],2) if qq else 'n/a':>6s} {fmt(qq['ssim_y_mean'],4) if qq else 'n/a':>7s}")

print("\n" + "=" * 100)
print("2. QUALITY vs BITRATE (each encoder's own curve; luma PSNR / SSIM against the source frames)")
print("=" * 100)
for wl, name in WL.items():
    print(f"\n-- {name}")
    for prefix, lab in (("rav1e.q", "rav1e (quantizer sweep = EXPERIMENT)"), ("svt.p10.crf", "svt p10 low-delay (CRF sweep)")):
        pts = curve(prefix, wl, "psnr_y_mean_db")
        ss = {t: q(t)["ssim_y_mean"] for _, _, t in pts}
        print(f"  {lab}")
        for b, p, t in pts:
            tag = t.split(".", 1)[1]
            print(f"     {tag:24s} {b:9.0f} kbps   PSNR {p:6.2f} dB   SSIM {ss[t]:.4f}")
    base = f"{wl}.rav1e.q60"
    if q(base):
        bb, pp, ss0 = kbps(base), q(base)["psnr_y_mean_db"], q(base)["ssim_y_mean"]
        sp = curve("svt.p10.crf", wl, "psnr_y_mean_db")
        ssm = curve("svt.p10.crf", wl, "ssim_y_mean")
        print(f"  ==> rav1e production point: {bb:.0f} kbps, PSNR {pp:.2f} dB, SSIM {ss0:.4f}")
        b_at_p = interp_at_quality(sp, pp)
        b_at_s = interp_at_quality(ssm, ss0)
        print(f"      SVT bitrate at the SAME PSNR: {fmt(b_at_p,0)} kbps"
              + (f" ({100*b_at_p/bb:.0f}% of rav1e)" if b_at_p else ""))
        print(f"      SVT bitrate at the SAME SSIM: {fmt(b_at_s,0)} kbps"
              + (f" ({100*b_at_s/bb:.0f}% of rav1e)" if b_at_s else ""))
        at_b = interp_at_bitrate(sp, bb)
        top = max(sp)
        if at_b is not None:
            print(f"      SVT PSNR at the SAME bitrate: {at_b:.2f} dB   SSIM: {fmt(interp_at_bitrate(ssm, bb),4)}")
        else:
            print(f"      SVT PSNR at the SAME bitrate: NOT MEASURABLE - SVT's whole CRF range tops out at "
                  f"{top[0]:.0f} kbps ({100*top[0]/bb:.0f}% of rav1e's), where its PSNR is already {top[1]:.2f} dB "
                  f"(rav1e: {pp:.2f} dB)")
        cbr = [t for t in R if t.startswith(f"{wl}.svt.p10.cbr")]
        for t in cbr:
            if q(t):
                print(f"      SVT CBR at rav1e's target bitrate: actual {kbps(t):.0f} kbps, "
                      f"PSNR {q(t)['psnr_y_mean_db']:.2f} dB, SSIM {q(t)['ssim_y_mean']:.4f}")
        t0 = f"{wl}.svt.p10.crf35.tune0"
        if q(t0):
            print(f"      SVT tune=0 (VQ) crf35: {kbps(t0):.0f} kbps, PSNR {q(t0)['psnr_y_mean_db']:.2f}, SSIM {q(t0)['ssim_y_mean']:.4f}"
                  f"   (tune=1/PSNR crf35: {kbps(f'{wl}.svt.p10.crf35.lpauto'):.0f} kbps, "
                  f"PSNR {q(f'{wl}.svt.p10.crf35.lpauto')['psnr_y_mean_db']:.2f}, SSIM {q(f'{wl}.svt.p10.crf35.lpauto')['ssim_y_mean']:.4f})")

print("\n" + "=" * 100)
print("3. REAL-TIME OPERATING POINTS (paced, drop-oldest queue of 4, 30 s, looped source)")
print("   sustained = 0 dropped frames AND end-to-end latency did not grow by more than one frame period")
print("=" * 100)
hdr = f"{'workload':22s} {'fps':>3s} {'config':18s} {'out fps':>8s} {'drop':>5s} {'p50':>7s} {'p95':>7s} {'max':>7s} {'grow':>7s} {'CPU%':>5s} {'RSS':>5s} {'kbps':>6s}  verdict"
print(hdr)
for fps in (15, 30):
    for wl, name in WL.items():
        for label, suffix in (("rav1e prod", "rav1e"), ("svt p10 auto", "svt.p10.lpauto"),
                              ("svt p10 lp4", "svt.p10.lp4"), ("svt p12 auto", "svt.p12.lpauto")):
            x = m(f"{wl}.paced{fps}.{suffix}")
            if not x:
                continue
            period = 1000.0 / fps
            grow = x["e2e_last_third_avg_ms"] - x["e2e_first_third_avg_ms"]
            drops = x["dropped_encoder_queue"] + x.get("dropped_source", 0)
            out_fps = (x["frames_encoded"] / x["run_seconds"])
            ok = drops == 0 and grow <= period
            print(f"{name:22s} {fps:>3d} {label:18s} {out_fps:>8.1f} {drops:>5d} "
                  f"{x['enc_latency_p50_ms']:>7.1f} {x['enc_latency_p95_ms']:>7.1f} {x['enc_latency_max_ms']:>7.1f} "
                  f"{grow:>+7.1f} {x['cpu_percent_steady']:>5} {x['rss_mb']:>5} {x['bitrate_kbps']:>6}  "
                  f"{'SUSTAINED' if ok else 'NOT sustained'}")
x = m("s720.paced15.svt.gst.ra.p10")
if x:
    print(f"\nGStreamer svtav1enc random-access (low delay deadlocks): 720p15 latency p50 {x['enc_latency_p50_ms']} ms, "
          f"p95 {x['enc_latency_p95_ms']} ms, first keyframe {x['first_keyframe_latency_ms']} ms")
