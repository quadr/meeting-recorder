// Controller smoke tests: fake DOM and IPC, no real accounts or uploads.
const { readFileSync } = require("node:fs");
const { join } = require("node:path");
const assert = require("node:assert/strict");
const { test } = require("node:test");
const { createCallaboUI } = require("../ui/callabo.js");
global.Option = class {
  constructor(text, value) { Object.assign(this, { text, value, selected: false, dataset: {} }); }
};
class Element {
  value = ""; disabled = false; listeners = {}; options = []; open = false;
  addEventListener(type, cb) { this.listeners[type] = cb; }
  replaceChildren(...options) { this.options = options; this.value = options[0]?.value ?? ""; }
  append(option) { this.options.push(option); if (!this.value) this.value = option.value; }
  get selectedOptions() { return this.options.filter((o) => o.selected); }
  focus() { this.focused = true; }
  showModal() { this.open = true; }
  close() { this.open = false; this.listeners.close?.(); }
}
const clone = (value) => JSON.parse(JSON.stringify(value));
const workspaces = [{ slug: "a", name: "A" }, { slug: "b", name: "B" }];
const profiles = {
  a: { teams: [{ id: 11, name: "A team", request_insight_extract_type: "custom", default_custom_insight_template_id: 99 }], labels: [{ id: 101, name: "A label" }], preferences: { team_ids: [11], label_ids: [101], transcribe_language: "ko" }, warnings: [] },
  b: { teams: [{ id: 22, name: "B team" }], labels: [], preferences: { team_ids: [22], transcribe_language: "en" }, warnings: [] },
};
const recording = { folder: "2026-10", name: "2026-10-01_11-13_meeting" };
function harness(override = async () => undefined) {
  const elements = new Map(), calls = [], errors = [], screens = [], uploads = new Map();
  const $ = (id) => { if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); };
  const invoke = async (command, args) => {
    calls.push({ command, args });
    const custom = await override(command, args);
    if (custom !== undefined) return custom;
    if (command === "callabo_auth_status") return true;
    if (command === "callabo_workspaces") return clone(workspaces);
    if (command === "callabo_dialog_data") return clone(profiles[args.workspace]);
    return null;
  };
  const ui = createCallaboUI({ $, invoke, t: (key) => key, showError: (title, error) => errors.push({ title, error }),
    showSettings: () => screens.push("settings"), saved: () => {}, uploads, refresh: () => {} });
  return { ui, $, calls, errors, screens, uploads };
}
const ids = (h, id = "callabo-upload-teams") => h.$(id).selectedOptions.map((o) => Number(o.value));
const sent = (h) => h.calls.filter((c) => c.command === "callabo_upload");

test("missing credential redirects to settings without uploading", async () => {
  const h = harness(async (cmd) => cmd === "callabo_auth_status" ? false : undefined);
  await h.ui.initialize("a"); await h.ui.open(recording);
  assert.deepEqual(h.screens, ["settings"]); assert.equal(sent(h).length, 0);
});
test("saved PAT is reused without returning it to JS", async () => {
  const h = harness(); await h.ui.initialize("a");
  assert.equal(h.$("callabo-token").value, "");
  assert.equal(h.$("callabo-token-status").textContent, "callabo.tokenStored");
  assert.deepEqual(h.calls.find((c) => c.command === "callabo_workspaces").args, { token: null });
  assert.equal(h.$("callabo-workspace").value, "a");
});
test("new PAT is cleared after secure save and never saved in config", async () => {
  const h = harness(); h.$("callabo-token").value = "pat_fake"; await h.ui.connect();
  assert.equal(h.$("callabo-token").value, "");
  assert.deepEqual(h.calls.find((c) => c.command === "callabo_workspaces").args, { token: "pat_fake" });
  assert(!h.calls.some((c) => c.command === "set_callabo_workspace" && "token" in c.args));
});
test("opening dialog does not upload: blank title, workspace scope, previous team", async () => {
  const h = harness(); await h.ui.initialize("a"); await h.ui.open(recording);
  assert.equal(h.$("callabo-upload-dialog").open, true);
  assert.equal(h.$("callabo-upload-title").value, ""); assert.equal(h.$("callabo-upload-scope").value, "workspace");
  assert.deepEqual(ids(h), [11]); assert(h.$("callabo-upload-template-info").textContent.includes("#99"));
  assert.equal(sent(h).length, 0);
});
test("workspace switch restores isolated preferences and resets title/scope", async () => {
  const h = harness(); await h.ui.initialize("a"); await h.ui.open(recording);
  h.$("callabo-upload-title").value = "A title"; h.$("callabo-upload-scope").value = "private";
  h.$("callabo-upload-workspace").value = "b"; await h.$("callabo-upload-workspace").listeners.change();
  assert.deepEqual(ids(h), [22]); assert.equal(h.$("callabo-upload-language").value, "en");
  assert.equal(h.$("callabo-upload-title").value, ""); assert.equal(h.$("callabo-upload-scope").value, "workspace");
  h.$("callabo-upload-workspace").value = "a"; await h.$("callabo-upload-workspace").listeners.change();
  assert.deepEqual(ids(h), [11]); assert.equal(h.$("callabo-upload-language").value, "ko");
  assert(!h.calls.some((c) => c.command === "set_callabo_workspace"));
});
test("cancel does not upload or save choices", async () => {
  const h = harness(); await h.ui.initialize("a"); await h.ui.open(recording);
  h.$("callabo-upload-cancel").listeners.click(); await h.ui.submit();
  assert.equal(sent(h).length, 0); assert.equal(h.$("callabo-upload-dialog").open, false);
});
test("submit passes dialog choices without PAT and prevents duplicate submission", async () => {
  let resolve;
  const h = harness(async (cmd) => cmd === "callabo_upload" ? new Promise((done) => { resolve = done; }) : undefined);
  await h.ui.initialize("a"); await h.ui.open(recording);
  const pending = h.ui.submit(); await h.ui.submit();
  assert.equal(sent(h).length, 1);
  assert.deepEqual(sent(h)[0].args, { folder: recording.folder, base: recording.name, workspace: "a", workspaceName: "A", options: {
    title: null, scope: "workspace", team_ids: [11], label_ids: [101], accessible_team_ids: [], accessible_user_ids: [], transcribe_language: "ko",
  } });
  assert.equal(h.uploads.get(`${recording.folder}::${recording.name}`), "creating");
  assert.equal(h.uploads.size, 1); resolve({ record_id: 42 }); await pending; assert.equal(h.uploads.size, 0);
});
test("custom title and additional access come from dialog only", async () => {
  const h = harness(); await h.ui.initialize("a"); await h.ui.open(recording);
  h.$("callabo-upload-title").value = "  Weekly sync  "; h.$("callabo-upload-scope").value = "team";
  h.$("callabo-upload-access-users").value = "12, 34"; await h.ui.submit();
  const options = sent(h)[0].args.options;
  assert.equal(options.title, "Weekly sync"); assert.equal(options.scope, "team"); assert.deepEqual(options.accessible_user_ids, [12, 34]);
});

test("uploaded recording can be uploaded to another workspace and uploaded again", async () => {
  const h = harness(); await h.ui.initialize("a");
  const item = { ...recording, callabo_workspaces: [{ slug: "a", name: "A" }] };
  await h.ui.open(item);
  h.$("callabo-upload-workspace").value = "b";
  await h.$("callabo-upload-workspace").listeners.change(); await h.ui.submit();
  assert.equal(sent(h)[0].args.workspace, "b"); assert.equal(sent(h)[0].args.workspaceName, "B");
  await h.ui.open(item); await h.ui.submit();
  assert.equal(sent(h).length, 2); assert.equal(sent(h)[1].args.workspace, "a");
  assert.equal(h.errors.length, 0); assert.equal(h.uploads.size, 0);
});

test("recording menu remains available after upload and displays workspace history", () => {
  const source = readFileSync(join(__dirname, "../ui/main.js"), "utf8");
  assert(!source.includes("callabo_record_id"));
  assert(source.includes('i18n.t("callabo.history"'));
  const condition = source.split("\n").find((line) => line.includes("if ((запись.mic || запись.system)"));
  assert(condition && !condition.includes("callabo_workspaces"));
});
test("stale selections must be explicitly cleared", async () => {
  const h = harness(async (cmd) => cmd === "callabo_dialog_data" ? { teams: [], labels: [], preferences: { team_ids: [999] }, warnings: [] } : undefined);
  await h.ui.initialize("a"); await h.ui.open(recording); assert.deepEqual(ids(h), [999]); await h.ui.submit();
  assert.equal(h.$("callabo-upload-error").textContent, "callabo.clearUnavailable"); assert.equal(sent(h).length, 0);
  h.$("callabo-upload-reset").listeners.click(); await h.ui.submit(); assert.deepEqual(sent(h)[0].args.options.team_ids, []);
});
test("out-of-order responses cannot leak teams across workspaces", async () => {
  let resolveA;
  const h = harness(async (cmd, args) => cmd === "callabo_dialog_data" && args.workspace === "a" ? new Promise((done) => { resolveA = done; }) : undefined);
  await h.ui.initialize("a"); const first = h.ui.open(recording); await new Promise((done) => setImmediate(done));
  h.$("callabo-upload-workspace").value = "b"; await h.$("callabo-upload-workspace").listeners.change();
  resolveA(clone(profiles.a)); await first; assert.deepEqual(ids(h), [22]);
});
test("failed upload clears pending state and identifies recording", async () => {
  const h = harness(async (cmd) => { if (cmd === "callabo_upload") throw "HTTP 403"; });
  await h.ui.initialize("a"); await h.ui.open(recording); await h.ui.submit();
  assert.equal(h.uploads.size, 0); assert(h.errors.at(-1).error.includes(recording.name));
});
test("forgetting PAT disables uploads", async () => {
  const h = harness(); await h.ui.initialize("a"); await h.$("callabo-forget").listeners.click(); await h.ui.open(recording);
  assert(h.calls.some((c) => c.command === "callabo_forget_token"));
  assert.equal(h.$("callabo-token-status").textContent, "callabo.tokenMissing"); assert.deepEqual(h.screens, ["settings"]);
});
test("HTML contains unique controller IDs and localized strings", () => {
  const source = readFileSync(join(__dirname, "../ui/callabo.js"), "utf8");
  const html = readFileSync(join(__dirname, "../ui/index.html"), "utf8");
  const strings = JSON.parse(readFileSync(join(__dirname, "../ui/i18n/strings.json"), "utf8"));
  for (const id of new Set(Array.from(source.matchAll(/\$\("([\w-]+)"\)/g), (m) => m[1]))) {
    assert.equal((html.match(new RegExp(`id="${id}"`, "g")) ?? []).length, 1, id);
  }
  for (const key of Array.from(source.matchAll(/t\("callabo\.([\w]+)"\)/g), (m) => m[1])) {
    assert.equal(typeof strings.callabo[key]?.en, "string", key); assert.equal(typeof strings.callabo[key]?.ru, "string", key);
  }
  assert(html.indexOf('src="callabo.js"') < html.indexOf('src="main.js"'));
});
