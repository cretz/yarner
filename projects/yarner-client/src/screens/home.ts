// Story home: start (from the beginning), load a save, play & edit (the author's place),
// set up, export, lock. Playing keeps no hidden place: where you are lasts until you
// leave, unless you save it.

import type { StoryInfo } from "../api.ts";
import { keepControl } from "../keep.ts";
import { slotGrid } from "../play/overlays.ts";
import type { Nav } from "../main.ts";
import type { Source } from "../source.ts";
import { action, h, modal, onCleanup, signal, toast, untrack, type Signal } from "../ui.ts";
import { openExport } from "./export.ts";
import { forgetStory } from "./title.ts";

/** The playthrough a player plays in (started fresh each time, or from a save). */
export const PLAYER = "Player";
/** The author's place while building (resumed where they left off). */
export const AUTHOR = "Author";

/** A new player playthrough at the beginning (the previous one, never saved, goes). */
export async function freshPlay(src: Source): Promise<string> {
  for (const p of await src.plays())
    if (p.name === PLAYER) await src.deletePlay(p.id).catch(() => {});
  return (await src.newPlay(PLAYER)).id;
}

/** The author's playthrough (made the first time). */
export async function authorPlay(src: Source): Promise<string> {
  const p = (await src.plays()).find((x) => x.name === AUTHOR);
  return p?.id ?? (await src.newPlay(AUTHOR)).id;
}

export function homeScreen(src: Source, nav: Nav): Node {
  const info = signal<StoryInfo | null>(null);
  const backdrop = signal<string | null>(null);
  const reload = async () => {
    const i = await src.info();
    info.set(i);
    const author = i.plays.find((p) => p.name === AUTHOR);
    if (author && !backdrop.peek()) {
      const v = await src.play(author.id).catch(() => null);
      const m = v?.beat?.screen;
      if (m?.status === "ready" && m.kind === "image") backdrop.set(src.mediaUrl(m.id));
    }
  };
  reload().catch((e: Error) => toast(e.message, "error"));
  // Being connected counts as attending the story (it stays unlocked).
  onCleanup(src.subscribe(null, (m) => m.type === "error" && toast(m.message, "error")));

  const newPlay = async () => {
    // Starting over drops where the reader is now (only saves keep it): ask first.
    const going = info()?.plays.some((x) => x.name === PLAYER && x.path.length > 0);
    if (
      going &&
      !confirm("Start over from the beginning? Where you are now is lost unless you saved it.")
    )
      return;
    nav.go({ to: "play", play: await freshPlay(src) });
  };

  return h(
    "div",
    { class: "home" },
    () =>
      backdrop()
        ? h("div", {
            class: "home-backdrop",
            style: `background-image:url("${backdrop()}")`,
          })
        : null,
    h("div", { class: "home-shade" }),
    () => {
      const i = info();
      if (!i) return h("div", { class: "home-card" }, h("p", { class: "muted" }, "Opening..."));
      return untrack(() => homeCard(src, nav, i, newPlay));
    },
  );
}

function homeCard(src: Source, nav: Nav, i: StoryInfo, newPlay: () => Promise<void>): Node {
  return h(
    "div",
    { class: "home-card" },
    h("p", { class: "eyebrow" }, "yarner"),
    h("h1", { class: "story-title" }, i.title || "Untitled"),
    h(
      "p",
      { class: "muted" },
      i.saves.length
        ? i.saves.length === 1
          ? "1 save"
          : `${i.saves.length} saves`
        : "No saves yet",
      i.started ? "" : ". The story hasn't started yet.",
    ),
    h(
      "div",
      { class: "menu" },
      // Where the player left off (leaving the play screen doesn't lose the place).
      (() => {
        const p = i.plays.find((x) => x.name === PLAYER && x.path.length > 0);
        return p
          ? h(
              "button",
              {
                class: "menu-item primary",
                title: "Carry on where you left off",
                onClick: () => nav.go({ to: "play", play: p.id }),
              },
              "Continue",
            )
          : null;
      })(),
      action(
        i.plays.some((x) => x.name === PLAYER && x.path.length > 0) ? "Start over" : "Start",
        newPlay,
        {
          class: i.plays.some((x) => x.name === PLAYER && x.path.length > 0)
            ? "menu-item"
            : "menu-item primary",
          title: "Play from the beginning",
        },
      ),
      h(
        "button",
        {
          class: "menu-item",
          disabled: !i.saves.length,
          onClick: () => openLoad(src, nav),
        },
        "Load",
      ),
      src.edit
        ? action(
            "Play & edit",
            async () => nav.go({ to: "play", play: await authorPlay(src), edit: true }),
            {
              class: "menu-item",
              title: "Build the story: play with the editing tools",
            },
          )
        : null,
      src.edit
        ? h("button", { class: "menu-item", onClick: () => nav.go({ to: "setup" }) }, "Setup")
        : null,
      src.edit
        ? h("button", { class: "menu-item", onClick: () => openExport(src, i.title) }, "Export")
        : null,
      src.edit
        ? h(
            "button",
            {
              class: "menu-item",
              title: "Start over in a new story with this one's setup (this one stays)",
              onClick: () => openNewFromSetup(src, nav, i.title),
            },
            "New story from this setup",
          )
        : null,
      src.kind === "server"
        ? h(
            "button",
            {
              class: "menu-item",
              title: "Back to your list of stories (this one stays open)",
              onClick: () => nav.go({ to: "title" }),
            },
            "All stories",
          )
        : null,
      h("button", { class: "menu-item", onClick: () => void nav.lock() }, "Lock"),
      src.edit
        ? h(
            "button",
            {
              class: "menu-item danger",
              onClick: () => openDelete(src, nav, i.title),
            },
            "Delete story",
          )
        : null,
    ),
    src.edit ? keepControl(src.edit, () => src.info()) : null,
  );
}

/**
 * A new, empty story with this one's setup: prompts, characters and their pictures,
 * settings. Directions given during play and the story so far stay here.
 */
export function openNewFromSetup(src: Source, nav: Nav, title: string) {
  const name = signal(title ? `${title} (new)` : "");
  const pass = signal("");
  const confirm = signal("");
  const problem = () =>
    pass().length < 8
      ? "Use at least 8 characters."
      : pass() !== confirm()
        ? "The passphrases don't match."
        : null;
  const field = (label: string, s: Signal<string>, attrs: Record<string, unknown>) =>
    h(
      "label",
      null,
      label,
      h("input", {
        ...attrs,
        value: () => s(),
        onInput: (e: Event) => s.set((e.target as HTMLInputElement).value),
      }),
    );
  modal("New story from this setup", (close) =>
    h(
      "form",
      { class: "form", onSubmit: (e: Event) => e.preventDefault() },
      h(
        "p",
        null,
        "A fresh story that starts with this one's setup: the story and style prompts, every character and place with their pictures, and the settings. Nothing written or played comes along, and this story stays as it is.",
      ),
      field("Title", name, { name: "copy-title", placeholder: "Untitled" }),
      field("Passphrase", pass, {
        type: "password",
        name: "copy-passphrase",
        autocomplete: "new-password",
      }),
      field("Confirm passphrase", confirm, {
        type: "password",
        name: "copy-confirm",
        autocomplete: "new-password",
      }),
      h(
        "p",
        { class: "muted small" },
        "Use this story's passphrase to keep both in one list. Forget it and the new story is gone for good.",
      ),
      () =>
        pass() || confirm() ? h("p", { class: "muted small" }, problem() ?? "Looks good.") : null,
      action(
        "Create story",
        async () => {
          const r = await src.edit!.newFromSetup(name().trim() || "Untitled", pass());
          close();
          toast("New story made from this setup.");
          nav.open(r.handle, { to: "setup" });
        },
        { class: "primary", type: "submit", disabled: () => !!problem() },
      ),
    ),
  );
}

/** Delete the story: its passphrase, typed again, confirms it. */
function openDelete(src: Source, nav: Nav, title: string) {
  const pass = signal("");
  modal("Delete story", (close) =>
    h(
      "form",
      {
        class: "form",
        onSubmit: (e: Event) => e.preventDefault(),
      },
      h(
        "p",
        null,
        `This deletes "${title || "Untitled"}" for good: every beat, picture, video and save. There is no undo.`,
      ),
      h(
        "label",
        null,
        "Its passphrase, to confirm",
        h("input", {
          type: "password",
          name: "delete-passphrase",
          autocomplete: "off",
          value: () => pass(),
          onInput: (e: Event) => pass.set((e.target as HTMLInputElement).value),
        }),
      ),
      action(
        "Delete for good",
        async () => {
          const handle = sessionStorage.getItem("yarner.handle");
          await src.edit!.deleteStory(pass());
          if (handle) forgetStory(handle);
          close();
          toast("Story deleted.");
          await nav.lock();
        },
        { class: "danger", type: "submit", disabled: () => !pass() },
      ),
    ),
  );
}

/** Saves list for the home screen: loading one continues its playthrough. */
function openLoad(src: Source, nav: Nav) {
  modal(
    "Load",
    (close) =>
      slotGrid(src, "load", async (_slot, s) => {
        if (!s) return;
        // Played on from the save, in a fresh player playthrough; with the editing tools
        // when the story can be edited (else Load lost Edit: Play & edit, then Load).
        const play = await freshPlay(src);
        await src.load(s.id, play);
        close();
        nav.go({ to: "play", play, edit: !!src.edit });
      }),
    "wide",
  );
}
