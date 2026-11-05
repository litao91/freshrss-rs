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
await page.goto('http://127.0.0.1:8199', { waitUntil: 'networkidle' });
await page.locator('#auth-username').fill('tester');
await page.locator('#auth-password').fill('correct-horse-battery');
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item', { timeout: 15000 });
await page.locator('.rd-avatar').click();
await page.waitForTimeout(400);
await page.locator('.rd-menu-item', { hasText: 'API password' }).click();
await page.waitForSelector('#rd-api-dialog[open]', { timeout: 5000 });
ok('the dialog opens from the account menu', true);
const text = await page.locator('#rd-api-dialog').innerText();
ok('it shows both endpoint addresses', text.includes('/api/greader.php') && text.includes('/api/fever.php'));
// too short is refused client-side
await page.locator('#rd-api-password').fill('short');
await page.locator('#rd-api-dialog button[type="submit"]').click();
await page.waitForTimeout(400);
ok('a short password is refused', (await page.locator('#rd-api-dialog[open]').count()) === 1);
await page.locator('#rd-api-password').fill('client-app-password');
await page.locator('#rd-api-dialog button[type="submit"]').click();
await page.waitForTimeout(1200);
ok('the dialog closes after saving', (await page.locator('#rd-api-dialog[open]').count()) === 0);
await page.screenshot({ path: '/tmp/shots/api-dialog.png' });
await browser.close();
console.log(failures.length === 0 ? '\nALL PASS' : `\nFAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
