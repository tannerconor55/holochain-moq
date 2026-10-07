#!/usr/bin/env python3
"""Stage 17G: tables from `results17g.jsonl` (measurement only).

    WORK=/path/to/dir python3 analyze_stage17g.py

"Sustained" means what a live share needs: nothing was dropped from the sharer's bounded raw queue
and the encoder kept up with the source (encoded fps >= 97% of the target). An unpaced average
above the target is deliberately NOT used.
"""
import json
import os
import statistics

WORK = os.environ["WORK"]
R = {}
for line in open(os.path.join(WORK, "results17g.jsonl")):
    if line.strip():
        r = json.loads(line)
        R[r["tag"]] = r
NAMES = {"s720": "synthetic 1280x720", "s1080": "synthetic 1920x1080", "f720": "screencast 1280x678",
         "f1080": "screencast 1888x998", "c720": "camera-like 1280x720 (SYNTHETIC)",
         "c1080": "camera-like 1920x1080 (SYNTHETIC)"}
ENC = ["rav1e", "svt1", "svt2", "svt4", "svt8"]
LABEL = {"rav1e": "rav1e (prod)", "svt1": "SVT 1 in flight", "svt2": "SVT 2 in flight",
         "svt4": "SVT 4 in flight", "svt8": "SVT 8 in flight"}


def m(tag):
    r = R.get(tag)
    return r["metrics"] if r and "error" not in r["metrics"] else None


def arr(x, key):
    v = x.get(key)
    return v if isinstance(v, list) else [None] * 5


def f(v, d=1):
    return "n/a" if v is None else f"{v:.{d}f}"


def verdict(x, fps):
    out = x["frames_encoded"] / x["run_seconds"]
    drops = x["dropped_input_total"]
    return ("SUSTAINED" if drops == 0 and out >= 0.97 * fps else "not sustained"), out, drops


print("=" * 150)
print("1. PACED REAL-TIME, 30 s, production Av1Source + DropOldest raw queue (capacity 4). p = percentile of end-to-end latency: source frame reaches the encoder queue -> packet done")
print("=" * 150)
hdr = (f"{'encoder':16s} {'out fps':>7s} {'drop%':>6s} {'inflt pk/avg':>12s} {'wait avg/max':>13s} | "
       f"{'e2e p50':>7s} {'p95':>6s} {'p99':>6s} {'max':>6s} | {'inter p50':>9s} {'p95':>6s} | {'key p50':>7s} {'p95':>6s} {'max':>6s} | "
       f"{'CPU%':>5s} {'cpu-s/fr':>8s} {'RSS':>4s} {'kbps':>6s}  verdict")
for fps in (30, 15):
    for wl in reversed(list(NAMES)):
        print(f"\n-- {NAMES[wl]} @ {fps} fps")
        print(hdr)
        for e in ENC:
            x = m(f"{wl}.p{fps}.{e}")
            if not x:
                continue
            v, out, drops = verdict(x, fps)
            e2e, ei, ek = arr(x, "e2e_lat_all_avg_p50_p95_p99_max_ms"), arr(x, "enc_lat_inter_avg_p50_p95_p99_max_ms"), arr(x, "enc_lat_key_avg_p50_p95_p99_max_ms")
            w = x["submit_wait_avg_max_ms"]
            offered = max(x["frames_from_source"], 1)
            cpu_s_fr = x["cpu_percent_steady"] / 100.0 / max(out, 0.01)
            print(f"{LABEL[e]:16s} {out:>7.1f} {100*drops/offered:>6.1f} {str(x['in_flight_max'])+'/'+f(x['in_flight_avg'],1):>12s} "
                  f"{f(w[0])+'/'+f(w[1]):>13s} | {f(e2e[1]):>7s} {f(e2e[2]):>6s} {f(e2e[3]):>6s} {f(e2e[4]):>6s} | "
                  f"{f(ei[1]):>9s} {f(ei[2]):>6s} | {f(ek[1]):>7s} {f(ek[2]):>6s} {f(ek[4]):>6s} | "
                  f"{x['cpu_percent_steady']:>5} {cpu_s_fr:>8.3f} {x['rss_excl_dump_mb']:>4} {x['bitrate_kbps']:>6}  {v}")

print("\n" + "=" * 150)
print("2. THE DECISIVE POINT: camera-like 1080p @ 30 fps, three runs each (run-to-run spread)")
print("=" * 150)
print(f"{'encoder':16s} {'out fps (3 runs)':>26s} {'drop% (3 runs)':>24s} {'e2e p95 (3 runs)':>26s} {'verdict'}")
for e in ENC:
    runs = [m(f"c1080.p30.{e}")] + [m(f"c1080.p30.{e}.r{n}") for n in (2, 3)]
    runs = [x for x in runs if x]
    if not runs:
        continue
    outs = [x["frames_encoded"] / x["run_seconds"] for x in runs]
    drops = [100 * x["dropped_input_total"] / max(x["frames_from_source"], 1) for x in runs]
    p95 = [arr(x, "e2e_lat_all_avg_p50_p95_p99_max_ms")[2] for x in runs]
    ok = all(verdict(x, 30)[0] == "SUSTAINED" for x in runs)
    print(f"{LABEL[e]:16s} {' / '.join(f'{o:.1f}' for o in outs):>26s} {' / '.join(f'{d:.1f}' for d in drops):>24s} "
          f"{' / '.join(f'{p:.0f}' for p in p95):>26s} {'SUSTAINED in all runs' if ok else 'not sustained'}")

print("\n" + "=" * 150)
print("3. SUBSCRIBER KEYFRAME REQUESTS every 10 frames, periodic keyframes OFF (keyint 1000) so each request forces one: request -> encoded keyframe")
print("   (the first pass, at the production keyint 5, is not reported: periodic keyframes satisfied nearly every request)")
print("=" * 150)
print(f"{'workload':34s} {'encoder':16s} {'requests rcv/forced/coalesced':>30s} {'request->encoded ms avg/p50/p95/p99/max':>50s} {'key enc-lat p50/max':>20s}")
for wl, fps in (("c720", 30), ("c1080", 15)):
    for e in ENC:
        x = m(f"{wl}.p{fps}.{e}.kreq2")
        if not x:
            continue
        d = arr(x, "kf_request_to_encoded_avg_p50_p95_p99_max_ms")
        ek = arr(x, "enc_lat_key_avg_p50_p95_p99_max_ms")
        print(f"{NAMES[wl] + ' @' + str(fps):34s} {LABEL[e]:16s} {str(x.get('kf_requests_received'))+'/'+str(x.get('kf_requests_forced'))+'/'+str(x.get('kf_requests_coalesced')):>30s} "
              f"{' '.join(f(v) for v in d):>50s} {f(ek[1])+'/'+f(ek[4]):>20s}")

print("\n" + "=" * 150)
print("4. EXPERIMENT (not a configuration change): level_of_parallelism 6 vs auto, camera-like 1080p @ 30 fps")
print("=" * 150)
for n in (1, 2, 4):
    for tag, lab in ((f"c1080.p30.svt{n}", "lp auto"), (f"c1080.p30.svt{n}.lp6", "lp 6")):
        x = m(tag)
        if x:
            v, out, drops = verdict(x, 30)
            ek, e2e = arr(x, "enc_lat_key_avg_p50_p95_p99_max_ms"), arr(x, "e2e_lat_all_avg_p50_p95_p99_max_ms")
            print(f"SVT {n} in flight, {lab:8s}: out {out:5.1f} fps, drops {100*drops/max(x['frames_from_source'],1):5.1f}%, key enc-lat p50 {f(ek[1])} ms, e2e p95 {f(e2e[2])} ms, CPU {x['cpu_percent_steady']}%  {v}")

print("\n" + "=" * 150)
print("5. UNPAUSED CAPACITY (lossless queue, 150 frames, 15 fps time base): how fast each depth goes when never waiting for the source")
print("=" * 150)
for wl in ("c1080", "s1080", "c720"):
    row = []
    for e in ENC[1:]:
        x = m(f"{wl}.unpaced.{e}")
        if x:
            row.append(f"{LABEL[e]}: {x['frames_encoded']/x['run_seconds']:.1f} fps @ {x['cpu_percent_steady']}% CPU")
    print(f"{NAMES[wl]:34s} " + " | ".join(row))

print("\n" + "=" * 150)
print("6. EXPERIMENT (not a default): periodic keyframe interval at the point that fails, camera-like 1080p @ 30 fps")
print("=" * 150)
print(f"{'config':26s} {'out fps':>7s} {'drop%':>6s} {'e2e p50':>7s} {'p95':>6s} {'p99':>6s} {'key enc p50':>11s} {'kbps':>7s} {'CPU%':>5s}  verdict")
for keyint, tagfmt in ((5, "c1080.p30.svt{n}"), (10, "c1080.p30.svt{n}.kint10"), (15, "c1080.p30.svt{n}.kint15"), (30, "c1080.p30.svt{n}.kint30")):
    for n in (1, 4):
        x = m(tagfmt.format(n=n))
        if x:
            v, out, drops = verdict(x, 30)
            e2e, ek = arr(x, "e2e_lat_all_avg_p50_p95_p99_max_ms"), arr(x, "enc_lat_key_avg_p50_p95_p99_max_ms")
            print(f"keyint {keyint:>2d}, SVT {n} in flight  {out:>7.1f} {100*drops/max(x['frames_from_source'],1):>6.1f} {f(e2e[1]):>7s} {f(e2e[2]):>6s} {f(e2e[3]):>6s} {f(ek[1]):>11s} {x['bitrate_kbps']:>7} {x['cpu_percent_steady']:>5}  {v}")
