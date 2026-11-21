// The license check's SPDX expression parser: AND binds tighter than OR, parentheses group, and
// anything it cannot parse is refused.

import { describe, expect, it } from 'vitest';
import { satisfies } from '../scripts/spdx.mjs';

const ALLOW = new Set(['MIT', 'ISC', 'Apache-2.0', 'BSD-3-Clause', 'Apache-2.0 WITH LLVM-exception']);

describe('satisfies', () => {
  it.each([
    ['MIT', true],
    ['GPL-3.0', false],
    ['(MIT)', true],
    ['MIT OR GPL-3.0', true],
    ['GPL-3.0 OR MIT', true],
    ['MIT AND ISC', true],
    ['MIT AND GPL-3.0', false],
    ['(MIT OR Apache-2.0) AND BSD-3-Clause', true],
    ['GPL-3.0 AND (LGPL-2.1 OR MIT OR ISC)', false],
    ['GPL-3.0 AND LGPL-2.1 OR MIT', true],
    ['MIT OR GPL-3.0 AND LGPL-2.1', true],
    ['(GPL-3.0 OR LGPL-2.1) AND MIT', false],
    ['Apache-2.0 WITH LLVM-exception', true],
    ['GPL-2.0 WITH Classpath-exception-2.0', false],
    ['((MIT))', true],
  ])('%s -> %s', (expr, ok) => {
    expect(satisfies(expr, ALLOW)).toBe(ok);
  });

  it.each(['', '(MIT', 'MIT)', 'MIT OR', 'AND MIT', 'MIT ISC', 'MIT WITH', '()'])('refuses malformed %j', (expr) => {
    expect(satisfies(expr, ALLOW)).toBe(false);
  });
});
