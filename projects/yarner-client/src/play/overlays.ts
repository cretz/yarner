// Play overlays: history, save, load and preferences. Plus the shared saves grid.

import type { HistoryEntry, PlayView, Save } from "../api.ts";
import type { Source } from "../source.ts";
import { action, fmtDate, h, modal, signal, toast, untrack, type Signal } from "../ui.ts";

export interface Prefs {
  /** Characters per second; at TEXT_INSTANT and above, lines appear at once. */
  text_speed: number;
  /** 1 (slow) to 10 (fast). */
  auto_speed: number;
  skip_unseen: boolean;
  show_ready: boolean;
  /** Ask for a name when saving (off: saves are just their slot). */
  name_saves: boolean;
  /** How opaque the dialogue box is, 0 (clear) to 1. */
  box_opacity: number;
  /** Play videos round and round (off: once, then hold the last frame). */
  loop_videos: boolean;
  /** Hear the characters' voices. */
  voices: boolean;
  /** Hear sound effects, background sounds and videos' sound. */
  sounds: boolean;
  /** How loud, 0 to 1. */
  volume: number;
}

export const TEXT_INSTANT = 200;
export const DEFAULT_PREFS: Prefs = {
  text_speed: 45,
  auto_speed: 5,
  skip_unseen: false,
  show_ready: true,
  name_saves: false,
  box_opacity: 0.3,
  loop_videos: false,
  voices: true,
  sounds: true,
  volume: 0.8,
};

export function readPrefs(raw: unknown): Prefs {
  const o = raw && typeof raw === "object" ? (raw as Partial<Prefs>) : {};
  return { ...DEFAULT_PREFS, ...o };
}

/** Milliseconds auto mode waits after a line of `chars` characters. */
export function autoDelay(p: Prefs, chars: number): number {
  return ((700 + chars * 45) * (11 - p.auto_speed)) / 5;
}

export function openHistory(view: PlayView, rewind: (index: number) => Promise<void>) {
  modal(
    "History",
    (close) => {
      const body = h(
        "div",
        { class: "history" },
        view.history.length ? null : h("p", { class: "muted" }, "Nothing yet."),
        view.history.map((e: HistoryEntry) =>
          h(
            "button",
            {
              class: "history-entry",
              "class:current": e.index === view.depth - 1,
              title: "Go back to here",
              "data-index": e.index,
              onClick: async () => {
                close();
                await rewind(e.index);
              },
            },
            e.lines.map((l) =>
              h(
                "p",
                { class: l.speaker ? "said" : "narration" },
                l.speaker ? h("b", null, l.speaker) : null,
                l.text,
              ),
            ),
            e.chose ? h("p", { class: "chose" }, `You chose: ${e.chose}`) : null,
          ),
        ),
      );
      requestAnimationFrame(() => (body.parentElement!.scrollTop = 1e9));
      return body;
    },
    "wide tall",
  );
}

/** Save: pick a slot (a taken one is replaced, after asking). `line`: the one on screen. */
export function openSave(src: Source, play: string, prefs: Prefs, line: number) {
  modal(
    "Save",
    (close) =>
      slotGrid(src, "save", async (slot, taken) => {
        if (taken && !confirm("Replace the save in this slot?")) return;
        let name = "";
        if (prefs.name_saves) {
          const n = prompt("Name this save", taken?.name ?? "");
          if (n === null) return;
          name = n.trim();
        }
        await src.save(play, name, slot, line);
        toast("Saved.");
        close();
      }),
    "wide",
  );
}

/** Load: pick a filled slot. */
export function openLoadInPlay(src: Source, play: string, apply: (v: PlayView, s: Save) => void) {
  modal(
    "Load",
    (close) =>
      slotGrid(src, "load", async (_slot, s) => {
        if (!s) return;
        apply(await src.load(s.id, play), s);
        close();
      }),
    "wide",
  );
}

export function openPrefs(src: Source, prefs: Signal<Prefs>) {
  const set = <K extends keyof Prefs>(k: K, v: Prefs[K]) => {
    const next = { ...prefs.peek(), [k]: v };
    prefs.set(next);
    src.setPrefs(next).catch((e: Error) => toast(e.message, "error"));
  };
  const range = (k: "text_speed" | "auto_speed", min: number, max: number, step: number) =>
    h("input", {
      type: "range",
      name: k,
      min,
      max,
      step,
      value: () => String(prefs()[k]),
      onChange: (e: Event) => set(k, Number((e.target as HTMLInputElement).value)),
    });
  const check = (
    k: "skip_unseen" | "show_ready" | "name_saves" | "loop_videos" | "voices" | "sounds",
  ) =>
    h("input", {
      type: "checkbox",
      name: k,
      checked: () => prefs()[k],
      onChange: (e: Event) => set(k, (e.target as HTMLInputElement).checked),
    });
  modal("Preferences", () =>
    h(
      "div",
      { class: "prefs" },
      h(
        "label",
        null,
        h("span", null, "Text speed"),
        range("text_speed", 10, TEXT_INSTANT, 5),
        h("small", null, () =>
          prefs().text_speed >= TEXT_INSTANT ? "instant" : `${prefs().text_speed} chars/s`,
        ),
      ),
      h(
        "label",
        null,
        h("span", null, "Auto-forward speed"),
        range("auto_speed", 1, 10, 1),
        h("small", null, () => `${prefs().auto_speed} / 10`),
      ),
      h(
        "label",
        { class: "check" },
        check("skip_unseen"),
        h("span", null, "Skip text I haven't read yet"),
      ),
      h(
        "label",
        null,
        h("span", null, "Dialogue box"),
        h("input", {
          type: "range",
          name: "box_opacity",
          min: 0,
          max: 1,
          step: 0.05,
          value: () => String(prefs().box_opacity),
          onInput: (e: Event) => set("box_opacity", Number((e.target as HTMLInputElement).value)),
        }),
        h("small", null, () => `${Math.round(prefs().box_opacity * 100)}% opaque`),
      ),
      h(
        "label",
        { class: "check" },
        check("show_ready"),
        h("span", null, "Show readiness indicators"),
      ),
      h("label", { class: "check" }, check("name_saves"), h("span", null, "Name saves")),
      h("label", { class: "check" }, check("loop_videos"), h("span", null, "Loop videos")),
      h("label", { class: "check" }, check("voices"), h("span", null, "Voices")),
      h(
        "label",
        { class: "check" },
        check("sounds"),
        h("span", null, "Sounds (effects, background, video sound)"),
      ),
      h(
        "label",
        null,
        h("span", null, "Volume"),
        h("input", {
          type: "range",
          name: "volume",
          min: 0,
          max: 1,
          step: 0.05,
          value: () => String(prefs().volume),
          onInput: (e: Event) => set("volume", Number((e.target as HTMLInputElement).value)),
        }),
        h("small", null, () => `${Math.round(prefs().volume * 100)}%`),
      ),
      h(
        "p",
        { class: "muted small" },
        "Keys: Space / Enter / click to go on, Backspace or scroll up to go back, hold Ctrl to skip, A auto, H hide, F fullscreen, M map, E edit, Esc close.",
      ),
    ),
  );
}

const PER_PAGE = 6;
const PAGES = 9;

/**
 * The save screen: pages of slots, each showing the screen saved there (its picture and
 * line) or empty. `pick` gets the slot and what's in it.
 */
export function slotGrid(
  src: Source,
  mode: "save" | "load",
  pick: (slot: number, s: Save | null) => Promise<void>,
): Node {
  const saves = signal<Save[] | null>(null);
  const page = signal(0);
  const reload = () => src.saves().then(saves.set, (e: Error) => toast(e.message, "error"));
  void reload();
  /** Each save by slot; older saves without one fill the free slots in order. */
  const bySlot = () => {
    const m = new Map<number, Save>();
    const list = saves() ?? [];
    for (const s of list) if (s.slot != null) m.set(s.slot, s);
    let free = 1;
    for (const s of [...list].filter((s) => s.slot == null).sort((a, b) => a.at - b.at)) {
      while (m.has(free)) free++;
      m.set(free, s);
    }
    return m;
  };
  return h(
    "div",
    { class: "saves", "data-mode": mode },
    () => {
      if (!saves()) return h("p", { class: "muted" }, "Loading...");
      const m = bySlot();
      const first = page() * PER_PAGE + 1;
      return untrack(() =>
        h(
          "div",
          { class: "save-grid" },
          Array.from({ length: PER_PAGE }, (_, i) => {
            const slot = first + i;
            const s = m.get(slot) ?? null;
            const picture = s?.thumb
              ? h("img", {
                  src: src.mediaUrl(s.thumb),
                  alt: "",
                  loading: "lazy",
                  // A video's screen: show a frame of it instead.
                  onError: (e: Event) => {
                    const img = e.target as HTMLImageElement;
                    const v = h("video", {
                      src: `${src.mediaUrl(s.thumb!)}#t=0.5`,
                      muted: true,
                      playsinline: true,
                      preload: "auto",
                    });
                    img.replaceWith(v);
                  },
                })
              : h("div", { class: "no-thumb" }, s ? "" : "Empty");
            const inner = [
              // Saved at a decision: the screen as it was, with its buttons.
              s?.choices?.length
                ? h(
                    "div",
                    { class: "save-shot" },
                    picture,
                    h(
                      "div",
                      { class: "save-choices" },
                      s.choices.map((c) => h("span", { class: "save-choice" }, c)),
                    ),
                  )
                : picture,
              h(
                "div",
                { class: "save-meta" },
                h("strong", null, s?.name || `Slot ${slot}`),
                s ? h("span", { class: "excerpt" }, s.excerpt) : null,
                s ? h("time", null, fmtDate(s.at)) : null,
              ),
            ];
            const usable = mode === "save" || !!s;
            return h(
              "div",
              {
                class: "save-slot",
                "class:empty": !s,
                "data-slot": slot,
                "data-save": s?.id ?? "",
              },
              usable
                ? action(inner, () => pick(slot, s), {
                    class: "save-open",
                    title:
                      mode === "save" ? (s ? "Save over this" : "Save here") : "Load this save",
                  })
                : h("div", { class: "save-open static" }, inner),
              s
                ? action(
                    "×",
                    async () => {
                      if (!confirm("Delete this save?")) return;
                      await src.deleteSave(s.id);
                      await reload();
                    },
                    { class: "icon save-delete", title: "Delete this save" },
                  )
                : null,
            );
          }),
        ),
      );
    },
    h(
      "nav",
      { class: "save-pages" },
      Array.from({ length: PAGES }, (_, i) =>
        h(
          "button",
          {
            "class:on": () => page() === i,
            "data-page": i + 1,
            onClick: () => page.set(i),
          },
          String(i + 1),
        ),
      ),
    ),
  );
}
