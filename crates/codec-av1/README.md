# holochain-moq-codec-av1

AV1 encode (rav1e, pure Rust; optionally SVT-AV1) and decode (rav1d, pure Rust) for the live video path.
Codec logic lives here and nowhere else: never in `realtime`, `protocol`, `node` or the DNA.

## Which encoder

**rav1e is the default and stays the default.** SVT-AV1 is an explicit opt-in:

```sh
HOLOCHAIN_MOQ_AV1_ENCODER=svt          # SVT-AV1 through its C library API, low delay, one frame in flight
HOLOCHAIN_MOQ_SVT_IN_FLIGHT=1          # experiment switch; leave at 1 (see below)
```

Both encoders produce exactly the same encoded-media contract (one access unit per source frame,
keyframes only where the worker asked for them, a temporal delimiter first and a sequence header in
every keyframe unit), so nothing downstream can tell them apart. In code: `Av1Config::encoder`.

### What Stages 17E-17G established (measured on an i5-12500T, 12 threads; details in the stage reports)

* **Keep SVT at one frame in flight.** Pipelining adds at most about 4% capacity, flips none of the
  12 tested real-time verdicts, and increases latency under overload (camera-like 1080p30 end-to-end
  p95: 181-194 ms at 1 in flight, 273-312 ms at 4-8). Frame k+1 cannot start before keyframe k has
  finished, so no amount of pipelining hides a keyframe.
* **SVT sustains 11 of 12 tested real-time points** (720p and 1080p, 15 and 30 fps, synthetic,
  screencast and camera-like content, paced with the sharer's bounded drop-oldest queue). rav1e
  sustains 1 of 12.
* **Camera-like 1080p30 does not sustain with keyframe interval 5** (about 26 fps, 9-24% dropped, at
  every in-flight depth). A labelled experiment with keyframe interval 10 sustained it with no drops
  (bitrate 31.9 -> 19.7 Mbps). **That is an experiment, not a production policy or a default.** The
  interval is a policy decision that also sets recovery and bitrate behaviour.
* Camera-like content is **synthetic** (texture, pan, moving objects, sensor noise), not camera
  footage; on it SVT needs 83-88% of rav1e's bitrate at equal PSNR, against 17-50% on easy content.

### Before any default encoder switch

1. Validate on **real camera footage** (there is no camera on the development machine).
2. Decide the **runtime-library and version requirement**: SVT-AV1 is loaded at run time
   (`libSvtAv1Enc.so.2`, version 2.3.x only; the generated bindings' struct layout is version
   specific and any other version is refused with a clear error). It is not linked or bundled.
3. Decide the **keyframe policy** (interval, and whether subscriber-requested keyframes are the
   recovery path; forced keyframes work on demand with SVT and are tested).
4. Keep in mind the constraints below.

### SVT-AV1 constraints worth knowing

* **One live encoder per process.** SVT-AV1 2.3.0 does not isolate concurrent encoder instances in a
  process: with several alive it crashes intermittently inside its own threads. `svt::SvtEncoder`
  enforces one at a time and waits (bounded) for the previous one to be torn down.
* Frames must be at least 64x64 and even-sized.
* The GStreamer `svtav1enc` plugin deadlocks in low-delay mode (the library's `get_packet` blocks
  there; the plugin drains until empty). It is not used.
* Thread-safety of one sender plus one receiver thread per handle is a property of 2.3.x's
  implementation, established from its source, not a documented contract; see `src/svt/mod.rs`.

### Reproducing the measurements

`av1-cpu-bench` (crate `screenshare`) and the scripts in `crates/screenshare/scripts/`:
`run_stage17e.py`, `run_stage17f.py`, `run_stage17g.py` and their `analyze_*.py`. The workloads are
deterministic (`av1-cpu-bench dump`, `make_camera_like.py`).

### Regenerating the SVT-AV1 bindings

`src/svt/ffi.rs` is generated: see `tools/gen_svt_ffi/README.md`.
