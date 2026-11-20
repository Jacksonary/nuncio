/**
 * Reminder chime for the in-app card.
 *
 * System notifications carry their own sound (`notifications.sound`), but a
 * reminder shown while the window is in the foreground never reaches the OS,
 * so the card needs its own short cue. Synthesized with Web Audio so there is
 * no asset to ship and no decoding delay.
 */
let context: AudioContext | null = null;

export function playReminderChime(): void {
  const Ctor =
    window.AudioContext ??
    (window as unknown as { webkitAudioContext?: typeof AudioContext })
      .webkitAudioContext;
  if (!Ctor) return;
  try {
    context ??= new Ctor();
    const ctx = context;
    if (ctx.state === "suspended") void ctx.resume().catch(() => undefined);
    const now = ctx.currentTime;
    const gain = ctx.createGain();
    gain.gain.setValueAtTime(0.0001, now);
    gain.gain.exponentialRampToValueAtTime(0.18, now + 0.015);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.8);
    gain.connect(ctx.destination);

    const first = ctx.createOscillator();
    first.type = "sine";
    first.frequency.setValueAtTime(880, now);
    first.connect(gain);
    first.start(now);
    first.stop(now + 0.18);

    const second = ctx.createOscillator();
    second.type = "sine";
    second.frequency.setValueAtTime(1174.66, now + 0.18);
    second.connect(gain);
    second.start(now + 0.18);
    second.stop(now + 0.8);
  } catch {
    // A missing or blocked audio context must never break the reminder.
  }
}
