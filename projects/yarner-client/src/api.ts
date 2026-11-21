// Typed wrappers for the yarner server's HTTP API. Shapes mirror the Rust view types
// (yarner-engine view.rs, model.rs, plays.rs, engine.rs).

import type { Editor, Source } from "./source.ts";
import { connect } from "./ws.ts";
import { signal } from "./ui.ts";

export type Anchor = string | null;
/** Where a prompt applies: the beat itself, it and everything after, or just the next. */
export type Scope = "here" | "from_here" | "next";

export interface Line {
  speaker?: string | null;
  text: string;
  /** How it's said: one of the speaker's deliveries, by name. */
  tone?: string | null;
}

/** One way a voice speaks, and its clip (audio media). */
export interface Delivery {
  name: string;
  how: string;
  clip?: string | null;
  /** What the clip says. */
  words: string;
}

/** A talking object's voice: how it sounds, and a clip per delivery. */
export interface Voice {
  description: string;
  /** What every made clip says. */
  words: string;
  deliveries: Delivery[];
}

/** A sound a beat plays: once at its line, or a background loop from it. */
export interface SoundCue {
  media: string;
  line: number;
  looped?: boolean;
}

export type MediaKind = "image" | "video" | "audio";
export type MediaStatus = "requested" | "ready" | "failed";

export interface MediaView {
  id: string;
  kind: MediaKind;
  status: MediaStatus;
  mime: string | null;
  caption: string;
  width: number;
  height: number;
}

export interface ChoiceView {
  label: string;
  /** One the player wrote themselves. */
  own?: boolean;
  /** The screen behind it is ready: written, and its picture painted. */
  ready: boolean;
  /** Its beat is being written. */
  generating: boolean;
  /** Its beat is written and its picture is being painted. */
  painting?: boolean;
  /** …and that picture is a video. */
  filming?: boolean;
  /** Its picture is made; its voices or sounds are being made. */
  voicing?: boolean;
  /** How far that picture or video is, 0..1, when known. */
  progress?: number;
  /** What's made for it failed and is tried again shortly. */
  retrying?: boolean;
  /** Waiting its turn: where it stands in line (1 = next). */
  line?: number;
  /** Started, but the generator is still loading the model that makes it. */
  loading?: boolean;
}

export type NextView =
  | {
      type: "continue";
      ready: boolean;
      generating: boolean;
      painting?: boolean;
      filming?: boolean;
      voicing?: boolean;
      progress?: number;
      retrying?: boolean;
      line?: number;
      loading?: boolean;
    }
  | { type: "choices"; choices: ChoiceView[] }
  | { type: "end" };

export interface BeatView {
  id: string;
  lines: Line[];
  shows: MediaView | null;
  screen: MediaView | null;
  label: string | null;
  next: NextView;
  pending: boolean;
  stale: boolean;
  seen: boolean;
  epoch: number;
  version: number;
  versions: string[];
  /** Each line's spoken clip (by line; null: not spoken). */
  voices?: (string | null)[];
  /** Sounds it plays. */
  sounds?: SoundCue[];
  /** The background sound playing on this screen. */
  ambience?: string | null;
  /** At this decision the player may write their own choice. */
  own_choices?: boolean;
}

export interface HistoryEntry {
  index: number;
  beat: string;
  lines: Line[];
  chose: string | null;
}

export interface PlayView {
  id: string;
  name: string;
  beat: BeatView | null;
  depth: number;
  favor: number | null;
  regenerating: boolean;
  waiting_for_start: boolean;
  /** Before the opening shows: what's being made for it and how far (0..1). */
  opening?: [string, number | null];
  state: [string, string][];
  history: HistoryEntry[];
  /** While waiting for the next beat: its lines written so far. */
  draft?: Line[] | null;
  /** The story's picture size: the play screen keeps this shape. */
  frame?: [number, number];
}

export interface Hop {
  beat: string;
  exit?: number | null;
}

export interface Play {
  id: string;
  name: string;
  path: Hop[];
  at: number;
}

export interface Save {
  id: string;
  name: string;
  play: string;
  path: Hop[];
  thumb: string | null;
  excerpt: string;
  /** The line of the beat on screen when saved. */
  line?: number | null;
  /** Saved at a decision: the choices that were on screen. */
  choices?: string[];
  /** Its place on the save screen. */
  slot?: number | null;
  at: number;
}

export interface MapNode {
  id: string;
  label: string | null;
  excerpt: string;
  thumb: string | null;
  /** The thumb is a video. */
  video?: boolean;
  pending: boolean;
  seen: boolean;
  on_path: boolean;
  ending: boolean;
  path_index: number | null;
}

export interface MapEdge {
  from: string;
  exit: number;
  to: string;
  label: string | null;
  rejoin: boolean;
}

export interface MapView {
  root: string | null;
  /** Every version of the opening (the root first). */
  openings?: string[];
  nodes: MapNode[];
  edges: MapEdge[];
  generating: [string, number][];
}

export interface PromptRev {
  id: string;
  rev: string;
  anchor: Anchor;
  scope: Scope;
  text: string;
  at: number;
}

export interface PromptView {
  id: string;
  rev: string;
  text: string;
  anchor: Anchor;
  scope: Scope;
  history: PromptRev[];
}

export interface RefImage {
  media: string;
  caption: string;
  /** What pictures drawn from it take from it ("face only"); empty: the model's call. */
  use_for?: string;
}

export interface ObjectRev {
  id: string;
  rev: string;
  anchor: Anchor;
  name: string;
  description: string;
  refs: RefImage[];
  talks?: boolean;
  voice?: Voice | null;
  removed: boolean;
  at: number;
}

export interface ObjectView {
  id: string;
  rev: string;
  name: string;
  description: string;
  refs: [RefImage, MediaView | null][];
  talks: boolean;
  voice: Voice | null;
  /** The voice's clips as they stand, by delivery. */
  clips: (MediaView | null)[];
  anchor: Anchor;
  history: ObjectRev[];
}

export interface Settings {
  text_model: string;
  image_model: string;
  image_edit_model: string;
  video_model: string;
  video_ref_model: string;
  width: number;
  height: number;
  video: boolean;
  think: boolean;
  /** Beats that must pass after a new picture before the next (0 = no limit). */
  picture_gap: number;
  /** Most beats one picture stays up before it must change (0 = no limit). */
  picture_max?: number;
  /** Most beats in a row without a choice before one must be offered (0 = no limit). */
  choice_max?: number;
  /** The story can't end before this many beats (0 = any time). */
  min_beats?: number;
  /** How the story is framed (pictures' shape; how it plays on a phone). */
  orientation?: "landscape" | "portrait";
  /** LoRA adapters switched on, by picture/video model id (only these are used). */
  loras?: Record<string, Lora[]>;
  /** Videos' own size; unset = the pictures' size. */
  video_size?: [number, number];
  /** Speak talking characters' lines aloud. */
  voices?: boolean;
  /** Sound effects and background sounds. */
  sounds?: boolean;
  /** The talking object whose voice reads the narration (unset: silent). */
  narrator?: string | null;
  voice_model?: string;
  voice_design_model?: string;
  sound_model?: string;
}

/** A LoRA adapter to blend in: a vault entry, at a weight. */
export interface Lora {
  id: string;
  weight: number;
}

/** An adapter the vault holds for a model (its name, and the weight it was added with). */
export interface Adapter {
  id: string;
  name: string;
  weight?: number;
}

/** A generator model and what it can do (from the server's `GET /api/models`). */
export interface ModelCaps {
  id: string;
  kind: "text" | "image" | "video" | "audio" | null;
  title: string;
  default: boolean;
  downloaded: boolean;
  pausable: boolean;
  max_refs: number;
  ref_tag: string | null;
  size_multiple: number;
  default_size: [number, number] | null;
  max_ref_audios?: number;
  makes_audio?: boolean;
  /** An audio model: speech or a sound, takes a description, speaks in a clip's voice. */
  audio?: { speech: boolean; instruct: boolean; clone: boolean; max_seconds: number } | null;
}

/** Delete every story on this server (no passphrase: asked "are you sure?" first). */
/** The generator's adapter vault (LoRAs) for every story: memory only on the server. */
export const vaultState = () => request<{ set: boolean }>("GET", "/api/vault");
export const setVault = (password: string | null) =>
  request<{ set: boolean; refused?: boolean }>("POST", "/api/vault", {
    password,
  });
/** The adapters the vault holds for a model; `null`: the vault isn't open. */
export const vaultAdapters = (model: string) =>
  request<{ adapters: Adapter[] | null }>(
    "GET",
    `/api/vault/adapters?model=${encodeURIComponent(model)}`,
  );

export const clearAll = () =>
  request<{ deleted: number }>("POST", "/api/clear-all", { confirm: true });

/** "Pause": the GPU given back to you (the alpha's thinfer stopped) or in use. */
export type GpuState = "running" | "pausing" | "paused" | "resuming";
export const gpuState = () => request<{ state: GpuState }>("GET", "/api/gpu").then((r) => r.state);
/** Resolves once it's done: the GPU free, or the background work going again. */
export const setGpuPaused = (paused: boolean) =>
  request<{ state: GpuState }>("POST", "/api/gpu", { paused }).then((r) => r.state);

export const listModels = () =>
  request<{ models: ModelCaps[] }>("GET", "/api/models").then((r) => r.models);

export interface SetupView {
  prompts: PromptView[];
  objects: ObjectView[];
  settings: Settings;
  library: MediaView[];
  /** The pictures-and-video guidance used until the author writes their own. */
  media_guide_default: string;
  /** What a new voice starts with (deliveries and the words its clips say). */
  voice_default: Voice;
}

export interface ExportReport {
  beats: number;
  pending_dropped: number;
  untaken: [string, string][];
  media_unfinished: number;
}

export type AdvanceResult = { type: "moved" } | { type: "waiting" } | { type: "ended" };

export interface StoryRef {
  handle: string;
  id: string;
  title: string;
}

export interface StoryInfo {
  id: string;
  title: string;
  plays: Play[];
  saves: Save[];
  prefs: unknown;
  keep_secs: number | null;
  started: boolean;
}

export interface QueueEntry {
  id: string;
  kind: "text" | "image" | "video" | "audio" | null;
  priority: number;
  running: boolean;
  progress: number | null;
  /** What it's for, from where the reader is ("Painting the next screen"). */
  label?: string;
  /** Seconds since it was sent. */
  secs?: number;
  /** Not sent yet: waiting for a free slot. */
  lined?: boolean;
}

/** Messages on the story websocket. */
export type ServerMsg =
  | { type: "beat"; beat: string }
  | { type: "media"; media: string }
  | { type: "play"; play: string }
  | { type: "setup" }
  | { type: "draft"; from: string; exit: number }
  | { type: "error"; message: string }
  | { type: "resync" }
  | {
      type: "queue";
      jobs: QueueEntry[];
      idle: boolean;
      writing?: Writing | null;
    };

/** How writing has gone lately (medians over recent beats). */
export interface Writing {
  samples: number;
  wall_secs: number;
  first_line_secs: number;
  prefill_secs: number;
  /** Share continued from the backend's cache. */
  resumed: number;
}

export interface ObjectInput {
  id?: string | null;
  name: string;
  description: string;
  refs: RefImage[];
  talks?: boolean;
  voice?: Voice | null;
  anchor: Anchor;
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

let lockedHandler: () => void = () => {};
/** Called when the server says the story is locked (401). */
export function onLocked(f: () => void) {
  lockedHandler = f;
}

async function request<T>(
  method: string,
  url: string,
  body?: unknown,
  init: RequestInit = {},
): Promise<T> {
  const headers = new Headers(init.headers);
  let payload = init.body;
  if (body !== undefined) {
    headers.set("content-type", "application/json");
    payload = JSON.stringify(body);
  }
  const res = await fetch(url, {
    ...init,
    method,
    headers,
    body: payload,
    cache: "no-store",
  });
  if (!res.ok) {
    let msg = `${res.status} ${res.statusText}`;
    try {
      msg = ((await res.json()) as { error?: string }).error ?? msg;
    } catch {
      // Not JSON: keep the status line.
    }
    if (res.status === 401) lockedHandler();
    throw new ApiError(res.status, msg);
  }
  return (await res.json()) as T;
}

// Before a story is open.

/** Whether the server has a generator connected (without one, stories play as far as
 * they're written). Read once at startup. */
export const generatorOn = signal(true);
export const checkServer = () =>
  request<{ ok: boolean; generator?: boolean }>("GET", "/api/ping")
    .then((r) => generatorOn.set(r.generator !== false))
    .catch(() => {});

export const unlock = (passphrase: string) =>
  request<{ stories: StoryRef[] }>("POST", "/api/unlock", { passphrase }).then((r) => r.stories);

export const createStory = (title: string, passphrase: string) =>
  request<StoryRef>("POST", "/api/stories", { title, passphrase });

export const importBundle = (file: Blob, bundlePassphrase: string, passphrase: string) =>
  request<StoryRef>("POST", "/api/import", undefined, {
    body: file,
    headers: {
      "content-type": "application/octet-stream",
      "x-bundle-passphrase": bundlePassphrase,
      "x-passphrase": passphrase,
    },
  });

const q = (params: Record<string, string | null | undefined>) => {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v != null) s.set(k, v);
  const t = s.toString();
  return t ? `?${t}` : "";
};

/** One open story on the server. Supports everything, including editing. */
export class StoryApi implements Source, Editor {
  readonly kind = "server";
  readonly edit: Editor = this;
  readonly handle: string;
  readonly base: string;

  constructor(handle: string) {
    this.handle = handle;
    this.base = `/api/s/${handle}`;
  }

  private call<T>(method: string, path: string, body?: unknown, init?: RequestInit) {
    return request<T>(method, this.base + path, body, init);
  }
  private p(play: string) {
    return `/plays/${encodeURIComponent(play)}`;
  }

  info = () => this.call<StoryInfo>("GET", "");
  lock = () => this.call<object>("POST", "/lock", {}).then(() => {});
  deleteStory = (passphrase: string) =>
    this.call<object>("POST", "/delete", { passphrase }).then(() => {});
  newFromSetup = (title: string, passphrase: string) =>
    this.call<StoryRef>("POST", "/new-from-setup", { title, passphrase });
  keep = (hours: number) => this.call<object>("POST", "/keep", { hours }).then(() => {});
  setTitle = (title: string) => this.call<object>("POST", "/title", { title }).then(() => {});
  changePassphrase = (passphrase: string) =>
    this.call<object>("POST", "/passphrase", { passphrase }).then(() => {});

  // Setup.
  setup = (play?: string | null) => this.call<SetupView>("GET", `/setup${q({ play })}`);
  setPrompt = (id: string, text: string, anchor: Anchor, scope: Scope = "from_here") =>
    this.call<{ rev: string }>("POST", "/prompt", { id, text, anchor, scope });
  everywhere = (rev: string) => this.call<{ rev: string }>("POST", "/prompt/everywhere", { rev });
  putObject = (o: ObjectInput) => this.call<{ id: string }>("POST", "/object", o);
  /** Save an object's voice and make clips of the deliveries in `make` (by index). */
  makeVoice = (id: string, voice: Voice, make: number[], anchor: Anchor = null) =>
    this.call<{ voice: Voice }>("POST", `/object/${encodeURIComponent(id)}/voice`, {
      voice,
      make,
      anchor,
    }).then((r) => r.voice);

  removeObject = (id: string, anchor: Anchor) =>
    this.call<object>("POST", `/object/${encodeURIComponent(id)}/remove`, {
      anchor,
    }).then(() => {});
  setSettings = (settings: Settings, anchor: Anchor) =>
    this.call<object>("POST", "/settings", { settings, anchor }).then(() => {});
  upload = (file: Blob, caption: string, objects: string[]) =>
    this.call<{ id: string }>(
      "POST",
      `/media${q({ caption, objects: objects.join(",") })}`,
      undefined,
      {
        body: file,
        headers: { "content-type": file.type || "application/octet-stream" },
      },
    );
  generate = (prompt: string, caption: string, objects: string[], refs: string[] = []) =>
    this.call<{ id: string }>("POST", "/media/generate", {
      prompt,
      caption,
      objects,
      refs,
    });
  mediaUrl = (id: string) => `${this.base}/media/${encodeURIComponent(id)}`;

  // Playthroughs.
  plays = () => this.call<Play[]>("GET", "/plays");
  newPlay = (name = "") => this.call<{ id: string }>("POST", "/plays", { name });
  play = (p: string) => this.call<PlayView>("GET", this.p(p));
  deletePlay = (p: string) => this.call<object>("DELETE", this.p(p)).then(() => {});
  advance = (p: string, exit?: number | null) =>
    this.call<{ result: AdvanceResult; view: PlayView }>("POST", `${this.p(p)}/advance`, {
      exit: exit ?? null,
    });
  back = (p: string) => this.call<PlayView>("POST", `${this.p(p)}/back`, {});
  rewind = (p: string, index: number) =>
    this.call<PlayView>("POST", `${this.p(p)}/rewind`, { index });
  keepGoing = (p: string) =>
    this.call<{ result: AdvanceResult; view: PlayView }>("POST", `${this.p(p)}/keep-going`, {});
  ownChoice = (p: string, text: string) =>
    this.call<{ result: AdvanceResult; view: PlayView }>("POST", `${this.p(p)}/own-choice`, {
      text,
    });
  favor = (p: string, exit: number | null) =>
    this.call<object>("POST", `${this.p(p)}/favor`, { exit }).then(() => {});
  save = (p: string, name: string, slot?: number, line?: number) =>
    this.call<{ id: string }>("POST", `${this.p(p)}/save`, {
      name,
      slot,
      line,
    });
  saves = () => this.call<Save[]>("GET", "/saves");
  load = (save: string, play: string) =>
    this.call<PlayView>("POST", `/saves/${encodeURIComponent(save)}/load`, {
      play,
    });
  deleteSave = (save: string) =>
    this.call<object>("DELETE", `/saves/${encodeURIComponent(save)}`).then(() => {});
  map = (play?: string | null) => this.call<MapView>("GET", `/map${q({ play })}`);
  prefs = () => this.call<unknown>("GET", "/prefs");
  setPrefs = (prefs: unknown) => this.call<object>("POST", "/prefs", prefs).then(() => {});
  queue = () =>
    this.call<{ jobs: QueueEntry[]; idle: boolean; writing?: Writing | null }>("GET", "/queue");

  // Edit mode.
  jump = (p: string, beat: string, via?: Hop[]) =>
    this.call<PlayView>("POST", `${this.p(p)}/jump`, {
      beat,
      via: via ?? null,
    });
  rerun = (p: string, prompt?: string, scope: Scope = "here") =>
    this.call<PlayView>("POST", `${this.p(p)}/rerun`, prompt ? { prompt, scope } : {});
  fork = (p: string, prompt?: string) =>
    this.call<PlayView>("POST", `${this.p(p)}/fork`, prompt ? { prompt } : {});
  version = (p: string, beat: string) =>
    this.call<PlayView>("POST", `${this.p(p)}/version`, { beat });

  /** The bundle (zip bytes) and what was left out. */
  async export(passphrase: string): Promise<{ blob: Blob; report: ExportReport | null }> {
    const res = await fetch(`${this.base}/export`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ passphrase }),
      cache: "no-store",
    });
    if (!res.ok) {
      const msg = await res
        .json()
        .then((j: { error?: string }) => j.error)
        .catch(() => null);
      if (res.status === 401) lockedHandler();
      throw new ApiError(res.status, msg ?? `${res.status} ${res.statusText}`);
    }
    const blob = await res.blob();
    const report = await this.call<ExportReport>("GET", "/export/report").catch(() => null);
    return { blob, report };
  }

  subscribe(play: string | null, on: (m: ServerMsg) => void): () => void {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    return connect(`${proto}//${location.host}${this.base}/ws${q({ play })}`, on);
  }
}
