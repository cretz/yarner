// The play screen: scene, dialog box with typewriter reveal, choices, quick menu, skip and
// auto, keyboard, and the doors to history / saves / prefs / map / edit mode.

import {
  generatorOn,
  type AdvanceResult,
  type PlayView,
  type QueueEntry,
  type Writing,
} from "../api.ts";
import type { Nav } from "../main.ts";
import { openMap } from "../map.ts";
import type { Source } from "../source.ts";
import {
  action,
  batch,
  computed,
  effect,
  h,
  layerCount,
  modal,
  onCleanup,
  signal,
  toast,
  untrack,
  when,
  type Child,
} from "../ui.ts";
import { editPanel } from "./edit.ts";
import {
  autoDelay,
  DEFAULT_PREFS,
  openHistory,
  openLoadInPlay,
  openPrefs,
  openSave,
  readPrefs,
  TEXT_INSTANT,
} from "./overlays.ts";
import { scene } from "./scene.ts";
import { sound } from "./sound.ts";

const FAVOR_MS = 600;

type Readiness = "ready" | "retrying" | "filming" | "painting" | "voicing" | "generating" | "idle";
/** Where the screen behind an exit stands. */
const readiness = (x: {
  ready: boolean;
  generating: boolean;
  painting?: boolean;
  filming?: boolean;
  voicing?: boolean;
  retrying?: boolean;
}): Readiness =>
  x.ready
    ? "ready"
    : x.retrying
      ? "retrying"
      : x.filming
        ? "filming"
        : x.painting
          ? "painting"
          : x.voicing
            ? "voicing"
            : x.generating
              ? "generating"
              : "idle";
// What's being made for the screen right now; its ring fills once across writing and
// painting (one progress for the whole screen, never restarting).
const WAIT_NOTE: Record<string, string> = {
  retrying: "failed, retrying…",
  filming: "filming…",
  painting: "painting…",
  voicing: "voicing…",
  generating: "writing…",
  idle: "up next…",
};
/**
 * "painting…"; for one waiting its turn, where it stands ("2nd in line…"); for one whose
 * model is still being loaded, that (a model switch can take a while).
 */
const waitNote = (state: string, x?: { line?: number; loading?: boolean }) => {
  const line = x?.line;
  if (state === "idle" && line)
    return `${line === 1 ? "next" : `${line}${line === 2 ? "nd" : line === 3 ? "rd" : "th"}`} in line…`;
  if (
    x?.loading &&
    (state === "generating" || state === "painting" || state === "filming" || state === "voicing")
  )
    return "loading the model…";
  return WAIT_NOTE[state] ?? "";
};

/**
 * A small progress ring: fills as the work goes (writing, painting, filming); spins while
 * it's waiting its turn (no progress yet).
 */
function ring(progress: () => number | undefined | null): Node {
  const R = 7;
  const C = 2 * Math.PI * R;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 18 18");
  svg.setAttribute("class", "ring");
  for (const [cls, el] of [
    ["ring-track", false],
    ["ring-arc", true],
  ] as const) {
    const c = document.createElementNS("http://www.w3.org/2000/svg", "circle");
    c.setAttribute("cx", "9");
    c.setAttribute("cy", "9");
    c.setAttribute("r", String(R));
    c.setAttribute("class", cls);
    if (el) {
      c.setAttribute("stroke-dasharray", String(C));
      // The arc follows `progress` (re-read with the view), and creeps on a little
      // between updates so it never sits still while work goes on; it snaps to each real
      // value and never runs more than a few points ahead of it.
      let shown = 0;
      let real: number | null = null;
      const draw = () => {
        c.setAttribute(
          "stroke-dashoffset",
          String(C * (1 - (real == null ? 0.25 : Math.max(0.04, Math.min(1, shown))))),
        );
        svg.classList.toggle("spin", real == null);
      };
      effect(() => {
        const p = progress();
        real = p ?? null;
        if (real != null && (real > shown || real < shown - 0.08)) shown = real;
        draw();
      });
      const t = setInterval(() => {
        // Waiting its turn (0): still. Working: creeps a little ahead.
        if (real == null || real === 0 || shown >= Math.min(0.97, real + 0.06)) return;
        shown += 0.004;
        draw();
      }, 250);
      onCleanup(() => clearInterval(t));
    }
    svg.append(c);
  }
  return svg;
}
const WAIT_TITLE: Record<string, string> = {
  retrying: "Making it failed; it's tried again shortly",
  filming: "Its video is being made",
  painting: "Its picture is being painted",
  voicing: "Its voices and sounds are being made",
  generating: "Being written",
  idle: "Written right after the one before it (click to put it first)",
};

export const keyOf = (v: PlayView | null) => (v?.beat ? `${v.beat.id}:${v.beat.epoch}` : "");

const report = (e: unknown) => toast(e instanceof Error ? e.message : String(e), "error");

export function playScreen(src: Source, playId: string, nav: Nav, withEdit = false): Node {
  // The editing tools only in Play & edit (and only where the story can be edited).
  const ed = withEdit ? src.edit : null;
  // Lines can only go as far as they're written: a story file, or no generator connected.
  const fixed = () => src.kind === "bundle" || !generatorOn();
  const view = signal<PlayView | null>(null);
  const lineIdx = signal(0);
  const typed = signal(0);
  const waiting = signal(false);
  const ended = signal(false);
  const skipOn = signal(false);
  const skipHeld = signal(false);
  const auto = signal(false);
  const hidden = signal(false);
  const editing = signal(false);
  const prefs = signal(DEFAULT_PREFS);
  const queue = signal<QueueEntry[]>([]);
  const writing = signal<Writing | null>(null);
  const setupTick = signal(0);
  const favored = signal<number | null>(null);
  let pendingExit: number | null = null;
  let busy = false;
  /** A move asked for while another was on its way: made right after it. */
  let queued: (() => void) | null = null;
  /** Settles the promise of the move queued (run or dropped). */
  let queuedDone: (() => void) | null = null;
  const dropQueued = () => {
    queued = null;
    const d = queuedDone;
    queuedDone = null;
    d?.();
  };

  const beat = () => view()?.beat ?? null;
  const lines = () => beat()?.lines ?? [];
  const line = () => lines()[lineIdx()] ?? null;
  const lineDone = () => {
    const l = line();
    return !l || typed() >= l.text.length;
  };
  const atLast = () => lineDone() && lineIdx() >= lines().length - 1;
  const skipping = () => skipOn() || skipHeld();
  const paused = () => editing() || layerCount() > 0;
  const showChoices = () => !!beat() && atLast() && beat()!.next.type === "choices" && !ended();
  // The next screen comes whole: written and painted. Until then the way on stays shut
  // and says what it's waiting for.
  const nextReady = () => {
    const n = beat()?.next;
    return n?.type === "continue" && n.ready;
  };
  const key = computed(() => keyOf(view()));
  // Only when it changes (a signal: the list isn't rebuilt on every refresh of the view;
  // 2026-11-21 alpha: the decision screen flashed).
  const choicesShown = computed(showChoices);

  const stopSkip = () => batch(() => (skipOn.set(false), skipHeld.set(false)));

  // The line you were on, kept for coming back to this screen (ids and a number only).
  const LINE = `yarner.line.${playId}`;
  const savedLine = (k: string): number => {
    try {
      const [sk, n] = (sessionStorage.getItem(LINE) ?? "").split("|");
      return sk === k ? Number(n) || 0 : 0;
    } catch {
      return 0;
    }
  };
  effect(() => {
    const k = key();
    const i = lineIdx();
    // A story file played in the browser keeps nothing at all.
    if (!k || src.kind === "bundle") return;
    try {
      sessionStorage.setItem(LINE, `${k}|${i}`);
    } catch {
      /* storage off: back at the beat's first line */
    }
  });

  /** Show a view. A different beat (or a re-written one) starts from its first line. */
  function apply(v: PlayView, toEnd = false) {
    const first = view.peek() == null;
    const changed = toEnd || keyOf(view.peek()) !== keyOf(v);
    batch(() => {
      view.set(v);
      const n = v.beat?.lines.length ?? 0;
      if (changed) {
        // Coming back to this screen: the line you were on.
        const back = first ? Math.min(savedLine(keyOf(v)), Math.max(0, n - 1)) : 0;
        lineIdx.set(toEnd ? Math.max(0, n - 1) : back);
        typed.set(toEnd ? Infinity : 0);
        ended.set(false);
        favored.set(v.favor);
        // `seen` is "seen before this visit": new text stops skipping.
        if (v.beat && !v.beat.seen && skipping() && !prefs.peek().skip_unseen) stopSkip();
      } else if (lineIdx.peek() >= n) lineIdx.set(Math.max(0, n - 1));
    });
  }

  // Responses can arrive out of order: only the newest request's view is shown.
  let issued = 0;
  let shown = 0;
  async function load(p: Promise<PlayView>, toEnd = false): Promise<PlayView> {
    const n = ++issued;
    const v = await p;
    if (n >= shown) {
      shown = n;
      apply(v, toEnd);
    }
    return v;
  }

  async function go(
    exit: number | null = null,
    how?: () => Promise<{ result: AdvanceResult; view: PlayView }>,
    retry = false,
  ) {
    // A move already on its way: a decision made meanwhile goes right after it, never
    // dropped (2026-11-21 alpha: "Go" on your own choice sometimes did nothing). Only on
    // the beat it was made at (a move queued on one beat once ran on the next, skipping a
    // screen); a retry is just dropped (the next one comes soon).
    if (busy) {
      if (!retry && (how || exit != null)) {
        dropQueued();
        const at = beat()?.id;
        return new Promise<void>((done) => {
          queuedDone = done;
          queued = () => {
            queuedDone = null;
            if (beat()?.id === at) void go(exit, how).finally(done);
            else done();
          };
        });
      }
      return;
    }
    // At a decision only a choice moves on (the server would read "no exit" as the first).
    if (!how && exit == null && beat()?.next.type === "choices") return;
    busy = true;
    try {
      const n = ++issued;
      // Tests only: a trace of every move sent (the page sets the array; nobody else does).
      const trace = (globalThis as { __yarnerMoves?: string[] }).__yarnerMoves;
      trace?.push(`${beat()?.id ?? "-"} exit ${exit} ${how ? "own" : ""} retry ${retry}`);
      const r = await (how ? how() : src.advance(playId, exit));
      trace?.push(`  -> ${r.result.type} ${r.view.beat?.id ?? "-"}`);
      // A new picture is fetched and decoded before the move shows, so the text and its
      // picture arrive together (the server has it; the browser still has to load it).
      if (r.result.type === "moved") await sc.prepare(r.view.beat?.screen);
      // A move always shows (a re-read sent meanwhile may predate it).
      if (n >= shown || r.result.type === "moved") {
        shown = n;
        apply(r.view);
      }
      if (r.result.type === "waiting") {
        // Own choice: the exit it was given (the new choice is the last one).
        const own = !!how && r.view.beat?.next.type === "choices";
        pendingExit =
          own && r.view.beat?.next.type === "choices" ? r.view.beat.next.choices.length - 1 : exit;
        // Your own choice goes first, and you tap it once its screen is ready (never
        // moves on by itself: 2026-11-22, the user).
        if (own && pendingExit != null) favored.set(pendingExit);
        waiting.set(true);
      } else {
        waiting.set(false);
        if (r.result.type === "ended") ended.set(true);
      }
    } catch (e) {
      waiting.set(false);
      // A move asked for a screen that has changed since (a choice of a beat that's no
      // longer up): nothing to tell the reader, just show where things are now.
      if (!how && /no exit/.test(String((e as Error)?.message ?? e))) {
        void load(src.play(playId)).catch(() => {});
      } else if (!retry) report(e);
      // Asked again and again while waiting (network down, server restarting): quiet,
      // the next try or the reader's own click says what's wrong.
    } finally {
      busy = false;
      const q = queued;
      queued = null;
      q?.();
    }
  }

  let refreshTimer = 0;
  // At most one re-read per 120 ms, never put off: events one after another kept pushing
  // a debounced one back, and the screen never caught up (2026-11-22 e2e).
  const refresh = () => {
    if (refreshTimer) return;
    refreshTimer = window.setTimeout(() => {
      refreshTimer = 0;
      load(src.play(playId)).catch(report);
    }, 120);
  };
  onCleanup(() => clearTimeout(refreshTimer));

  onCleanup(
    src.subscribe(playId, (m) => {
      if (m.type === "queue")
        return batch(() => (queue.set(m.jobs), writing.set(m.writing ?? null)));
      if (m.type === "error") return toast(m.message, "error");
      if (m.type === "setup" || m.type === "resync") setupTick.update((x) => x + 1);
      if (m.type === "draft") return;
      // Before the opening: it shows as soon as it's ready. "Before the opening" only once
      // the first read said so (`waiting`): before that, no beat just means not loaded yet,
      // and asking to move on then skipped a beat on coming back to a story (2026-11-22).
      const opening = waiting.peek() && !beat();
      if ((m.type === "beat" || m.type === "media") && opening)
        void go(pendingExit, undefined, true);
      refresh();
    }),
  );

  // Back to the page (another app, a locked phone): background tabs barely run timers and
  // can miss events, so re-read now and ask again for what the reader is waiting on
  // (2026-11-21 alpha: painting seemed stuck after the page lost focus).
  const onVisible = () => {
    if (document.visibilityState !== "visible") return;
    refresh();
    if (waiting.peek() && !beat()) void go(pendingExit, undefined, true);
  };
  document.addEventListener("visibilitychange", onVisible);
  onCleanup(() => document.removeEventListener("visibilitychange", onVisible));

  // Waiting for the opening: retry now and then too (events can be missed). Later
  // screens only re-read the view: the reader moves on when they choose to.
  effect(() => {
    if (!waiting() || beat()) return;
    const t = setInterval(() => void go(pendingExit, undefined, true), 600);
    return () => clearInterval(t);
  });
  // While a picture or video on the way ahead is being made, re-read now and then for its
  // progress (it isn't announced as an event).
  effect(() => {
    const n = beat()?.next;
    const making =
      (n?.type === "continue" && (!!n.painting || !!n.voicing || n.generating)) ||
      (n?.type === "choices" && n.choices.some((c) => c.painting || c.voicing || c.generating));
    if (!making) return;
    const t = setInterval(refresh, 1000);
    return () => clearInterval(t);
  });
  // Something asked for the next screen; once the beat changes that's moot.
  effect(() => {
    key();
    dropQueued();
    untrack(() => waiting.set(false));
  });

  src
    .prefs()
    .then((p) => prefs.set(readPrefs(p)))
    .catch(() => {});
  load(src.play(playId))
    .then((v) => {
      if (!v.beat) {
        waiting.set(true);
        void go();
      }
    })
    .catch(report);

  // Interaction.

  function step() {
    if (paused()) return;
    if (hidden()) return hidden.set(false);
    const b = beat();
    if (!b || ended()) return;
    const l = line();
    if (l && typed() < l.text.length) return typed.set(Infinity);
    if (lineIdx() < b.lines.length - 1)
      return batch(() => (lineIdx.update((i) => i + 1), typed.set(0)));
    if (b.next.type === "choices") return;
    if (b.next.type === "end") return ended.set(true);
    // Not ready: asking puts it first in line, but the reader stays put until it is.
    void go();
  }

  async function back() {
    if (paused()) return;
    stopSkip();
    const wasEnded = ended();
    batch(() => (waiting.set(false), ended.set(false)));
    if (wasEnded) return;
    if (lineIdx() > 0) return batch(() => (lineIdx.update((i) => i - 1), typed.set(Infinity)));
    if ((view()?.depth ?? 0) <= 1) return;
    await load(src.back(playId), true).catch(report);
  }

  function choose(exit: number) {
    const n = beat()?.next;
    if (n?.type !== "choices" || !n.choices[exit]?.ready) {
      // Not ready yet: it goes first, and you tap it again once its screen is ready
      // (never moves on by itself: 2026-11-22 alpha, the user wants the second tap).
      favored.set(exit);
      src.favor(playId, exit).catch(() => {});
      return;
    }
    // Ready: this one, whatever was picked before (it goes right after a move under way).
    stopSkip();
    void go(exit);
  }

  const rewind = (index: number) => load(src.rewind(playId, index)).then(() => {}, report);

  // Typewriter.
  effect(() => {
    const l = line();
    const cps = prefs().text_speed;
    if (!l) return;
    if (cps >= TEXT_INSTANT || skipping()) return typed.set(Infinity);
    const len = l.text.length;
    let acc = Math.min(untrack(typed), len);
    let last = performance.now();
    let raf = 0;
    const tick = (t: number) => {
      if (typed.peek() >= len) return;
      acc = Math.max(acc, typed.peek()) + ((t - last) / 1000) * cps;
      last = t;
      typed.set(acc >= len ? Infinity : Math.floor(acc));
      if (acc < len) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  });

  // Skip: fast through lines and continue beats; stops at choices, unseen text, unready beats.
  effect(() => {
    if (!skipping() || paused()) return;
    const t = setInterval(() => {
      const b = beat();
      if (!b || busy) return;
      if (!lineDone()) return typed.set(Infinity);
      if (lineIdx() < b.lines.length - 1)
        return batch(() => (lineIdx.update((i) => i + 1), typed.set(Infinity)));
      if (b.next.type !== "continue") return stopSkip();
      // Skipping waits (with the indicator up) for the next screen to be ready.
      if (!b.next.ready) return;
      void go();
    }, 70);
    return () => clearInterval(t);
  });

  // Auto: go on after a pause that grows with the line's length.
  effect(() => {
    if (!auto() || paused() || skipping() || ended() || hidden()) return;
    const b = beat();
    if (!b || !lineDone()) return;
    if (atLast() && b.next.type === "choices") return;
    if (atLast() && !fixed() && b.next.type === "continue" && !nextReady()) return;
    const t = setTimeout(step, autoDelay(prefs(), line()?.text.length ?? 0));
    return () => clearTimeout(t);
  });

  /**
   * Steer: a direction for the story from here on (a lasting one, like a story prompt
   * from this beat; steering again replaces it from that point), or for just the next
   * beat. Beats written ahead are rewritten to follow it before you reach them.
   */
  const openSteer = () => {
    const b = beat();
    const steerEd = src.edit;
    if (!b || !steerEd) return;
    const text = signal("");
    const span = signal<"from_here" | "next" | "pictures">("from_here");
    const opt = (v: "from_here" | "next" | "pictures", label: string) =>
      h(
        "label",
        { class: "check" },
        h("input", {
          type: "radio",
          name: "steer-span",
          checked: () => span() === v,
          onChange: () => span.set(v),
        }),
        h("span", null, label),
      );
    modal("Steer the story", (close) =>
      h(
        "form",
        {
          class: "form",
          onSubmit: (e: Event) => e.preventDefault(),
        },
        h("textarea", {
          name: "steer",
          rows: 3,
          placeholder: 'Where should it go? e.g. "the keeper starts to trust the stranger"',
          value: () => text(),
          onInput: (e: Event) => text.set((e.target as HTMLTextAreaElement).value),
          onKeyDown: (e: KeyboardEvent) => e.stopPropagation(),
        }),
        h(
          "div",
          { class: "radios" },
          opt("from_here", "The story, from here on"),
          opt("next", "Just the next beat"),
          opt("pictures", "Pictures and video, from here on"),
        ),
        h("p", { class: "muted small" }, () =>
          span() === "pictures"
            ? 'Added word for word to every picture and video made from here, e.g. "she looks straight at the viewer". Steering pictures again replaces it.'
            : "Anything already written ahead is rewritten to follow it (pictures too) before you get there. What you've seen stays.",
        ),
        action(
          "Steer",
          async () => {
            const t = text().trim();
            if (span() === "next") await steerEd.setPrompt(`next:${b.id}`, t, b.id, "next");
            else if (span() === "pictures")
              await steerEd.setPrompt("pictures", t, b.id, "from_here");
            else await steerEd.setPrompt("steer", t, b.id, "from_here");
            toast(
              span() === "next"
                ? "The next beat will go that way."
                : span() === "pictures"
                  ? "Every picture and video from here will have that."
                  : "The story will go that way from here.",
            );
            close();
          },
          { class: "primary", type: "submit", disabled: () => !text().trim() },
        ),
      ),
    );
  };

  const toggleEdit = () => {
    if (!ed) return;
    batch(() => {
      editing.update((x) => !x);
      auto.set(false);
      stopSkip();
    });
  };
  // Fullscreen turns a phone to the story's shape (landscape unless the story is
  // portrait); out of fullscreen the screen keeps that shape, letterboxed.
  const toggleFullscreen = () => {
    if (document.fullscreenElement) return void document.exitFullscreen();
    const [w, hh] = view()?.frame ?? [16, 9];
    const lock = (screen.orientation as unknown as { lock?: (o: string) => Promise<void> }).lock;
    void document.documentElement
      .requestFullscreen?.()
      .then(() => lock?.call(screen.orientation, w >= hh ? "landscape" : "portrait"))
      .catch(() => {});
  };
  const showMap = () =>
    openMap(src, playId, {
      editing: editing(),
      seenJump: !!src.edit,
      frame: view()?.frame,
      pick: async (n) => {
        if (editing() && ed) await load(ed.jump(playId, n.id));
        else if (n.path_index != null) await rewind(n.path_index);
        // Seen on another route: there, as if that way had been taken.
        else if (n.seen && src.edit) await load(src.edit.jump(playId, n.id));
      },
    });

  const onKey = (e: KeyboardEvent) => {
    if ((e.target as HTMLElement).closest?.("input, textarea, select, [contenteditable]")) return;
    if (layerCount() > 0) return;
    if (e.key === "Control") {
      if (!e.repeat && !editing()) skipHeld.set(true);
      return;
    }
    if (e.ctrlKey || e.metaKey || e.altKey) return;
    const onButton = (e.target as HTMLElement).closest?.("button");
    switch (e.key.toLowerCase()) {
      case " ":
      case "enter":
        if (onButton) return;
        e.preventDefault();
        if (!e.repeat) step();
        break;
      case "backspace":
      case "pageup":
        e.preventDefault();
        void back();
        break;
      case "a":
        auto.update((x) => !x);
        break;
      case "h":
        hidden.update((x) => !x);
        break;
      case "f":
        toggleFullscreen();
        break;
      case "e":
        toggleEdit();
        break;
      case "m":
        showMap();
        break;
      case "escape":
        if (editing()) editing.set(false);
        else hidden.set(false);
        break;
    }
  };
  const onKeyUp = (e: KeyboardEvent) => e.key === "Control" && skipHeld.set(false);
  const onBlur = () => skipHeld.set(false);
  addEventListener("keydown", onKey);
  addEventListener("keyup", onKeyUp);
  addEventListener("blur", onBlur);
  onCleanup(() => {
    removeEventListener("keydown", onKey);
    removeEventListener("keyup", onKeyUp);
    removeEventListener("blur", onBlur);
  });

  let lastWheel = 0;
  const onWheel = (e: WheelEvent) => {
    if (paused() || e.deltaY >= 0 || Date.now() - lastWheel < 350) return;
    lastWheel = Date.now();
    void back();
  };

  // Rendering.

  const sc = scene(
    src.mediaUrl.bind(src),
    () => beat()?.screen ?? null,
    () => prefs.peek().loop_videos,
    // A video's own sound: on the screen that shows it, when its lines aren't voiced
    // separately (a video with a soundtrack says them itself).
    () => {
      const b = beat();
      return (
        prefs.peek().sounds &&
        b?.shows?.kind === "video" &&
        !(b.voices ?? []).some(Boolean) &&
        !(b.sounds ?? []).length
      );
    },
  );

  // Voices and sounds: each line's voice as it shows (the next line or moving on cuts
  // it), sound effects at their lines, the scene's background sound throughout.
  const au = sound(src.mediaUrl.bind(src));
  onCleanup(() => au.stop());
  effect(() => au.setVolume(prefs().volume));
  let spokenAt = "";
  effect(() => {
    const k = key();
    const i = lineIdx();
    const b = beat();
    const p = prefs();
    if (!b) return;
    const at = `${k}|${i}`;
    // Only when the line changes (not every re-read of the same screen).
    if (at === spokenAt) return;
    spokenAt = at;
    untrack(() => {
      au.cut();
      if (skipping()) return;
      if (p.voices) au.speak(b.voices?.[i]);
      if (p.sounds)
        for (const c of b.sounds ?? []) if (!c.looped && c.line === i) au.effect(c.media);
    });
  });
  effect(() => au.ambience(prefs().sounds && !ended() ? beat()?.ambience : null));
  const stop = (e: Event) => e.stopPropagation();

  // The "next" wait note under the dialogue: rebuilt only when it comes or goes.
  const nextWaitShown = computed(
    () => atLast() && !fixed() && beat()?.next.type === "continue" && !nextReady(),
  );
  const nextState = computed(() => {
    const n = beat()?.next;
    if (!n || n.type !== "continue") return "";
    return readiness(n);
  });
  const NEXT_TITLE: Record<string, string> = {
    ready: "What comes next is ready",
    painting: "The next picture is being painted",
    filming: "The next scene's video is being made",
    voicing: "The next screen's voices and sounds are being made",
    generating: "What comes next is being written",
    idle: "What comes next isn't started yet",
  };

  // Rebuilt for a new beat and when a choice is added (your own: it shows at once).
  const choiceLabels = computed(() => {
    const n = beat()?.next;
    return n?.type === "choices" ? n.choices.map((c) => c.label).join("") : "";
  });
  const choices = () => {
    key();
    choiceLabels();
    return untrack(() => {
      const b = beat();
      if (!b || b.next.type !== "choices") return null;
      return h(
        "div",
        { class: "choices", onClick: stop },
        b.next.choices.map((c, i) => choiceButton(c.label, i, !!c.own)),
        // The player's own choice: only when there's a generator to write what follows
        // (otherwise choices are what was written, own ones included).
        !fixed() && generatorOn() && b.own_choices && src.ownChoice ? ownChoiceBox() : null,
        // In a read-only copy, the line may stop where its last player stopped.
        fixed() && b.next.choices.every((c) => !c.ready)
          ? h("p", { class: "choices-note" }, "This copy of the story ends here.")
          : null,
      );
    });
  };

  /** "Write your own": a box to type a decision (and anything else) in. */
  function ownChoiceBox(): Node {
    const open = signal(false);
    const text = signal("");
    /** Sent: shown as taken right away, until it's in the list (a moment later). */
    const sent = signal("");
    const submit = () => {
      const t = text().trim();
      if (!t || !src.ownChoice || sent()) return;
      stopSkip();
      sent.set(t);
      void go(null, () => src.ownChoice!(playId, t)).finally(() => {
        // Still here (the list didn't rebuild, e.g. it failed): let them try again.
        sent.set("");
      });
    };
    return h("div", { class: "own-choice" }, () =>
      sent()
        ? h(
            "div",
            { class: "choice own shut", "data-state": "generating" },
            h("span", { class: "label" }, sent()),
            h(
              "span",
              { class: "wait-note generating" },
              ring(() => undefined),
              "getting it ready…",
            ),
          )
        : open()
          ? h(
              "form",
              {
                class: "own-form",
                onSubmit: (e: Event) => {
                  e.preventDefault();
                  submit();
                },
              },
              h("textarea", {
                name: "own-choice",
                rows: 2,
                maxlength: 300,
                placeholder: "What do you do? (and anything else you want to happen)",
                value: () => text(),
                onInput: (e: Event) => text.set((e.target as HTMLTextAreaElement).value),
                onKeyDown: (e: KeyboardEvent) => {
                  e.stopPropagation();
                  if (e.key === "Enter" && !e.shiftKey) {
                    e.preventDefault();
                    submit();
                  }
                },
              }),
              h(
                "button",
                {
                  class: "primary",
                  type: "submit",
                  disabled: () => !text().trim(),
                  // On touch, go at the touch itself: the keyboard closing moves the layout
                  // before a click would land (2026-11-21 alpha: "Go" seemed dead).
                  onPointerDown: (e: PointerEvent) => {
                    if (e.pointerType === "mouse") return;
                    e.preventDefault();
                    submit();
                  },
                },
                "Go",
              ),
            )
          : h(
              "button",
              {
                class: "choice own-open",
                title: "Write your own choice",
                onClick: () => open.set(true),
              },
              h("span", { class: "label" }, "⌨ Write your own…"),
            ),
    );
  }

  function choiceButton(label: string, i: number, own = false): Node {
    let timer = 0;
    let longPressed = false;
    const c = () => {
      const n = beat()?.next;
      return n?.type === "choices" ? n.choices[i] : null;
    };
    // Computed: equal states don't re-render (each re-read rebuilt the note and its ring,
    // restarting the ring's creep and spin every second).
    const state = computed(() => {
      const x = c();
      return x ? readiness(x) : "idle";
    });
    // Live, an unready choice is shut until its screen is ready (a click favors it); in
    // a read-only copy it's a path nobody took.
    const shut = () => !c()?.ready;
    const favor = () => {
      favored.set(i);
      src.favor(playId, i).catch(() => {});
    };
    const arm = (e: PointerEvent) => {
      clearTimeout(timer);
      longPressed = false;
      timer = window.setTimeout(() => {
        favor();
        if (e.pointerType === "touch") longPressed = true;
      }, FAVOR_MS);
    };
    return h(
      "button",
      {
        class: "choice",
        "class:own": own,
        "data-exit": i,
        "data-state": state,
        "aria-disabled": () => (shut() ? "true" : "false"),
        "class:shut": shut,
        // A read-only story only has the choices someone took before it was exported.
        disabled: () => fixed() && !c()?.ready,
        title: () =>
          fixed()
            ? !c()?.ready
              ? src.kind === "bundle"
                ? "Nobody took this path in this story."
                : "Not written yet, and no generator is connected to write it."
              : ""
            : (WAIT_TITLE[state()] ?? ""),
        "class:favored": () => favored() === i && !c()?.ready,
        onPointerEnter: (e: PointerEvent) => e.pointerType === "mouse" && arm(e),
        onPointerDown: (e: PointerEvent) => e.pointerType !== "mouse" && arm(e),
        onPointerLeave: () => clearTimeout(timer),
        onPointerUp: () => clearTimeout(timer),
        onContextMenu: (e: Event) => e.preventDefault(),
        onClick: () => {
          clearTimeout(timer);
          if (longPressed) return void (longPressed = false);
          choose(i);
        },
      },
      h("span", { class: "label" }, label),
      () =>
        !fixed() && state() !== "ready"
          ? h(
              "span",
              { class: () => `wait-note ${state()}` },
              // Still while it waits its turn: a moving ring always means work going on.
              ring(() => (state() === "idle" ? 0 : c()?.progress)),
              () => waitNote(state(), c() ?? undefined),
            )
          : null,
      () =>
        prefs().show_ready && state() === "ready"
          ? h("span", { class: "ready-dot ready", title: "Ready" })
          : null,
    );
  }

  const qbtn = (label: string, run: () => void, props: Record<string, unknown> = {}) =>
    h(
      "button",
      {
        ...props,
        onClick: (e: Event) => {
          (e.currentTarget as HTMLElement).blur();
          run();
        },
      },
      label,
    );

  const quick = h(
    "nav",
    { class: "quick", onClick: stop },
    qbtn("Back", () => void back(), { title: "Back (Backspace)" }),
    qbtn(
      "History",
      () => {
        const v = view();
        if (v) openHistory(v, rewind);
      },
      { title: "History" },
    ),
    qbtn("Skip", () => skipOn.update((x) => !x), {
      "class:on": skipping,
      title: "Skip (hold Ctrl)",
    }),
    qbtn("Auto", () => auto.update((x) => !x), {
      "class:on": auto,
      title: "Auto (A)",
    }),
    qbtn("Save", () => openSave(src, playId, prefs(), lineIdx.peek())),
    qbtn("Load", () =>
      openLoadInPlay(src, playId, (v, s) => {
        apply(v);
        // Back on the line it was saved at (at a decision: its buttons up again).
        const n = v.beat?.lines.length ?? 0;
        if (s.line != null && n)
          batch(() => (lineIdx.set(Math.min(s.line!, n - 1)), typed.set(Infinity)));
      }),
    ),
    qbtn("Prefs", () => openPrefs(src, prefs)),
    qbtn("Map", showMap, { title: "Story map (M)" }),
    // Steering is for anyone with a generator (Play & edit or not): one tool, not two.
    () =>
      src.edit && !fixed()
        ? qbtn("Steer…", () => openSteer(), {
            title: "Steer the story or its pictures from here",
          })
        : null,
    ed
      ? qbtn("Edit", toggleEdit, {
          "class:on": editing,
          title: "Edit mode (E)",
        })
      : null,
    qbtn("Hide", () => hidden.set(true), { title: "Hide the interface (H)" }),
    qbtn("Home", () => nav.go({ to: "home" }), { title: "Story home" }),
  );

  const dialog = h(
    "div",
    {
      class: "dialog",
      "class:narration": () => !line()?.speaker,
      // Nothing to read before the opening: no empty box with a "go on" arrow.
      "class:empty": () => !beat(),
    },
    () => {
      const s = line()?.speaker;
      return s ? h("div", { class: "nameplate" }, s) : null;
    },
    h(
      "p",
      { class: "say" },
      h("span", null, () => (line()?.text ?? "").slice(0, typed())),
      h("span", { class: "rest", "aria-hidden": "true" }, () =>
        (line()?.text ?? "").slice(typed()),
      ),
    ),
    // Every wait note in one place, under the words: this screen's picture still on its
    // way (going back to a beat painted later), or at the end of a beat whose next
    // screen isn't ready, what it's waiting for.
    () =>
      sc.painting() && !(atLast() && beat()?.next.type === "continue" && !nextReady())
        ? h(
            "div",
            {
              class: "next-wait painting",
              title: "This screen's picture is being painted",
            },
            ring(() => undefined),
            h("span", null, "painting…"),
          )
        : null,
    () =>
      nextWaitShown()
        ? h(
            "div",
            {
              class: () => `next-wait ${nextState()}`,
              "class:asked": waiting,
              title: () => NEXT_TITLE[nextState()] ?? "",
            },
            ring(() => {
              const n = beat()?.next;
              if (nextState() === "idle") return 0;
              return n?.type === "continue" ? n.progress : undefined;
            }),
            h("span", null, () => {
              const n = beat()?.next;
              return waitNote(nextState(), n?.type === "continue" ? n : undefined);
            }),
          )
        : null,
    h("span", {
      class: "ctc",
      "class:show": () =>
        lineDone() &&
        !showChoices() &&
        (!atLast() || fixed() || beat()?.next.type !== "continue" || nextReady()),
    }),
    h("span", {
      class: () => `next-dot ${nextState()}`,
      "class:show": () => prefs().show_ready && !!nextState(),
      title: () => NEXT_TITLE[nextState()] ?? "",
    }),
  );

  const endCard = (): Child =>
    h(
      "div",
      { class: "endcard", onClick: stop },
      // Not final: the story can always keep going (with a generator).
      h("h2", null, !fixed() && generatorOn() && src.keepGoing ? "The end?" : "The End"),
      h("p", { class: "muted" }, "This path ends here. Others may not."),
      h(
        "div",
        { class: "row center" },
        h("button", { onClick: () => void back() }, "Go back"),
        // The story ended but you want more: it goes on from here.
        !fixed() && generatorOn() && src.keepGoing
          ? h(
              "button",
              {
                class: "primary",
                onClick: () => {
                  ended.set(false);
                  void go(null, () => src.keepGoing!(playId));
                },
              },
              "Keep going",
            )
          : null,
        h("button", { onClick: showMap }, "Story map"),
        h("button", { class: "primary", onClick: () => nav.go({ to: "home" }) }, "Story home"),
      ),
    );

  return h(
    "div",
    {
      class: "play",
      "class:editing": editing,
      "class:hide-ui": hidden,
      // The story's shape (letterboxed to fit) and the dialogue box's opacity.
      style: () => {
        const [w, hh] = view()?.frame ?? [16, 9];
        return `--frame-w:${w || 16};--frame-h:${hh || 9};--box-alpha:${prefs().box_opacity}`;
      },
      "data-beat": () => beat()?.id ?? "",
      "data-epoch": () => beat()?.epoch ?? "",
      "data-depth": () => view()?.depth ?? 0,
      "data-next": () => nextState(),
      "data-line": lineIdx,
      onWheel,
      onClick: step,
    },
    sc.el,
    h("div", { class: "vignette" }),
    h(
      "div",
      { class: "status" },
      () => (skipping() ? h("span", { class: "chip" }, "Skipping") : null),
      () => (auto() ? h("span", { class: "chip" }, "Auto") : null),
    ),
    // A video on screen: loop on/off (kept as a preference, for every video from now
    // on), pause (paused, or played through, it holds its frame; play replays it), sound.
    () => {
      const v = sc.video();
      if (!v) return null;
      const muted = signal(v.muted);
      const paused = signal(v.paused);
      v.onplay = () => paused.set(false);
      v.onpause = () => paused.set(true);
      return h(
        "div",
        { class: "video-controls", onClick: (e: Event) => e.stopPropagation() },
        h(
          "button",
          {
            class: () => (prefs().loop_videos ? "icon loop on" : "icon loop"),
            "aria-pressed": () => String(prefs().loop_videos),
            title: () => (prefs().loop_videos ? "Looping videos (tap to stop)" : "Loop videos"),
            onClick: () => {
              const on = !prefs.peek().loop_videos;
              const next = { ...prefs.peek(), loop_videos: on };
              prefs.set(next);
              src.setPrefs(next).catch(report);
              v.loop = on;
              // Played through and holding its frame: looping starts it going again.
              if (on && v.paused) void v.play().catch(() => {});
            },
          },
          "↻",
        ),
        h(
          "button",
          {
            class: "icon",
            title: () => (paused() ? "Play" : "Pause"),
            onClick: () => (v.paused ? void v.play().catch(() => {}) : v.pause()),
          },
          () => (paused() ? "▶" : "❚❚"),
        ),
        h(
          "button",
          {
            class: "icon",
            title: () => (muted() ? "Sound on" : "Sound off"),
            onClick: () => {
              v.muted = !v.muted;
              muted.set(v.muted);
            },
          },
          () => (muted() ? "🔇" : "🔊"),
        ),
      );
    },
    () =>
      view() && !beat()
        ? h(
            "div",
            { class: "opening" },
            h(
              "div",
              { class: "opening-ring" },
              ring(() => view()?.opening?.[1] ?? undefined),
            ),
            // What's being made now; the ring fills once across writing and painting.
            h("p", null, () => {
              const what = view()?.opening?.[0];
              return what === "painting"
                ? "Painting the opening…"
                : what === "filming"
                  ? "Filming the opening…"
                  : what === "voicing"
                    ? "Voicing the opening…"
                    : "Writing the opening…";
            }),
          )
        : null,
    when(ended, endCard),
    // Choices sit right above the dialogue, in one column with it.
    // Tracked directly (not `when`, which doesn't follow what's inside): the list is
    // rebuilt when a choice is added, so your own shows at once.
    h("div", { class: "hud" }, () => (choicesShown() ? choices() : null), dialog, quick),
    h(
      "button",
      {
        class: "icon fullscreen",
        title: "Fullscreen (F)",
        onClick: (e: Event) => {
          e.stopPropagation();
          toggleFullscreen();
        },
      },
      "⛶",
    ),
    when(
      () => editing() && !!ed,
      () =>
        editPanel({
          edit: ed!,
          info: () => src.info(),
          play: playId,
          view,
          load,
          queue,
          writing,
          setupTick,
          map: showMap,
          close: () => editing.set(false),
        }),
    ),
  );
}
