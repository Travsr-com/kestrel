// A small SPDX license expression evaluator for the license check.
//
//   expr   = term ("OR" term)*
//   term   = factor ("AND" factor)*
//   factor = "(" expr ")" | id ["WITH" id]
//
// AND binds tighter than OR. `id WITH exception` passes when that exact pair is allowed or when
// `id` alone is (an exception only adds permissions). Anything that does not parse is refused.

/**
 * True when the SPDX expression `expr` can be satisfied using only licenses in `allow`.
 * @param {string} expr
 * @param {Set<string>} allow
 * @returns {boolean}
 */
export function satisfies(expr, allow) {
  const tokens = expr.match(/\(|\)|[^\s()]+/g) ?? [];
  let pos = 0;

  /** @returns {boolean | null} null when the expression is malformed */
  function parseExpr() {
    let ok = parseTerm();
    while (ok !== null && tokens[pos] === 'OR') {
      pos += 1;
      const right = parseTerm();
      ok = right === null ? null : ok || right;
    }
    return ok;
  }

  /** @returns {boolean | null} */
  function parseTerm() {
    let ok = parseFactor();
    while (ok !== null && tokens[pos] === 'AND') {
      pos += 1;
      const right = parseFactor();
      ok = right === null ? null : ok && right;
    }
    return ok;
  }

  /** @returns {boolean | null} */
  function parseFactor() {
    const token = tokens[pos];
    if (token === undefined || token === ')' || token === 'AND' || token === 'OR' || token === 'WITH') return null;
    pos += 1;
    if (token === '(') {
      const inner = parseExpr();
      if (inner === null || tokens[pos] !== ')') return null;
      pos += 1;
      return inner;
    }
    if (tokens[pos] === 'WITH') {
      const exception = tokens[pos + 1];
      if (exception === undefined || ['(', ')', 'AND', 'OR', 'WITH'].includes(exception)) return null;
      pos += 2;
      return allow.has(`${token} WITH ${exception}`) || allow.has(token);
    }
    return allow.has(token);
  }

  const result = parseExpr();
  return result === true && pos === tokens.length;
}
