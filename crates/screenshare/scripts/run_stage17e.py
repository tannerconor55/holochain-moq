#!/usr/bin/env python3
"""Stage 17E experiment driver (measurement only): runs the whole comparison matrix and appends one
JSON line per run to `results.jsonl`. Resumable: a tag that is already recorded is skipped.

    WORK=/path/to/dir  BENCH=target/release/av1-cpu-bench  SVT_APP=/path/to/SvtAv1EncApp \
    python3 run_stage17e.py [phase1|phase2|all]

`WORK` holds the four `*.i420` dumps (made with `av1-cpu-bench dump`), the encoded streams and
`results.jsonl`. Runs are strictly sequential so they never compete for CPU.
"""
import json
import os
import subprocess
import sys
import time

WORK = os.environ["WORK"]
BENCH = os.environ.get("BENCH", "target/release/av1-cpu-bench")
HERE = os.path.dirname(os.path.abspath(__file__))
SVT = os.path.join(HERE, "svt_bench.py")
RESULTS = os.path.join(WORK, "results.jsonl")
WORKLOADS = ["s720", "s1080", "f720", "f1080"]  # synthetic / screencast, 720p- and 1080p-class


def done_tags():
    if not os.path.exists(RESULTS):
        return set()
    return {json.loads(l)["tag"] for l in open(RESULTS) if l.strip()}


DONE = done_tags()


def record(rec):
    with open(RESULTS, "a") as f:
        f.write(json.dumps(rec) + "\n")
    DONE.add(rec["tag"])
    print(f"[{time.strftime('%H:%M:%S')}] {rec['tag']}", flush=True)


def run_json(cmd, timeout):
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    if p.returncode != 0:
        return {"error": f"rc={p.returncode}: {(p.stderr or p.stdout)[-300:]}"}
    try:
        return json.loads(p.stdout.strip().splitlines()[-1])
    except Exception as e:  # noqa: BLE001
        return {"error": f"unparseable output: {e}: {p.stdout[-200:]}"}


def score(wl, stream):
    return run_json([BENCH, "quality", "--input", f"{WORK}/{wl}.i420", "--stream", stream], 600)


def rav1e(tag, wl, unpaced=True, fps=None, q=None, seconds=30):
    if tag in DONE:
        return None
    stream = f"{WORK}/{tag}.frames" if unpaced else None
    cmd = [BENCH, "rav1e", "--input", f"{WORK}/{wl}.i420", "--json"]
    if unpaced:
        cmd += ["--unpaced", "--out", stream]
    else:
        cmd += ["--seconds", str(seconds)]
    if fps:
        cmd += ["--fps", str(fps)]
    if q is not None:
        cmd += ["--quantizer", str(q)]
    m = run_json(cmd, 1200)
    rec = {"tag": tag, "wl": wl, "kind": "rav1e", "metrics": m}
    if stream and "error" not in m:
        rec["quality"] = score(wl, stream)
    record(rec)
    return m


def svt(tag, wl, unpaced=True, fps=None, seconds=30, **kw):
    """kw: preset, crf, bitrate, lp, tune, backend, pred_struct, extra"""
    if tag in DONE:
        return None
    stream = f"{WORK}/{tag}.frames" if unpaced else None
    cmd = [sys.executable, SVT, "--input", f"{WORK}/{wl}.i420", "--json", "--backend", kw.get("backend", "app")]
    cmd += ["--unpaced"] if unpaced else ["--seconds", str(seconds)]
    if stream:
        cmd += ["--out", stream]
    if fps:
        cmd += ["--fps", str(fps)]
    for k, flag in (("preset", "--preset"), ("crf", "--crf"), ("bitrate", "--bitrate"), ("lp", "--lp"),
                    ("tune", "--tune"), ("pred_struct", "--pred-struct"), ("extra", "--extra")):
        if kw.get(k) is not None:
            cmd += [flag, str(kw[k])]
    m = run_json(cmd, 1200)
    rec = {"tag": tag, "wl": wl, "kind": "svt", "cfg": kw, "metrics": m}
    if stream and "error" not in m:
        rec["quality"] = score(wl, stream)
    record(rec)
    return m


def phase1():
    for wl in WORKLOADS:
        base = rav1e(f"{wl}.rav1e.q60", wl)
        if base is None:
            base = next(json.loads(l)["metrics"] for l in open(RESULTS) if json.loads(l)["tag"] == f"{wl}.rav1e.q60")
        # EXPERIMENT: rav1e quantizer sweep, only to build its rate-distortion curve.
        for q in (40, 80, 100, 120, 150):
            rav1e(f"{wl}.rav1e.q{q}", wl, q=q)
        # SVT-AV1 low delay (pred-struct 1, lookahead 0), keyframe every 5 frames, CRF sweep.
        for crf in (25, 30, 35, 40, 45, 50, 55):
            svt(f"{wl}.svt.p10.crf{crf}.lpauto", wl, preset=10, crf=crf)
        svt(f"{wl}.svt.p10.crf35.lp4", wl, preset=10, crf=35, lp=4)
        svt(f"{wl}.svt.p12.crf35.lpauto", wl, preset=12, crf=35)
        svt(f"{wl}.svt.p12.crf35.lp4", wl, preset=12, crf=35, lp=4)
        svt(f"{wl}.svt.p8.crf35.lpauto", wl, preset=8, crf=35)
        svt(f"{wl}.svt.p10.crf35.tune0", wl, preset=10, crf=35, tune=0)
        # Same-bitrate comparison: CBR at the rav1e production bitrate for this workload.
        if base and "bitrate_kbps" in base:
            kbps = int(base["bitrate_kbps"])
            svt(f"{wl}.svt.p10.cbr{kbps}.lpauto", wl, preset=10, bitrate=max(kbps, 1))


def phase2():
    # Real-time operating points: paced, drop-oldest queue of 4, looped, 30 s each.
    for fps in (15, 30):
        for wl in WORKLOADS:
            rav1e(f"{wl}.paced{fps}.rav1e", wl, unpaced=False, fps=fps)
            svt(f"{wl}.paced{fps}.svt.p10.lpauto", wl, unpaced=False, fps=fps, preset=10, crf=35)
            svt(f"{wl}.paced{fps}.svt.p10.lp4", wl, unpaced=False, fps=fps, preset=10, crf=35, lp=4)
            svt(f"{wl}.paced{fps}.svt.p12.lpauto", wl, unpaced=False, fps=fps, preset=12, crf=35)
    # GStreamer svtav1enc, random access only (low delay deadlocks in this plugin/library pairing):
    # what its reordering costs in latency.
    svt("s720.paced15.svt.gst.ra.p10", "s720", unpaced=False, fps=15, preset=10, crf=35,
        backend="gst", pred_struct=2)


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    if which in ("phase1", "all"):
        phase1()
    if which in ("phase2", "all"):
        phase2()
    print("ALLDONE17E", flush=True)
