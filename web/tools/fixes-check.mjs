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

await page.goto(BASE, { waitUntil: 'networkidle' });
await page.locator('#auth-username').fill('tester');
await page.locator('#auth-password').fill('correct-horse-battery');
await page.locator('button[type="submit"]').first().click();
await page.waitForSelector('.rd-items .rd-item', { timeout: 15000 });
await page.waitForTimeout(600);

// --- 1. lazy images heal and carry no referrer -------------------------------
await page.locator('.rd-items .rd-item').first().click();
await page.waitForSelector('.rd-read .rd-prose', { timeout: 15000 });
await page.locator('.rd-fulltext-btn').click();
await page.waitForTimeout(6000);
const imgs = await page.evaluate(() =>
  [...document.querySelectorAll('.rd-prose img')].map((i) => ({
    w: i.naturalWidth,
    src: i.getAttribute('src') ?? '',
    ref: i.getAttribute('referrerpolicy'),
    placeholder: (i.getAttribute('src') ?? '').includes('/images/v2/t.png'),
  })),
);
console.log(`prose imgs: ${imgs.length}`, JSON.stringify(imgs.slice(0, 4)));
// A text-only article legitimately has none; what must never happen is a placeholder or a
// referrer-leaking image. The dedicated ithome probe covers the lazy-heal on an image-heavy page.
ok('no scraped image is a 1x1 placeholder', imgs.every((i) => i.w > 1 && !i.placeholder));
ok('no placeholder src survives', imgs.every((i) => !i.placeholder));
ok('remote images send no referrer', imgs.every((i) => i.ref === 'no-referrer'));
await page.screenshot({ path: '/tmp/shots/images-healed.png' });
await page.locator('.rd-read-bar .rd-back').click().catch(() => {});
await page.waitForTimeout(400);

// --- 2. the add dialog quotes a broken bridge --------------------------------
await page.locator('.rd-add').click();
await page.waitForSelector('#rd-add-dialog[open]', { timeout: 8000 });
await page.locator('#rd-add-dialog input[type="url"], #rd-add-dialog input[type="text"]').first()
  .fill('https://rss.aishort.top/?type=36kr');
await page.locator('#rd-add-dialog button[type="submit"]').first().click();
await page.waitForTimeout(6000);
const dialogText = await page.locator('#rd-add-dialog').innerText();
ok('the dialog quotes the upstream error', dialogText.includes('无法获取内容'), dialogText.replace(/\s+/g, ' ').slice(0, 160));
await page.screenshot({ path: '/tmp/shots/add-broken-bridge.png' });
await page.locator('#rd-add-dialog .rd-dialog-actions button').first().click();
await page.waitForTimeout(600);

// --- 3. a subscription can be removed ----------------------------------------
const row = page.locator('.rd-tree-link', { hasText: 'Rust Blog' }).first();
ok('the throwaway feed is in the sidebar', (await row.count()) === 1);
await row.click({ button: 'right' });
await page.waitForSelector('#rd-feed-dialog[open]', { timeout: 8000 });
const removeBtn = page.locator('#rd-feed-dialog .rd-feed-remove button');
ok('the dialog offers removal', (await removeBtn.count()) === 1, await removeBtn.innerText());
await removeBtn.click();
await page.waitForTimeout(300);
ok('the first press only arms it', (await removeBtn.innerText()).length > 0 && (await page.locator('#rd-feed-dialog[open]').count()) === 1);
ok('arming explains the consequence', (await page.locator('#rd-feed-dialog .rd-feed-remove .rd-field-error').count()) === 1);
await page.screenshot({ path: '/tmp/shots/remove-armed.png' });
await removeBtn.click();
await page.waitForTimeout(2500);
ok('the second press removes it', (await page.locator('.rd-tree-link', { hasText: 'Rust Blog' }).count()) === 0);
ok('the dialog closed', (await page.locator('#rd-feed-dialog[open]').count()) === 0);
await page.screenshot({ path: '/tmp/shots/remove-done.png' });

await browser.close();
console.log(failures.length === 0 ? '\nALL PASS' : `\nFAILURES: ${failures.join(', ')}`);
process.exit(failures.length === 0 ? 0 : 1);
