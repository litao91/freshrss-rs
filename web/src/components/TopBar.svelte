<script lang="ts">
  import {
    MAX_SIZE,
    MIN_SIZE,
    THEME_LABELS,
    nudgeSize,
    prefs,
    setDensity,
    setTheme,
    toggleNav,
    type Density,
    type ThemeName,
    type ThemePref,
  } from '../lib/prefs.svelte';
  import { session, signOut } from '../lib/session.svelte';

  interface Props {
    /** Current contents of `#rd-search`, bound so `/` can focus and fill it. */
    search: string;
    onSearch: (query: string) => void;
    onRefresh: () => void;
    onShowShortcuts: () => void;
    /** True while a refresh job is running; spins the icon. */
    refreshing: boolean;
  }

  let { search = $bindable(''), onSearch, onRefresh, onShowShortcuts, refreshing }: Props = $props();

  let searchEl: HTMLInputElement | undefined = $state();
  let displayMenu: HTMLDetailsElement | undefined = $state();
  let accountMenu: HTMLDetailsElement | undefined = $state();

  const themeOptions: { value: ThemePref; label: string }[] = [
    { value: 'kagit', label: THEME_LABELS.kagit },
    { value: 'sepya', label: THEME_LABELS.sepya },
    { value: 'gece', label: THEME_LABELS.gece },
    { value: 'auto', label: 'Auto' },
  ];

  const densities: { value: Density; label: string }[] = [
    { value: 'rahat', label: 'Comfortable' },
    { value: 'sikisik', label: 'Compact' },
  ];

  /** Exposed so the `/` shortcut can focus the box. */
  export function focusSearch(): void {
    searchEl?.focus();
    searchEl?.select();
  }

  function closeMenus(): void {
    if (displayMenu) displayMenu.open = false;
    if (accountMenu) accountMenu.open = false;
  }

  function submitSearch(event: Event): void {
    event.preventDefault();
    onSearch(search.trim());
    closeMenus();
  }

  async function handleSignOut(): Promise<void> {
    closeMenus();
    await signOut();
  }
</script>

<header class="rd-top">
  <button
    class="rd-icon-btn rd-nav-toggle"
    type="button"
    aria-expanded={prefs.navOpen}
    aria-controls="rd-nav"
    title="Menu"
    onclick={() => toggleNav()}
  >
    <i class="ri-menu-line" aria-hidden="true"></i>
    <span class="rd-sr">Menu</span>
  </button>

  <button
    class="rd-icon-btn rd-nav-dock"
    type="button"
    data-rd-nav-dock
    aria-expanded={prefs.navOpen}
    aria-controls="rd-nav"
    title="Subscriptions panel (b)"
    onclick={() => toggleNav()}
  >
    <i class="ri-sidebar-fold-line" aria-hidden="true"></i>
    <i class="ri-sidebar-unfold-line" aria-hidden="true"></i>
    <span class="rd-sr">Subscriptions panel</span>
  </button>

  <a class="rd-brand" href="/">
    <svg class="brand-mark" viewBox="8 4 48 55" aria-hidden="true" focusable="false">
      <path class="brand-mark-ink" d="M10.5 22h10v20h-10z"></path>
      <path
        class="brand-mark-accent"
        d="M12.5 12H28.5C38.5 12 46.5 11 53.5 6C49.5 18 39.5 23 26.5 23H10.5V14A2 2 0 0 1 12.5 12Z"
      ></path>
      <path class="brand-mark-ink" d="M10.5 27H24.5C32.5 27 37.5 26 42.5 22C39.5 32 32.5 37 22.5 37H10.5Z"></path>
      <path class="brand-mark-accent" d="M10.5 40H20.5V46C20.5 52.5 16 56.5 10.5 57Z"></path>
    </svg>
    <span class="rd-brand-name">freshrss-rs</span>
  </a>

  <form class="rd-search" role="search" onsubmit={submitSearch}>
    <label class="rd-sr" for="rd-search">Search articles</label>
    <i class="ri-search-line" aria-hidden="true"></i>
    <input
      id="rd-search"
      type="search"
      placeholder="Search articles"
      autocomplete="off"
      spellcheck="false"
      bind:this={searchEl}
      bind:value={search}
    />
  </form>

  <div class="rd-top-actions">
    <button
      class="rd-icon-btn"
      type="button"
      title="Refresh (r)"
      disabled={refreshing}
      onclick={() => {
        onRefresh();
        closeMenus();
      }}
    >
      <i class="ri-refresh-line" class:is-spinning={refreshing} aria-hidden="true"></i>
      <span class="rd-sr">Refresh</span>
    </button>

    <details class="rd-menu" bind:this={displayMenu}>
      <summary class="rd-icon-btn" title="Display">
        <i class="ri-font-size-2" aria-hidden="true"></i>
        <span class="rd-sr">Display</span>
      </summary>
      <div class="rd-menu-panel rd-display">
        <p class="rd-menu-label" id="rd-theme-label">Appearance</p>
        <div class="rd-themes" role="group" aria-labelledby="rd-theme-label">
          {#each themeOptions as option (option.value)}
            <button
              class="rd-theme-opt"
              type="button"
              data-feeddi-theme={option.value}
              aria-pressed={prefs.themePref === option.value}
              onclick={() => setTheme(option.value)}
            >
              <span class="rd-swatch" aria-hidden="true">
                {#if option.value === 'auto'}
                  <span class="rd-swatch-face" data-theme-preview="kagit"></span>
                  <span class="rd-swatch-face" data-theme-preview="gece"></span>
                {:else}
                  <span class="rd-swatch-face" data-theme-preview={option.value as ThemeName}></span>
                {/if}
              </span>
              {option.label}
            </button>
          {/each}
        </div>

        <p class="rd-menu-label" id="rd-size-label">Text size</p>
        <div class="rd-stepper" role="group" aria-labelledby="rd-size-label">
          <button
            class="rd-icon-btn"
            type="button"
            title="Smaller text (-)"
            disabled={prefs.size <= MIN_SIZE}
            onclick={() => nudgeSize(-1)}
          >
            <i class="ri-subtract-line" aria-hidden="true"></i>
            <span class="rd-sr">Smaller text</span>
          </button>
          <output class="rd-stepper-value" for="rd-search">{prefs.size}</output>
          <button
            class="rd-icon-btn"
            type="button"
            title="Larger text (+)"
            disabled={prefs.size >= MAX_SIZE}
            onclick={() => nudgeSize(1)}
          >
            <i class="ri-add-line" aria-hidden="true"></i>
            <span class="rd-sr">Larger text</span>
          </button>
        </div>

        <p class="rd-menu-label" id="rd-density-label">List density</p>
        <div class="rd-segment" role="group" aria-labelledby="rd-density-label">
          {#each densities as option (option.value)}
            <button
              type="button"
              data-rd-density={option.value}
              aria-pressed={prefs.density === option.value}
              onclick={() => setDensity(option.value)}
            >
              {option.label}
            </button>
          {/each}
        </div>

        <label class="rd-menu-label" for="rd-lang">Language</label>
        <select id="rd-lang" class="rd-input" data-feeddi-lang value={session.config?.language ?? 'en'}>
          <option value="en" lang="en">English</option>
        </select>
      </div>
    </details>

    <button class="rd-icon-btn rd-hide-touch" type="button" title="Keyboard shortcuts (?)" onclick={onShowShortcuts}>
      <i class="ri-keyboard-box-line" aria-hidden="true"></i>
      <span class="rd-sr">Keyboard shortcuts</span>
    </button>

    {#if session.user}
      <details class="rd-menu rd-menu--end" bind:this={accountMenu}>
        <summary class="rd-avatar" title="Account menu">
          <span aria-hidden="true">{session.user.initial}</span>
          <span class="rd-sr">Account menu</span>
        </summary>
        <div class="rd-menu-panel">
          <p class="rd-menu-user">{session.user.username}</p>
          <button class="rd-menu-item" type="button" onclick={handleSignOut}>
            <i class="ri-logout-box-r-line" aria-hidden="true"></i>
            Sign out
          </button>
        </div>
      </details>
    {/if}
  </div>
</header>
