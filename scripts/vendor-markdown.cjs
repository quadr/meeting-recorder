// Keep the offline desktop bundle reproducible from the pinned npm dependency.
const fs = require('node:fs');
const path = require('node:path');
const source = path.dirname(require.resolve('marked/package.json'));
const target = path.join(__dirname, '../ui/vendor');
fs.mkdirSync(target, { recursive: true });
fs.copyFileSync(path.join(source, 'lib/marked.umd.js'), path.join(target, 'marked.umd.js'));
fs.copyFileSync(path.join(source, 'LICENSE'), path.join(target, 'marked.LICENSE'));
