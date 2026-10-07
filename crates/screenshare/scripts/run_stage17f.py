#!/usr/bin/env python3
"""Stage 17F measurement matrix for the in-process SVT-AV1 library wrapper (measurement only).

    WORK=/path/to/dir BENCH=target/release/av1-cpu-bench python3 run_stage17f.py

Appends one JSON line per run to `$WORK/results17f.jsonl`; a tag already recorded is skipped.
`$WORK` holds the `*.i420` dumps (`av1-cpu-bench dump`, `make_camera_like.py`). Sequential on purpose.
"""
import json
import os
import subprocess
import time

WORK = os.environ["WORK"]
BENCH = os.environ.get("BENCH", "target/release/av1-cpu-bench")
RESULTS = os.path.join(WORK, "results17f.jsonl")
WL = ["s720", "s1080", "f720", "f1080", "c720", "c1080"]  # synthetic / screencast / camera-like
done = {json.loads(l)["tag"] for l in open(RESULTS)} if os.path.exists(RESULTS) else set()


def run_json(cmd, timeout=1500):
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    if p.returncode != 0:
        return {"error": f"rc={p.returncode}: {(p.stderr or p.stdout)[-300:]}"}
    try:
        return json.loads(p.stdout.strip().splitlines()[-1])
    except Exception as e:  # noqa: BLE001
        return {"error": f"unparseable: {e}: {p.stdout[-200:]}"}


def rec(tag, kind, m, quality=None):
    with open(RESULTS, "a") as f:
        f.write(json.dumps({"tag": tag, "kind": kind, "metrics": m, "quality": quality}) + "\n")
    done.add(tag)
    print(f"[{time.strftime('%H:%M:%S')}] {tag}", flush=True)


def score(wl, stream):
    return run_json([BENCH, "quality", "--input", f"{WORK}/{wl}.i420", "--stream", stream], 900)


def lib(tag, wl, paced=False, fps=None, seconds=30, keep=True, **kw):
    if tag in done:
        return
    stream = f"{WORK}/{tag}.frames"
    cmd = [BENCH, "svt", "--input", f"{WORK}/{wl}.i420", "--json"]
    cmd += ["--paced", "--seconds", str(seconds)] if paced else ["--out", stream]
    if fps:
        cmd += ["--fps", str(fps)]
    for k, v in kw.items():
        cmd += [f"--{k.replace('_', '-')}", str(v)]
    m = run_json(cmd)
    q = score(wl, stream) if (not paced and "error" not in m) else None
    rec(tag, "lib", m, q)


def rav1e(tag, wl, paced=False, fps=None, seconds=30, q=None):
    if tag in done:
        return
    stream = f"{WORK}/{tag}.frames"
    cmd = [BENCH, "rav1e", "--input", f"{WORK}/{wl}.i420", "--json"]
    cmd += ["--seconds", str(seconds)] if paced else ["--unpaced", "--out", stream]
    if fps:
        cmd += ["--fps", str(fps)]
    if q is not None:
        cmd += ["--quantizer", str(q)]
    m = run_json(cmd)
    sc = score(wl, stream) if (not paced and "error" not in m) else None
    rec(tag, "rav1e", m, sc)


for wl in WL:  # capacity + quality + contract, forced keyframe every 5 (the worker's policy)
    lib(f"{wl}.lib.crf35", wl, crf=35)
for wl in ("c720", "c1080"):  # rav1e baseline and both encoders' curves on the camera-like content
    rav1e(f"{wl}.rav1e.q60", wl)
    for crf in (25, 30, 40, 45, 50, 55):
        lib(f"{wl}.lib.crf{crf}", wl, crf=crf)
for q in (40, 80, 100, 120, 150):
    rav1e(f"c720.rav1e.q{q}", "c720", q=q)
for q in (100, 150):
    rav1e(f"c1080.rav1e.q{q}", "c1080", q=q)
for fps in (15, 30):  # real-time points, 30 s, looped
    for wl in WL:
        lib(f"{wl}.paced{fps}.lib", wl, paced=True, fps=fps)
    for wl in ("c720", "c1080"):
        rav1e(f"{wl}.paced{fps}.rav1e", wl, paced=True, fps=fps)
print("ALLDONE17F", flush=True)
