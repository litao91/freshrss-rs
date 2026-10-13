import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8099';
const browser = await chromium.launch({
  executablePath: join(homedir(), '.cache/ms-playwright/chromium-1228/chrome-linux64/chrome'),
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const seen = [];
page.on('response', (r) => {
  if (r.request().resourceType() === 'image') seen.push([r.status(), r.url().slice(0, 95)]);
});
page.on('requestfailed', (r) => {
  if (r.request().resourceType() === 'image') seen.push(['FAIL', r.url().slice(0, 95)]);
});
await page.goto(BASE, { waitUntil: 'networkidle' });
await page.locator('#auth-username').fill('tester');
await page.locator('#auth-password').fill('correct-horse-battery');
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item', { timeout: 15000 });
await page.locator('.rd-items .rd-item', { hasText: 'IT之家' }).first().click();
await page.waitForSelector('.rd-read .rd-prose', { timeout: 15000 });
await page.locator('.rd-fulltext-btn').click();
await page.waitForTimeout(6000);
const imgs = await page.evaluate(() =>
  [...document.querySelectorAll('.rd-prose img')].map((i) => `${i.naturalWidth}x${i.naturalHeight} ${(i.getAttribute('src') ?? '').slice(0, 80)}`),
);
console.log(`prose imgs after fulltext: ${imgs.length}`);
for (const i of imgs) console.log(' ', i);
console.log('--- network ---');
for (const [s, u] of seen) console.log(s, u);
await page.screenshot({ path: '/tmp/shots/img-burst.png' });
await browser.close();
