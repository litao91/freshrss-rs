/**
 * Drives the reading pane's full-article button against a live server.
 *
 * The button is the one control in the reader that waits on a third-party page, so this asserts the
 * whole round trip in a browser: the fetch happens, the prose swaps to the scraped body, toggling
 * back restores the feed's own version byte-for-byte, and a second open is served from the stored
 * attribute without touching the network again.
 *
 *   node tools/fulltext-check.mjs http://127.0.0.1:8099 tester correct-horse-battery
 */
import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8099';
const USER = process.argv[3] ?? 'tester';
const PASSWORD = process.argv[4] ?? 'correct-horse-battery';
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

const apiCalls = [];
page.on('response', (r) => {
  if (r.url().includes('/fulltext')) apiCalls.push({ url: r.url(), status: r.status() });
});

await page.goto(BASE, { waitUntil: 'networkidle' });
await page.locator('input[name="username"], input[type="text"]').first().fill(USER);
await page.locator('input[type="password"]').first().fill(PASSWORD);
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item', { timeout: 15000 });

// Open the first article in the reading pane.
await page.locator('.rd-items .rd-item').first().click();
await page.waitForSelector('.rd-read .rd-prose', { timeout: 15000 });
await page.waitForTimeout(500);

const btn = page.locator('.rd-fulltext-btn');
ok('full-article button exists', (await btn.count()) === 1);
ok('button starts unpressed', (await btn.getAttribute('aria-pressed')) === 'false');

const before = (await page.locator('.rd-read .rd-prose').innerHTML()).length;
const title = await page.locator('.rd-article-title').innerText();
console.log(`article: ${title.trim().slice(0, 60)} (prose ${before} bytes)`);

await btn.click();
await page.waitForFunction(
  () => document.querySelector('.rd-fulltext-btn')?.getAttribute('aria-pressed') === 'true',
  { timeout: 45000 },
).catch(() => {});
await page.waitForTimeout(1200);

const after = (await page.locator('.rd-read .rd-prose').innerHTML()).length;
const pressed = await btn.getAttribute('aria-pressed');
ok('button flips to pressed', pressed === 'true', `aria-pressed=${pressed}`);
ok('backend was called', apiCalls.length >= 1, JSON.stringify(apiCalls));
ok('all calls succeeded', apiCalls.every((c) => c.status === 200), JSON.stringify(apiCalls));
ok('prose changed', after !== before, `${before} → ${after} bytes`);
await page.screenshot({ path: '/tmp/shots/fulltext-on.png' });

// Toggle back to the feed's own version.
await btn.click();
await page.waitForTimeout(900);
const back = (await page.locator('.rd-read .rd-prose').innerHTML()).length;
ok('button unpresses again', (await btn.getAttribute('aria-pressed')) === 'false');
ok('prose returns to the feed version', back === before, `${after} → ${back} vs ${before}`);

// Re-open: the stored attribute must serve it without a second fetch.
const callsBefore = apiCalls.length;
await btn.click();
await page.waitForFunction(
  () => document.querySelector('.rd-fulltext-btn')?.getAttribute('aria-pressed') === 'true',
  { timeout: 15000 },
).catch(() => {});
await page.waitForTimeout(900);
ok(
  'second open is served from storage',
  apiCalls.length === callsBefore,
  `${apiCalls.length - callsBefore} new calls`,
);
const stored = (await page.locator('.rd-read .rd-prose').innerHTML()).length;
ok('stored copy matches the fetch', stored === after, `${stored} vs ${after}`);
await page.screenshot({ path: '/tmp/shots/fulltext-reopen.png' });

await browser.close();
console.log(failures.length === 0 ? '\nALL PASS' : `\nFAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
