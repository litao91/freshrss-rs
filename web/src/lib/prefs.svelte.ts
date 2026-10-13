/**
 * Reader preferences: theme, text size, list density, sidebar dock and focus mode.
 *
 * These live on `<html>` as `data-*` attributes and a single CSS custom property, exactly as the
 * reference reader does, so the inline script in `index.html` can apply them before first paint and
 * the stylesheet never needs a component to cooperate.
 *
 * The localStorage keys are the reference reader's own (`feeddi-*`) so a user's saved preferences
 * carry over.
 */

export type ThemeName = 'kagit' | 'sepya' | 'gece';
export type ThemePref = ThemeName | 'auto';
export type Density = 'rahat' | 'sikisik';
export type Toggle = 'acik' | 'kapali';
/** The UI languages `lib/i18n.ts` has a dictionary for. */
export type Language = 'en' | 'tr';

const KEYS = {
  theme: 'feeddi-theme',
  size: 'feeddi-reader-size',
  density: 'feeddi-reader-density',
  nav: 'feeddi-reader-nav',
  focus: 'feeddi-reader-focus',
  lang: 'feeddi-lang',
} as const;

export const THEME_LABELS: Record<ThemeName, string> = {
  kagit: 'Paper',
  sepya: 'Sepia',
  gece: 'Night',
};

export const MIN_SIZE = 15;
export const MAX_SIZE = 28;
export const DEFAULT_SIZE = 19;

/** Named steps the reference reader's size control offers; anything else is a raw pixel size. */
const SIZE_PRESETS: Record<string, number> = { s: 17, m: 19, l: 21 };

export const prefs = $state({
  themePref: 'auto' as ThemePref,
  /** The theme actually applied, which differs from `themePref` when it is `auto`. */
  theme: 'kagit' as ThemeName,
  size: DEFAULT_SIZE,
  density: 'rahat' as Density,
  navOpen: true,
  focus: false,
  /**
   * The UI language. It lives here rather than in `i18n.ts` because this module is compiled with
   * runes, so `t()` reading `prefs.language` is what makes every translated string re-render when
   * the `#rd-lang` select changes; a plain `.ts` module cannot hold reactive state.
   */
  language: 'en' as Language,
});

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    // Private browsing modes can throw on localStorage access; fall back to defaults rather than
    // breaking the whole reader.
    return null;
  }
}

function write(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // A quota or permission error must not stop the preference from applying for this session.
  }
}

function prefersDark(): boolean {
  return typeof window !== 'undefined' && !!window.matchMedia?.('(prefers-color-scheme: dark)').matches;
}

function resolveTheme(pref: ThemePref): ThemeName {
  if (pref === 'auto') return prefersDark() ? 'gece' : 'kagit';
  return pref;
}

function applyTheme(): void {
  const root = document.documentElement;
  const theme = resolveTheme(prefs.themePref);
  prefs.theme = theme;
  root.setAttribute('data-theme', theme);
  root.setAttribute('data-bs-theme', theme === 'gece' ? 'dark' : 'light');
  root.setAttribute('data-theme-pref', prefs.themePref);
  // Keep the browser chrome (mobile address bar) in step with the page.
  const meta = document.querySelector('meta[name="theme-color"]');
  const color = theme === 'gece' ? '#16130f' : theme === 'sepya' ? '#f1e6cf' : '#f6f2ea';
  meta?.setAttribute('content', color);
}

function applySize(): void {
  const clamped = Math.min(MAX_SIZE, Math.max(MIN_SIZE, prefs.size));
  prefs.size = clamped;
  document.documentElement.style.setProperty('--rd-prose-size', `${clamped / 16}rem`);
}

function applyDensity(): void {
  document.documentElement.setAttribute('data-rd-density', prefs.density);
}

function applyNav(): void {
  document.documentElement.setAttribute('data-rd-nav', prefs.navOpen ? 'acik' : 'kapali');
}

function applyFocus(): void {
  document.documentElement.setAttribute('data-rd-focus', prefs.focus ? 'acik' : 'kapali');
}

function applyLanguage(): void {
  // Keeps `lang` on <html> in step so hyphenation, screen readers and the browser's own
  // translate offer follow the language the reader actually chose.
  document.documentElement.setAttribute('lang', prefs.language);
}

/** Reads persisted preferences and applies them. Call once at startup, after `index.html`'s script. */
export function initPrefs(): void {
  const storedTheme = read(KEYS.theme);
  if (storedTheme === 'kagit' || storedTheme === 'sepya' || storedTheme === 'gece' || storedTheme === 'auto') {
    prefs.themePref = storedTheme;
  }

  const storedSize = read(KEYS.size);
  if (storedSize !== null) {
    const preset = SIZE_PRESETS[storedSize];
    const parsed = preset ?? Number.parseInt(storedSize, 10);
    if (Number.isFinite(parsed)) prefs.size = parsed;
  }

  const storedDensity = read(KEYS.density);
  if (storedDensity === 'rahat' || storedDensity === 'sikisik') prefs.density = storedDensity;

  prefs.navOpen = read(KEYS.nav) !== 'kapali';
  prefs.focus = read(KEYS.focus) === 'acik';

  const storedLang = read(KEYS.lang);
  if (storedLang === 'en' || storedLang === 'tr') prefs.language = storedLang;

  applyTheme();
  applySize();
  applyDensity();
  applyNav();
  applyFocus();
  applyLanguage();

  // Follow the OS when the preference is `auto`, so flipping dark mode mid-session is picked up.
  window.matchMedia?.('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (prefs.themePref === 'auto') applyTheme();
  });
}

export function setTheme(pref: ThemePref): void {
  prefs.themePref = pref;
  write(KEYS.theme, pref);
  applyTheme();
}

export function cycleTheme(): void {
  const order: ThemePref[] = ['kagit', 'sepya', 'gece', 'auto'];
  const next = order[(order.indexOf(prefs.themePref) + 1) % order.length];
  setTheme(next);
}

export function setSize(size: number): void {
  prefs.size = size;
  applySize();
  write(KEYS.size, String(prefs.size));
}

export function nudgeSize(delta: number): void {
  setSize(prefs.size + delta);
}

export function setDensity(density: Density): void {
  prefs.density = density;
  applyDensity();
  write(KEYS.density, density);
}

/** `b` and the dock button both call this. */
export function toggleNav(): void {
  prefs.navOpen = !prefs.navOpen;
  applyNav();
  write(KEYS.nav, prefs.navOpen ? 'acik' : 'kapali');
}

export function setNavOpen(open: boolean): void {
  if (prefs.navOpen === open) return;
  toggleNav();
}

/** `f`: hides the list while reading. */
export function toggleFocus(): void {
  prefs.focus = !prefs.focus;
  applyFocus();
  write(KEYS.focus, prefs.focus ? 'acik' : 'kapali');
}

/** The `#rd-lang` select calls this; every `t()` caller re-renders because `prefs` is reactive. */
export function setLanguage(language: Language): void {
  if (prefs.language === language) return;
  prefs.language = language;
  applyLanguage();
  write(KEYS.lang, language);
}
