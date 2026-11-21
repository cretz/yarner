// Where a story comes from. The server (`StoryApi`) supports everything; a read-only
// source (a static player reading an exported bundle) implements `Source` alone and sets
// `edit` to null, which hides every editing feature.

import type {
  Anchor,
  AdvanceResult,
  ExportReport,
  Hop,
  MapView,
  ObjectInput,
  Play,
  PlayView,
  Save,
  Scope,
  ServerMsg,
  Settings,
  SetupView,
  StoryInfo,
  StoryRef,
  Voice,
} from "./api.ts";

export interface Source {
  readonly kind: "server" | "bundle";
  /** Editing features; null when the source is read-only. */
  readonly edit: Editor | null;

  info(): Promise<StoryInfo>;
  lock(): Promise<void>;
  mediaUrl(id: string): string;
  /** Live events; returns an unsubscribe function. */
  subscribe(play: string | null, on: (m: ServerMsg) => void): () => void;

  plays(): Promise<Play[]>;
  newPlay(name?: string): Promise<{ id: string }>;
  play(p: string): Promise<PlayView>;
  deletePlay(p: string): Promise<void>;
  advance(p: string, exit?: number | null): Promise<{ result: AdvanceResult; view: PlayView }>;
  back(p: string): Promise<PlayView>;
  rewind(p: string, index: number): Promise<PlayView>;
  favor(p: string, exit: number | null): Promise<void>;
  /** The story ended here: write on from it (needs a generator). */
  keepGoing?(p: string): Promise<{ result: AdvanceResult; view: PlayView }>;
  /** At a decision, the player's own choice (needs a generator). */
  ownChoice?(p: string, text: string): Promise<{ result: AdvanceResult; view: PlayView }>;
  /** Save a playthrough's place, in `slot` of the save screen (replacing what's there). */
  save(p: string, name: string, slot?: number, line?: number): Promise<{ id: string }>;
  saves(): Promise<Save[]>;
  load(save: string, play: string): Promise<PlayView>;
  deleteSave(save: string): Promise<void>;
  map(play?: string | null): Promise<MapView>;
  prefs(): Promise<unknown>;
  setPrefs(prefs: unknown): Promise<void>;
}

export interface Editor {
  /** Delete the story for good (the passphrase again confirms it). */
  deleteStory(passphrase: string): Promise<void>;
  /** A new, empty story with this one's setup (this one is left as it is). */
  newFromSetup(title: string, passphrase: string): Promise<StoryRef>;
  keep(hours: number): Promise<void>;
  setTitle(title: string): Promise<void>;
  setup(play?: string | null): Promise<SetupView>;
  setPrompt(id: string, text: string, anchor: Anchor, scope?: Scope): Promise<{ rev: string }>;
  everywhere(rev: string): Promise<{ rev: string }>;
  putObject(o: ObjectInput): Promise<{ id: string }>;
  /** Save an object's voice and make clips of the deliveries in `make` (by index). */
  makeVoice(id: string, voice: Voice, make: number[], anchor?: Anchor): Promise<Voice>;
  removeObject(id: string, anchor: Anchor): Promise<void>;
  setSettings(settings: Settings, anchor: Anchor): Promise<void>;
  upload(file: Blob, caption: string, objects: string[]): Promise<{ id: string }>;
  generate(prompt: string, caption: string, objects: string[]): Promise<{ id: string }>;
  jump(p: string, beat: string, via?: Hop[]): Promise<PlayView>;
  rerun(p: string, prompt?: string, scope?: Scope): Promise<PlayView>;
  fork(p: string, prompt?: string): Promise<PlayView>;
  version(p: string, beat: string): Promise<PlayView>;
  export(passphrase: string): Promise<{ blob: Blob; report: ExportReport | null }>;
}
