// After `vite build`: write Brotli (.br) and gzip (.gz) copies of the
// text assets next to them. The server embeds them and answers with the
// encoding the browser accepts (server/src/http/web_static.rs). index.html is
// left alone: the server rewrites its <base href> per request.
import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

const dist = path.resolve(import.meta.dirname, '..', 'dist');
const COMPRESSIBLE = /\.(js|mjs|css|svg|json|txt|webmanifest)$/;

function* files(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) yield* files(full);
    else yield full;
  }
}

let before = 0;
let after = 0;
for (const file of [...files(dist)]) {
  if (!COMPRESSIBLE.test(file)) continue;
  const data = fs.readFileSync(file);
  if (data.length < 1024) continue;
  const br = zlib.brotliCompressSync(data, {
    params: { [zlib.constants.BROTLI_PARAM_QUALITY]: 11, [zlib.constants.BROTLI_PARAM_SIZE_HINT]: data.length },
  });
  const gz = zlib.gzipSync(data, { level: 9 });
  fs.writeFileSync(`${file}.br`, br);
  fs.writeFileSync(`${file}.gz`, gz);
  before += data.length;
  after += br.length;
}
console.log(`compressed assets: ${(before / 1024).toFixed(0)} KiB → ${(after / 1024).toFixed(0)} KiB (brotli)`);
