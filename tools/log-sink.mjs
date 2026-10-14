// Rig-only console sink for iron-svelte-client's log-forward.ts (POST JSON string[] to :9097).
// Also takes EGFX capture messages: POST /egfx/<captureId>/<seq> raw bytes
// -> <out dir>/egfx-dumps/<captureId>/egfx-<seq>.bin.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
const out = process.argv[2];
const dumpRoot = path.join(path.dirname(out), 'egfx-dumps');
const cors = { 'access-control-allow-origin': '*', 'access-control-allow-headers': 'content-type', 'access-control-allow-methods': 'POST, OPTIONS' };
http.createServer((req, res) => {
  if (req.method === 'OPTIONS') { res.writeHead(204, cors); return res.end(); }
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    const egfx = /^\/egfx\/(\d+)\/(\d+)$/.exec(req.url ?? '');
    if (egfx) {
      const dir = path.join(dumpRoot, egfx[1]);
      if (!fs.existsSync(dir)) {
        fs.mkdirSync(dir, { recursive: true });
        console.log(`[sink] new EGFX capture -> ${dir}`);
      }
      fs.writeFileSync(path.join(dir, `egfx-${egfx[2].padStart(5, '0')}.bin`), Buffer.concat(chunks));
    } else {
      try { fs.appendFileSync(out, JSON.parse(Buffer.concat(chunks).toString('utf8')).join('\n') + '\n'); } catch { /* ignore malformed batch */ }
    }
    res.writeHead(204, cors); res.end();
  });
}).listen(9097, '127.0.0.1', () => console.log(`[sink] 127.0.0.1:9097 -> ${out}, EGFX captures under ${dumpRoot}`));
