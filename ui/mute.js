// Only acknowledged audio-thread state is shown; revisions reject stale IPC.
(function (root) {
  function createMuteUI({ $, invoke, t, showError, onChange = () => {} }) {
    let current = { mic: false, system: false, revision: -1 };
    let recording = false, unavailable = false, fatal = false;
    const pending = new Set();
    const buttons = { mic: $("mute-mic"), system: $("mute-system") };

    function render() {
      for (const source of ["mic", "system"]) {
        const button = buttons[source];
        const muted = current[source];
        button.disabled = !recording || fatal || pending.has(source) || (source === "system" && unavailable);
        button.setAttribute("aria-pressed", String(muted));
        button.setAttribute("aria-busy", String(pending.has(source)));
        const label = t(source === "mic" ? "mute.mic" : "mute.system");
        button.setAttribute("aria-label", label);
        button.title = label + ": " + t(muted ? "mute.off" : "mute.on");
      }
    }

    function apply(value) {
      if (!value || value.revision < current.revision) return;
      const changed = value.mic !== current.mic || value.system !== current.system;
      current = { ...value };
      render();
      if (changed) onChange();
    }

    async function toggle(source) {
      if (buttons[source].disabled) return;
      pending.add(source);
      render();
      try {
        apply(await invoke("set_mute", { source, muted: !current[source] }));
      } catch (error) {
        // A disk error can occur after mute took effect. Recover actual state.
        try { apply((await invoke("get_state")).mute); } catch (_) {}
        showError(t("mute.error"), error);
      } finally {
        pending.delete(source);
        render();
      }
    }

    for (const source of ["mic", "system"]) {
      buttons[source].addEventListener("click", () => toggle(source));
    }
    return {
      apply, render,
      setRecording(value) { recording = value; render(); },
      setUnavailable(value) { unavailable = value; render(); },
      setFatal() { fatal = true; render(); },
      muted(source) { return current[source]; },
      accepts(value) { return !value || value.revision >= current.revision; },
    };
  }
  root.createMuteUI = createMuteUI;
  if (typeof module !== "undefined") module.exports = { createMuteUI };
})(typeof window !== "undefined" ? window : globalThis);
