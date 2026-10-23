/**
 * Drives the "mark all above / below as read" gestures in a real browser.
 *
 * What the Rust tests already pin is which rows the server marks; what only a browser can show is
 * that the four surfaces reach it — the two shortcuts, the toolbar menu, the right-click menu and
 * the reading pane's buttons — that the menus are the svelte-widgets ones wearing this reader's
 * tokens rather than the library's defaults, and that the reader ends up where they started.
 *
 *   node tools/mark-from-check.mjs http://127.0.0.1:8098 reader correct-horse-battery
 *
 * Needs a database seeded from `scripts/seed-demo.sql`, which holds more articles than one page,
 * because the point of anchoring server-side is that it reaches rows nobody has scrolled to. The
 * run mutates read state, so re-seed before repeating it.
 */
import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8098';
const USER = process.argv[3] ?? 'reader';
const PASSWORD = process.argv[4] ?? 'correct-horse-battery';
const SHOTS = process.env.SHOTS ?? '/tmp/shots';

const failures = [];
const checks = [];

function check(name, ok, detail = '') {
  checks.push({ name, ok, detail });
  if (!ok) failures.push(`${name}${detail ? ` — ${detail}` : ''}`);
}

async function shot(page, name) {
  await page.screenshot({ path: join(SHOTS, `${name}.png`), fullPage: false });
}

const browser = await chromium.launch({
  executablePath: join(homedir(), '.cache/ms-playwright/chromium-1228/chrome-linux64/chrome'),
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });

const consoleErrors = [];
page.on('console', (msg) => {
  if (msg.type() === 'error') consoleErrors.push(msg.text());
});
page.on('pageerror', (err) => consoleErrors.push(String(err)));

/** The rows on screen, in listing order. */
const rows = () =>
  page.evaluate(() =>
    [...document.querySelectorAll('.rd-items .rd-item')].map((el) => ({
      id: el.id.replace('rd-item-', ''),
      title: el.querySelector('.rd-item-title')?.textContent ?? '',
      read: el.classList.contains('is-read'),
      selected: el.classList.contains('is-selected'),
    })),
  );

const indexOfSelected = async () => (await rows()).findIndex((r) => r.selected);

/** The open menu, or nothing: `ActionMenu` removes the element entirely once it is dismissed. */
const openMenu = () => page.locator('menu.action-menu');

const menuItems = () =>
  page.evaluate(() =>
    [...document.querySelectorAll('menu.action-menu button[role^="menuitem"]')].map((b) => ({
      label: b.querySelector('.rd-mark-item-label')?.textContent?.trim() ?? b.textContent?.trim(),
      keys: [...b.querySelectorAll('kbd')].map((k) => k.textContent.trim()),
      icon: b.querySelector('.rd-mark-item > i')?.className ?? '',
    })),
  );

/** `--fd-surface` resolved to an `rgb()` triple, so it can be compared with a computed colour. */
const tokenRgb = (name) =>
  page.evaluate((token) => {
    const probe = document.createElement('div');
    probe.style.color = `var(${token})`;
    document.body.append(probe);
    const value = getComputedStyle(probe).color;
    probe.remove();
    return value;
  }, name);

const scrollTop = () =>
  page.evaluate(() => document.querySelector('#rd-list-scroll').scrollTop);

/**
 * Indexes whose read state changed between two snapshots.
 *
 * Compared rather than asserted against "everything unread": the seeded database already has read
 * articles in it, so a check that assumes the rows below an anchor start unread is checking the
 * fixture, not the feature.
 */
function changed(before, after) {
  const flipped = [];
  for (let i = 0; i < Math.max(before.length, after.length); i += 1) {
    if (before[i]?.read !== after[i]?.read) flipped.push(i);
  }
  return flipped;
}

try {
  await page.goto(BASE, { waitUntil: 'networkidle' });

  // --- sign in -----------------------------------------------------------
  if ((await page.locator('.rd-auth').count()) > 0) {
    await page.locator('input[name="username"], input[type="text"]').first().fill(USER);
    await page.locator('input[type="password"]').first().fill(PASSWORD);
    await page.locator('button[type="submit"]').first().click();
    await page.waitForSelector('.rd-app', { timeout: 10000 });
  }
  await page.waitForSelector('.rd-items .rd-item', { timeout: 10000 });
  check('the reader signed in and listed articles', (await rows()).length > 0);

  // The gestures split a listing, so every row has to stay put: under the unread filter a row that
  // becomes read would be the thing under test and the thing that vanishes at once.
  await page.keyboard.press('u');
  await page.waitForTimeout(1200);
  const loaded = await rows();
  check('the All filter is on, so read rows stay visible', loaded.some((r) => r.read));

  // Asserted up front, before anything scrolls: reaching rows the reader has not loaded is the
  // entire reason the anchor is resolved on the server instead of by slicing `entries.items`.
  const paginated = (await page.locator('#rd-sentinel .rd-btn').count()) > 0;
  check(
    'the listing holds pages that are not loaded',
    paginated && loaded.length > 0,
    `${loaded.length} rows on screen`,
  );

  // --- the toolbar menu --------------------------------------------------
  const trigger = page.locator('.rd-mark-menu');
  check('the toolbar carries a mark-from-here menu', (await trigger.count()) === 1);
  check(
    'with no article focused the menu is disabled rather than guessing an anchor',
    (await trigger.getAttribute('aria-disabled')) === 'true',
  );
  check(
    'and it says why',
    ((await trigger.getAttribute('title')) ?? '').length > 0,
    await trigger.getAttribute('title'),
  );

  // `j` opens an article and puts the keyboard cursor on it, which is what the menu anchors to.
  await page.keyboard.press('j');
  await page.waitForTimeout(900);
  check('after `j` the menu has an anchor', (await trigger.getAttribute('aria-disabled')) === null);

  await trigger.click();
  await page.waitForSelector('menu.action-menu', { timeout: 3000 });
  await shot(page, 'toolbar-menu');

  const items = await menuItems();
  check('the menu offers exactly the two directions', items.length === 2, `${items.length} items`);
  const labels = items.map((i) => i.label).join(' | ');
  check('and names both directions', labels.includes('above') && labels.includes('below'), labels);
  check(
    'each item shows the binding that does the same thing',
    items.every((i) => i.keys.length > 0),
    JSON.stringify(items.map((i) => i.keys)),
  );
  check(
    'each item keeps its Remixicon glyph rather than the library’s own icon set',
    items.every((i) => i.icon.startsWith('ri-')),
    items.map((i) => i.icon).join(' | '),
  );

  // The point of the token layer: the menu must wear this reader's surface, not the library's
  // `light-dark(#fff, #2a2a2e)` default, which would sit visibly apart from `.rd-menu-panel`.
  const menuBg = await page.evaluate(() =>
    getComputedStyle(document.querySelector('menu.action-menu')).backgroundColor,
  );
  const surface = await tokenRgb('--fd-surface');
  check('the menu is painted with the reader’s own surface token', menuBg === surface, `${menuBg} vs ${surface}`);
  const menuRadius = await page.evaluate(() =>
    getComputedStyle(document.querySelector('menu.action-menu')).borderRadius,
  );
  check(
    'and its corner radius comes from the token layer',
    menuRadius !== '5pt' && menuRadius !== '0px',
    menuRadius,
  );

  // Arrow keys and Escape are the library's job now; what matters is that they still work here.
  await page.keyboard.press('ArrowDown');
  await page.waitForTimeout(200);
  check(
    'arrow keys move within the open menu',
    await page.evaluate(() => document.activeElement?.closest('menu.action-menu') !== null),
  );

  // Escape must close the menu and nothing else. App's global handler closes the open article on
  // Escape and cancels the default, so without the menu claiming the key first this both strands
  // the popover and throws the reader out of the article they were reading.
  await page.keyboard.press('Escape');
  await page.waitForTimeout(400);
  check('Escape closes the menu', (await openMenu().count()) === 0);
  check(
    'and does not also close the article underneath it',
    (await page.locator('.rd-app.is-reading').count()) === 1,
  );

  // --- Shift+K marks everything above the cursor -------------------------
  // Anchored by a click rather than by counting `j` presses: the reading pane fetches each article
  // it lands on and `goNext` does nothing until that resolves, so keyboard navigation is not
  // deterministic enough to assert an index against.
  //
  // The row is picked so that several above it are still unread. Anchoring on a row whose
  // neighbours are all read already would pass "marked everything above" by marking nothing.
  const initial = await rows();
  const at = initial.findIndex((_, i) => initial.slice(0, i).filter((r) => !r.read).length >= 3);
  check('the listing has a row with unread articles above it', at > 0, `index ${at}`);

  await page.locator('.rd-items .rd-item .rd-item-btn').nth(at).click();
  await page.waitForSelector('.rd-app.is-reading', { timeout: 5000 });
  await page.waitForTimeout(700);
  check('the click put the keyboard cursor on that row', (await indexOfSelected()) === at, `index ${await indexOfSelected()}`);

  // Scrolled deliberately: the gesture is "I have caught up to here", so ending up back at the top
  // of the list would defeat it.
  await page.evaluate(() => document.querySelector('#rd-list-scroll').scrollTo({ top: 120 }));
  await page.waitForTimeout(300);
  const scrollBefore = await scrollTop();

  const beforeAbove = await rows();
  await page.keyboard.press('Shift+K');
  await page.waitForTimeout(1200);
  await shot(page, 'after-shift-k');
  const afterAbove = await rows();

  const flippedAbove = changed(beforeAbove, afterAbove);
  check(
    'Shift+K marked every row above the cursor read',
    afterAbove.slice(0, at).every((r) => r.read),
    afterAbove
      .slice(0, at)
      .filter((r) => !r.read)
      .map((r) => r.title)
      .join(', '),
  );
  check(
    'and touched nothing at or below it',
    flippedAbove.every((i) => i < at),
    `rows ${flippedAbove.join(', ')} changed, cursor at ${at}`,
  );
  check(
    'and left the reader exactly where they were',
    (await scrollTop()) === scrollBefore,
    `${scrollBefore} → ${await scrollTop()}`,
  );
  check(
    'and kept the row count, rather than restarting the listing',
    afterAbove.length === beforeAbove.length,
    `${beforeAbove.length} → ${afterAbove.length}`,
  );

  const toastText = await page.locator('.toast-stack .toast-message').textContent();
  check(
    'a toast reported how many articles moved',
    /\d/.test(toastText ?? '') && (await page.locator('.toast-stack .toast').count()) === 1,
    JSON.stringify(toastText),
  );

  // --- the right-click menu anchors on the row clicked -------------------
  // Chosen below the cursor on purpose: if the menu anchored on the open article instead, the
  // boundary would land in the wrong place and the rows between the two would show it.
  const target = at + 6;
  check('there are rows below the cursor to right-click', target < afterAbove.length);

  await page.locator('.rd-items .rd-item').nth(target).click({ button: 'right' });
  // `ActionMenu` defers the popover by a tick: Chromium fires `contextmenu` before `pointerup`,
  // which would light-dismiss a menu opened synchronously from the same gesture.
  await page.waitForSelector('menu.action-menu', { timeout: 3000 });
  await shot(page, 'row-context-menu');
  check('a right-click offers the same two directions', (await menuItems()).length === 2);

  const box = await page.evaluate(() => {
    const r = document.querySelector('menu.action-menu').getBoundingClientRect();
    return { w: Math.round(r.width), h: Math.round(r.height), top: Math.round(r.top) };
  });
  check('the menu is positioned on screen', box.w > 0 && box.h > 0 && box.top >= 0, JSON.stringify(box));

  const beforeContext = await rows();
  await page.locator('menu.action-menu button[role^="menuitem"]').first().click();
  await page.waitForTimeout(1200);
  await shot(page, 'after-context-above');
  const afterContext = await rows();

  check('the menu closed after the action', (await openMenu().count()) === 0);
  const flippedContext = changed(beforeContext, afterContext);
  check(
    'the right-click menu marked everything above the row clicked',
    afterContext.slice(0, target).every((r) => r.read),
    afterContext
      .slice(0, target)
      .filter((r) => !r.read)
      .map((r) => `${r.title}`)
      .join(', '),
  );
  check(
    'and it anchored on the row clicked, not on the open article',
    flippedContext.every((i) => i < target) && flippedContext.some((i) => i >= at),
    `rows ${flippedContext.join(', ')} changed; clicked ${target}, open article ${at}`,
  );

  // The region is the list, so a right-click on the chrome around it belongs to the browser.
  await page.locator('.rd-list-head').click({ button: 'right' });
  await page.waitForTimeout(500);
  check('a right-click outside the list opens no menu', (await openMenu().count()) === 0);

  // --- the reading pane, and reaching rows nobody scrolled to ------------
  await page.evaluate(() => document.querySelector('#rd-list-scroll').scrollTo({ top: 0 }));
  await page.waitForTimeout(400);
  await page.locator('.rd-items .rd-item .rd-item-btn').nth(2).click();
  await page.waitForSelector('.rd-read .rd-article', { timeout: 10000 });
  await page.waitForTimeout(700);

  const aboveBtn = page.locator('.rd-read-bar button[title^="Mark all above as read"]');
  const belowBtn = page.locator('.rd-read-bar button[title^="Mark all below as read"]');
  check('the reading pane has a mark-above button', (await aboveBtn.count()) === 1);
  check('and a mark-below button', (await belowBtn.count()) === 1);

  // `ActionButton` is an inline grid sized to a hidden label sizer; the reader's icon buttons are
  // square. Without the override the button is as wide as the word "Working…".
  const geometry = await aboveBtn.evaluate((el) => {
    const r = el.getBoundingClientRect();
    return `${Math.round(r.width)}x${Math.round(r.height)}`;
  });
  const [w, h] = geometry.split('x').map(Number);
  check('the reading pane buttons stayed square icon buttons', Math.abs(w - h) < 2 && w > 0, geometry);
  await shot(page, 'reading-pane-buttons');

  // The list still holds one page while the scope holds more, so marking below a row near the top
  // is the case that proves the server placed the anchor rather than the client slicing what it had.
  const beforePane = await rows();
  const unreadLoadedBelow = beforePane.slice(3).filter((r) => !r.read).length;

  await belowBtn.click();
  await page.waitForTimeout(1800);
  await shot(page, 'after-reading-pane-below');

  const reported = await page.locator('.toast-stack .toast-message').textContent();
  const marked = Number((reported ?? '').match(/\d+/)?.[0] ?? NaN);
  check('the mark-below button reported a count', Number.isFinite(marked), JSON.stringify(reported));
  check(
    'and it reached articles below the fold, not just the loaded page',
    marked > unreadLoadedBelow,
    `marked ${marked}, but ${unreadLoadedBelow} unread rows were loaded below the anchor`,
  );

  const afterPane = await rows();
  check(
    'every loaded row below the open article is now read',
    afterPane.slice(3).every((r) => r.read),
    afterPane
      .slice(3)
      .filter((r) => !r.read)
      .map((r) => r.title)
      .join(', '),
  );
  check(
    'the open article was not swept up in it',
    (await page.locator('.rd-read .rd-article').count()) === 1,
  );
} catch (err) {
  failures.push(`threw: ${err instanceof Error ? err.message : String(err)}`);
} finally {
  await browser.close();
}

const passed = checks.filter((c) => c.ok).length;
console.log(`\n${passed}/${checks.length} checks passed`);
for (const c of checks) {
  if (!c.ok) console.log(`  FAIL  ${c.name}${c.detail ? ` — ${c.detail}` : ''}`);
}
// A throw mid-run is not a check, so it would otherwise be invisible: the summary would read
// "30/30 passed" while half the script never ran.
for (const f of failures) {
  if (f.startsWith('threw:')) console.log(`  THREW ${f}`);
}
// The pre-login `GET /me` is expected to 401; that is how the client finds out it is signed out.
const real = consoleErrors.filter((e) => !/favicon|404|401 \(Unauthorized\)/i.test(e));
if (real.length > 0) {
  console.log(`\n${real.length} console error(s):`);
  for (const e of real.slice(0, 10)) console.log(`  ${e}`);
}
if (failures.length > 0 || real.length > 0) process.exit(1);
console.log('\nNo console errors.');
