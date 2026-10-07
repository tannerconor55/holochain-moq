#!/usr/bin/env python3
"""Stage 17E: replay an `av1-cpu-bench dump` through GStreamer's `svtav1enc` (measurement only).

The counterpart of `av1-cpu-bench rav1e`: the SAME dump file, the same metrics, the same stream
file format (`[key u8][len u32 LE][payload]`), so `av1-cpu-bench quality` scores both encoders with
the one repository decoder.

    svt_bench.py --input s720.i420 --unpaced --preset 10 --crf 35 --out s720.svt.frames
    svt_bench.py --input s720.i420 --fps 15 --seconds 30 --preset 10 --crf 35        # real time

Two backends:
  --backend gst  GStreamer `svtav1enc` (random-access prediction only: with this GStreamer 1.28.2 /
                 SVT-AV1 2.3.0 combination the plugin DEADLOCKS in low-delay mode, `pred-struct=1`).
  --backend app  the `SvtAv1EncApp` CLI (SVT_APP env var; same 2.3.0 library) over pipes, which
                 supports low delay properly. Encoder entry is "the frame was fully written to its
                 stdin"; there is no in-encoder visibility.

GStreamer pipeline: appsrc -> svtav1enc -> appsink, in this process (so process CPU/RSS include the encoder
threads). Stage timestamps: `push` (frame handed to appsrc), `enter` (buffer reaches the
encoder's sink pad, a pad probe), `out` (packet reaches appsink). appsrc's bounded queue plays the
role of the production raw-frame queue: 4 buffers; blocking when unpaced (lossless) and dropping
the OLDEST when paced (the sharer's `DropOldest`).
"""
import argparse
import json
import os
import struct
import sys
import threading
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gst  # noqa: E402

Gst.init(None)
NS = 1_000_000_000
TICK = os.sysconf("SC_CLK_TCK")


def read_dump(path):
    with open(path, "rb") as f:
        d = f.read()
    assert d[:4] == b"I420", "not an av1-cpu-bench dump"
    w, h, fps = struct.unpack("<III", d[4:16])
    fl = w * h * 3 // 2
    body = memoryview(d)[16:]
    assert len(body) % fl == 0
    return w, h, fps, body, len(body) // fl, fl


def proc_cpu_s():
    with open("/proc/self/stat") as f:
        parts = f.read().rsplit(")", 1)[1].split()
    return (int(parts[11]) + int(parts[12])) / TICK  # utime + stime, all threads


def rss_mb():
    with open("/proc/self/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024.0
    return 0.0


def wall_now():
    return time.monotonic() - START


START = time.monotonic()


def pct(sorted_v, p):
    if not sorted_v:
        return float("nan")
    return sorted_v[int(round((len(sorted_v) - 1) * p))]


def stat(v):
    s = sorted(v)
    return (sum(s) / max(len(s), 1), pct(s, 0.5), pct(s, 0.95), s[-1] if s else float("nan"))


def build_report(a, w, h, fps, pushed, t_push, t_enter, outs, cpu_samples, rss_max, cpu_total, wall, pstr, extra):
    outs.sort(key=lambda o: o[1])
    got = [o for o in outs if o[0] >= 0]
    enc_l = [(o[1] - t_enter[o[0]]) / 1e6 for o in got if o[0] in t_enter]
    wait_l = [(t_enter[i] - t_push[i]) / 1e6 for i in t_enter if i in t_push]
    e2e_l = [(o[1] - t_push[o[0]]) / 1e6 for o in got if o[0] in t_push]
    ea, e50, e95, emax = stat(enc_l)
    wa, _, _, wmax = stat(wait_l)
    _, p50, p95, pmax = stat(e2e_l)
    third = max(len(e2e_l) // 3, 1)
    head = sum(e2e_l[:third]) / third
    tail = sum(e2e_l[-third:]) / third
    by_idx = sorted(got, key=lambda o: o[0])
    keys = [o for o in by_idx if o[3]]
    first_key = next(((o[1] - t_enter[o[0]]) / 1e6 for o in sorted(got, key=lambda o: o[1]) if o[3] and o[0] in t_enter), None)
    total_bytes = sum(o[2] for o in got)
    nframes = len(got)
    media_s = nframes / fps
    key_gaps = [b[0] - a_[0] for a_, b in zip(keys, keys[1:])]
    steady = cpu_samples[1:] if len(cpu_samples) > 2 else cpu_samples
    entered = len(t_enter)
    first_push = min(t_push.values()) if t_push else 0
    last_out = max(o[1] for o in got) if got else 0
    active_s = (last_out - first_push) / NS if got else float("nan")
    rep = {
        "encoder": a.label,
        "input": os.path.basename(a.input),
        "resolution": f"{w}x{h}",
        "mode": "unpaced" if a.unpaced else "paced",
        "target_fps": fps,
        "run_seconds": round(wall, 1),
        "svt_preset": a.preset,
        "svt_crf": a.crf if not a.bitrate else None,
        "svt_target_bitrate_kbps": a.bitrate or None,
        "svt_lp": a.lp or "auto",
        "svt_intra_period_length": a.intra_period,
        "svt_parameters_string": pstr,
        **extra,
        "frames_from_source": pushed,
        "frames_entered_encoder": entered,
        "frames_encoded": nframes,
        "dropped_encoder_queue": pushed - entered,
        "overall_fps": round(nframes / active_s, 2) if got else None,
        "enc_latency_avg_ms": round(ea, 1),
        "enc_latency_p50_ms": round(e50, 1),
        "enc_latency_p95_ms": round(e95, 1),
        "enc_latency_max_ms": round(emax, 1),
        "queue_wait_avg_ms": round(wa, 1),
        "queue_wait_max_ms": round(wmax, 1),
        "e2e_p50_ms": round(p50, 1),
        "e2e_p95_ms": round(p95, 1),
        "e2e_max_ms": round(pmax, 1),
        "e2e_first_third_avg_ms": round(head, 1),
        "e2e_last_third_avg_ms": round(tail, 1),
        "first_keyframe_latency_ms": round(first_key, 1) if first_key is not None else None,
        "total_bytes": total_bytes,
        "bitrate_kbps": round(total_bytes * 8 / media_s / 1000) if nframes else None,
        "bytes_per_frame": round(total_bytes / max(nframes, 1)),
        "keyframes": len(keys),
        "keyframe_gap_frames": sorted(set(key_gaps)),
        "avg_keyframe_bytes": round(sum(o[2] for o in keys) / max(len(keys), 1)),
        "avg_interframe_bytes": round(sum(o[2] for o in got if not o[3]) / max(nframes - len(keys), 1)),
        "cpu_percent_steady": round(sum(steady) / max(len(steady), 1)),
        "cpu_percent_total": round(cpu_total),
        "cpu_cores": os.cpu_count(),
        "rss_mb": round(rss_max),
    }
    return rep, by_idx


def is_key_tu(tu):
    """True when the first frame header in this temporal unit is a KEY_FRAME (show_existing_frame=0,
    frame_type=0: the top three bits of its first payload byte)."""
    i = 0
    while i < len(tu):
        hdr = tu[i]
        typ = (hdr >> 3) & 0xF
        ext = (hdr >> 2) & 1
        has_size = (hdr >> 1) & 1
        i += 1 + ext
        if not has_size:
            return False
        size, shift = 0, 0
        while True:
            b = tu[i]
            i += 1
            size |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                break
        if typ in (3, 6):  # OBU_FRAME_HEADER / OBU_FRAME
            return (tu[i] & 0xE0) == 0
        i += size
    return False


def cpu_of(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().rsplit(")", 1)[1].split()
        return (int(parts[11]) + int(parts[12])) / TICK
    except OSError:
        return None


def rss_of(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) / 1024.0
    except OSError:
        pass
    return 0.0


def run_app(a, w, h, fps, body, n, fl):
    import collections
    import resource
    import subprocess

    app = os.environ.get("SVT_APP", "SvtAv1EncApp")
    dur = NS // fps
    total = min(n, a.frames or n) if a.unpaced else 10**9
    cmd = [app, "-i", "stdin", "-b", "stdout", "-w", str(w), "-h", str(h), "--fps-num", str(fps),
           "--fps-denom", "1", "--preset", str(a.preset), "--pred-struct", str(a.pred_struct),
           "--keyint", str(a.keyint), "--progress", "0"]
    if a.pred_struct == 1:
        cmd += ["--lookahead", "0"]
    cmd += ["--rc", "2", "--tbr", str(a.bitrate)] if a.bitrate else ["--rc", "0", "--crf", str(a.crf if a.crf >= 0 else 35)]
    if a.lp:
        cmd += ["--lp", str(a.lp)]
    if a.tune is not None:
        cmd += ["--tune", str(a.tune)]
    if a.extra:
        cmd += a.extra.split()
    # SvtAv1EncApp writes its IVF output through a fully-buffered stdio stream: tiny interframes sit
    # in that buffer for several frames (measured: 211 ms p50 vs 4 ms). Without unbuffered stdout
    # the latency numbers would measure the pipe, not the encoder, so it is not optional.
    import shutil
    if not shutil.which("stdbuf"):
        print(json.dumps({"error": "stdbuf (coreutils) is required to measure app-backend latency"}))
        sys.exit(2)
    cmd = ["stdbuf", "-o0"] + cmd
    err = None if a.verbose else subprocess.DEVNULL
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err, bufsize=0)

    lock = threading.Lock()
    cond = threading.Condition(lock)
    q = collections.deque()
    t_push, t_enter, outs, sent = {}, {}, [], []
    produced = [0]
    stop_push = threading.Event()
    prod_done = threading.Event()

    def producer():
        t0 = time.monotonic_ns()
        i = 0
        while i < total and not stop_push.is_set():
            if not a.unpaced:
                wait = t0 + i * dur - time.monotonic_ns()
                if wait > 0:
                    time.sleep(wait / NS)
            with cond:
                if a.unpaced:
                    while len(q) >= a.queue:  # lossless: the source waits
                        cond.wait()
                t_push[i] = time.monotonic_ns()
                q.append(i)
                if len(q) > a.queue:  # DropOldest, as the sharer's live queue
                    q.popleft()
                cond.notify_all()
            i += 1
        produced[0] = i
        with cond:
            prod_done.set()
            cond.notify_all()

    def writer():
        while True:
            with cond:
                while not q and not prod_done.is_set():
                    cond.wait()
                if not q:
                    break
                i = q.popleft()
                sent.append(i)
                cond.notify_all()
            k = (i % n) * fl
            try:
                proc.stdin.write(bytes(body[k:k + fl]))
            except (BrokenPipeError, OSError):
                break
            with lock:
                t_enter[i] = time.monotonic_ns()
        try:
            proc.stdin.close()
        except OSError:
            pass

    def read_exact(k):
        buf = bytearray()
        while len(buf) < k:
            c = proc.stdout.read(k - len(buf))
            if not c:
                return None
            buf += c
        return bytes(buf)

    reader_done = threading.Event()

    def reader():
        hdr = read_exact(32)  # IVF file header
        j = 0
        while hdr:
            fh = read_exact(12)
            if not fh:
                break
            size = struct.unpack("<I", fh[:4])[0]
            data = read_exact(size)
            if data is None:
                break
            now = time.monotonic_ns()
            with lock:
                idx = sent[j] if j < len(sent) else -1
            outs.append((idx, now, size, is_key_tu(data), data if a.out else None))
            j += 1
        reader_done.set()

    for t in (producer, writer, reader):
        threading.Thread(target=t, daemon=True).start()
    wall0 = time.monotonic()
    self_cpu0 = proc_cpu_s()
    cpu_samples, rss_max = [], 0.0
    last_cpu, last_t = 0.0, wall0
    deadline = wall0 + a.seconds
    while not reader_done.is_set():
        time.sleep(1.0)
        now = time.monotonic()
        c = cpu_of(proc.pid)
        if c is not None:
            cpu_samples.append(100.0 * (c - last_cpu) / (now - last_t))
            last_cpu = c
        last_t = now
        rss_max = max(rss_max, rss_of(proc.pid))
        if a.verbose:
            print(f"[{now - wall0:.0f}s] produced={len(t_push)} written={len(t_enter)} out={len(outs)} cpu={cpu_samples[-1] if cpu_samples else 0:.0f}%", file=sys.stderr)
        if not a.unpaced and now >= deadline and not stop_push.is_set():
            stop_push.set()
    proc.wait()
    wall = time.monotonic() - wall0
    ru = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu_total = 100.0 * (ru.ru_utime + ru.ru_stime) / wall
    if proc.returncode not in (0, None):
        print(json.dumps({"error": f"SvtAv1EncApp exited {proc.returncode}"}))
        sys.exit(3)
    rep, by_idx = build_report(a, w, h, fps, produced[0], t_push, t_enter, outs, cpu_samples, rss_max,
                               cpu_total, wall, " ".join(cmd),
                               {"backend": "SvtAv1EncApp (libsvtav1enc 2.3.0)",
                                "harness_cpu_percent": round(100.0 * (proc_cpu_s() - self_cpu0) / wall)})
    return rep, by_idx


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--backend", choices=["gst", "app"], default="app")
    ap.add_argument("--tune", type=int, default=None, help="app backend: svt --tune (library default 1 = PSNR)")
    ap.add_argument("--input", required=True)
    ap.add_argument("--fps", type=int)
    ap.add_argument("--unpaced", action="store_true")
    ap.add_argument("--seconds", type=int, default=30)
    ap.add_argument("--preset", type=int, default=10)
    ap.add_argument("--crf", type=int, default=-1, help="CRF mode QP (0-63); -1 = off")
    ap.add_argument("--bitrate", type=int, default=0, help="target kbit/s (CBR with --rc 2)")
    ap.add_argument("--lp", type=int, default=0, help="level of parallelism (0 = library auto)")
    ap.add_argument("--intra-period", type=int, default=4,
                    help="svtav1enc intra-period-length (4 gives a keyframe every 5 frames; verified in the output)")
    ap.add_argument("--keyint", type=int, default=5, help="app backend: GOP size in frames (production: 5)")
    ap.add_argument("--pred-struct", type=int, default=1, help="1 = low delay")
    ap.add_argument("--extra", default="", help="more colon-delimited svt key=value pairs")
    ap.add_argument("--queue", type=int, default=4, help="appsrc queue capacity (production raw queue: 4)")
    ap.add_argument("--frames", type=int, help="unpaced: encode only the first N frames of the dump")
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("--out")
    ap.add_argument("--label", default="svtav1enc")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    w, h, dump_fps, body, n, fl = read_dump(a.input)
    fps = a.fps or dump_fps
    dur = NS // fps
    total = min(n, a.frames or n) if a.unpaced else 10**9

    params = [f"pred-struct={a.pred_struct}"]
    if a.lp:
        params.append(f"lp={a.lp}")
    if a.bitrate:
        params.append("rc=2")
    if a.extra:
        params.append(a.extra)
    pstr = ":".join(params)

    if a.backend == "app":
        rep, by_idx = run_app(a, w, h, fps, body, n, fl)
        return finish(a, rep, by_idx)

    pipe = Gst.Pipeline.new("p")
    src = Gst.ElementFactory.make("appsrc", "src")
    enc = Gst.ElementFactory.make("svtav1enc", "enc")
    sink = Gst.ElementFactory.make("appsink", "sink")
    if not enc:
        print(json.dumps({"error": "svtav1enc element is not available"}))
        sys.exit(2)
    src.set_property("caps", Gst.Caps.from_string(f"video/x-raw,format=I420,width={w},height={h},framerate={fps}/1"))
    src.set_property("format", Gst.Format.TIME)
    src.set_property("is-live", True)
    src.set_property("max-buffers", a.queue)
    src.set_property("max-bytes", 0)
    src.set_property("block", a.unpaced)
    src.set_property("leaky-type", 0 if a.unpaced else 2)  # none (lossless) / downstream = drop oldest
    enc.set_property("preset", a.preset)
    enc.set_property("intra-period-length", a.intra_period)
    if a.bitrate:
        enc.set_property("target-bitrate", a.bitrate)
    elif a.crf >= 0:
        enc.set_property("crf", a.crf)
    enc.set_property("parameters-string", pstr)
    sink.set_property("emit-signals", True)
    sink.set_property("sync", False)
    sink.set_property("max-buffers", 0)
    sink.set_property("drop", False)
    sink.set_property("caps", Gst.Caps.from_string("video/x-av1,stream-format=obu-stream,alignment=tu"))
    for e in (src, enc, sink):
        pipe.add(e)
    src.link(enc)
    enc.link(sink)

    lock = threading.Lock()
    t_push, t_enter, outs = {}, {}, []  # idx -> ns ; outs: (idx, t_out, size, key, payload)

    def on_enter(pad, info):
        b = info.get_buffer()
        if b is not None and b.pts != Gst.CLOCK_TIME_NONE:
            with lock:
                t_enter.setdefault(b.pts // dur, time.monotonic_ns())
        return Gst.PadProbeReturn.OK

    enc.get_static_pad("sink").add_probe(Gst.PadProbeType.BUFFER, on_enter)
    keep = a.out is not None

    def on_sample(s):
        smp = s.emit("pull-sample")
        b = smp.get_buffer()
        now = time.monotonic_ns()
        size = b.get_size()
        key = not b.has_flags(Gst.BufferFlags.DELTA_UNIT)
        payload = b.extract_dup(0, size) if keep else None
        with lock:
            outs.append((b.pts // dur if b.pts != Gst.CLOCK_TIME_NONE else -1, now, size, key, payload))
        return Gst.FlowReturn.OK

    sink.connect("new-sample", on_sample)

    bus = pipe.get_bus()
    done = threading.Event()
    err = []

    def watch():
        while not done.is_set():
            m = bus.timed_pop_filtered(200 * 1_000_000, Gst.MessageType.ERROR | Gst.MessageType.EOS)
            if m is None:
                continue
            if m.type == Gst.MessageType.ERROR:
                err.append(str(m.parse_error()))
            done.set()

    threading.Thread(target=watch, daemon=True).start()

    stop_push = threading.Event()
    pushed = [0]

    def pusher():
        t0 = time.monotonic_ns()
        i = 0
        while i < total and not stop_push.is_set():
            if not a.unpaced:
                wait = t0 + i * dur - time.monotonic_ns()
                if wait > 0:
                    time.sleep(wait / NS)
            k = (i % n) * fl
            buf = Gst.Buffer.new_wrapped(bytes(body[k:k + fl]))
            buf.pts = i * dur
            buf.duration = dur
            with lock:
                t_push[i] = time.monotonic_ns()
            src.emit("push-buffer", buf)
            i += 1
        pushed[0] = i
        src.emit("end-of-stream")

    cpu0, wall0 = proc_cpu_s(), time.monotonic()
    pipe.set_state(Gst.State.PLAYING)
    threading.Thread(target=pusher, daemon=True).start()

    cpu_samples, rss_max = [], 0.0
    last_cpu, last_t = cpu0, wall0
    deadline = wall0 + a.seconds
    while not done.is_set():
        time.sleep(1.0)
        now = time.monotonic()
        c = proc_cpu_s()
        cpu_samples.append(100.0 * (c - last_cpu) / (now - last_t))
        last_cpu, last_t = c, now
        rss_max = max(rss_max, rss_mb())
        if a.verbose:
            print(f"[{wall_now():.0f}s] pushed={len(t_push)} entered={len(t_enter)} out={len(outs)} cpu={cpu_samples[-1]:.0f}%", file=sys.stderr)
        if not a.unpaced and now >= deadline and not stop_push.is_set():
            stop_push.set()  # the pusher then sends EOS and the pipeline drains
    wall = time.monotonic() - wall0
    cpu_total = 100.0 * (proc_cpu_s() - cpu0) / wall
    pipe.set_state(Gst.State.NULL)
    if err:
        print(json.dumps({"error": err[0]}))
        sys.exit(3)

    rep, by_idx = build_report(a, w, h, fps, pushed[0], t_push, t_enter, outs, cpu_samples, rss_max, cpu_total, wall, pstr, {"gst": Gst.version_string()})
    return finish(a, rep, by_idx)


def finish(a, rep, by_idx):
    if a.out:
        with open(a.out, "wb") as f:
            for o in by_idx:
                f.write(bytes([1 if o[3] else 0]) + struct.pack("<I", o[2]) + o[4])
        rep["stream_file"] = a.out
    if a.json:
        print(json.dumps(rep))
    else:
        for k, v in rep.items():
            print(f"  {k} = {v}")


if __name__ == "__main__":
    main()
