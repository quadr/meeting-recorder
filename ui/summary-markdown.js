// Parse Markdown locally; construct only allowlisted DOM elements, never remote HTML.
(function (root) {
  const parser = typeof module !== 'undefined' && module.exports ? require('./vendor/marked.umd.js') : root.marked;
  const entities = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: '\u00a0' };
  function decode(text) {
    return String(text ?? '').replace(/&(#x[\da-f]+|#\d+|amp|lt|gt|quot|apos|nbsp);/gi, (raw, name) => {
      if (name[0] !== '#') return entities[name.toLowerCase()];
      const code = name[1].toLowerCase() === 'x' ? parseInt(name.slice(2), 16) : Number(name.slice(1));
      return code > 0 && code <= 0x10ffff && !(code >= 0xd800 && code <= 0xdfff) ? String.fromCodePoint(code) : '\ufffd';
    });
  }
  function plain(tokens) {
    return tokens.map(token => token.type === 'html' ? '' : token.tokens ? plain(token.tokens) : decode(token.text)).join('');
  }
  function oneLineSummary(source) {
    const tokens = parser.lexer(source);
    const index = tokens.findIndex(token => token.type === 'heading' &&
      /^(한\s*줄\s*요약|one[ -]line summary|tl;?dr)\s*[:：]?$/i.test(plain(token.tokens).trim()));
    if (index < 0) return null;
    const next = tokens.slice(index + 1).find(token => token.type !== 'space');
    if (next?.type !== 'paragraph') return null;
    return plain(next.tokens).replace(/\s+/g, ' ').trim() || null;
  }
  function renderMarkdown(source, { doc = document, openLink = () => {} } = {}) {
    const el = (tag, text) => {
      const node = doc.createElement(tag);
      if (text != null) node.textContent = text;
      return node;
    };
    function append(parent, tokens) {
      for (const token of tokens ?? []) {
        let node;
        switch (token.type) {
          case 'space': case 'def': continue;
          case 'heading': node = el(`h${token.depth}`); break;
          case 'paragraph': node = el('p'); break;
          case 'strong': case 'em': case 'del': case 'blockquote': node = el(token.type); break;
          case 'br': case 'hr': parent.append(el(token.type)); continue;
          case 'codespan': parent.append(el('code', token.text)); continue;
          case 'code': {
            const pre = el('pre'); pre.append(el('code', token.text)); parent.append(pre); continue;
          }
          case 'list': {
            node = el(token.ordered ? 'ol' : 'ul');
            if (token.ordered) node.setAttribute('start', String(token.start));
            for (const item of token.items) {
              const li = el('li'); append(li, item.tokens); node.append(li);
            }
            parent.append(node); continue;
          }
          case 'checkbox': {
            node = el('span', token.checked ? '☑ ' : '☐ ');
            node.className = 'callabo-task-marker'; parent.append(node); continue;
          }
          case 'table': {
            const wrap = el('div'); wrap.className = 'callabo-table-scroll';
            wrap.tabIndex = 0;
            const table = el('table'), head = el('thead'), body = el('tbody');
            const row = (cells, tag) => {
              const tr = el('tr');
              for (const cell of cells) {
                const td = el(tag); append(td, cell.tokens); tr.append(td);
              }
              return tr;
            };
            head.append(row(token.header, 'th'));
            for (const cells of token.rows) body.append(row(cells, 'td'));
            table.append(head, body); wrap.append(table); parent.append(wrap); continue;
          }
          case 'link': {
            const href = decode(token.href);
            if (/^(https?:\/\/|mailto:)/i.test(href)) {
              node = el('a'); node.setAttribute('href', href);
              node.addEventListener('click', event => { event.preventDefault(); event.stopPropagation(); openLink(href); });
            } else node = el('span');
            break;
          }
          // Remote images and HTML remain inert text, with no external requests.
          case 'html': parent.append(el('span', token.text)); continue;
          case 'image': parent.append(el('span', decode(token.text))); continue;
          default: node = el('span');
        }
        if (token.tokens) append(node, token.tokens);
        else node.textContent = decode(token.text);
        parent.append(node);
      }
    }
    const content = el('div'); content.className = 'callabo-summary-full';
    append(content, parser.lexer(source));
    return content;
  }
  const api = { renderMarkdown, oneLineSummary };
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.summaryMarkdown = api;
})(typeof window !== 'undefined' ? window : globalThis);
