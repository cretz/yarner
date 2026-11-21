// Voices and sounds on the play screen. A line's voice plays as the line shows, and the
// next line (or moving on) cuts it. A sound effect plays once at its line. A background
// sound loops from screen to screen until another takes over or the scene changes, fading
// in and out.

const FADE_MS = 800;
/** Background sounds sit under the voices. */
const BACKGROUND = 0.5;

export interface Sound {
  /** Speak a line's clip (`null`: silence), cutting the one before. */
  speak(id: string | null | undefined): void;
  /** Play a sound effect once. */
  effect(id: string): void;
  /** The background sound (`null`: none), crossfading from the one playing. */
  ambience(id: string | null | undefined): void;
  /** Stop the voice and sound effects (moving on). */
  cut(): void;
  /** Stop everything at once. */
  stop(): void;
  setVolume(v: number): void;
}

export function sound(url: (id: string) => string): Sound {
  let volume = 1;
  let voice: HTMLAudioElement | null = null;
  const effects = new Set<HTMLAudioElement>();
  let bg: { id: string; el: HTMLAudioElement; timer: number } | null = null;

  const play = (a: HTMLAudioElement) => {
    // Refused before the reader has touched the page (autoplay rules): quietly nothing.
    a.play().catch(() => {});
  };
  // Move an element's volume to `to` over FADE_MS, then call `done`.
  const fade = (a: HTMLAudioElement, to: number, done?: () => void) => {
    const from = a.volume;
    const t0 = performance.now();
    const t = window.setInterval(() => {
      const k = Math.min(1, (performance.now() - t0) / FADE_MS);
      a.volume = Math.max(0, Math.min(1, from + (to - from) * k));
      if (k >= 1) {
        clearInterval(t);
        done?.();
      }
    }, 40);
    return t;
  };

  const stopVoice = () => {
    voice?.pause();
    voice = null;
  };
  const stopEffects = () => {
    for (const a of effects) a.pause();
    effects.clear();
  };

  return {
    speak(id) {
      stopVoice();
      if (!id) return;
      const a = new Audio(url(id));
      a.volume = volume;
      voice = a;
      play(a);
    },
    effect(id) {
      const a = new Audio(url(id));
      a.volume = volume;
      effects.add(a);
      a.addEventListener("ended", () => effects.delete(a));
      play(a);
    },
    ambience(id) {
      if ((bg?.id ?? null) === (id ?? null)) return;
      if (bg) {
        const old = bg;
        clearInterval(old.timer);
        fade(old.el, 0, () => old.el.pause());
        bg = null;
      }
      if (!id) return;
      const el = new Audio(url(id));
      el.loop = true;
      el.volume = 0;
      play(el);
      bg = { id, el, timer: fade(el, volume * BACKGROUND) };
    },
    cut() {
      stopVoice();
      stopEffects();
    },
    stop() {
      stopVoice();
      stopEffects();
      if (bg) {
        clearInterval(bg.timer);
        bg.el.pause();
        bg = null;
      }
    },
    setVolume(v) {
      volume = Math.max(0, Math.min(1, v));
      if (voice) voice.volume = volume;
      for (const a of effects) a.volume = volume;
      if (bg) {
        clearInterval(bg.timer);
        bg.el.volume = volume * BACKGROUND;
      }
    },
  };
}
