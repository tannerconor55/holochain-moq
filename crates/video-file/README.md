# holochain-moq-video-file

A `VideoFrameSource` that plays a video file by running a decoder (GStreamer) as a child process and
reading raw I420 frames from its output.

## Known issue: a load-dependent race in one test (tracked separately)

`dropping_the_source_stops_the_decoder_and_both_threads` can hang, and the hung test spins a CPU core
forever, when the machine is busy. It is a race in the **test's own setup**, not in playback:

* The test uses `sh -c yes` as a stand-in decoder. Here `sh` (dash) **forks** `yes` instead of
  exec'ing it, and `Drop` kills only the direct child.
* If `sh` is killed after it has forked `yes` (which is what CPU contention makes likely), `yes`
  outlives it and keeps the pipe open, so the reader thread never sees end-of-file and `Drop`'s
  `join` never returns.
* Measured with a script that involves none of this crate's code: an `yes` is orphaned after an
  immediate kill of `sh` in 0 of 150 tries on an idle machine and in 109 of 150 with 12 busy loops.
  The full suite hung in 4 of 25 runs while other work was running, and passed 20 of 20 both
  sequentially and in parallel on an idle machine.

Not fixed here (out of scope of the milestone in which it was found). Likely fixes: `sh -c "exec yes"`
in the test, and/or killing the child's whole process group in `Drop`. Real decoders are started
directly by `gst-launch-1.0`, not through a shell, so playback is not affected the same way.
