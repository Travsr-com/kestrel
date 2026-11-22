// Issue #45, scenario 12: the Run view shows a fan-out live. The tree grows as children are
// spawned (one shows as running before the last exists), every status reaches Completed without a
// reload, every child's stream shows its model call, the budget bar matches the API, and the Fleet and
// Approvals pages load without console errors.
import { expect, test } from '@playwright/test';
import { api, formatUsd, kestrel, readTree, statusOf, type Node, type RunSummary } from './kestrel';

test('the spawn tree grows and completes live', async ({ page, request }) => {
  const { token, root, children } = kestrel();
  const errors: string[] = [];
  page.on('console', (msg) => {
    if (msg.type() === 'error') errors.push(msg.text());
  });
  page.on('pageerror', (err) => errors.push(err.message));

  // The address `kestrel up` prints carries the token; the hash picks the run.
  await page.goto(`/?token=${token}#/runs/${root}`);
  await expect(page.getByRole('heading', { level: 1, name: 'lead' })).toBeVisible();
  const tree = page.getByRole('navigation', { name: 'Spawn tree' });
  await expect(tree).toBeVisible();
  // Gone after a reload, so its presence at the end proves there was none.
  await page.evaluate(() => {
    (window as unknown as { kestrelNoReload: boolean }).kestrelNoReload = true;
  });

  // Watch the children appear. The parent waits a while before its first spawn, then spawns one
  // every few seconds.
  let nodes: Node[] = await readTree(page);
  console.log(`children on the page when it opened: ${nodes.length - 1}`);
  let runningBeforeLast = false;
  const seen: string[] = [];
  const deadline = Date.now() + 240_000;
  while (Date.now() < deadline) {
    nodes = await readTree(page);
    const kids = nodes.filter((n) => n.id !== root);
    for (const kid of kids) if (!seen.includes(kid.id)) seen.push(kid.id);
    if (kids.length < children && kids.some((k) => statusOf(k) === 'Running')) runningBeforeLast = true;
    if (kids.length >= children) break;
    await page.waitForTimeout(200);
  }
  expect(seen, `children seen as they appeared: ${JSON.stringify(nodes)}`).toHaveLength(children);
  expect(runningBeforeLast, 'a child showed as Running before the last child existed').toBe(true);

  // Every run reaches Completed, live.
  await expect
    .poll(async () => (await readTree(page)).map(statusOf), { timeout: 240_000 })
    .toEqual(Array<string>(children + 1).fill('Completed'));
  expect(await page.evaluate(() => (window as unknown as { kestrelNoReload?: boolean }).kestrelNoReload)).toBe(true);
  expect(page.url()).toContain(`#/runs/${root}`);
  expect(page.url(), 'the UI took the token out of the address').not.toContain('token=');

  // Every child's stream shows its model call. Each node's link is described by its status, whose
  // id carries the run id, so the loop opens each child by its id.
  const kids = (await readTree(page)).filter((n) => n.id !== root);
  expect(kids, 'the children in the tree').toHaveLength(children);
  const events = page.getByRole('list', { name: 'Events' });
  for (const kid of kids) {
    const link = tree.locator(`a[aria-describedby="node-status-${kid.id} node-cost-${kid.id}"]`);
    await expect(link, `the tree link of child ${kid.id}`).toHaveText('worker');
    await link.click();
    await expect(page).toHaveURL(new RegExp(`#/runs/${kid.id}`));
    await expect(link, `child ${kid.id} is the selected run`).toHaveAttribute('aria-current', 'page');
    await expect(page.getByRole('heading', { level: 1, name: 'worker' })).toBeVisible();
    await expect(
      events.getByText(/Model call: claude-e2e-1, 1000 in, 200 out, \$0\.006465/),
      `the model call of child ${kid.id}`,
    ).toBeVisible();
  }

  // Back on the root, the budget bar says what the API says.
  await tree.getByRole('link', { name: 'lead', exact: true }).click();
  await expect(page.getByRole('heading', { level: 1, name: 'lead' })).toBeVisible();
  await expect
    .poll(
      async () => {
        const run = await api<RunSummary>(request, `/api/runs/${root}`);
        const runs = await api<RunSummary[]>(request, `/api/runs?root=${root}`);
        const carved = runs.filter((r) => r.parent === root).reduce((sum, r) => sum + (r.limit ?? 0), 0);
        const want = [
          `${formatUsd(run.spent)} of ${formatUsd(run.limit)} limit`,
          `Carved to children ${formatUsd(carved)}`,
        ];
        const shown = (await page.locator('.budget').innerText()).replace(/\s+/g, ' ');
        return want.every((w) => shown.includes(w)) ? 'matches' : `shown "${shown}", the API says "${want.join('; ')}"`;
      },
      { timeout: 30_000 },
    )
    .toBe('matches');

  // Fleet and Approvals load without console errors.
  const nav = page.getByRole('navigation', { name: 'Main' });
  errors.length = 0;
  await nav.getByRole('link', { name: 'Fleet' }).click();
  await expect(page.getByRole('heading', { level: 1, name: 'Fleet' })).toBeVisible();
  await page.waitForTimeout(1_000);
  expect(errors, 'console errors on Fleet').toEqual([]);
  await nav.getByRole('link', { name: /^Approvals/ }).click();
  await expect(page.getByRole('heading', { level: 1, name: 'Approvals' })).toBeVisible();
  await page.waitForTimeout(1_000);
  expect(errors, 'console errors on Approvals').toEqual([]);
});
