// Persistent data comes from the Rust receipt; this module owns presentation and
// bounded foreground polling only. No credentials or remote HTML enter the DOM.
(function (root) {
  const markdown = typeof module !== 'undefined' && module.exports ? require('./summary-markdown.js') : root.summaryMarkdown;
  function createCallaboRecords({ invoke, t, refresh, upload, uploads, keyOf,
    doc = document, storage = root.localStorage, now = () => Date.now() }) {
    let recordings = [], enabled = false;
    const running = new Set();
    const next = new Map(), failures = new Map(), uploadErrors = new Map();
    const choices = new Map(), expanded = new Set();
    const key = (item) => keyOf(item.folder, item.name);
    // Receipt UUID survives local rename/move and app restart.
    const choiceKey = (item) => `callabo.primary.${item.callabo_links?.[0]?.uuid ?? key(item)}`;
    function selected(item) {
      const links = item.callabo_links ?? [];
      let chosen = choices.get(choiceKey(item));
      if (!chosen) { try { chosen = storage?.getItem(choiceKey(item)); } catch {} }
      return links.find(link => link.uuid === chosen) ?? links.at(-1);
    }
    function choose(item, uuid) {
      choices.set(choiceKey(item), uuid);
      try { storage?.setItem(choiceKey(item), uuid); } catch {}
      refresh(); tick();
    }
    function title(item) { return selected(item)?.remote?.title || null; }
    function state(item) {
      const stage = uploads.get(key(item));
      if (stage) return stage;
      if (uploadErrors.has(key(item))) return 'uploadFailed';
      const link = selected(item);
      if (!link) return 'notUploaded';
      if (failures.has(link.uuid) || link.remote?.error) return 'unavailable';
      return link.remote?.status ?? 'checking';
    }
    function signature(item) { return [selected(item)?.uuid, state(item), failures.get(selected(item)?.uuid)]; }
    async function sync(item, link, force = false) {
      if (running.has(link.uuid) || uploads.has(key(item)) || item.recording_now) return;
      if (!force && (!enabled || doc.hidden || now() < (next.get(link.uuid) ?? 0))) return;
      running.add(link.uuid);
      next.set(link.uuid, now() + 60_000);
      try {
        const result = await invoke('callabo_sync_record', { folder: item.folder ?? null, base: item.name, uuid: link.uuid });
        if (result?.uuid !== link.uuid) throw new Error(t('callabo.unavailable'));
        failures.delete(link.uuid);
        // Keep the latest result in memory until list_recordings reloads it.
        Object.assign(link, result);
        const retry = result.remote?.error ? 120_000 : result.remote?.status === 'processing' ? 15_000 : 300_000;
        next.set(link.uuid, now() + retry);
      } catch (error) {
        failures.set(link.uuid, String(error));
        next.set(link.uuid, now() + 120_000);
      } finally {
        running.delete(link.uuid);
        await refresh();
      }
    }
    async function tick() {
      if (running.size || !enabled || doc.hidden) return;
      const due = recordings.map(item => ({item, link:selected(item)}))
        .filter(({item,link}) => link && !item.recording_now && !uploads.has(key(item)) && now() >= (next.get(link.uuid) ?? 0))
        .sort((a,b) => (next.get(a.link.uuid) ?? 0) - (next.get(b.link.uuid) ?? 0));
      // Oldest due first prevents a large list from starving its older records.
      if (due.length) await sync(due[0].item, due[0].link);
    }
    function setRecordings(items) { recordings = items; }
    function setEnabled(value) { enabled = value; }
    function uploadFailed(item, error) { uploadErrors.set(key(item), String(error)); }
    function uploadStarted(item) { uploadErrors.delete(key(item)); }
    function startUpload(item) { uploadErrors.delete(key(item)); return upload(item); }
    function el(tag, className, text) {
      const node = doc.createElement(tag); node.className = className;
      if (text != null) node.textContent = text;
      return node;
    }
    function mark() {
      const img = el('img', 'callabo-mark'); img.src = 'assets/callabo-mark.svg'; img.alt = ''; img.width = 24; img.height = 24;
      return img;
    }
    function button(text, action, disabled = false) {
      const node = el('button', 'sec callabo-action', text); node.type = 'button'; node.disabled = disabled;
      node.addEventListener('click', event => { event.stopPropagation(); action(); }); return node;
    }
    function render(item) {
      if (item.recording_now || !(item.mic || item.system)) return null;
      const panel = el('section', 'callabo-record'); panel.setAttribute('aria-label', 'Callabo');
      const link = selected(item), current = state(item), busy = uploads.has(key(item));
      if (!link && !busy && !uploadErrors.has(key(item))) {
        panel.classList.add('callabo-empty');
        const send = button(t('callabo.upload'), () => startUpload(item)); send.prepend(mark()); panel.append(send); return panel;
      }
      const header = el('div', 'callabo-record-header');
      const identity = el('div', 'callabo-identity');
      const caption = el('div', 'callabo-caption');
      const status = el('div', 'callabo-status', t(`callabo.${current === 'unavailable' ? 'statusUnavailable' : current}`)); status.setAttribute('role', 'status');
      caption.append(status);
      if (link && !busy) caption.append(el('div', 'callabo-context', link.workspace_name));
      identity.append(mark(), caption); header.append(identity); panel.append(header);
      if (busy) {
        header.append(button(t('callabo.uploadBusy'), () => {}, true));
        const progress = el('div', 'prog'); progress.setAttribute('role', 'progressbar');
        progress.setAttribute('aria-label', t(`callabo.${current}`)); progress.append(el('i', '')); panel.append(progress);
      } else {
        if (link) header.append(button(t('callabo.open'), () => invoke('open_url', { url: link.url }).catch(error => {
          failures.set(link.uuid, String(error)); refresh();
        })));
        if (current === 'uploadFailed') {
          panel.append(el('p', 'callabo-context', uploadErrors.get(key(item))));
          panel.append(button(t('callabo.retryUpload'), () => startUpload(item)));
        }
      }
      if (link && !busy) {
        if ((item.callabo_links ?? []).length > 1) {
          const select = el('select', 'callabo-link-select'); select.setAttribute('aria-label', t('callabo.primaryLink'));
          for (const other of item.callabo_links) {
            const option = el('option', '', `${other.workspace_name} · #${other.record_id}`); option.value = other.uuid;
            option.selected = other.uuid === link.uuid; select.append(option);
          }
          select.addEventListener('change', () => choose(item, select.value)); panel.append(select);
        }
        if (current === 'processing' || current === 'checking') panel.append(el('p', 'callabo-context', t(`callabo.${current}Hint`)));
        const text = link.remote?.summary;
        if (text) {
          const oneLine = markdown.oneLineSummary(text);
          const preview = oneLine ? el('p', 'callabo-summary-preview', oneLine) : null;
          const detail = el('details', 'callabo-summary'); detail.open = expanded.has(link.uuid);
          detail.append(el('summary', '', t('callabo.summaryMore')), markdown.renderMarkdown(text, {
            doc, openLink: url => invoke('open_url', { url }).catch(error => {
              failures.set(link.uuid, String(error)); refresh();
            }),
          }));
          detail.addEventListener('toggle', () => {
            if (detail.open) expanded.add(link.uuid); else expanded.delete(link.uuid);
            if (preview) preview.hidden = detail.open;
          });
          if (preview) { preview.hidden = detail.open; panel.append(preview); }
          panel.append(detail);
        } else if (current === 'ready') panel.append(el('p', 'callabo-context', t('callabo.noSummary')));
        const footer = el('div', 'callabo-record-footer');
        if (link.remote?.synced_at) {
          const date = new Date(link.remote.synced_at * 1000).toLocaleString();
          footer.append(el('span', 'callabo-context', t('callabo.syncedAt', { date })));
        }
        const retry = button(t(current === 'unavailable' ? 'callabo.checkAgain' : 'callabo.refresh'), async () => {
          retry.disabled = true;
          try { await sync(item, link, true); } finally { retry.disabled = false; }
        });
        footer.append(retry); panel.append(footer);
        if (current === 'unavailable') panel.append(el('p', 'callabo-context callabo-error', failures.get(link.uuid) || link.remote?.error));
      }
      return panel;
    }
    return { selected, choose, title, state, signature, render, setRecordings, setEnabled, tick, sync, uploadFailed, uploadStarted };
  }
  if (typeof module !== 'undefined' && module.exports) module.exports = { createCallaboRecords };
  else root.createCallaboRecords = createCallaboRecords;
})(typeof window !== 'undefined' ? window : globalThis);
