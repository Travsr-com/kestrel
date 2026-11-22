// Issue #45, scenario 13: a child calls kestrel.ask_human. The Approvals page shows the question,
// the test types an answer and sends it, the row goes away, and the child runs to the end.
// `tests/ui.rs` then checks the child got exactly the text typed here.
import { expect, test } from '@playwright/test';
import { api, kestrel, type RunSummary } from './kestrel';

test('a question is answered from the Approvals page', async ({ page, request }) => {
  const { token, askRoot, question, answer } = kestrel();

  await page.goto(`/?token=${token}#/approvals`);
  await expect(page.getByRole('heading', { level: 1, name: 'Approvals' })).toBeVisible();
  const list = page.getByRole('list', { name: 'Pending approvals' });
  const row = list.getByRole('listitem').filter({ hasText: question });
  // The child asks once its sandbox is up and its parent spawned it.
  await expect(row).toBeVisible({ timeout: 180_000 });
  await expect(row.getByText(/^Question, waiting/)).toBeVisible();

  // Selecting it in the list shows it in the detail panel, where it is answered.
  await row.getByRole('button').click();
  const panel = page.getByRole('region', { name: /asks a question$/ });
  await expect(panel).toBeVisible();
  await expect(panel.getByText('Question', { exact: true })).toBeVisible();
  await expect(panel.getByText(question)).toBeVisible();
  await panel.getByLabel('Answer').fill(answer);
  await panel.getByRole('button', { name: 'Send answer' }).click();
  await expect(row).toHaveCount(0);

  // The child goes on with the answer and completes.
  const runs = await api<RunSummary[]>(request, `/api/runs?root=${askRoot}`);
  const child = runs.find((r) => r.parent === askRoot);
  expect(child, `a child under ${askRoot}: ${JSON.stringify(runs)}`).toBeDefined();
  await expect
    .poll(async () => (await api<RunSummary>(request, `/api/runs/${child?.id}`)).status, { timeout: 120_000 })
    .toBe('Completed');
});
