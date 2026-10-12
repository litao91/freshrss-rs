/**
 * Transient notifications, rendered into `.rd-toast`.
 *
 * One message at a time: a reader that queues toasts ends up hiding the one the user actually needs
 * to read behind three "Marked as read" confirmations.
 */

export type ToastKind = 'info' | 'error';

interface ToastState {
  message: string;
  kind: ToastKind;
  /** Bumped on every show so the CSS transition replays even for identical text. */
  token: number;
  visible: boolean;
}

export const toast = $state<ToastState>({ message: '', kind: 'info', token: 0, visible: false });

let hideTimer: ReturnType<typeof setTimeout> | undefined;

function show(message: string, kind: ToastKind, ms: number): void {
  if (hideTimer !== undefined) clearTimeout(hideTimer);
  toast.message = message;
  toast.kind = kind;
  toast.token += 1;
  toast.visible = true;
  hideTimer = setTimeout(() => {
    toast.visible = false;
    hideTimer = undefined;
  }, ms);
}

export function notify(message: string): void {
  show(message, 'info', 2600);
}

/** Errors stay up longer — they usually need acting on. */
export function notifyError(message: string): void {
  show(message, 'error', 6000);
}

export function dismiss(): void {
  if (hideTimer !== undefined) clearTimeout(hideTimer);
  hideTimer = undefined;
  toast.visible = false;
}
