import axe from 'axe-core';

/**
 * Runs axe-core on `node` and returns the violations of serious or critical impact.
 * `color-contrast` is off: jsdom has no layout or canvas, so axe cannot compute colours there.
 */
export async function seriousViolations(node: Element): Promise<axe.Result[]> {
  const result = await axe.run(node, {
    resultTypes: ['violations'],
    rules: { 'color-contrast': { enabled: false } },
  });
  return result.violations.filter((v) => v.impact === 'serious' || v.impact === 'critical');
}

/** The rule ids of `violations`, for readable assertion failures. */
export function ids(violations: axe.Result[]): string[] {
  return violations.map((v) => v.id);
}
