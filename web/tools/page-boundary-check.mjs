/**
 * Drives the reading pane's `Next` across the boundary between two pages of a listing.
 *
 * What the Rust tests pin is which articles the server names as neighbours; what only a browser can
 * show is that the button stays live on the last row that has arrived, that it waits for the page
 * the list has usually already asked for rather than giving up on it, that the reader lands in the
 * list instead of off the edge of it, and that the button does die at the true end. The second page
 * is held in flight until the script releases it, because that is the state the bug lived in: the
 * reader is on the last loaded row, more rows exist, and `neighbour` could only see loaded ones.
 *
 *   node tools/page-boundary-check.mjs http://127.0.0.1:8098 reader correct-horse-battery
 *
 * Needs a database seeded from `scripts/seed-demo.sql`, which holds more unread articles than one
 * page of 40, and it marks articles read as it goes, so re-seed before repeating it.
 */
import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8098';
const USER = process.argv[3] ?? 'reader';
const PASSWORD = process.argv[4] ?? 'correct-horse-battery';
let release;
const released = new Promise((resolve) => {
  release = resolve;
});
let hold = true;

const results = [];
function check(name, ok, detail = '') {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${ok || !detail ? '' : ` — ${detail}`}`);
}

const browser = await chromium.launch({
  executablePath: join(homedir(), '.cache/ms-playwright/chromium-1228/chrome-linux64/chrome'),
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = [];
page.on('pageerror', (err) => errors.push(String(err)));
page.on('console', (msg) => msg.type() === 'error' && errors.push(msg.text()));

// Hold every continuation page until released; first pages and everything else pass through.
await page.route('**/api/v1/entries**', async (route) => {
  if (hold && route.request().url().includes('cursor=')) await released;
  await route.continue();
});

/** The highest listing index rendered, which under windowing is the last row that has arrived. */
const lastIndex = () =>
  page.evaluate(() =>
    Math.max(...[...document.querySelectorAll('.rd-items [data-index]')].map((e) => Number(e.dataset.index))),
  );

const openIndex = (index) =>
  page.evaluate((i) => document.querySelector(`[data-index="${i}"] .rd-item-btn`)?.click(), index);

const paneTitle = () => page.locator('.rd-article-title').textContent();
const nextBtn = page.locator('.rd-read-bar button[title^="Next"]');

await page.goto(BASE, { waitUntil: 'networkidle' });
if ((await page.locator('.rd-auth').count()) > 0) {
  await page.locator('input[name="username"], input[type="text"]').first().fill(USER);
  await page.locator('input[type="password"]').first().fill(PASSWORD);
  await page.locator('button[type="submit"]').first().click();
  await page.waitForSelector('.rd-app', { timeout: 10000 });
}
await page.waitForSelector('.rd-items .rd-item', { timeout: 10000 });
await page.waitForTimeout(600);

check('the listing has a page that has not arrived', (await page.locator('.rd-list-end .rd-btn').count()) === 1);

// Scroll to the bottom: the list asks for the next page, and the route handler holds it there.
await page.locator('#rd-list-scroll').evaluate((el) => el.scrollTo({ top: el.scrollHeight }));
await page.waitForTimeout(1000);
check('the held page is on its way', (await page.locator('.rd-list-end .rd-spin').count()) === 1);

const boundary = await lastIndex();
const rowTitle = await page.evaluate(
  (i) => document.querySelector(`[data-index="${i}"] .rd-item-title`)?.textContent ?? '',
  boundary,
);
await openIndex(boundary);
await page.waitForSelector('.rd-read .rd-article', { timeout: 10000 });
await page.waitForTimeout(700);
check(
  'the pane opened the last row that has arrived',
  ((await paneTitle()) ?? '').trim() === rowTitle.trim(),
  `${JSON.stringify(rowTitle)} vs ${JSON.stringify(await paneTitle())}`,
);

// The bug: this used to be disabled, because no loaded row sits below this one.
check('next stays live at the end of a page', await nextBtn.isEnabled());

await nextBtn.click();
await page.waitForTimeout(500);
check(
  'and waits for the page rather than giving up',
  ((await paneTitle()) ?? '').trim() === rowTitle.trim(),
  'still on the same article while the page is held',
);

hold = false;
release();
await page.waitForTimeout(1800);
const moved = ((await paneTitle()) ?? '').trim();
check('then walks into the page that landed', moved !== rowTitle.trim(), `${JSON.stringify(moved)}`);

const selected = await page.evaluate(() => {
  const row = document.querySelector('.rd-items .rd-item.is-selected');
  return row ? (row.querySelector('.rd-item-title')?.textContent ?? '').trim() : null;
});
check('and the list follows the reader there', selected === moved, `${JSON.stringify(selected)}`);

// Now the whole listing has arrived: the last row really is the last article, and `Next` must die.
await page.locator('#rd-list-scroll').evaluate((el) => el.scrollTo({ top: el.scrollHeight }));
await page.waitForTimeout(1200);
check(
  'no pages are left to load',
  (await page.locator('.rd-list-end .rd-btn').count()) === 0 &&
    (await page.locator('.rd-list-end .rd-hint').count()) === 1,
);
const end = await lastIndex();
await openIndex(end);
await page.waitForTimeout(900);
check('next is disabled at the true end of the listing', !(await nextBtn.isEnabled()), `index ${end}`);

check(
  'no console errors',
  errors.filter((e) => !/favicon|404|401 \(Unauthorized\)/i.test(e)).length === 0,
  errors.slice(0, 3).join(' | '),
);

await browser.close();
const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length > 0 ? 1 : 0);
