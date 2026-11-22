// PR #85 review: an approval's action can be 50,000 characters, so lists show only its first line,
// capped.

import { describe, expect, it } from 'vitest';
import { firstLine } from '../src/lib/format';

describe('firstLine', () => {
  it('keeps a short single line as it is', () => {
    expect(firstLine('git push origin main')).toBe('git push origin main');
    expect(firstLine('')).toBe('');
  });

  it('cuts a long line at 120 characters by default and says so with "..."', () => {
    expect(firstLine('a'.repeat(120))).toBe('a'.repeat(120));
    expect(firstLine('a'.repeat(121))).toBe(`${'a'.repeat(120)}...`);
    expect(firstLine('b'.repeat(50_000))).toBe(`${'b'.repeat(120)}...`);
  });

  it('takes a different cap', () => {
    expect(firstLine('abcdef', 3)).toBe('abc...');
    expect(firstLine('abc', 3)).toBe('abc');
  });

  it('keeps only the first line, and says more was left out', () => {
    expect(firstLine('cat <<EOF\nhello\nEOF')).toBe('cat <<EOF...');
    expect(firstLine('one\r\ntwo')).toBe('one...');
    expect(firstLine(`${'x'.repeat(30_000)}\n${'y'.repeat(20_000)}`)).toBe(`${'x'.repeat(120)}...`);
  });

  it('skips blank lines at the start', () => {
    expect(firstLine('\n\n  npm test\nmore')).toBe('npm test...');
    expect(firstLine('npm test\n')).toBe('npm test');
  });

  it('never cuts a character made of two UTF-16 units in half', () => {
    const cut = firstLine(`${'a'.repeat(119)}\u{1F600}tail`);
    expect(cut).toBe(`${'a'.repeat(119)}...`);
  });
});
