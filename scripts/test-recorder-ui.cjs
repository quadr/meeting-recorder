// Structural regressions for the Callabo-only recorder UI. No live credentials.
const { readFileSync, existsSync } = require("node:fs");
const { join } = require("node:path");
const assert = require("node:assert/strict");
const { test } = require("node:test");
const root = join(__dirname, "..");
const source = readFileSync(join(root, "ui/main.js"), "utf8");
const html = readFileSync(join(root, "ui/index.html"), "utf8");
const strings = JSON.parse(readFileSync(join(root, "ui/i18n/strings.json"), "utf8"));

test("main UI only addresses existing unique HTML elements", () => {
  for (const id of new Set(Array.from(source.matchAll(/\$\("([\w-]+)"\)/g), (m) => m[1]))) {
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
  assert(source.includes('i18n.t("callabo.history"'));
  assert(source.includes('invoke("rename_recording"'));
  assert(source.includes('invoke("delete_recording"'));
});
