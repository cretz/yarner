// App shell: the open story (its handle lives in sessionStorage only) and the screen router.
// Routes live in the location hash and carry ids only, never story content.

import { checkServer, onLocked, StoryApi } from "./api.ts";
import { playScreen } from "./play/play.ts";
import { clearThumbs } from "./map.ts";
import { homeScreen } from "./screens/home.ts";
import { setupScreen } from "./screens/setup.ts";
import { titleScreen } from "./screens/title.ts";
import type { Source } from "./source.ts";
import { h, signal, untrack } from "./ui.ts";

export type Route =
  | { to: "title"; msg?: string }
  | { to: "home" }
  | { to: "setup" }
  /** `edit`: with the editing tools (Play & edit); otherwise just playing. */
  | { to: "play"; play: string; edit?: boolean };

export interface Nav {
  go(r: Route): void;
  /** Make a story the open one (after unlock / create / import). */
  open(handle: string, then?: Route): void;
  /** Open a read-only story (a story file played in the browser). */
  openSource(s: Source, then?: Route): void;
  /** Lock the story and go back to the title screen. */
  lock(): Promise<void>;
  readonly src: Source | null;
}

const KEY = "yarner.handle";
let src: Source | null = null;
const route = signal<Route>({ to: "title" });

function hashOf(r: Route): string {
  return r.to === "play"
    ? `#/${r.edit ? "edit" : "play"}/${r.play}`
    : r.to === "title"
      ? "#/"
      : `#/${r.to}`;
}

function parseHash(): Route {
  const [, a, b] = location.hash.split("/");
  if (a === "play" && b) return { to: "play", play: b };
  if (a === "edit" && b) return { to: "play", play: b, edit: true };
  if (a === "setup") return { to: "setup" };
  if (a === "home") return { to: "home" };
  // "#/" is the story list, also after a refresh (2026-11-21 alpha: a refresh there
  // jumped back into the last story).
  return { to: "title" };
}

const nav: Nav = {
  get src() {
    return src;
  },
  go(r) {
    if (r.to !== "title" && !src) r = { to: "title" };
    if (location.hash !== hashOf(r)) history.pushState(null, "", hashOf(r));
    route.set(r);
  },
  open(handle, then = { to: "home" }) {
    sessionStorage.setItem(KEY, handle);
    // On the story list from now on (handles only), however it was opened.
    try {
      const list: string[] = JSON.parse(sessionStorage.getItem("yarner.opened") ?? "[]");
      if (!list.includes(handle))
        sessionStorage.setItem("yarner.opened", JSON.stringify([...list, handle]));
    } catch {
      /* storage off: the list lasts until a refresh */
    }
    src = new StoryApi(handle);
    nav.go(then);
  },
  openSource(s, then = { to: "home" }) {
    sessionStorage.removeItem(KEY);
    src = s;
    nav.go(then);
  },
  async lock() {
    const s = src;
    forget();
    nav.go({ to: "title" });
    await s?.lock().catch(() => {});
  },
};

function forget() {
  const handle = sessionStorage.getItem(KEY);
  sessionStorage.removeItem(KEY);
  // Locked means forgotten: off the story list, and no place kept for it.
  try {
    const list: string[] = JSON.parse(sessionStorage.getItem("yarner.opened") ?? "[]");
    const left = list.filter((h) => h !== handle);
    if (left.length) sessionStorage.setItem("yarner.opened", JSON.stringify(left));
    else sessionStorage.removeItem("yarner.opened");
    for (const k of Object.keys(sessionStorage))
      if (k.startsWith("yarner.line.")) sessionStorage.removeItem(k);
  } catch {
    /* storage off: nothing kept anyway */
  }
  // A read-only story only lives in this tab: closing it frees its memory.
  if (src?.kind === "bundle") void src.lock();
  src = null;
  clearThumbs();
}

onLocked(() => {
  if (!src) return;
  forget();
  nav.go({
    to: "title",
    msg: "The story was locked. Enter its passphrase to open it again.",
  });
});

addEventListener("popstate", () => nav.go(src ? parseHash() : { to: "title" }));

function screen(r: Route): Node {
  if (r.to === "title" || !src) return titleScreen(nav, r.to === "title" ? r.msg : undefined);
  if (r.to === "setup") return src.edit ? setupScreen(src, nav) : homeScreen(src, nav);
  if (r.to === "play") return playScreen(src, r.play, nav, !!r.edit);
  return homeScreen(src, nav);
}

async function start() {
  void checkServer();
  const handle = sessionStorage.getItem(KEY);
  if (handle) {
    const s = new StoryApi(handle);
    try {
      await s.info();
      src = s;
      nav.go(parseHash());
    } catch {
      forget();
      nav.go({ to: "title" });
    }
  } else nav.go({ to: "title" });
  document.getElementById("app")!.append(
    h("div", { class: "app" }, () => {
      const r = route();
      return untrack(() => screen(r));
    }),
  );
}

void start();
