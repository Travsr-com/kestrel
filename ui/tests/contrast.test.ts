// PR #85 review: secondary text colours must keep WCAG AA contrast (4.5:1) on the surfaces they
// sit on. Read from `tokens.css` itself, so a change to a token is checked.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

// Tests run from `ui/`.
const css = readFileSync(resolve('src/styles/tokens.css'), 'utf8');

/** The `#rrggbb` value of a custom property in `tokens.css`. */
function token(name: string): string {
  const found = new RegExp(`--${name}:\\s*(#[0-9a-fA-F]{6})\\s*;`).exec(css);
  if (!found?.[1]) throw new Error(`no colour token --${name}`);
  return found[1];
}

/** WCAG 2 relative luminance of a `#rrggbb` colour. */
function luminance(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  }) as [number, number, number];
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

describe('text contrast', () => {
  it('computes the WCAG ratio', () => {
    expect(contrast('#000000', '#ffffff')).toBeCloseTo(21, 5);
    expect(contrast('#777777', '#ffffff')).toBeCloseTo(4.48, 2);
    expect(() => token('color-nothing')).toThrow('no colour token --color-nothing');
  });

  it.each([
    ['color-faint', 'color-surface'],
    ['color-faint', 'color-sidebar'],
    ['color-muted', 'color-surface'],
    ['color-muted', 'color-sidebar'],
  ])('--%s on --%s is at least 4.5:1', (text, background) => {
    expect(contrast(token(text), token(background))).toBeGreaterThanOrEqual(4.5);
  });
});
