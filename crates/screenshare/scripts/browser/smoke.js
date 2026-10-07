// Stage 17G smoke test: publisher (whatever encoder the environment selects) -> real Chrome, then
//   A. public live session  B. publisher kill -9 + replacement (reconnect)  C. normal session end.
//   ADMIN_A=127.0.0.1:<port> HOLOCHAIN_MOQ_AV1_ENCODER=svt HOLOCHAIN_MOQ_SVT_IN_FLIGHT=4 node smoke.js [tag]
const L = require("./lib.js");
const cp = require("child_process");
const path = require("path");
const BIN = path.resolve(__dirname, "../../../../target/release/screenshare");
const tag = process.argv[2] || "smoke";
function pub(title) {
  const p = cp.spawn(BIN, ["--admin", process.env.ADMIN_A, "share", "--source", "synthetic", "--title", title], { stdio: ["ignore", "pipe", "pipe"] });
  let out = ""; p.stdout.on("data", (d) => (out += d)); p.stderr.on("data", (d) => (out += d)); p.log = () => out; return p;
}
const waitLive = async (p) => { for (let i = 0; i < 60; i++) { if (/state: live/.test(p.log())) return; await L.sleep(500); } throw new Error("publisher not live: " + p.log().slice(-300)); };
const svtLoaded = (p) => { try { return require("fs").readFileSync(`/proc/${p.pid}/maps`, "utf8").includes("libSvtAv1Enc"); } catch (_) { return null; } };
(async () => {
  const p1 = pub(`${tag}-1`); await waitLive(p1);
  console.log("publisher encoder library mapped (SVT):", svtLoaded(p1));
  const { browser, page, errors } = await L.open();
  await L.watchByTitle(page, `${tag}-1`);
  await L.sleep(15000);
  const pre = await L.stats(page); const preUi = await L.ui(page);
  console.log("A live  ", JSON.stringify({ ui: preUi.headline + "|" + preUi.video + "|epoch " + preUi.epoch, ...L.brief(pre) }));
  const tKill = Date.now(); p1.kill("SIGKILL"); await L.sleep(1500);
  const p2 = pub(`${tag}-2`); await waitLive(p2);
  let recovered = null, sawStall = false, sawEpoch1 = false;
  for (let i = 0; i < 90; i++) {
    await L.sleep(1000);
    const u = await L.ui(page), s = await L.stats(page);
    if (/stalled/.test(u.video)) sawStall = true;
    if (u.epoch === "1") sawEpoch1 = true;
    if (sawEpoch1 && u.video === "decoding" && s.frames_displayed > pre.frames_displayed + 20) { recovered = Math.round((Date.now() - tKill) / 1000); break; }
  }
  const mid = await L.stats(page), midUi = await L.ui(page);
  console.log("B reconnect", JSON.stringify({ recovered_at_s: recovered, stalled_seen: sawStall, epoch1_seen: sawEpoch1, ui: midUi.headline + "|" + midUi.video + "|epoch " + midUi.epoch + "|" + midUi.size, ...L.brief(mid), configure_calls: mid.cfg.length }));
  const before = mid.decoder_errors;
  p2.kill("SIGINT");
  let endUi = null, endStats = null;
  for (let i = 0; i < 40; i++) { await L.sleep(1000); endUi = await L.ui(page); endStats = await L.stats(page).catch(() => null); if (/^(Ended|Ready)/.test(endUi.headline || "")) break; }
  console.log("C end   ", JSON.stringify({ ui: endUi.headline + " - " + endUi.detail, decoder_errors_before: before, decoder_errors_after: endStats && endStats.decoder_errors, ws: endStats && endStats.ws_open + "/" + endStats.ws_close, publisher_log_tail: p2.log().trim().split("\n").slice(-1)[0], page_errors: errors }));
  await browser.close(); try { p2.kill("SIGKILL"); } catch (_) {}
})().catch((e) => { console.error("FAIL", e); process.exit(1); });
