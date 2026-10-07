#!/usr/bin/env python3
"""Stage 17G measurement matrix: pipelined SVT-AV1 vs rav1e through the PRODUCTION `Av1Source`
(measurement only).

    WORK=/path/to/dir BENCH=target/release/av1-cpu-bench python3 run_stage17g.py

`$WORK` holds the six `*.i420` dumps (`av1-cpu-bench dump` for s720/s1080/f720/f1080,
`make_camera_like.py` for c720/c1080). One JSON line per run goes to `$WORK/results17g.jsonl`;
a tag already recorded is skipped, so a crash can simply be re-run. Runs are strictly sequential.

Every run uses the production path: the real `Av1Source`, a paced source and the sharer's bounded
DropOldest raw queue (capacity 4). `--encoder`/`--in-flight`/`--lp` only choose what backs it.
"""
import json
import os
import subprocess
import time

WORK = os.environ["WORK"]
BENCH = os.environ.get("BENCH", "target/release/av1-cpu-bench")
RESULTS = os.path.join(WORK, "results17g.jsonl")
WL = ["s720", "s1080", "f720", "f1080", "c720", "c1080"]
ENCODERS = [("rav1e", []), ("svt1", ["--encoder", "svt", "--in-flight", "1"]),
            ("svt2", ["--encoder", "svt", "--in-flight", "2"]),
            ("svt4", ["--encoder", "svt", "--in-flight", "4"]),
            ("svt8", ["--encoder", "svt", "--in-flight", "8"])]
done = {json.loads(l)["tag"] for l in open(RESULTS)} if os.path.exists(RESULTS) else set()


def run(tag, wl, enc_args, fps, seconds=30, unpaced=False, extra=()):
    if tag in done:
        return
    cmd = [BENCH, "rav1e", "--input", f"{WORK}/{wl}.i420", "--json", "--fps", str(fps), *enc_args, *extra]
    cmd += ["--unpaced"] if unpaced else ["--seconds", str(seconds)]
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=1500)
    if p.returncode != 0:
        m = {"error": f"rc={p.returncode}: {(p.stderr or p.stdout)[-300:]}"}
    else:
        try:
            m = json.loads(p.stdout.strip().splitlines()[-1])
        except Exception as e:  # noqa: BLE001
            m = {"error": f"unparseable: {e}: {p.stdout[-200:]}"}
    with open(RESULTS, "a") as f:
        f.write(json.dumps({"tag": tag, "wl": wl, "fps": fps, "metrics": m}) + "\n")
    done.add(tag)
    print(f"[{time.strftime('%H:%M:%S')}] {tag}", flush=True)


# 1. the main grid: every encoder x workload x rate, paced, 30 s
for fps in (30, 15):
    for wl in reversed(WL):  # the hard camera-like content first
        for name, args in ENCODERS:
            run(f"{wl}.p{fps}.{name}", wl, args, fps)

# 2. run-to-run variance at the decisive point: camera-like 1080p30, two more runs each
for rep in (2, 3):
    for name, args in ENCODERS:
        run(f"c1080.p30.{name}.r{rep}", "c1080", args, 30)

# 3. subscriber keyframe requests every 10 frames: request -> keyframe latency
for wl, fps in (("c720", 30), ("c1080", 15)):
    for name, args in ENCODERS:
        run(f"{wl}.p{fps}.{name}.kreq", wl, args, fps, extra=["--request-keys-every", "10"])

# 4. EXPERIMENT: does frame-level parallelism (lp 6) matter more than pipelining at camera-like 1080p30?
for n in (1, 2, 4):
    run(f"c1080.p30.svt{n}.lp6", "c1080", ["--encoder", "svt", "--in-flight", str(n), "--lp", "6"], 30)

# 5. unpaced capacity (lossless queue, all 150 frames)
for wl in ("c1080", "s1080", "c720"):
    for name, args in ENCODERS[1:]:
        run(f"{wl}.unpaced.{name}", wl, args, 15, unpaced=True)
# 6. (added after the first pass showed the request experiment above was satisfied by the periodic
#    keyframes) requests with periodic keyframes effectively off, so every request forces a keyframe
for wl, fps in (("c720", 30), ("c1080", 15)):
    for name, args in ENCODERS:
        run(f"{wl}.p{fps}.{name}.kreq2", wl, args, fps, extra=["--keyint", "1000", "--request-keys-every", "10"])

# 7. EXPERIMENT (not a default): what keyframe interval does at the failing point, camera-like 1080p30
for keyint in (10, 15, 30):
    for n in (1, 4):
        run(f"c1080.p30.svt{n}.kint{keyint}", "c1080", ["--encoder", "svt", "--in-flight", str(n)], 30, extra=["--keyint", str(keyint)])
print("ALLDONE17G2", flush=True)
