/**
 * Checks that the login page follows the server's registration policy.
 *
 *   node tools/auth-policy-check.mjs http://127.0.0.1:8097 http://127.0.0.1:8099 operator correct-horse-battery
 *
 * The first base must be a closed server, the second an open one.
 */
import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const CLOSED = process.argv[2] ?? 'http://127.0.0.1:8097';
const OPEN = process.argv[3] ?? 'http://127.0.0.1:8099';
const USER = process.argv[4] ?? 'operator';
const PASSWORD = process.argv[5] ?? 'correct-horse-battery';

const browser = await chromium.launch({
  executablePath: join(homedir(), '.cache/ms-playwright/chromium-1228/chrome-linux64/chrome'),
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const failures = [];
const ok = (name, cond, extra = '') => {
  console.log(`${cond ? 'PASS' : 'FAIL'} ${name}${extra ? ` — ${extra}` : ''}`);
  if (!cond) failures.push(name);
};

// --- closed server: no sign-up affordance, and the account the CLI made can log in ---
let page = await browser.newPage({ viewport: { width: 1280, height: 860 } });
await page.goto(CLOSED, { waitUntil: 'networkidle' });
await page.waitForSelector('.rd-auth-card', { timeout: 15000 });

ok('closed: no sign-up switch', (await page.locator('.rd-auth-switch button').count()) === 0);
ok(
  'closed: the page says registration is closed',
  (await page.locator('.rd-auth-switch').innerText()).includes('Registration is closed'),
);
ok('closed: no email field', (await page.locator('#auth-email').count()) === 0);
await page.screenshot({ path: '/tmp/shots/auth-closed.png' });

await page.locator('#auth-username').fill(USER);
await page.locator('#auth-password').fill(PASSWORD);
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item, .rd-list', { timeout: 15000 });
ok('closed: the CLI-created account signs in', true);
await page.close();

// --- open server: the sign-up form is still offered ---
page = await browser.newPage({ viewport: { width: 1280, height: 860 } });
await page.goto(OPEN, { waitUntil: 'networkidle' });
await page.waitForSelector('.rd-auth-card', { timeout: 15000 });
const switchText = await page.locator('.rd-auth-switch').innerText();
ok('open: the sign-up switch is offered', switchText.includes('Sign up'), switchText.trim());
await page.locator('.rd-auth-switch button').first().click();
await page.waitForTimeout(300);
ok('open: switching reveals the email field', (await page.locator('#auth-email').count()) === 1);
await page.screenshot({ path: '/tmp/shots/auth-open.png' });
await page.close();

await browser.close();
console.log(failures.length === 0 ? '\nALL PASS' : `\nFAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
