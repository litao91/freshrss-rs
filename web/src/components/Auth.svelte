<script lang="ts">
  import { ApiError } from '../lib/api';
  import { t } from '../lib/i18n';
  import { signIn, signUp, session } from '../lib/session.svelte';
  import { notifyError } from '../lib/toast.svelte';

  let mode = $state<'login' | 'register'>('login');
  let username = $state('');
  let password = $state('');
  let email = $state('');
  let busy = $state(false);
  /** Field-level message, shown inline rather than as a toast because it belongs to the form. */
  let formError = $state<string | null>(null);

  /**
   * A server configured `registration.mode = "closed"` (or a `first-user-only` server that already
   * has its account) refuses every sign-up, so offering the form would only set the visitor up for
   * a refusal. Unknown policy — the probe failed — keeps the form: the endpoint stays the authority.
   */
  const canRegister = $derived(session.policy?.can_register !== false);
  const active = $derived(canRegister ? mode : 'login');

  const title = $derived(active === 'login' ? t('auth.loginTitle') : t('auth.registerTitle'));
  const lead = $derived(active === 'login' ? t('auth.loginLead') : t('auth.registerLead'));
  const submitLabel = $derived(active === 'login' ? t('auth.loginSubmit') : t('auth.registerSubmit'));

  function switchMode(next: 'login' | 'register'): void {
    if (next === 'register' && !canRegister) return;
    mode = next;
    formError = null;
  }

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    if (busy) return;

    busy = true;
    formError = null;
    try {
      if (active === 'login') {
        await signIn(username, password);
      } else {
        await signUp(username, password, email.trim() || undefined);
      }
    } catch (err) {
      // A 4xx is the user's input; anything else belongs in the toast, not under the field.
      if (err instanceof ApiError && err.status >= 400 && err.status < 500) {
        formError = err.message;
      } else {
        notifyError(err instanceof Error ? err.message : t('auth.unreachable'));
        formError = t('auth.wentWrong');
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
        {t('auth.unreachable')} {session.error}
      </p>
    {/if}

    <form class="rd-auth-form" method="post" onsubmit={submit} novalidate>
      <div>
        <label class="rd-label" for="auth-username">
          {active === 'login' ? 'Username or email' : 'Username'}
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

      {#if active === 'register'}
        <div>
          <label class="rd-label" for="auth-email">{t('auth.email')} <span class="rd-hint">{t('auth.emailOptional')}</span></label>
          <input id="auth-email" class="rd-input" type="email" autocomplete="email" bind:value={email} />
        </div>
      {/if}

      <div>
        <label class="rd-label" for="auth-password">{t('auth.password')}</label>
        <input
          id="auth-password"
          class="rd-input"
          type="password"
          autocomplete={active === 'login' ? 'current-password' : 'new-password'}
          minlength={8}
          required
          bind:value={password}
        />
      </div>

      {#if formError}
        <p class="rd-field-error" role="alert">{formError}</p>
      {/if}

      <button class="rd-btn rd-btn--primary" type="submit" disabled={busy}>
        {#if busy}{t('auth.working')}{:else}{submitLabel}{/if}
      </button>
    </form>

    <p class="rd-auth-switch">
      {#if !canRegister}
        {t('auth.closed')}
      {:else if active === 'login'}
        {t('auth.toRegister')}
        <button type="button" onclick={() => switchMode('register')}>{t('auth.registerSubmit')}</button>
      {:else}
        {t('auth.toLogin')}
        <button type="button" onclick={() => switchMode('login')}>{t('auth.loginSubmit')}</button>
      {/if}
    </p>
  </div>
</div>
