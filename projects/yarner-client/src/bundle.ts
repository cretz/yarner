// A read-only source: an exported story file opened in the browser through the wasm
// player. Nothing is stored: playthroughs, saves and prefs live in memory for this tab,
// decrypted media only as blob URLs, revoked when the story is closed. Choices nobody
// took before export are unavailable.

import type { Player } from "../../yarner-wasm/pkg/yarner_wasm.js";
import type {
  AdvanceResult,
  Hop,
  MapView,
  Play,
  PlayView,
  Save,
  ServerMsg,
  StoryInfo,
} from "./api.ts";
import type { Source } from "./source.ts";

let wasm: Promise<typeof import("../../yarner-wasm/pkg/yarner_wasm.js")> | null = null;

async function load() {
  wasm ??= import("../../yarner-wasm/pkg/yarner_wasm.js").then(async (m) => {
    await m.default();
    return m;
  });
  return wasm;
}

/** What the save screen shows of a save at `line` (as the server keeps it). */
function saveScreen(v: PlayView, line?: number) {
  const ls = v.beat?.lines ?? [];
  const i = line == null ? 0 : Math.min(line, Math.max(0, ls.length - 1));
  const next = v.beat?.next;
  const atChoices = line != null && i + 1 >= ls.length && next?.type === "choices";
  return {
    excerpt: ls[i]?.text.slice(0, 120) ?? "",
    line: line == null ? null : i,
    choices: atChoices ? next.choices.map((c) => c.label) : [],
  };
}

/** Open a story file. Rejects with "wrong passphrase" on a bad passphrase. */
export async function openBundle(file: Blob, passphrase: string): Promise<BundleSource> {
  const m = await load();
  const bytes = new Uint8Array(await file.arrayBuffer());
  return new BundleSource(new m.Player(bytes, passphrase));
}

let ids = 0;
const newId = (p: string) => `${p}${++ids}`;

export class BundleSource implements Source {
  readonly kind = "bundle" as const;
  readonly edit = null;
  private player: Player | null;
  private title: string;
  private playsById = new Map<string, Play>();
  private savesById = new Map<string, Save>();
  private prefsValue: unknown = {};
  private urls = new Map<string, string>();

  constructor(player: Player) {
    this.player = player;
    this.title = player.title();
  }

  private p(): Player {
    if (!this.player) throw new Error("The story is closed.");
    return this.player;
  }

  private path(play: string): Hop[] {
    const p = this.playsById.get(play);
    if (!p) throw new Error("No such playthrough.");
    return p.path;
  }

  private setPath(play: string, path: Hop[]) {
    const p = this.playsById.get(play);
    if (p) p.path = path;
  }

  private view(play: string): PlayView {
    const v = JSON.parse(this.p().view(JSON.stringify(this.path(play)))) as PlayView;
    const p = this.playsById.get(play)!;
    return { ...v, id: p.id, name: p.name };
  }

  async info(): Promise<StoryInfo> {
    return {
      id: "bundle",
      title: this.title,
      plays: [...this.playsById.values()].sort((a, b) => b.at - a.at),
      saves: await this.saves(),
      prefs: this.prefsValue,
      keep_secs: null,
      started: true,
    };
  }

  async lock(): Promise<void> {
    for (const u of this.urls.values()) URL.revokeObjectURL(u);
    this.urls.clear();
    this.player?.free();
    this.player = null;
  }

  mediaUrl(id: string): string {
    let u = this.urls.get(id);
    if (!u) {
      try {
        const p = this.p();
        const mime = p.media_mime(id) || "application/octet-stream";
        u = URL.createObjectURL(new Blob([p.media(id) as BlobPart], { type: mime }));
      } catch {
        // Not in this file (unfinished when exported): an empty image.
        u = "data:,";
      }
      this.urls.set(id, u);
    }
    return u;
  }

  subscribe(_play: string | null, _on: (m: ServerMsg) => void): () => void {
    return () => {};
  }

  async plays(): Promise<Play[]> {
    return [...this.playsById.values()];
  }

  async newPlay(name?: string): Promise<{ id: string }> {
    const id = newId("p");
    const path = JSON.parse(this.p().start()) as Hop[];
    this.playsById.set(id, {
      id,
      name: name || `Playthrough ${this.playsById.size + 1}`,
      path,
      at: Date.now(),
    });
    return { id };
  }

  async play(p: string): Promise<PlayView> {
    return this.view(p);
  }

  async deletePlay(p: string): Promise<void> {
    this.playsById.delete(p);
  }

  async advance(
    p: string,
    exit?: number | null,
  ): Promise<{ result: AdvanceResult; view: PlayView }> {
    const r = JSON.parse(this.p().advance(JSON.stringify(this.path(p)), exit ?? undefined)) as {
      result: "moved" | "ended" | "unavailable";
      path: Hop[];
    };
    if (r.result === "moved") {
      this.setPath(p, r.path);
      this.playsById.get(p)!.at = Date.now();
    }
    // A choice nobody took before export can't be followed: treat it as the end of
    // this line (the client disables those choices anyway).
    return {
      result: { type: r.result === "moved" ? "moved" : "ended" },
      view: this.view(p),
    };
  }

  async back(p: string): Promise<PlayView> {
    const path = this.path(p);
    if (path.length > 1) {
      path.pop();
      path[path.length - 1] = { beat: path[path.length - 1].beat, exit: null };
    }
    return this.view(p);
  }

  async rewind(p: string, index: number): Promise<PlayView> {
    const path = this.path(p).slice(0, index + 1);
    if (path.length) path[path.length - 1] = { beat: path[path.length - 1].beat, exit: null };
    this.setPath(p, path);
    return this.view(p);
  }

  async favor(): Promise<void> {}

  async save(p: string, name: string, slot?: number, line?: number): Promise<{ id: string }> {
    const v = this.view(p);
    const id = newId("s");
    if (slot != null)
      for (const [k, s] of this.savesById) if (s.slot === slot) this.savesById.delete(k);
    this.savesById.set(id, {
      id,
      name: name || "Save",
      play: p,
      path: structuredClone(this.path(p)),
      thumb: v.beat?.screen?.id ?? null,
      ...saveScreen(v, line),
      slot: slot ?? null,
      at: Date.now(),
    });
    return { id };
  }

  async saves(): Promise<Save[]> {
    return [...this.savesById.values()].sort((a, b) => b.at - a.at);
  }

  async load(save: string, play: string): Promise<PlayView> {
    const s = this.savesById.get(save);
    if (!s) throw new Error("No such save.");
    if (!this.playsById.has(play))
      this.playsById.set(play, {
        id: play,
        name: s.name,
        path: [],
        at: Date.now(),
      });
    this.setPath(play, structuredClone(s.path));
    return this.view(play);
  }

  async deleteSave(save: string): Promise<void> {
    this.savesById.delete(save);
  }

  async map(play?: string | null): Promise<MapView> {
    const path = play ? this.path(play) : [];
    return JSON.parse(this.p().map(JSON.stringify(path))) as MapView;
  }

  async prefs(): Promise<unknown> {
    return this.prefsValue;
  }

  async setPrefs(prefs: unknown): Promise<void> {
    this.prefsValue = prefs;
  }
}
