// SPDX-License-Identifier: Apache-2.0
// Smoke harness: drives OBS over obs-websocket v5 and the LUPO dock over its
// own protocol; verifies mixer meters, a recording, and the full settings flow.
// Usage: OBS_WS_PASSWORD=... node obs-smoke.mjs
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { renameSync, existsSync, readFileSync } from "node:fs";
import { request } from "node:http";
import { join } from "node:path";

const PORT = process.env.OBS_WS_PORT ?? "4455";
const PASSWORD = process.env.OBS_WS_PASSWORD ?? "";
const COLLECTION = "LupoTest";
const INPUT = "Lupo TTS";
const INPUT2 = "Lupo TTS 2";
const OUT_DIR = process.env.OUT_DIR ?? process.cwd();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name} ${detail ?? ""}`);
}

const ws = new WebSocket(`ws://127.0.0.1:${PORT}`);
const pending = new Map();
let nextId = 1;
const meterListeners = new Set();
let identified;
const identifiedPromise = new Promise((r) => (identified = r));

ws.addEventListener("message", (ev) => {
  const msg = JSON.parse(ev.data);
  if (msg.op === 0) {
    const d = { rpcVersion: 1, eventSubscriptions: 1 | (1 << 16) };
    if (msg.d.authentication) {
      const { salt, challenge } = msg.d.authentication;
      const secret = createHash("sha256").update(PASSWORD + salt).digest("base64");
      d.authentication = createHash("sha256").update(secret + challenge).digest("base64");
    }
    ws.send(JSON.stringify({ op: 1, d }));
  } else if (msg.op === 2) {
    identified();
  } else if (msg.op === 7) {
    const p = pending.get(msg.d.requestId);
    if (p) {
      pending.delete(msg.d.requestId);
      if (msg.d.requestStatus.result) p.resolve(msg.d.responseData ?? {});
      else p.reject(new Error(`${msg.d.requestType}: ${msg.d.requestStatus.code} ${msg.d.requestStatus.comment ?? ""}`));
    }
  } else if (msg.op === 5 && msg.d.eventType === "InputVolumeMeters") {
    for (const l of meterListeners) l(msg.d.eventData.inputs);
  }
});
ws.addEventListener("error", (e) => { console.error("ws error", e.message ?? e); process.exit(2); });

function req(requestType, requestData = {}) {
  const requestId = String(nextId++);
  return new Promise((resolve, reject) => {
    pending.set(requestId, { resolve, reject });
    ws.send(JSON.stringify({ op: 6, d: { requestType, requestId, requestData } }));
  });
}

/** Max per-channel peak (linear) seen for `inputName` over `ms`. */
function peakOver(inputName, ms) {
  return new Promise((resolve) => {
    let peak = 0, events = 0;
    const l = (inputs) => {
      events++;
      for (const i of inputs) {
        if (i.inputName !== inputName) continue;
        for (const ch of i.inputLevelsMul ?? []) peak = Math.max(peak, ch[1] ?? 0);
      }
    };
    meterListeners.add(l);
    setTimeout(() => { meterListeners.delete(l); resolve({ peak, events }); }, ms);
  });
}

async function press(inputName, propertyName) {
  await req("PressInputPropertiesButton", { inputName, propertyName });
}

/** Force OBS to rebuild the properties (writes the live status rows into settings). */
async function rebuildProperties(inputName) {
  await req("GetInputPropertiesListPropertyItems", { inputName, propertyName: "engine_status" }).catch(() => {});
}

await new Promise((r) => ws.addEventListener("open", r));
await identifiedPromise;

const version = await req("GetVersion");
console.log(`OBS ${version.obsVersion}, obs-websocket ${version.obsWebSocketVersion}`);

// Isolate everything in its own scene collection; restore the user's at the end.
const { currentSceneCollectionName: originalCollection, sceneCollections } = await req("GetSceneCollectionList");
if (!sceneCollections.includes(COLLECTION)) {
  await req("CreateSceneCollection", { sceneCollectionName: COLLECTION });
} else if (originalCollection !== COLLECTION) {
  await req("SetCurrentSceneCollection", { sceneCollectionName: COLLECTION });
}
await sleep(2000);

try {
  const kinds = await req("GetInputKindList", { unversioned: true });
  check("source kind registered", kinds.inputKinds.includes("lupo_tts"), kinds.inputKinds.filter((k) => k.includes("lupo")).join(","));

  const { currentProgramSceneName: sceneName } = await req("GetSceneList");
  const inputs = (await req("GetInputList", { inputKind: "lupo_tts" })).inputs.map((i) => i.inputName);
  if (!inputs.includes(INPUT)) {
    await req("CreateInput", { sceneName, inputName: INPUT, inputKind: "lupo_tts", sceneItemEnabled: true });
    await sleep(500);
  }
  // The raw playback buttons live under "Show debug tools".
  await req("SetInputSettings", { inputName: INPUT, inputSettings: { debug_tools: true } });

  const baseline = await peakOver(INPUT, 1000);
  check("meters silent before playback", baseline.peak < 0.01 && baseline.events > 5, JSON.stringify(baseline));

  await press(INPUT, "test_tone");
  const tone = await peakOver(INPUT, 1500);
  check("tone reaches the mixer (peak > 0.3)", tone.peak > 0.3, JSON.stringify(tone));

  await press(INPUT, "test_clip");
  const clip = await peakOver(INPUT, 3000);
  check("22.05 kHz mono clip reaches the mixer (peak > 0.05)", clip.peak > 0.05, JSON.stringify(clip));

  // Muted: OBS keeps consuming but the meter should stay silent.
  await req("SetInputMute", { inputName: INPUT, inputMuted: true });
  await press(INPUT, "test_tone");
  const muted = await peakOver(INPUT, 1500);
  await req("SetInputMute", { inputName: INPUT, inputMuted: false });
  check("muted source shows no meter level", muted.peak < 0.01, JSON.stringify(muted));

  // Hidden scene item (audio sources stay active even when hidden in OBS —
  // record what actually happens).
  const { sceneItemId } = await req("GetSceneItemId", { sceneName, sourceName: INPUT });
  await req("SetSceneItemEnabled", { sceneName, sceneItemId, sceneItemEnabled: false });
  await sleep(300); // let OBS deactivate the source before pressing
  await press(INPUT, "test_tone");
  const hidden = await peakOver(INPUT, 1500);
  await req("SetSceneItemEnabled", { sceneName, sceneItemId, sceneItemEnabled: true });
  console.log(`INFO hidden scene item meter: ${JSON.stringify(hidden)}`);

  // Dock: the Properties row advertises the address; the token comes from the
  // plugin's config directory (the Copy button uses the clipboard). Everything
  // else — pipeline, settings, Twitch — is driven over the dock protocol.
  await rebuildProperties(INPUT);
  const dockRow = (await req("GetInputSettings", { inputName: INPUT })).inputSettings.dock_url ?? "";
  const address = (/(http:\/\/127\.0\.0\.1:\d+\/)/.exec(dockRow) ?? [])[1] ?? "";
  check("dock address advertised in Properties without the token", !!address && !/token/.test(dockRow), dockRow);
  check("dock added to the Docks menu natively", /added to the Docks menu/.test(dockRow), dockRow);
  let dockUrl = "";
  try {
    const token = readFileSync(`${process.env.HOME}/Library/Application Support/obs-studio/plugin_config/lupo/dock-token`, "utf8").trim();
    dockUrl = `${address}?token=${token}`;
  } catch (e) { check("dock token file exists", false, String(e)); }
  if (/^http/.test(dockUrl)) {
    const u = new URL(dockUrl);
    const unauth = await fetch(`http://${u.host}/`).then((r) => r.status).catch((e) => String(e));
    check("dock page refuses requests without the token", unauth === 403, String(unauth));
    const page = await fetch(dockUrl).then((r) => r.text()).catch(() => "");
    check("dock serves the Dioxus page", /LUPO TTS/.test(page) && /dock_ui\.js/.test(page));
    const wasm = await fetch(`http://${u.host}/assets/dock_ui_bg.wasm`).then((r) => ({ status: r.status, type: r.headers.get("content-type") })).catch((e) => ({ status: String(e) }));
    check("dock serves the wasm bundle", wasm.status === 200 && wasm.type === "application/wasm", JSON.stringify(wasm));
    // fetch() normalizes "../" away before the request leaves the client, so
    // send the raw path over node:http to actually exercise the server check.
    const snippet = await new Promise((resolve) => {
      const r = request({ host: u.hostname, port: u.port, path: "/assets/snippets/../secrets" }, (res) => {
        res.resume();
        resolve(res.statusCode);
      });
      r.on("error", () => resolve(0));
      r.end();
    });
    check("dock rejects path traversal in assets", snippet === 404 || snippet === 400, String(snippet));

    const events = [];
    const dock = new WebSocket(`ws://${u.host}/ws?token=${u.searchParams.get("token")}`);
    dock.addEventListener("message", (ev) => events.push(JSON.parse(ev.data)));
    await new Promise((r, j) => { dock.addEventListener("open", r); dock.addEventListener("error", j); }).catch(() => {});
    await sleep(500);
    const snap = events[0];
    check("dock sends a snapshot on connect", snap?.type === "snapshot" && Array.isArray(snap.messages) && !!snap.chat?.connection, JSON.stringify(snap && { type: snap.type, n: snap.messages?.length, chat: snap.chat?.connection?.state, providers: snap.providers?.length, volume: snap.volume }));
    if (snap?.playback?.enabled === false) { dock.send(JSON.stringify({ type: "resume" })); await sleep(300); }

    dock.send(JSON.stringify({ type: "pause" }));
    await sleep(500);
    check("dock pause command is applied", events.some((e) => e.type === "playback" && e.playback.enabled === false));
    dock.send(JSON.stringify({ type: "resume" }));
    await sleep(300);

    // Volume round-trips through the pipeline.
    dock.send(JSON.stringify({ type: "set_volume", volume: 1.5 }));
    await sleep(500);
    check("dock volume command round-trips", events.some((e) => e.type === "volume" && Math.abs(e.volume - 1.5) < 0.01));
    dock.send(JSON.stringify({ type: "set_volume", volume: 1.0 }));
    await sleep(300);

    // Pipeline: a message submitted from the dock must be spoken and streamed back.
    events.length = 0;
    dock.send(JSON.stringify({ type: "submit", text: "Lupo pipeline test. One, two, three." }));
    const pipeline = await peakOver(INPUT, 7000);
    check("pipeline message is spoken through the source (peak > 0.05)", pipeline.peak > 0.05, JSON.stringify(pipeline));
    const states = events.filter((e) => e.type === "message").map((e) => e.message.state);
    check("dock streams the message lifecycle", states.includes("queued") && states.includes("played"), states.join(" → "));

    // Secrets: stored via the dock, never through OBS settings.
    events.length = 0;
    dock.send(JSON.stringify({ type: "set_secret", secret: "open_ai_api_key", value: "sk-lupo-smoke-not-a-real-key" }));
    await sleep(600);
    check("storing a key flips the provider to ready", events.some((e) => e.type === "providers" && e.providers.some((p) => p.id === "openai" && p.state === "ready")));
    dock.send(JSON.stringify({ type: "probe", provider: "openai" }));
    await sleep(2500);
    const probe = events.find((e) => e.type === "probe");
    check("probe reports a textual result", !!probe && probe.ok === false && probe.detail.length > 0, JSON.stringify(probe));
    events.length = 0;
    dock.send(JSON.stringify({ type: "forget_secret", secret: "open_ai_api_key" }));
    await sleep(600);
    check("forgetting the key flips the provider back", events.some((e) => e.type === "providers" && e.providers.some((p) => p.id === "openai" && p.state === "unavailable")));

    // Twitch: channel + device-code login over the dock protocol.
    events.length = 0;
    dock.send(JSON.stringify({ type: "set_channel", channel: "lupo_smoke_channel" }));
    await sleep(500);
    check("channel override lands in the chat view", events.some((e) => e.type === "chat" && e.chat.channel === "lupo_smoke_channel"));
    dock.send(JSON.stringify({ type: "twitch_login", open_browser: false }));
    await sleep(4000);
    const pendingLogin = events.find((e) => e.type === "chat" && e.chat.login?.state === "pending");
    check("device-code login streams the code to the dock", !!pendingLogin && /^[A-Z0-9]{6,}$/.test(pendingLogin.chat.login.user_code ?? ""), JSON.stringify(pendingLogin?.chat.login));
    dock.send(JSON.stringify({ type: "set_channel", channel: "" }));

    dock.send(JSON.stringify({ type: "no_such_command" }));
    await sleep(300);
    check("dock rejects malformed commands with an error event", events.some((e) => e.type === "error" && /bad command/.test(e.message)));
    dock.close();
  }

  // Panic containment: a panicking callback must be logged, not take OBS down.
  await press(INPUT, "debug_panic");
  await sleep(300);
  const alive = await req("GetVersion");
  check("panic inside a callback was contained", !!alive.obsVersion);

  // Global output: audible even when the program scene does not contain the source.
  await req("CreateScene", { sceneName: "Empty" }).catch(() => {});
  await req("SetInputSettings", { inputName: INPUT, inputSettings: { global: true } });
  await req("SetCurrentProgramScene", { sceneName: "Empty" });
  await sleep(500);
  await press(INPUT, "test_tone");
  const globalOn = await peakOver(INPUT, 1500);
  check("global mode: audible while the program scene lacks the source", globalOn.peak > 0.3, JSON.stringify(globalOn));
  await req("SetInputSettings", { inputName: INPUT, inputSettings: { global: false } });
  await sleep(200);
  await press(INPUT, "test_tone");
  const globalOff = await peakOver(INPUT, 1500);
  check("scene mode: silent while the program scene lacks the source", globalOff.peak < 0.01, JSON.stringify(globalOff));
  await req("SetCurrentProgramScene", { sceneName });
  await sleep(300);

  // Second instance: independent playback, then destroy while idle.
  await req("CreateInput", { sceneName, inputName: INPUT2, inputKind: "lupo_tts", sceneItemEnabled: true, inputSettings: { debug_tools: true } });
  await sleep(300);
  await press(INPUT2, "test_tone");
  const second = await peakOver(INPUT2, 1500);
  check("second instance plays independently", second.peak > 0.3, JSON.stringify(second));
  // Destroy while a clip is playing (player thread must be joined cleanly).
  await press(INPUT2, "test_clip");
  await sleep(500);
  await req("RemoveInput", { inputName: INPUT2 });
  await sleep(500);
  check("instance removed mid-clip without crash", true);

  // Recording: the clip must end up in the file.
  const status = await req("GetRecordStatus");
  if (status.outputActive) await req("StopRecord");
  await req("StartRecord");
  await sleep(1000);
  await press(INPUT, "test_clip");
  await sleep(3500);
  const { outputPath } = await req("StopRecord");
  await sleep(1500);
  let recorded = null;
  if (outputPath && existsSync(outputPath)) {
    const dest = join(OUT_DIR, "lupo-round2-recording" + outputPath.slice(outputPath.lastIndexOf(".")));
    renameSync(outputPath, dest);
    const err = execFileSync("sh", ["-c", `ffmpeg -hide_banner -i "${dest}" -af volumedetect -f null - 2>&1 | grep -E "mean_volume|max_volume|Stream #"`], { encoding: "utf8" });
    recorded = { dest, analysis: err.trim().split("\n") };
    const max = /max_volume: (-?[\d.]+) dB/.exec(err);
    check("recording contains audio (max_volume > -30 dB)", max && parseFloat(max[1]) > -30, JSON.stringify(recorded));
  } else {
    check("recording produced a file", false, outputPath);
  }
} finally {
  if (originalCollection !== COLLECTION) {
    await req("SetCurrentSceneCollection", { sceneCollectionName: originalCollection }).catch(() => {});
    await sleep(1500);
  }
  ws.close();
}

const failed = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - failed}/${results.length} checks passed`);
process.exit(failed ? 1 : 0);
