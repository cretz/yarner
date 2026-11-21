// Title screen: open stories with a passphrase, create one, or import a bundle.

import {
  clearAll,
  createStory,
  importBundle,
  setVault,
  StoryApi,
  unlock,
  vaultState,
  type StoryRef,
} from "../api.ts";
import { openBundle } from "../bundle.ts";
import type { Nav } from "../main.ts";
import { action, bind, h, modal, signal, toast, untrack } from "../ui.ts";

type Tab = "open" | "new" | "import" | "file";

/**
 * The stories the last passphrase opened, for the story list. Titles stay in memory;
 * only their handles (opaque ids, no content) are kept in this tab's sessionStorage, so
 * the list survives a refresh.
 */
let opened: StoryRef[] = [];
const OPENED = "yarner.opened";

function keep(list: StoryRef[]) {
  opened = list;
  try {
    if (list.length) sessionStorage.setItem(OPENED, JSON.stringify(list.map((s) => s.handle)));
    else sessionStorage.removeItem(OPENED);
  } catch {
    /* storage off: the list lasts until a refresh */
  }
}

/** The handles kept in the tab (null: storage off). */
function storedHandles(): string[] | null {
  try {
    return JSON.parse(sessionStorage.getItem(OPENED) ?? "[]");
  } catch {
    return null;
  }
}

/**
 * The list, brought in line with the tab's: stories opened elsewhere (created, imported,
 * from a link) are asked for and added; ones locked or forgotten since drop out. Merged
 * when the answers come, so a story unlocked meanwhile stays.
 */
async function restore(): Promise<StoryRef[]> {
  const handles = storedHandles();
  if (!handles) return opened;
  const fresh = await Promise.all(
    handles
      .filter((h) => !opened.some((s) => s.handle === h))
      .map((handle) =>
        new StoryApi(handle)
          .info()
          .then((i) => ({ handle, id: i.id, title: i.title }) as StoryRef)
          .catch(() => null),
      ),
  );
  const now = storedHandles() ?? handles;
  const merged = [...opened];
  for (const s of fresh) if (s && !merged.some((o) => o.handle === s.handle)) merged.push(s);
  keep(merged.filter((s) => now.includes(s.handle)));
  return opened;
}

/** A story was deleted: drop it from the list. */
export function forgetStory(handle: string) {
  keep(opened.filter((s) => s.handle !== handle));
}

/** Is a yarner server behind this page? A static host (the player alone) has none. */
let server: Promise<boolean> | null = null;
function hasServer(): Promise<boolean> {
  server ??= fetch("api/ping", { cache: "no-store" })
    .then((r) => (r.headers.get("content-type") ?? "").includes("application/json"))
    .catch(() => false);
  return server;
}

export function titleScreen(nav: Nav, msg?: string): Node {
  const tab = signal<Tab>("open");
  const online = signal(true);
  void hasServer().then((ok) => {
    online.set(ok);
    if (!ok) tab.set("file");
  });
  const allTabs: [Tab, string][] = [
    ["open", "Open"],
    ["new", "New story"],
    ["import", "Import"],
    ["file", "Play a file"],
  ];
  const tabs = () => (online() ? allTabs : allTabs.filter(([t]) => t === "file"));
  return h(
    "div",
    { class: "title-screen" },
    h("div", { class: "title-glow" }),
    h(
      "div",
      { class: "title-card" },
      h("h1", { class: "logo" }, "yarner"),
      h("p", { class: "tagline" }, "Build and play interactive adventure stories."),
      msg ? h("p", { class: "notice" }, msg) : null,
      h("nav", { class: "tabs" }, () =>
        tabs().map(([t, label]) =>
          h(
            "button",
            {
              "class:on": () => tab() === t,
              onClick: () => tab.set(t),
              "data-tab": t,
            },
            label,
          ),
        ),
      ),
      () =>
        // Hidden until it works (2026-11-21: stories still opened after clearing).
        online()
          ? h(
              "button",
              {
                class: "link danger clear-all",
                onClick: async () => {
                  if (
                    !confirm(
                      "Delete EVERY story on this server, with all their pictures, video and saves? There is no undo.",
                    )
                  )
                    return;
                  try {
                    const r = await clearAll();
                    keep([]);
                    toast(`Deleted ${r.deleted} ${r.deleted === 1 ? "story" : "stories"}.`);
                    location.hash = "#/";
                    location.reload();
                  } catch (e) {
                    toast((e as Error).message, "error");
                  }
                },
              },
              "Clear all stories",
            )
          : null,
      () => (online() ? vaultButton() : null),
      () => {
        const t = tab();
        return untrack(() =>
          t === "open"
            ? openForm(nav)
            : t === "new"
              ? newForm(nav)
              : t === "import"
                ? importForm(nav)
                : fileForm(nav),
        );
      },
    ),
  );
}

/**
 * The generator's LoRA vault for every story (not per story): its password opens the
 * adapters stored for each model, and every picture and video uses them at their own
 * weights. Kept in the server's memory only: asked again after it restarts.
 */
function vaultButton(): Node {
  const set = signal(false);
  vaultState().then(
    (v) => set.set(v.set),
    () => {},
  );
  const open = () =>
    modal("LoRA vault", (close) => {
      const pw = signal("");
      return h(
        "form",
        { class: "form", onSubmit: (e: Event) => e.preventDefault() },
        h(
          "p",
          { class: "muted small" },
          "Opens your thinfer adapter vault: then pick which adapters each story uses, per model, in its Settings. Kept in memory only, never saved.",
        ),
        h(
          "label",
          null,
          "Vault password",
          h("input", {
            type: "password",
            name: "vault-password",
            autocomplete: "off",
            ...bind(pw),
          }),
        ),
        h(
          "div",
          { class: "row" },
          action(
            "Use it",
            async () => {
              const r = await setVault(pw());
              set.set(r.set);
              if (r.refused) return toast("That password doesn't open the vault.", "error");
              if (r.set) toast("Vault open: pick adapters per model in a story's Settings.");
              else toast("The generator here doesn't take a LoRA vault.", "error");
              close();
            },
            { class: "primary", type: "submit", disabled: () => !pw() },
          ),
          () =>
            set()
              ? action("Forget it", async () => {
                  set.set((await setVault(null)).set);
                  close();
                })
              : null,
        ),
      );
    });
  return h("button", { class: "link vault-open", onClick: open }, () =>
    set() ? "LoRA vault: on" : "LoRA vault…",
  );
}

function openForm(nav: Nav): Node {
  const pass = signal("");
  // Back from a story or after a refresh: the list is still there, no passphrase needed.
  const found = signal<StoryRef[] | null>(opened.length ? opened : null);
  void restore().then((s) => {
    if (s.length || found()) found.set(s.length ? [...s] : null);
  });
  const submit = async () => {
    if (!pass()) return;
    const s = await unlock(pass());
    // Opened before with another passphrase: kept in the list too.
    keep([...opened.filter((o) => !s.some((x) => x.handle === o.handle)), ...s]);
    found.set(opened);
  };
  return h(
    "form",
    {
      class: "form",
      onSubmit: (e: Event) => {
        e.preventDefault();
        submit().catch((err: Error) => toast(err.message, "error"));
      },
    },
    h(
      "label",
      null,
      "Passphrase",
      h("input", {
        type: "password",
        name: "passphrase",
        autocomplete: "current-password",
        autofocus: true,
        ...bind(pass),
      }),
    ),
    h("button", { class: "primary", type: "submit" }, "Open"),
    () => {
      const s = found();
      if (!s) return null;
      return untrack(() => {
        if (!s.length) return h("p", { class: "muted" }, "No stories open with that passphrase.");
        return h(
          "div",
          { class: "story-list" },
          h("p", { class: "muted" }, "Pick a story:"),
          s.map((r) =>
            h(
              "button",
              { class: "story-pick", onClick: () => nav.open(r.handle) },
              r.title || "Untitled",
            ),
          ),
        );
      });
    },
  );
}

/** Play an exported story file right here, read-only, with nothing stored. */
function fileForm(nav: Nav): Node {
  const file = signal<File | null>(null);
  const pass = signal("");
  return h(
    "form",
    { class: "form", onSubmit: (e: Event) => e.preventDefault() },
    h(
      "p",
      { class: "muted" },
      "Play an exported story in this browser. Nothing is stored: closing the tab forgets it.",
    ),
    h(
      "label",
      null,
      "Story file",
      h("input", {
        type: "file",
        name: "story-file",
        accept: ".yarner,application/zip",
        onChange: (e: Event) => file.set((e.target as HTMLInputElement).files?.[0] ?? null),
      }),
    ),
    h(
      "label",
      null,
      "Passphrase",
      h("input", {
        type: "password",
        name: "file-passphrase",
        autocomplete: "off",
        ...bind(pass),
      }),
    ),
    action("Play", async () => nav.openSource(await openBundle(file()!, pass())), {
      class: "primary",
      type: "submit",
      disabled: () => !file() || !pass(),
    }),
  );
}

const WARNING =
  "There is no recovery. Your story is encrypted with this passphrase and nobody can open it without it: if you forget it, the story is gone for good.";

function newForm(nav: Nav): Node {
  const title = signal("");
  const pass = signal("");
  const confirm = signal("");
  const problem = () =>
    pass().length < 8
      ? "Use at least 8 characters."
      : pass() !== confirm()
        ? "The passphrases don't match."
        : null;
  return h(
    "form",
    { class: "form", onSubmit: (e: Event) => e.preventDefault() },
    h(
      "label",
      null,
      "Title",
      h("input", { name: "title", placeholder: "Untitled", ...bind(title) }),
    ),
    h(
      "label",
      null,
      "Passphrase",
      h("input", {
        type: "password",
        name: "new-passphrase",
        autocomplete: "new-password",
        ...bind(pass),
      }),
    ),
    h(
      "label",
      null,
      "Confirm passphrase",
      h("input", {
        type: "password",
        name: "confirm",
        autocomplete: "new-password",
        ...bind(confirm),
      }),
    ),
    h("p", { class: "warning" }, WARNING),
    () =>
      pass() || confirm() ? h("p", { class: "muted small" }, problem() ?? "Looks good.") : null,
    action(
      "Create story",
      async () => {
        const r = await createStory(title().trim() || "Untitled", pass());
        nav.open(r.handle, { to: "setup" });
      },
      { class: "primary", type: "submit", disabled: () => !!problem() },
    ),
  );
}

function importForm(nav: Nav): Node {
  const file = signal<File | null>(null);
  const bundlePass = signal("");
  const pass = signal("");
  const confirm = signal("");
  const problem = () =>
    !file()
      ? "Pick a .yarner bundle."
      : !bundlePass()
        ? "Enter the bundle's passphrase."
        : pass().length < 8
          ? "Use at least 8 characters for the new passphrase."
          : pass() !== confirm()
            ? "The new passphrases don't match."
            : null;
  return h(
    "form",
    { class: "form", onSubmit: (e: Event) => e.preventDefault() },
    h(
      "label",
      null,
      "Story bundle",
      h("input", {
        type: "file",
        name: "bundle",
        accept: ".yarner,.zip,application/zip",
        onChange: (e: Event) => file.set((e.target as HTMLInputElement).files?.[0] ?? null),
      }),
    ),
    h(
      "label",
      null,
      "Bundle passphrase",
      h("input", {
        type: "password",
        name: "bundle-passphrase",
        ...bind(bundlePass),
      }),
    ),
    h(
      "label",
      null,
      "New passphrase (to open it here from now on)",
      h("input", {
        type: "password",
        name: "import-passphrase",
        autocomplete: "new-password",
        ...bind(pass),
      }),
    ),
    h(
      "label",
      null,
      "Confirm new passphrase",
      h("input", {
        type: "password",
        name: "import-confirm",
        autocomplete: "new-password",
        ...bind(confirm),
      }),
    ),
    h("p", { class: "warning" }, WARNING),
    () => h("p", { class: "muted small" }, problem() ?? "Ready to import."),
    action(
      "Import story",
      async () => {
        const r = await importBundle(file()!, bundlePass(), pass());
        nav.open(r.handle);
      },
      { class: "primary", type: "submit", disabled: () => !!problem() },
    ),
  );
}
