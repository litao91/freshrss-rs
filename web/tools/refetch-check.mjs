import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';
const browser = await chromium.launch({
  executablePath: join(homedir(), '.cache/ms-playwright/chromium-1228/chrome-linux64/chrome'),
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const failures = [];
const ok = (n, c, x = '') => { console.log(`${c ? 'PASS' : 'FAIL'} ${n}${x ? ` — ${x}` : ''}`); if (!c) failures.push(n); };
let fulltextCalls = 0;
page.on('request', (r) => { if (r.url().includes('/fulltext')) fulltextCalls += 1; });
await page.goto('http://127.0.0.1:8199', { waitUntil: 'networkidle' });
await page.locator('#auth-username').fill('tester');
await page.locator('#auth-password').fill('correct-horse-battery');
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item', { timeout: 15000 });
await page.locator('.rd-items .rd-item').first().click();
await page.waitForSelector('.rd-read .rd-prose', { timeout: 15000 });
ok('no re-fetch button before any full article', (await page.locator('.rd-fulltext-refetch').count()) === 0);
await page.locator('.rd-fulltext-btn').click();
await page.waitForFunction(() => document.querySelector('.rd-fulltext-btn')?.getAttribute('aria-pressed') === 'true', { timeout: 45000 });
await page.waitForTimeout(800);
ok('the re-fetch button appears with the full article', (await page.locator('.rd-fulltext-refetch').count()) === 1);
const before = fulltextCalls;
await page.locator('.rd-fulltext-refetch').click();
await page.waitForTimeout(4000);
ok('re-fetching calls the endpoint again', fulltextCalls === before + 1, `${before} → ${fulltextCalls}`);
ok('the pane still shows the full article', (await page.locator('.rd-fulltext-btn').getAttribute('aria-pressed')) === 'true');
await page.screenshot({ path: '/tmp/shots/refetch.png' });
// the auth page speaks the catalogue now; a fresh context, because the session cookie is HttpOnly
const fresh = await browser.newContext({ viewport: { width: 1280, height: 860 } });
const signedOut = await fresh.newPage();
await signedOut.goto('http://127.0.0.1:8199', { waitUntil: 'networkidle' });
const authText = await signedOut.locator('.rd-auth-card').innerText();
ok('the auth page renders from the catalogue', authText.includes('Log in') && authText.includes('Sign in to your reader.'));
await browser.close();
console.log(failures.length === 0 ? '\nALL PASS' : `\nFAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
