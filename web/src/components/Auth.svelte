<script lang="ts">
  import { ApiError } from '../lib/api';
  import { signIn, signUp, session } from '../lib/session.svelte';
  import { notifyError } from '../lib/toast.svelte';

  let mode = $state<'login' | 'register'>('login');
  let username = $state('');
  let password = $state('');
  let email = $state('');
  let busy = $state(false);
  /** Field-level message, shown inline rather than as a toast because it belongs to the form. */
  let formError = $state<string | null>(null);

  const title = $derived(mode === 'login' ? 'Log in' : 'Create your account');
  const lead = $derived(
    mode === 'login'
      ? 'Sign in to your reader.'
      : 'Pick a username and password. The first account on this server becomes its administrator.',
  );
  const submitLabel = $derived(mode === 'login' ? 'Log in' : 'Sign up');

  function switchMode(next: 'login' | 'register'): void {
    mode = next;
    formError = null;
  }

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    if (busy) return;

    busy = true;
    formError = null;
    try {
      if (mode === 'login') {
        await signIn(username, password);
      } else {
        await signUp(username, password, email.trim() || undefined);
      }
    } catch (err) {
      // A 4xx is the user's input; anything else belongs in the toast, not under the field.
      if (err instanceof ApiError && err.status >= 400 && err.status < 500) {
        formError = err.message;
      } else {
        notifyError(err instanceof Error ? err.message : 'Could not reach the server');
        formError = 'Something went wrong. Please try again.';
      }
    } finally {
      busy = false;
    }
  }
</script>

<div class="rd-auth">
  <div class="rd-auth-card">
    <div class="rd-auth-brand">
      <svg class="brand-mark" viewBox="8 4 48 55" aria-hidden="true" focusable="false">
        <path class="brand-mark-ink" d="M10.5 22h10v20h-10z"></path>
        <path
          class="brand-mark-accent"
          d="M12.5 12H28.5C38.5 12 46.5 11 53.5 6C49.5 18 39.5 23 26.5 23H10.5V14A2 2 0 0 1 12.5 12Z"
        ></path>
        <path class="brand-mark-ink" d="M10.5 27H24.5C32.5 27 37.5 26 42.5 22C39.5 32 32.5 37 22.5 37H10.5Z"></path>
        <path class="brand-mark-accent" d="M10.5 40H20.5V46C20.5 52.5 16 56.5 10.5 57Z"></path>
      </svg>
      <span>freshrss-rs</span>
    </div>

    <h1>{title}</h1>
    <p class="rd-auth-lead">{lead}</p>

    {#if session.error}
      <p class="rd-field-error" role="alert">
        The server could not be reached: {session.error}
      </p>
    {/if}

    <form class="rd-auth-form" method="post" onsubmit={submit} novalidate>
      <div>
        <label class="rd-label" for="auth-username">
          {mode === 'login' ? 'Username or email' : 'Username'}
        </label>
        <input
          id="auth-username"
          class="rd-input"
          type="text"
          autocomplete="username"
          autocapitalize="none"
          spellcheck="false"
          required
          bind:value={username}
        />
      </div>

      {#if mode === 'register'}
        <div>
          <label class="rd-label" for="auth-email">Email <span class="rd-hint">(optional)</span></label>
          <input id="auth-email" class="rd-input" type="email" autocomplete="email" bind:value={email} />
        </div>
      {/if}

      <div>
        <label class="rd-label" for="auth-password">Password</label>
        <input
          id="auth-password"
          class="rd-input"
          type="password"
          autocomplete={mode === 'login' ? 'current-password' : 'new-password'}
          minlength={8}
          required
          bind:value={password}
        />
      </div>

      {#if formError}
        <p class="rd-field-error" role="alert">{formError}</p>
      {/if}

      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy}>
        {#if busy}Working…{:else}{submitLabel}{/if}
      </button>
    </form>

    <p class="rd-auth-switch">
      {#if mode === 'login'}
        Don't have an account?
        <button type="button" onclick={() => switchMode('register')}>Sign up</button>
      {:else}
        Already have an account?
        <button type="button" onclick={() => switchMode('login')}>Log in</button>
      {/if}
    </p>
  </div>
</div>
