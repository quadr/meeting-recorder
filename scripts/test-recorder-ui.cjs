// Structural regressions for the Callabo-only recorder UI. No live credentials.
const { readFileSync, existsSync } = require("node:fs");
const { join } = require("node:path");
const assert = require("node:assert/strict");
const { test } = require("node:test");
const root = join(__dirname, "..");
const source = readFileSync(join(root, "ui/main.js"), "utf8");
const html = readFileSync(join(root, "ui/index.html"), "utf8");
const muteSource = readFileSync(join(root, "ui/mute.js"), "utf8");
const strings = JSON.parse(readFileSync(join(root, "ui/i18n/strings.json"), "utf8"));

test("main UI only addresses existing unique HTML elements", () => {
  for (const id of new Set(Array.from((source + muteSource).matchAll(/\$\("([\w-]+)"\)/g), (m) => m[1]))) {
    assert.equal((html.match(new RegExp(`id="${id}"`, "g")) ?? []).length, 1, id);
  }
});

test("static and runtime strings remain localized", () => {
  const keys = new Set(Array.from(source.matchAll(/i18n\.t(?:Both)?\("([\w.]+)"/g), (m) => m[1]));
  for (const match of html.matchAll(/data-i18n="([\w.]+)"/g)) keys.add(match[1]);
  for (const match of html.matchAll(/data-i18n-attr="([^"]+)"/g)) {
    for (const pair of match[1].split(";")) keys.add(pair.split(":")[1]);
  }
  for (const key of keys) {
    const [section, name] = key.split(".");
    assert.equal(typeof strings[section]?.[name]?.en, "string", key);
    assert.equal(typeof strings[section]?.[name]?.ru, "string", key);
  }
  for (const key of new Set(Array.from(muteSource.matchAll(/"(mute\.[\w]+)"/g), (m) => m[1]))) {
    assert.equal(typeof strings.mute[key.split(".")[1]]?.en, "string", key);
    assert.equal(typeof strings.mute[key.split(".")[1]]?.ru, "string", key);
  }
});

const { createMuteUI } = require("../ui/mute.js");
function muteHarness(invoke) {
  const elements = new Map(), errors = [];
  const $ = (id) => {
    if (!elements.has(id)) elements.set(id, {
      disabled: false, attributes: {}, listeners: {},
      setAttribute(key, value) { this.attributes[key] = value; },
      addEventListener(type, handler) { this.listeners[type] = handler; },
    });
    return elements.get(id);
  };
  const ui = createMuteUI({ $, invoke, t: (key) => key, showError: (...error) => errors.push(error) });
  ui.apply({ mic: false, system: false, revision: 0 });
  ui.setRecording(true);
  return { ui, $, errors };
}

test("mute buttons wait for audio-thread acknowledgement and remain independent", async () => {
  let resolve;
  const calls = [];
  const { ui, $ } = muteHarness((command, args) => {
    calls.push({ command, args });
    return new Promise((done) => { resolve = done; });
  });
  const pending = $("mute-mic").listeners.click();
  assert.equal($("mute-mic").disabled, true);
  assert.equal($("mute-system").disabled, false);
  assert.equal($("mute-mic").attributes["aria-pressed"], "false");
  await $("mute-mic").listeners.click();
  assert.equal(calls.length, 1, "double click must not enqueue a second command");
  assert.deepEqual(calls[0], { command: "set_mute", args: { source: "mic", muted: true } });
  resolve({ mic: true, system: false, revision: 1 });
  await pending;
  assert.equal(ui.muted("mic"), true);
  assert.equal(ui.muted("system"), false);
  assert.equal($("mute-mic").attributes["aria-pressed"], "true");
  assert.equal($("mute-mic").disabled, false);
});

test("mute revisions reject stale events and restore the state on window load", () => {
  const { ui, $ } = muteHarness(async () => {});
  ui.apply({ mic: true, system: true, revision: 9 });
  ui.apply({ mic: false, system: false, revision: 8 });
  assert.equal(ui.muted("mic"), true);
  assert.equal(ui.muted("system"), true);
  assert.equal(ui.accepts({ revision: 8 }), false);
  ui.apply({ mic: false, system: false, revision: 10 });
  ui.setRecording(false);
  assert.equal($("mute-mic").disabled, true);
  assert.equal($("mute-system").disabled, true);
  ui.setRecording(true);
  assert.equal($("mute-mic").attributes["aria-pressed"], "false");
});

test("mute failures recover acknowledged state and allow retry", async () => {
  const { ui, $, errors } = muteHarness(async (command) => {
    if (command === "set_mute") throw new Error("disk failure");
    return { mute: { mic: true, system: false, revision: 1 } };
  });
  await $("mute-mic").listeners.click();
  assert.equal(ui.muted("mic"), true, "mute may have taken effect before a write error");
  assert.equal($("mute-mic").disabled, false);
  assert.equal(errors.length, 1);
});

test("unavailable system audio and fatal capture disable the appropriate controls", () => {
  const { ui, $ } = muteHarness(async () => {});
  ui.setUnavailable(true);
  assert.equal($("mute-system").disabled, true);
  assert.equal($("mute-mic").disabled, false);
  ui.setFatal();
  assert.equal($("mute-mic").disabled, true);
});

test("transcript settings, actions, timers and IPC are gone", () => {
  assert(!/transcribe_recording|cancel_transcription|transcribe-progress|local_model_|set_audio_retention|stt-url|stt-key|transcribe-mode|tip-bubble|транскрипции|стадия_идёт/.test(source + html));
  for (const section of ["transcribe", "server", "local", "retention"]) assert(!(section in strings));
  for (const module of ["transcribe", "whisper_cpp", "local", "recording", "retention"]) {
    assert(!existsSync(join(root, `src-tauri/src/${module}.rs`)), module);
  }
});

test("recording and Callabo controls and workspace history stay available", () => {
  for (const id of ["rec-btn", "stop-btn", "mic", "check", "callabo-token", "callabo-upload-dialog", "callabo-upload-language"]) {
    assert(html.includes(`id="${id}"`), id);
  }
  assert(source.includes('listen("callabo-progress"'));
  assert(source.includes('callaboRecords.render(запись)'));
  assert(source.includes('invoke("rename_recording"'));
  assert(source.includes('invoke("delete_recording"'));
});
