import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8099';
const browser = await chromium.launch({
  executablePath: join(homedir(), '.cache/ms-playwright/chromium-1228/chrome-linux64/chrome'),
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const failures = [];
const ok = (name, cond, extra = '') => {
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${extra ? ` — ${extra}` : ''}`);
  if (!cond) failures.push(name);
};

const sidebar = async () =>
  Number((await page.locator('.rd-tree > li').first().locator('.rd-count > span').first().innerText()).trim());
const listUnread = async () =>
  Number((await page.locator('.rd-segment-count').first().innerText()).trim());
/** The badge on the row of the feed the first article belongs to: the per-feed aggregate. */
const feedBadge = async (feedName) =>
  Number(
    (await page
      .locator('.rd-tree--nested .rd-tree-link', { hasText: feedName })
      .first()
      .locator('.rd-count > span')
      .first()
      .innerText()
      .catch(() => '0'))
      .trim(),
  );

await page.goto(BASE, { waitUntil: 'networkidle' });
await page.locator('#auth-username').fill('tester');
await page.locator('#auth-password').fill('correct-horse-battery');
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item', { timeout: 15000 });
await page.waitForTimeout(600);

ok('sidebar and list agree before', (await sidebar()) === (await listUnread()), `${await sidebar()} vs ${await listUnread()}`);
const before = await sidebar();
const firstRow = page.locator('.rd-items .rd-item').first();
const feedName = (await firstRow.locator('.rd-item-feed').innerText()).trim();
const beforeFeed = await feedBadge(feedName);

await firstRow.click();
// The sync is debounced, so give it its window plus the round trip.
await page.waitForTimeout(1500);

const afterSide = await sidebar();
const afterList = await listUnread();
const afterFeed = await feedBadge(feedName);
ok('both drop by one after marking read', afterSide === before - 1 && afterList === before - 1, `${before} → sidebar ${afterSide}, list ${afterList}`);
ok('sidebar and list agree after', afterSide === afterList);
ok('the feed row’s badge follows too', afterFeed === beforeFeed - 1, `${feedName}: ${beforeFeed} → ${afterFeed}`);

await browser.close();
console.log(failures.length === 0 ? '\nALL PASS' : `\nFAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
