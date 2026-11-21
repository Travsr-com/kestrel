// Fails if any package in package-lock.json has a license outside the allow list. The list follows
// `deny.toml`: permissive licenses compatible with Apache-2.0. Reads the lockfile only, so it needs
// no network and no installed packages. Run with `npm run licenses`.

import { readFileSync } from 'node:fs';

const ALLOW = new Set([
  'MIT',
  'Apache-2.0',
  'ISC',
  'BSD-2-Clause',
  'BSD-3-Clause',
  '0BSD',
  'BlueOak-1.0.0',
  'CC0-1.0',
  'MPL-2.0',
  'Zlib',
]);

/**
 * True when an SPDX expression is satisfied by the allow list (`OR`: any side, `AND`: both).
 * @param {string} expr
 * @returns {boolean}
 */
function allowed(expr) {
  const e = expr.trim().replace(/^\((.*)\)$/, '$1');
  if (/ OR /.test(e)) return e.split(/ OR /).some(allowed);
  if (/ AND /.test(e)) return e.split(/ AND /).every(allowed);
  return ALLOW.has(e);
}

const lock = JSON.parse(readFileSync(new URL('../package-lock.json', import.meta.url), 'utf8'));
const bad = [];
for (const [path, pkg] of Object.entries(lock.packages ?? {})) {
  if (path === '' || pkg.link) continue;
  const license = typeof pkg.license === 'string' ? pkg.license : null;
  if (license === null || !allowed(license)) bad.push(`${path.replace(/^.*node_modules\//, '')}: ${license ?? 'no license'}`);
}

if (bad.length > 0) {
  console.error(`error: ${bad.length} package(s) with a license outside the allow list:`);
  for (const line of bad) console.error(`  ${line}`);
  process.exit(1);
}
console.log(`ok: ${Object.keys(lock.packages).length - 1} packages, all licenses allowed`);
