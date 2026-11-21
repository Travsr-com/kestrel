// Checks the build after `npm run build`: `dist/index.html` exists and the JS and CSS ship under
// the budget, gzipped. Run with `npm run size`.

import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

const BUDGET_BYTES = 150 * 1024;
const dist = fileURLToPath(new URL('../dist/', import.meta.url));

if (!existsSync(join(dist, 'index.html'))) {
  console.error('error: dist/index.html is missing, run `npm run build` first');
  process.exit(1);
}

/**
 * Every file under `dir`, as paths relative to it.
 * @param {string} dir
 * @param {string} [prefix]
 * @returns {string[]}
 */
function files(dir, prefix = '') {
  return readdirSync(join(dir, prefix)).flatMap((name) => {
    const rel = join(prefix, name);
    return statSync(join(dir, rel)).isDirectory() ? files(dir, rel) : [rel];
  });
}

let total = 0;
for (const rel of files(dist).filter((f) => /\.(js|css|html)$/.test(f))) {
  const size = gzipSync(readFileSync(join(dist, rel))).length;
  total += size;
  console.log(`${(size / 1024).toFixed(1).padStart(8)} kB  ${rel}`);
}
console.log(`${(total / 1024).toFixed(1).padStart(8)} kB  total gzipped (budget ${BUDGET_BYTES / 1024} kB)`);
if (total > BUDGET_BYTES) {
  console.error('error: the UI bundle is over budget');
  process.exit(1);
}
