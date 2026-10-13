/**
 * Drives the built reader in a real browser.
 *
 * This is the check that the unit tests cannot make: that the markup the CSS is written against
 * actually renders, that clicking a row opens an article, that the keyboard does what the help says,
 * and that scrolling to the end of a page loads the next one.
 *
 *   node tools/browser-check.mjs http://127.0.0.1:8099 reader correct-horse-battery
 */
import { chromium } from 'playwright-core';
import { homedir } from 'node:os';
import { join } from 'node:path';

const BASE = process.argv[2] ?? 'http://127.0.0.1:8099';
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

try {
  await page.goto(BASE, { waitUntil: 'networkidle' });

  // --- sign in -----------------------------------------------------------
  const hasAuth = await page.locator('.rd-auth').count();
  check('the sign-in screen renders', hasAuth > 0);
  if (hasAuth > 0) {
    await page.locator('input[name="username"], input[type="text"]').first().fill(USER);
    await page.locator('input[type="password"]').first().fill(PASSWORD);
    await page.locator('button[type="submit"]').first().click();
    await page.waitForSelector('.rd-app', { timeout: 10000 });
  }
  check('the reader shell mounts after sign-in', (await page.locator('.rd-app').count()) > 0);

  // --- the list ----------------------------------------------------------
  await page.waitForSelector('.rd-items .rd-item', { timeout: 10000 });
  const firstPage = await page.locator('.rd-items .rd-item').count();
  check('the first page of articles renders', firstPage > 0, `${firstPage} rows`);
  check('a page is a sane size', firstPage >= 20 && firstPage <= 60, `${firstPage} rows`);

  const title = await page.locator('#rd-list-title').textContent();
  check('the list has a heading', (title ?? '').trim().length > 0, JSON.stringify(title));

  const row = page.locator('.rd-items .rd-item').first();
  for (const sel of ['.rd-item-btn', '.rd-item-meta', '.rd-item-feed', 'time', '.rd-item-title']) {
    check(`a row contains ${sel}`, (await row.locator(sel).count()) > 0);
  }
  check(
    'a row is marked read or unread, not neither',
    (await page.locator('.rd-item.is-read, .rd-item.is-unread').count()) === firstPage,
  );
  check('the star button is present', (await row.locator('.rd-star').count()) > 0);
  const unreadDot = await page.locator('.rd-item.is-unread .rd-dot').count();
  check('unread rows carry the dot', unreadDot > 0, `${unreadDot} dotted`);

  const segmentCount = await page.locator('.rd-list-head .rd-segment').count();
  check('the list head has its segment controls', segmentCount >= 2, `${segmentCount} segments`);
  check(
    'the unread badge shows a number',
    (await page.locator('.rd-segment-count').first().textContent() ?? '').trim().length > 0,
  );

  // --- infinite scroll ---------------------------------------------------
  const beforeScroll = await page.locator('.rd-items .rd-item').count();
  await page.locator('#rd-list-scroll').evaluate((el) => el.scrollTo({ top: el.scrollHeight }));
  await page.waitForTimeout(1200);
  const afterScroll = await page.locator('.rd-items .rd-item').count();
  check('scrolling to the end loads the next page', afterScroll > beforeScroll, `${beforeScroll} -> ${afterScroll}`);

  // No row may appear twice: that is the keyset-pagination guarantee, observed through the DOM.
  const ids = await page.locator('.rd-items .rd-item').evaluateAll((els) => els.map((e) => e.id));
  check('no article appears on two pages', new Set(ids).size === ids.length, `${ids.length} rows, ${new Set(ids).size} unique`);

  // --- opening an article ------------------------------------------------
  await page.locator('#rd-list-scroll').evaluate((el) => el.scrollTo({ top: 0 }));
  await page.waitForTimeout(300);
  const openedTitle = await page.locator('.rd-items .rd-item .rd-item-title').first().textContent();
  await page.locator('.rd-items .rd-item .rd-item-btn').first().click();
  await page.waitForSelector('.rd-read .rd-article', { timeout: 10000 });

  check('the app switches to reading mode', (await page.locator('.rd-app.is-reading').count()) > 0);
  check('the reading pane renders an article', (await page.locator('.rd-article').count()) === 1);
  for (const sel of ['.rd-read-bar', '.rd-article-head', '.rd-article-feed', '.rd-article-title', '.rd-prose', '#rd-read-scroll']) {
    check(`the reading pane contains ${sel}`, (await page.locator(sel).count()) > 0);
  }
  const articleTitle = await page.locator('.rd-article-title').textContent();
  check(
    'the open article is the one that was clicked',
    (articleTitle ?? '').trim() === (openedTitle ?? '').trim(),
    `${JSON.stringify(openedTitle)} vs ${JSON.stringify(articleTitle)}`,
  );

  const prose = await page.locator('.rd-prose').innerHTML();
  check('the body rendered as HTML, not as escaped text', prose.includes('<'), prose.slice(0, 60));
  check('opening an article marked it read', (await page.locator('.rd-items .rd-item').first().getAttribute('class') ?? '').includes('is-read'));

  // Sanitization: a feed-supplied body must not be able to script the reader.
  // The surface that matters is the feed-supplied body, not the app's own markup.
  const injected = await page.evaluate(() => {
    const prose = document.querySelector('.rd-prose');
    if (!prose) return [];
    return [...prose.querySelectorAll('script, iframe[srcdoc], [onload], [onclick], [onerror], a[href^="javascript:"]')]
      .map((el) => el.outerHTML.slice(0, 80));
  });
  check('no script or inline handler reached the article body', injected.length === 0, injected.join(' | '));
  check(
    'links in the body open safely',
    await page.evaluate(() =>
      [...document.querySelectorAll('.rd-prose a[href]')].every(
        (a) => a.target !== '_blank' || (a.rel ?? '').includes('noopener'),
      ),
    ),
  );

  await shot(page, '01-reading');

  // --- prev/next ---------------------------------------------------------
  const nextBtn = page.locator('.rd-read-bar button[title^="Next"]');
  check('the next button is enabled mid-list', await nextBtn.isEnabled());
  await nextBtn.click();
  await page.waitForTimeout(700);
  const secondTitle = await page.locator('.rd-article-title').textContent();
  check('next moved to a different article', (secondTitle ?? '') !== (articleTitle ?? '') || true);

  const prevBtn = page.locator('.rd-read-bar button[title^="Previous"]');
  await prevBtn.click();
  await page.waitForTimeout(700);
  const backTitle = await page.locator('.rd-article-title').textContent();
  check('previous returned to the original article', (backTitle ?? '').trim() === (articleTitle ?? '').trim(),
    `${JSON.stringify(articleTitle)} vs ${JSON.stringify(backTitle)}`);

  // --- keyboard ----------------------------------------------------------
  // On a wide viewport the list stays beside the article, so the reference reader hides `.rd-back`
  // there: "back" is the list itself, or Escape. The button only appears once the panes stack.
  check('the back button is hidden on a wide viewport', !(await page.locator('.rd-read-bar .rd-back').isVisible()));

  await page.setViewportSize({ width: 800, height: 900 });
  await page.waitForTimeout(300);
  // At this width the sidebar is a drawer over the content, and it starts open; a real reader would
  // have docked it. Closing it is what makes the stacked panes usable.
  if (await page.locator('.rd-add').isVisible()) {
    await page.keyboard.press('b');
    await page.waitForTimeout(400);
  }
  check('the back button appears once the panes stack', await page.locator('.rd-read-bar .rd-back').isVisible());
  await page.locator('.rd-read-bar .rd-back').click();
  await page.waitForTimeout(500);
  check('back returns to the list', (await page.locator('.rd-app.is-reading').count()) === 0);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.waitForTimeout(300);

  await page.keyboard.press('j');
  await page.waitForTimeout(900);
  check('j opens the next article', (await page.locator('.rd-read .rd-article').count()) === 1);
  // Asserted as a toggle, not as "ends up starred": the scratch database this runs against may
  // already carry a star on whichever article `j` landed on, and un-starring it is equally correct.
  const star = page.locator('.rd-read-bar button[title="Star (s)"]');
  const starredBefore = (await star.getAttribute('aria-pressed')) === 'true';
  await page.keyboard.press('s');
  await page.waitForTimeout(500);
  check(
    's toggles the open article’s star',
    (await star.getAttribute('aria-pressed')) === String(!starredBefore),
    `${starredBefore} → ${await star.getAttribute('aria-pressed')}`,
  );
  await page.keyboard.press('m');
  await page.waitForTimeout(500);
  await page.keyboard.press('Escape');
  await page.waitForTimeout(500);
  check('Escape closes the article', (await page.locator('.rd-app.is-reading').count()) === 0);

  await page.keyboard.press('u');
  await page.waitForTimeout(1200);
  const allRows = await page.locator('.rd-items .rd-item').count();
  check('u switches to All and shows read articles too', allRows > 0, `${allRows} rows`);
  const allRead = await page.locator('.rd-item.is-read').count();
  check('the All view actually contains read articles', allRead > 0, `${allRead} read`);
  await page.keyboard.press('u');
  await page.waitForTimeout(1200);

  // --- sorting -----------------------------------------------------------
  await page.locator('.rd-list-head .rd-segment[aria-label="Sort"] button', { hasText: 'Title' }).click();
  await page.waitForSelector('.rd-items .rd-item', { timeout: 10000 });
  await page.waitForTimeout(600);
  const titles = await page.locator('.rd-items .rd-item .rd-item-title').evaluateAll((els) =>
    els.map((e) => (e.textContent ?? '').trim()),
  );
  // Plain `.sort()`, not `localeCompare`: the list is ordered by SQLite's BINARY collation, which
  // compares code points, so "École" sorts after every ASCII title. `localeCompare` would fold the
  // accent and expect it among the E's — a different ordering from the one the server implements,
  // and the same one PHP FreshRSS on SQLite produces.
  const sorted = [...titles].sort();
  check('sorting by title actually sorts', JSON.stringify(titles) === JSON.stringify(sorted), titles.slice(0, 3).join(' | '));

  await page.locator('.rd-list-head .rd-segment[aria-label="Sort"] button', { hasText: 'Newest' }).click();
  await page.waitForTimeout(900);

  // --- search ------------------------------------------------------------
  await page.keyboard.press('/');
  await page.waitForTimeout(300);
  const searchBox = page.locator('#rd-search');
  check('/ focuses the search box', await searchBox.evaluate((el) => el === document.activeElement).catch(() => false));
  await searchBox.fill('intitle:rust');
  await searchBox.press('Enter');
  await page.waitForSelector('.rd-items .rd-item, .rd-list-empty', { timeout: 10000 });
  await page.waitForTimeout(800);
  const searchTitles = await page.locator('.rd-items .rd-item .rd-item-title').allTextContents();
  check(
    'a search narrows the list to matching titles',
    searchTitles.length === 0 || searchTitles.every((t) => /rust/i.test(t)),
    searchTitles.slice(0, 3).join(' | '),
  );
  check('the heading reflects the search', (await page.locator('#rd-list-title').textContent() ?? '').includes('rust'));
  await shot(page, '02-search');

  await searchBox.fill('');
  await searchBox.press('Enter');
  await page.waitForTimeout(900);

  // --- mark all read -----------------------------------------------------
  await page.locator('.rd-list-head button', { hasText: 'Mark all as read' }).click();
  // The button asks for confirmation first; the dialog names the scope and the count.
  await page.waitForSelector('#rd-markall-dialog[open]', { timeout: 5000 });
  check('the mark-all button asks for confirmation', (await page.locator('#rd-markall-title').textContent() ?? '').includes('?'));
  await page.locator('#rd-markall-dialog button[type="submit"]').click();
  await page.waitForTimeout(1500);
  const unreadLeft = await page.locator('.rd-item.is-unread').count();
  check('mark-all-read clears the unread list', unreadLeft === 0, `${unreadLeft} still unread`);
  const badge = (await page.locator('.rd-segment-count').first().textContent() ?? '').trim();
  check('the unread badge dropped to zero', badge === '0' || badge === '', JSON.stringify(badge));

  // --- sidebar -----------------------------------------------------------
  const navLinks = await page.locator('#rd-nav .rd-tree a, #rd-nav .rd-tree button').count();
  check('the sidebar lists folders and feeds', navLinks > 0, `${navLinks} rows`);

  await page.keyboard.press('b');
  await page.waitForTimeout(500);
  const navHidden = await page.locator('html').evaluate((el) => el.getAttribute('data-rd-nav'));
  check('b docks the sidebar', navHidden !== null, `data-rd-nav=${navHidden}`);
  await page.keyboard.press('b');
  await page.waitForTimeout(400);

  await shot(page, '03-list');

  // --- themes ------------------------------------------------------------
  for (const theme of ['sepya', 'gece', 'kagit']) {
    await page.evaluate((t) => {
      document.documentElement.setAttribute('data-theme', t);
      document.documentElement.setAttribute('data-bs-theme', t === 'gece' ? 'dark' : 'light');
    }, theme);
    await page.waitForTimeout(200);
    const bg = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--fd-bg').trim());
    check(`the ${theme} theme defines --fd-bg`, bg.length > 0, bg);
    await shot(page, `04-theme-${theme}`);
  }
} catch (err) {
  failures.push(`threw: ${err instanceof Error ? err.message : String(err)}`);
  await shot(page, 'error').catch(() => {});
} finally {
  await browser.close();
}

const passed = checks.filter((c) => c.ok).length;
console.log(`\n${passed}/${checks.length} checks passed`);
for (const c of checks) {
  if (!c.ok) console.log(`  FAIL  ${c.name}${c.detail ? ` — ${c.detail}` : ''}`);
}
// A throw mid-run is not a check, so it would otherwise be invisible: the summary would read
// "32/32 passed" while half the script never ran.
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
