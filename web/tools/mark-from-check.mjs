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
 * list is windowed, so rows are addressed by the `data-index` `VirtualList` puts on each row's
 * wrapper — a listing position, not a position in the DOM — and the run mutates read state, so
 * re-seed before repeating it.
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

/** The rendered window, keyed by listing position rather than by place in the DOM. */
const rows = () =>
  page.evaluate(() =>
    [...document.querySelectorAll('.rd-items .rd-item')].map((el) => ({
      id: el.id.replace('rd-item-', ''),
      title: el.querySelector('.rd-item-title')?.textContent ?? '',
      read: el.classList.contains('is-read'),
      selected: el.classList.contains('is-selected'),
      index: Number(el.parentElement?.dataset.index ?? -1),
    })),
  );

/** Read state per listing index, so two windows can be compared row for row. */
const byIndex = (list) => new Map(list.map((r) => [r.index, r.read]));

/** Indexes whose read state differs between two snapshots of possibly different windows. */
function flipped(before, after) {
  const out = [];
  for (const [index, read] of after) {
    if (before.has(index) && before.get(index) !== read) out.push(index);
  }
  return out.sort((a, b) => a - b);
}

const rowHeight = () =>
  page.evaluate(
    () => document.querySelector('.rd-items .rd-item')?.parentElement?.getBoundingClientRect().height ?? 0,
  );

let rowHeightCached = 0;

const scrollToIndex = (index) =>
  page.evaluate(
    ([top, size]) => document.querySelector('#rd-list-scroll').scrollTo({ top: top * size }),
    [index, rowHeightCached],
  );

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
  rowHeightCached = await rowHeight();
  check('the reader signed in and listed articles', (await rows()).length > 0);
  check('every rendered row is exactly one row tall', rowHeightCached > 40 && rowHeightCached < 140, `${rowHeightCached}px`);

  // The gestures split a listing, so every row has to stay put: under the unread filter a row that
  // becomes read would otherwise be the thing under test and the thing that vanishes at once.
  await page.keyboard.press('u');
  await page.waitForTimeout(1200);
  rowHeightCached = await rowHeight();
  check('the All filter is on, so read rows stay visible', (await rows()).some((r) => r.read));

  // Asserted up front, before anything scrolls: reaching rows the reader has not loaded is the
  // entire reason the anchor is resolved on the server instead of by slicing `entries.items`.
  check(
    'the listing holds pages that are not loaded',
    (await page.locator('.rd-list-end .rd-btn').count()) > 0,
  );
  const windowed = await rows();
  check(
    'and the list renders a window rather than everything',
    windowed.length > 4 && windowed.length < 40,
    `${windowed.length} rows in the window`,
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
  // `light-dark(#fff, #2a2a2e)` default, which would sit visibly apart from the panels beside it.
  const menuBg = await page.evaluate(() =>
    getComputedStyle(document.querySelector('menu.action-menu')).backgroundColor,
  );
  const surface = await tokenRgb('--fd-surface');
  check('the menu is painted with the reader’s own surface token', menuBg === surface, `${menuBg} vs ${surface}`);

  // Arrow keys and Escape are the library's job now; what matters is that they still work here.
  await page.keyboard.press('ArrowDown');
  await page.waitForTimeout(200);
  check(
    'arrow keys move within the open menu',
    await page.evaluate(() => document.activeElement?.closest('menu.action-menu') !== null),
  );
  await page.keyboard.press('Escape');
  await page.waitForTimeout(400);
  check('Escape closes the menu', (await openMenu().count()) === 0);
  check(
    'and does not also close the article underneath it',
    (await page.locator('.rd-app.is-reading').count()) === 1,
  );

  // --- Shift+K marks everything above the cursor -------------------------
  // Row 12 sits past the seeded read rows, so there is something above it to mark, and it is
  // reached by scrolling the window rather than by assuming the whole listing is in the DOM.
  const ANCHOR = 12;
  await scrollToIndex(6);
  await page.waitForTimeout(500);
  await page.locator(`[data-index="${ANCHOR}"] .rd-item-btn`).click();
  await page.waitForTimeout(900);
  check(
    'the click put the keyboard cursor on that row',
    (await rows()).some((r) => r.index === ANCHOR && r.selected),
  );

  // Scrolled deliberately: the gesture is "I have caught up to here", so ending up back at the top
  // of the list would defeat it.
  const scrollBefore = await scrollTop();
  const beforeAbove = byIndex(await rows());

  await page.keyboard.press('Shift+K');
  await page.waitForTimeout(1200);
  await shot(page, 'after-shift-k');
  const afterAbove = byIndex(await rows());

  check(
    'Shift+K marked every rendered row above the cursor read',
    [...afterAbove.entries()].every(([index, read]) => index >= ANCHOR || read),
    [...afterAbove.entries()]
      .filter(([index, read]) => index < ANCHOR && !read)
      .map(([index]) => index)
      .join(', '),
  );
  check(
    'and touched nothing at or below it',
    flipped(beforeAbove, afterAbove).every((index) => index < ANCHOR),
    `rows ${flipped(beforeAbove, afterAbove).join(', ')} changed, cursor at ${ANCHOR}`,
  );
  check(
    'and left the reader exactly where they were',
    (await scrollTop()) === scrollBefore,
    `${scrollBefore} → ${await scrollTop()}`,
  );

  const toastText = await page.locator('.toast-stack .toast-message').textContent();
  check(
    'a toast reported how many articles moved',
    /\d/.test(toastText ?? '') && (await page.locator('.toast-stack .toast').count()) === 1,
    JSON.stringify(toastText),
  );

  // --- the right-click menu anchors on the row clicked -------------------
  // Below the cursor on purpose: if the menu anchored on the open article instead, the boundary
  // would land in the wrong place and the rows between the two would show it.
  const TARGET = ANCHOR + 6;
  await scrollToIndex(ANCHOR - 2);
  await page.waitForTimeout(500);
  const beforeContext = byIndex(await rows());

  await page.locator(`[data-index="${TARGET}"] .rd-item`).click({ button: 'right' });
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

  await page.locator('menu.action-menu button[role^="menuitem"]').first().click();
  await page.waitForTimeout(1200);
  await shot(page, 'after-context-above');
  const afterContext = byIndex(await rows());

  check('the menu closed after the action', (await openMenu().count()) === 0);
  check(
    'the right-click menu marked every rendered row above the row clicked',
    [...afterContext.entries()].every(([index, read]) => index >= TARGET || read),
    [...afterContext.entries()]
      .filter(([index, read]) => index < TARGET && !read)
      .map(([index]) => index)
      .join(', '),
  );
  check(
    'and it anchored on the row clicked, not on the open article',
    flipped(beforeContext, afterContext).some((index) => index > ANCHOR) &&
      flipped(beforeContext, afterContext).every((index) => index < TARGET),
    `rows ${flipped(beforeContext, afterContext).join(', ')} changed; clicked ${TARGET}, open article ${ANCHOR}`,
  );

  // The region is the list, so a right-click on the chrome around it belongs to the browser.
  await page.locator('.rd-list-head').click({ button: 'right' });
  await page.waitForTimeout(500);
  check('a right-click outside the list opens no menu', (await openMenu().count()) === 0);

  // --- the reading pane, and reaching rows nobody scrolled to ------------
  await page.evaluate(() => document.querySelector('#rd-list-scroll').scrollTo({ top: 0 }));
  await page.waitForTimeout(500);
  await page.locator('[data-index="2"] .rd-item-btn').click();
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

  // The window holds a slice of one page while the scope holds several pages, so marking below a
  // row near the top is the case that proves the server placed the anchor rather than the client
  // slicing what it had rendered.
  const beforePane = await rows();
  const unreadRenderedBelow = beforePane.filter((r) => r.index > 2 && !r.read).length;

  await belowBtn.click();
  await page.waitForTimeout(1800);
  await shot(page, 'after-reading-pane-below');

  const reported = await page.locator('.toast-stack .toast-message').textContent();
  const marked = Number((reported ?? '').match(/\d+/)?.[0] ?? NaN);
  check('the mark-below button reported a count', Number.isFinite(marked), JSON.stringify(reported));
  check(
    'and it reached articles outside the rendered window',
    marked > unreadRenderedBelow,
    `marked ${marked}, but only ${unreadRenderedBelow} unread rows were rendered below the anchor`,
  );

  const afterPane = await rows();
  check(
    'every rendered row below the open article is now read',
    afterPane.every((r) => r.index <= 2 || r.read),
    afterPane
      .filter((r) => r.index > 2 && !r.read)
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
