/**
 * Transient notifications.
 *
 * The queue, the live regions and the countdown belong to svelte-widgets now; what stays here is
 * the vocabulary the rest of the app speaks — `notify` for "this happened", `notifyError` for
 * "this did not" — and the two durations this reader has always used.
 *
 * The hand-rolled store showed one message at a time because a reader queuing confirmations ends
 * up reading the third "Marked as read" instead of the error that mattered. The library's queue
 * answers that differently and better: an error ranks above an info toast and preempts the one on
 * screen rather than waiting behind it, and anything beyond the soft cap is dropped rather than
 * stacked.
 */
import { toast } from 'svelte-widgets/toast-queue';

export function notify(message: string): void {
  toast.show(message, { priority: 'info', duration_ms: 2600 });
}

/** Errors stay up longer — they usually need acting on. */
export function notifyError(message: string): void {
  toast.show(message, { priority: 'error', duration_ms: 6000 });
}
