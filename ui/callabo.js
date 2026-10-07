// Callabo settings and per-upload dialog. A saved PAT never returns to JS.
(function (root) {
  function createCallaboUI({ $, invoke, t, language = () => "en", keyOf = (folder, base) => `${folder ?? ""}::${base}`, showError, showSettings, saved, uploads, refresh, onAuthChange = () => {}, onUploadError = () => {}, onUploadStart = () => {} }) {
    let workspaces = [];
    let defaultWorkspace = null;
    let connected = false;
    let hasCredential = false;
    let recording = null;
    let data = null;
    let generation = 0;
    let loading = false;
    let submitting = false;
    const dialog = $("callabo-upload-dialog");
    const cleanPreferences = () => ({ team_ids: [], label_ids: [], accessible_team_ids: [], accessible_user_ids: [], transcribe_language: "default" });
    const fail = (e) => showError(t("callabo.error"), String(e));

    function tokenStatus() {
      $("callabo-token-status").textContent = t(hasCredential ? "callabo.tokenStored" : "callabo.tokenMissing");
      $("callabo-forget").disabled = !hasCredential;
    }
    function setDefaultWorkspace(workspace) { defaultWorkspace = workspace; }
    async function saveDefaultWorkspace() {
      const workspace = $("callabo-workspace").value || null;
      await invoke("set_callabo_workspace", { workspace });
      defaultWorkspace = workspace;
      saved("callabo-saved");
    }
    async function connect() {
      const button = $("callabo-connect");
      const token = $("callabo-token").value.trim();
      button.disabled = true;
      button.textContent = t("callabo.connecting");
      $("callabo-forget").disabled = true;
      connected = false;
      $("callabo-workspace").disabled = true;
      try {
        const result = await invoke("callabo_workspaces", { token: token || null });
        // Do not bind results from a replaced input to a different account.
        if (token !== $("callabo-token").value.trim()) return;
        workspaces = result;
        hasCredential = true;
        onAuthChange(true);
        $("callabo-token").value = "";
        tokenStatus();
        const select = $("callabo-workspace");
        select.replaceChildren(new Option(t("callabo.choose"), ""));
        for (const workspace of workspaces) select.append(new Option(workspace.name, workspace.slug));
        select.value = workspaces.some((w) => w.slug === defaultWorkspace)
          ? defaultWorkspace : workspaces.length === 1 ? workspaces[0].slug : "";
        select.disabled = false;
        connected = true;
        if (select.value && select.value !== defaultWorkspace) await saveDefaultWorkspace();
      } catch (e) { fail(e); }
      finally { button.disabled = false; button.textContent = t("callabo.connect"); tokenStatus(); }
    }
    async function initialize(workspace) {
      defaultWorkspace = workspace;
      hasCredential = await invoke("callabo_auth_status");
      onAuthChange(hasCredential);
      tokenStatus();
      if (hasCredential) await connect();
    }

    function selectedIds(id) { return Array.from($(id).selectedOptions, (option) => Number(option.value)); }
    function setChoices(id, items, ids) {
      const select = $(id);
      select.replaceChildren();
      for (const item of items) {
        const option = new Option(item.name, String(item.id));
        option.selected = ids.includes(item.id);
        select.append(option);
      }
      // Keep stale selections visible until the user explicitly clears them.
      for (const id of ids.filter((id) => !items.some((item) => item.id === id))) {
        const option = new Option(`${t("callabo.unavailable")} #${id}`, String(id));
        option.selected = true;
        option.dataset.unavailable = "true";
        select.append(option);
      }
    }
    function buttonState() { $("callabo-upload-submit").disabled = loading || submitting || !recording; }
    function templateInfo() {
      const ids = selectedIds("callabo-upload-teams");
      const teams = (data?.teams ?? []).filter((team) => ids.includes(team.id));
      $("callabo-upload-template-info").textContent = teams.map((team) =>
        `${team.name}: ${team.request_insight_extract_type ?? "inherit"}` +
        (team.default_custom_insight_template_id ? ` · #${team.default_custom_insight_template_id}` : "")
      ).join("\n");
    }
    async function loadWorkspace() {
      const epoch = ++generation;
      const workspace = $("callabo-upload-workspace").value;
      loading = true;
      data = null;
      buttonState();
      $("callabo-upload-error").textContent = t("callabo.loadingOptions");
      $("callabo-upload-options").disabled = true;
      // The defaults requested by the user apply on every open/workspace change.
      $("callabo-upload-title").value = "";
      $("callabo-upload-scope").value = "workspace";
      for (const id of ["callabo-upload-teams", "callabo-upload-labels", "callabo-upload-access-teams"]) $(id).replaceChildren();
      $("callabo-upload-access-users").value = "";
      try {
        const result = await invoke("callabo_dialog_data", { workspace });
        if (epoch !== generation || !dialog.open) return;
        data = result;
        const p = { ...cleanPreferences(), ...result.preferences };
        setChoices("callabo-upload-teams", result.teams, p.team_ids);
        setChoices("callabo-upload-labels", result.labels, p.label_ids);
        setChoices("callabo-upload-access-teams", result.teams, p.accessible_team_ids);
        $("callabo-upload-access-users").value = p.accessible_user_ids.join(", ");
        $("callabo-upload-language").value = p.transcribe_language;
        $("callabo-upload-options").disabled = false;
        $("callabo-upload-error").textContent = (result.warnings ?? []).join("\n");
        templateInfo();
      } catch (e) {
        if (epoch === generation && dialog.open) $("callabo-upload-error").textContent = String(e);
      } finally {
        if (epoch === generation) { loading = false; buttonState(); }
      }
    }
    async function open(item) {
      if (!connected || $("callabo-token").value.trim()) {
        showSettings();
        fail(t("callabo.configure"));
        $("callabo-token").focus();
        return;
      }
      if (dialog.open || uploads.has(keyOf(item.folder, item.name))) return;
      recording = item;
      $("callabo-upload-recording").textContent = item.name;
      const select = $("callabo-upload-workspace");
      select.replaceChildren();
      for (const workspace of workspaces) select.append(new Option(workspace.name, workspace.slug));
      select.value = workspaces.some((w) => w.slug === defaultWorkspace) ? defaultWorkspace : workspaces[0]?.slug ?? "";
      const languages = $("callabo-upload-language");
      languages.replaceChildren();
      const names = new Intl.DisplayNames([language()], { type: "language" });
      for (const code of ["default", "detect", "multi", "ko", "en", "ja", "zh", "es", "fr", "de", "ru", "pt", "it", "ar", "hi", "id", "vi", "th", "nl"]) {
        languages.append(new Option(["default", "detect", "multi"].includes(code) ? t(`callabo.language_${code}`) : names.of(code), code));
      }
      dialog.showModal();
      await loadWorkspace();
    }
    function readOptions() {
      for (const id of ["callabo-upload-teams", "callabo-upload-labels", "callabo-upload-access-teams"]) {
        if (Array.from($(id).selectedOptions).some((option) => option.dataset.unavailable)) throw t("callabo.clearUnavailable");
      }
      const userText = $("callabo-upload-access-users").value.trim();
      const users = userText ? userText.split(/[\s,]+/).map((part) => {
        if (!/^\d+$/.test(part) || !Number.isSafeInteger(Number(part)) || Number(part) <= 0) throw t("callabo.invalidIds");
        return Number(part);
      }) : [];
      const options = {
        title: $("callabo-upload-title").value.trim() || null,
        scope: $("callabo-upload-scope").value,
        team_ids: selectedIds("callabo-upload-teams"),
        label_ids: selectedIds("callabo-upload-labels"),
        accessible_team_ids: selectedIds("callabo-upload-access-teams"),
        accessible_user_ids: users,
        transcribe_language: $("callabo-upload-language").value,
      };
      if (options.scope === "team" && !options.team_ids.length) throw t("callabo.teamRequired");
      return options;
    }
    async function submit(event) {
      event?.preventDefault();
      if (loading || submitting || !data || !recording) return;
      let options;
      try { options = readOptions(); }
      catch (e) { $("callabo-upload-error").textContent = String(e); return; }
      const item = recording;
      const key = keyOf(item.folder, item.name);
      if (uploads.has(key)) return;
      const workspace = $("callabo-upload-workspace").value;
      submitting = true;
      onUploadStart(item);
      uploads.set(key, "creating");
      refresh();
      dialog.close();
      try {
        await invoke("callabo_upload", { folder: item.folder ?? null, base: item.name, workspace,
          workspaceName: workspaces.find((w) => w.slug === workspace)?.name ?? null, options });
      } catch (e) { onUploadError(item, e); fail(`${item.name}: ${e}`); }
      finally { submitting = false; uploads.delete(key); refresh(); buttonState(); }
    }

    $("callabo-token").addEventListener("input", () => { connected = false; $("callabo-workspace").disabled = true; });
    $("callabo-connect").addEventListener("click", connect);
    $("callabo-workspace").addEventListener("change", () => saveDefaultWorkspace().catch(fail));
    $("callabo-token-help").addEventListener("click", () => invoke("open_url", { url: "https://callabo.ai/blog/personal-access-token" }).catch(fail));
    $("callabo-forget").addEventListener("click", async () => {
      try {
        await invoke("callabo_forget_token");
        $("callabo-token").value = "";
        hasCredential = false; connected = false; workspaces = [];
        onAuthChange(false);
        $("callabo-workspace").disabled = true;
        tokenStatus();
      } catch (e) { fail(e); }
    });
    $("callabo-upload-workspace").addEventListener("change", loadWorkspace);
    $("callabo-upload-teams").addEventListener("change", templateInfo);
    $("callabo-upload-form").addEventListener("submit", submit);
    $("callabo-upload-cancel").addEventListener("click", () => dialog.close());
    $("callabo-upload-reset").addEventListener("click", () => {
      for (const id of ["callabo-upload-teams", "callabo-upload-labels", "callabo-upload-access-teams"]) {
        for (const option of $(id).options) option.selected = false;
      }
      $("callabo-upload-access-users").value = "";
      templateInfo();
    });
    dialog.addEventListener("close", () => { ++generation; recording = null; loading = false; data = null; });
    return { initialize, setDefaultWorkspace, open, connect, submit };
  }
  if (typeof module !== "undefined" && module.exports) module.exports = { createCallaboUI };
  else root.createCallaboUI = createCallaboUI;
})(typeof window !== "undefined" ? window : globalThis);
