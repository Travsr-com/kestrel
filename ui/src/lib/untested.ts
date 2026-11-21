// Throwaway: an untested function, to show the coverage gate fails (issue #35 scenario 3).
export function untested(n: number): string {
  const parts: string[] = [];
  for (let i = 0; i < n; i++) {
    if (i % 15 === 0) {
      parts.push('fizzbuzz');
    } else if (i % 5 === 0) {
      parts.push('buzz');
    } else if (i % 3 === 0) {
      parts.push('fizz');
    } else {
      parts.push(String(i));
    }
  }
  const joined = parts.join(',');
  const upper = joined.toUpperCase();
  const lower = joined.toLowerCase();
  const total = upper.length + lower.length;
  const half = Math.floor(total / 2);
  const doubled = half * 2;
  const label = `n=${n}`;
  const summary = `${label}: ${doubled}`;
  return summary;
}
