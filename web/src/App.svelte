<script lang="ts">
  import { onMount } from 'svelte';

  // Placeholder shell for stage S0. The real reader (top bar, nav tree, entry list, reading
  // pane, dialogs) replaces this as later stages land.
  let version = $state<{ name: string; version: string; schema: number } | null>(null);
  let error = $state<string | null>(null);

  onMount(async () => {
    try {
      const res = await fetch('/version');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      version = await res.json();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
    }
  });
</script>

<div class="rd-app">
  <header class="rd-top">
    <a class="rd-brand" href="/">
      <svg class="brand-mark" viewBox="8 4 48 55" aria-hidden="true" focusable="false">
        <path class="brand-mark-ink" d="M10.5 22h10v20h-10z"></path>
        <path
          class="brand-mark-accent"
          d="M12.5 12H28.5C38.5 12 46.5 11 53.5 6C49.5 18 39.5 23 26.5 23H10.5V14A2 2 0 0 1 12.5 12Z"
        ></path>
        <path
          class="brand-mark-ink"
          d="M10.5 27H24.5C32.5 27 37.5 26 42.5 22C39.5 32 32.5 37 22.5 37H10.5Z"
        ></path>
        <path class="brand-mark-accent" d="M10.5 40H20.5V46C20.5 52.5 16 56.5 10.5 57Z"></path>
      </svg>
      <span class="rd-brand-name">freshrss-rs</span>
    </a>
  </header>

  <nav id="rd-nav" class="rd-nav" aria-label="Subscriptions">
    <div class="rd-nav-inner">
      <p class="rd-nav-empty">Subscriptions appear here once you sign in.</p>
    </div>
  </nav>

  <section id="icerik" class="rd-list">
    <div class="rd-list-head">
      <div class="rd-list-title-row">
        <h1 id="rd-list-title" class="rd-list-title">Reader</h1>
      </div>
    </div>
    <div id="rd-list-scroll" class="rd-list-body">
      <div class="rd-list-empty">
        <i class="ri-rss-line" aria-hidden="true"></i>
        {#if error}
          <p>Could not reach the API: {error}</p>
        {:else if version}
          <p>Backend {version.name} {version.version}, schema v{version.schema}.</p>
        {:else}
          <p>Contacting the backend…</p>
        {/if}
      </div>
    </div>
  </section>
</div>
