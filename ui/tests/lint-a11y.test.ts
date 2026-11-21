// Scenario 4: the lint config treats Svelte a11y problems as errors. A component with an image
// lacking alt text, or a button with no name, fails `npm run lint`.

import { ESLint } from 'eslint';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

const testsDir = dirname(fileURLToPath(import.meta.url));

/** The errors ESLint reports for one fixture, as `rule: message`. */
async function lint(file: string): Promise<string[]> {
  // The fixtures are ignored by the normal lint run, so lint them with the same rules but
  // without the ignore list.
  const eslint = new ESLint({ cwd: join(testsDir, '..'), ignore: false });
  const [result] = await eslint.lintFiles([join(testsDir, 'fixtures', file)]);
  return (result?.messages ?? []).filter((m) => m.severity === 2).map((m) => `${m.ruleId}: ${m.message}`);
}

describe('a11y lint rules', () => {
  it('fail on an image without alt text', async () => {
    const errors = await lint('ImgNoAlt.svelte');
    expect(errors).toEqual([expect.stringMatching(/^svelte\/valid-compile: .*alt.*\(a11y_missing_attribute\)/s)]);
  }, 30_000);

  it('fail on a button with no name', async () => {
    const errors = await lint('ButtonNoName.svelte');
    expect(errors).toEqual([expect.stringMatching(/^svelte\/valid-compile: .*\(a11y_consider_explicit_label\)/s)]);
  }, 30_000);

  it('pass on an accessible component', async () => {
    expect(await lint('Accessible.svelte')).toEqual([]);
  }, 30_000);
});
