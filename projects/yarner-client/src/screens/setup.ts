// Story setup: the story prompt, visual style, extra direction, objects (anything the story
// should know about, with reference pictures) and settings. Edits here apply everywhere.

import { keepControl } from "../keep.ts";
import {
  listModels,
  vaultAdapters,
  vaultState,
  type Adapter,
  type MediaView,
  type ModelCaps,
  type ObjectView,
  type RefImage,
  type Settings,
  type SetupView,
  type StoryInfo,
  type Voice,
} from "../api.ts";
import type { Nav } from "../main.ts";
import { authorPlay, openNewFromSetup } from "./home.ts";
import type { Editor, Source } from "../source.ts";
import {
  action,
  bind,
  computed,
  effect,
  h,
  onCleanup,
  signal,
  toast,
  untrack,
  when,
  type Signal,
} from "../ui.ts";

/**
 * Story sizes by shape: thinfer's FastH3 / MiniMax-H3 ladder (multiples of 32). 480p is
 * the floor (below it faces and detail fall apart), 768p is H3's native size. Pictures
 * and video share it. The first is the default.
 */
const RESOLUTIONS: Record<"landscape" | "portrait", [number, number, string][]> = {
  landscape: [
    [832, 480, "480p"],
    [1024, 576, "576p"],
    [1280, 704, "720p"],
    [1344, 768, "768p, native"],
    // 3:2 at about 1 MP: a size image models are trained at.
    [1216, 832, "3:2, sharp"],
  ],
  portrait: [
    // 9:16 like a phone, sharper than 480p at about 1.5x its cost.
    [576, 1024, "576p"],
    [480, 832, "480p"],
    [704, 1280, "720p"],
    [768, 1344, "768p, native"],
    [832, 1216, "2:3, sharp"],
  ],
};

/** Model settings: the label, and which of the generator's models fit it. */
const MODEL_FIELDS: [keyof Settings, string, (m: ModelCaps) => boolean][] = [
  ["text_model", "Text model", (m) => m.kind === "text"],
  ["image_model", "Picture model", (m) => m.kind === "image"],
  [
    "image_edit_model",
    "Picture model with references",
    (m) => m.kind === "image" && m.max_refs > 0,
  ],
  ["video_model", "Video model", (m) => m.kind === "video"],
  ["video_ref_model", "Video model with references", (m) => m.kind === "video" && m.max_refs > 0],
  [
    "voice_model",
    "Voice model (speaks in a clip's voice)",
    (m) => !!m.audio?.speech && m.audio.clone,
  ],
  [
    "voice_design_model",
    "Voice design model (a voice from a description)",
    (m) => !!m.audio?.speech && m.audio.instruct,
  ],
  ["sound_model", "Sound model", (m) => !!m.audio && !m.audio.speech],
];

/** Settings fields shown as model pickers, defaulted when a story predates them. */
const MODEL_DEFAULTS: Partial<Settings> = {
  voice_model: "qwen3-tts",
  voice_design_model: "qwen3-tts-design",
  sound_model: "moss-sfx",
};

export function setupScreen(src: Source, nav: Nav): Node {
  const ed = src.edit;
  if (!ed) return h("div", { class: "page" }, h("p", null, "This story can't be edited."));
  const setup = signal<SetupView | null>(null);
  const info = signal<StoryInfo | null>(null);
  const reload = () => ed.setup(null).then(setup.set, (e: Error) => toast(e.message, "error"));
  void reload();
  src.info().then(info.set, () => {});
  let timer = 0;
  onCleanup(
    src.subscribe(null, (m) => {
      if (m.type === "setup" || m.type === "media" || m.type === "resync") {
        clearTimeout(timer);
        timer = window.setTimeout(reload, 150);
      }
      if (m.type === "error") toast(m.message, "error");
    }),
  );

  const play = async () => {
    // Started from the setup: the author, in their place, with the editing tools.
    nav.go({ to: "play", play: await authorPlay(src), edit: true });
  };

  return h(
    "div",
    { class: "page setup" },
    h(
      "header",
      { class: "page-head" },
      h("button", { class: "link", onClick: () => nav.go({ to: "home" }) }, "‹ Story home"),
      h("h1", null, "Setup"),
      h("span", { class: "grow" }),
      h(
        "button",
        {
          class: "link",
          name: "new-from-setup",
          title: "Start over in a new story with this setup (this one stays)",
          onClick: () => openNewFromSetup(src, nav, info()?.title ?? ""),
        },
        "New story from this setup",
      ),
      action(() => (info()?.started ? "Play" : "Start the story"), play, {
        class: "primary",
        name: "start",
      }),
    ),
    () => {
      const i = info();
      return i ? untrack(() => titleField(ed, i)) : null;
    },
    // Set the story up, start it running in the background and go: all from here.
    keepControl(ed, () => src.info()),
    () => (setup() ? null : h("p", { class: "muted" }, "Loading...")),
    when(setup, () => body(ed, src, setup as Signal<SetupView>, reload)),
  );
}

function titleField(ed: Editor, i: StoryInfo): Node {
  const t = signal(i.title);
  return h(
    "section",
    { class: "card" },
    h(
      "label",
      null,
      "Title",
      h("input", {
        name: "story-title",
        ...bind(t),
        onChange: () =>
          ed.setTitle(t().trim() || "Untitled").catch((e: Error) => toast(e.message, "error")),
      }),
    ),
  );
}

function body(
  ed: Editor,
  src: Source,
  setup: Signal<SetupView>,
  reload: () => Promise<unknown>,
): Node[] {
  const promptText = (id: string) => setup.peek().prompts.find((p) => p.id === id)?.text ?? "";
  const directionIds = computed(() =>
    setup()
      .prompts.map((p) => p.id)
      .filter((id) => !["story", "style", "media"].includes(id) && !id.startsWith("here:"))
      .join("\n"),
  );
  const added = signal<string[]>([]);
  const objectIds = computed(() =>
    setup()
      .objects.map((o) => o.id)
      .join("\n"),
  );

  return [
    h(
      "section",
      { class: "card" },
      h("h2", null, "The story"),
      h(
        "p",
        { class: "muted small" },
        "What is it about? Who is the player, where does it start, what is the tone? Write as much or as little as you like.",
      ),
      promptField(
        ed,
        "story",
        promptText("story"),
        8,
        "A lighthouse keeper finds a door in the rock that wasn't there yesterday…",
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Visual style"),
      h("p", { class: "muted small" }, "How the pictures should look."),
      promptField(
        ed,
        "style",
        promptText("style"),
        3,
        "Painterly, muted colors, soft light, 35mm film grain",
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Pictures and video"),
      h(
        "p",
        { class: "muted small" },
        "When your story should change the picture, and which moments deserve a video. The storyteller decides as it goes, by this.",
      ),
      promptField(
        ed,
        "media",
        promptText("media") || setup.peek().media_guide_default,
        4,
        "When to change pictures, when to use video…",
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Extra direction"),
      h("p", { class: "muted small" }, "Anything else the storyteller should keep in mind."),
      () => {
        const ids = [
          ...directionIds().split("\n").filter(Boolean),
          ...added().filter((x) => !directionIds().includes(x)),
        ];
        return untrack(() =>
          ids.map((id) =>
            promptField(
              ed,
              id,
              promptText(id),
              2,
              "e.g. Keep chapters short; let the player fail.",
              // An empty direction is no direction: removing one saves it empty (from here).
              async () => {
                if (promptText(id).trim()) await ed.setPrompt(id, "", null, "from_here");
                added.update((a) => a.filter((x) => x !== id));
                await reload();
              },
            ),
          ),
        );
      },
      h(
        "button",
        {
          class: "link",
          onClick: () => added.update((a) => [...a, `d${Math.random().toString(16).slice(2, 10)}`]),
        },
        "+ Add direction",
      ),
    ),
    h(
      "section",
      { class: "card" },
      h("h2", null, "Objects"),
      h(
        "p",
        { class: "muted small" },
        "Anything the story should know about: people, places, things. Give each a name, a description, and reference pictures if you have them.",
      ),
      h("div", { class: "objects" }, () => {
        const ids = objectIds();
        return untrack(() =>
          ids
            ? ids.split("\n").map((id) =>
                objectEditor(
                  ed,
                  src,
                  () => setup().objects.find((o) => o.id === id) ?? null,
                  () => setup().library,
                  reload,
                  () => setup().voice_default,
                ),
              )
            : null,
        );
      }),
      newObject(ed),
    ),
    untrack(() => settingsCard(ed, setup.peek().settings, () => setup().objects)),
  ];
}

/** A prompt textarea that saves (everywhere) when it loses focus. */
function promptField(
  ed: Editor,
  id: string,
  initial: string,
  rows: number,
  placeholder: string,
  remove?: () => Promise<unknown>,
): Node {
  const text = signal(initial);
  let saved = initial;
  const state = signal<"" | "saving" | "saved">("");
  const save = async () => {
    if (text() === saved) return;
    state.set("saving");
    try {
      await ed.setPrompt(id, text(), null, "from_here");
      saved = text();
      state.set("saved");
    } catch (e) {
      state.set("");
      toast((e as Error).message, "error");
    }
  };
  return h(
    "div",
    { class: "prompt-field" },
    h("textarea", {
      name: `prompt-${id}`,
      rows,
      placeholder,
      ...bind(text),
      onBlur: save,
    }),
    h("small", { class: "save-state" }, () =>
      state() === "saving" ? "Saving…" : state() === "saved" ? "Saved" : "",
    ),
    remove
      ? action("Remove", remove, {
          class: "danger link prompt-remove",
          title: "Drop this direction",
        })
      : null,
  );
}

function newObject(ed: Editor): Node {
  const name = signal("");
  const desc = signal("");
  return h(
    "form",
    { class: "new-object", onSubmit: (e: Event) => e.preventDefault() },
    h("h3", null, "Add an object"),
    h("input", { name: "new-object-name", placeholder: "Name", ...bind(name) }),
    h("textarea", {
      name: "new-object-description",
      rows: 3,
      placeholder: "Description",
      ...bind(desc),
    }),
    action(
      "Add",
      async () => {
        await ed.putObject({
          name: name().trim(),
          description: desc(),
          refs: [],
          anchor: null,
        });
        name.set("");
        desc.set("");
      },
      { type: "submit", disabled: () => !name().trim() },
    ),
  );
}

/** Edit one object (everywhere): name, description, reference pictures. */
export function objectEditor(
  ed: Editor,
  src: Source,
  o: () => ObjectView | null,
  library: () => MediaView[],
  refresh: () => Promise<unknown>,
  voiceDefault?: () => Voice,
): Node {
  const first = o()!;
  const id = first.id;
  const name = signal(first.name);
  const desc = signal(first.description);
  const talks = signal(first.talks);
  const fresh = (): Voice =>
    structuredClone(voiceDefault?.() ?? { description: "", words: "", deliveries: [] });
  const voice = signal<Voice>(first.voice ? structuredClone(first.voice) : fresh());
  const voiceDirty = () =>
    talks() && JSON.stringify(voice()) !== JSON.stringify(o()?.voice ?? fresh());
  const genPrompt = signal("");
  const painting = signal<string[]>([]);
  // Picture caption / "use for" edits not saved yet, by picture: part of the card's one
  // Save (they used to save on leaving the field, and Save stayed off after editing them).
  const edits = signal<Record<string, Partial<RefImage>>>({});
  const refsNow = () => (o()?.refs ?? []).map(([r]) => ({ ...r, ...edits()[r.media] }));
  const refsDirty = () =>
    (o()?.refs ?? []).some(([r]) => {
      const e = edits()[r.media];
      return (
        !!e &&
        ((e.caption ?? r.caption) !== r.caption ||
          (e.use_for ?? r.use_for ?? "") !== (r.use_for ?? ""))
      );
    });
  const dirty = () =>
    name() !== (o()?.name ?? "") ||
    desc() !== (o()?.description ?? "") ||
    refsDirty() ||
    talks() !== (o()?.talks ?? false) ||
    voiceDirty();

  const put = async (refs: RefImage[]) => {
    const r = await ed.putObject({
      id,
      name: name().trim() || first.name,
      description: desc(),
      refs: refs.map((x) => ({
        ...x,
        use_for: x.use_for?.trim() || undefined,
      })),
      talks: talks(),
      voice: talks() ? voice() : (o()?.voice ?? null),
      anchor: null,
    });
    edits.set({});
    return r;
  };

  // Attach generated pictures once they're ready.
  const attachReady = computed(() => {
    const lib = library();
    return painting().filter((m) => {
      const st = lib.find((x) => x.id === m)?.status;
      return st && st !== "requested";
    });
  });
  // Poll while pictures are being painted (their "ready" event can come before we listen).
  effect(() => {
    if (!painting().length) return;
    const t = setInterval(() => void refresh(), 1000);
    return () => clearInterval(t);
  });
  const watch = computed(() => attachReady().join());
  let attaching = false;
  const tryAttach = async () => {
    const ids = attachReady();
    if (!ids.length || attaching) return;
    attaching = true;
    try {
      const lib = library();
      const ok = ids.filter((m) => lib.find((x) => x.id === m)?.status === "ready");
      if (ids.length > ok.length) toast("A picture couldn't be painted.", "error");
      painting.update((p) => p.filter((m) => !ids.includes(m)));
      if (ok.length)
        await put([
          ...refsNow(),
          ...ok.map((m) => ({
            media: m,
            caption: lib.find((x) => x.id === m)?.caption ?? "",
          })),
        ]);
    } finally {
      attaching = false;
    }
  };
  effect(() => {
    if (watch()) untrack(() => void tryAttach());
  });

  const upload = async (files: FileList | null) => {
    if (!files?.length) return;
    const refs = refsNow();
    for (const f of files) {
      const caption = f.name.replace(/\.[^.]+$/, "");
      const { id: media } = await ed.upload(f, caption, [id]);
      refs.push({ media, caption });
    }
    await put(refs);
  };

  return h(
    "div",
    { class: "object-card", "data-object": id },
    h("input", {
      class: "object-name",
      name: "object-name",
      placeholder: "Name",
      ...bind(name),
    }),
    h("textarea", {
      name: "object-description",
      rows: 3,
      placeholder: "Description",
      ...bind(desc),
    }),
    h(
      "div",
      { class: "refs" },
      () => {
        const refs = o()?.refs ?? [];
        return untrack(() =>
          refs.map(([r, m], i) =>
            refTile(
              src,
              { ...r, ...edits()[r.media] },
              m,
              (patch) =>
                edits.update((e) => ({
                  ...e,
                  [r.media]: { ...e[r.media], ...patch },
                })),
              async () => {
                const next = refsNow();
                next.splice(i, 1);
                await put(next);
              },
            ),
          ),
        );
      },
      () =>
        painting().map(() =>
          h("div", { class: "ref painting-tile" }, h("span", null, "painting…")),
        ),
    ),
    h(
      "div",
      { class: "row wrap" },
      h(
        "label",
        { class: "button" },
        "Upload pictures",
        h("input", {
          type: "file",
          name: "ref-upload",
          accept: "image/*",
          multiple: true,
          hidden: true,
          onChange: async (e: Event) => {
            const input = e.target as HTMLInputElement;
            try {
              await upload(input.files);
            } catch (err) {
              toast((err as Error).message, "error");
            }
            input.value = "";
          },
        }),
      ),
    ),
    // A picture prompt runs to a few lines: a text area, not a one-line field.
    h("textarea", {
      name: "gen-prompt",
      rows: 3,
      // A plain background keeps a reference from leaking its place into every scene.
      placeholder: "Or describe a picture to paint (a plain background works best)",
      ...bind(genPrompt),
    }),
    h(
      "div",
      { class: "row" },
      h("span", { class: "grow" }),
      action(
        "Paint",
        async () => {
          const prompt = genPrompt().trim();
          const desc0 = `${name()}: ${prompt}`;
          const { id: media } = await ed.generate(desc0, prompt, [id]);
          painting.update((p) => [...p, media]);
          void refresh();
          genPrompt.set("");
        },
        { disabled: () => !genPrompt().trim() },
      ),
    ),
    h(
      "label",
      { class: "check" },
      h("input", {
        type: "checkbox",
        name: "object-talks",
        checked: () => talks(),
        onChange: (e: Event) => talks.set((e.target as HTMLInputElement).checked),
      }),
      h("span", null, "Talks (gets a voice)"),
    ),
    when(talks, () => voiceEditor(ed, src, id, voice, o, refresh)),
    h(
      "div",
      { class: "row" },
      action("Save changes", () => put(refsNow()), {
        class: "primary",
        disabled: () => !dirty(),
      }),
      h("span", { class: "grow" }),
      action(
        "Remove",
        async () => {
          if (confirm(`Remove "${o()?.name}" from the story?`)) await ed.removeObject(id, null);
        },
        { class: "danger link" },
      ),
    ),
  );
}

/**
 * A talking object's voice: how it sounds, and a clip per delivery (calm, angry, ...),
 * made from the description (all at once, so they're one voice saying the same words in
 * different ways) or uploaded. Every line it says is spoken in the voice of its
 * delivery's clip; with none made, from the description alone.
 */
function voiceEditor(
  ed: Editor,
  src: Source,
  id: string,
  voice: Signal<Voice>,
  o: () => ObjectView | null,
  refresh: () => Promise<unknown>,
): Node {
  // A clip as it stands now (made, being made, failed), by its id.
  const clipView = (clip?: string | null) =>
    clip ? (o()?.clips.find((c) => c?.id === clip) ?? null) : null;
  const making = () =>
    voice().deliveries.some((d) => {
      const c = clipView(d.clip);
      return !!d.clip && (!c || c.status === "requested");
    });
  // Poll while clips are being made (their "ready" event can come before we listen).
  effect(() => {
    if (!making()) return;
    const t = setInterval(() => void refresh(), 1000);
    return () => clearInterval(t);
  });
  const patch = (i: number, p: Partial<Voice["deliveries"][number]>) =>
    voice.update((v) => ({
      ...v,
      deliveries: v.deliveries.map((d, j) => (j === i ? { ...d, ...p } : d)),
    }));
  const make = async (which: number[]) => {
    const v = await ed.makeVoice(id, voice(), which, null);
    voice.set(structuredClone(v));
    void refresh();
  };
  const upload = async (i: number, files: FileList | null) => {
    const f = files?.[0];
    if (!f) return;
    const d = voice().deliveries[i];
    const { id: media } = await ed.upload(f, d.name, [id]);
    patch(i, { clip: media });
    // Saved at once (an uploaded clip is the voice): its words can be changed after.
    const v = voice();
    const x = o();
    if (x)
      await ed.putObject({
        id,
        name: x.name,
        description: x.description,
        refs: x.refs.map(([r]) => r),
        talks: true,
        voice: v,
        anchor: null,
      });
    void refresh();
  };
  const described = () => voice().description.trim().length > 0;
  return h(
    "div",
    { class: "voice" },
    h("h3", null, "Voice"),
    h("textarea", {
      name: "voice-description",
      rows: 2,
      placeholder: "How it sounds: e.g. a warm, low woman's voice, a slight rasp, unhurried",
      value: () => voice().description,
      onInput: (e: Event) =>
        voice.update((v) => ({ ...v, description: (e.target as HTMLTextAreaElement).value })),
    }),
    h(
      "label",
      null,
      "What its clips say",
      h("input", {
        name: "voice-words",
        value: () => voice().words,
        onInput: (e: Event) =>
          voice.update((v) => ({ ...v, words: (e.target as HTMLInputElement).value })),
      }),
    ),
    h(
      "p",
      { class: "muted small" },
      "Each line is spoken in the voice of its delivery's clip, so make the clips and listen: make any again until it sounds right, or upload your own (a clean clip of one voice, 3 to 15 seconds, and what it says). With no clips, lines are spoken from the description alone, a little different each time.",
    ),
    h("div", { class: "deliveries" }, () =>
      voice().deliveries.map((d, i) => {
        const c = clipView(d.clip);
        return untrack(() =>
          h(
            "div",
            { class: "delivery", "data-delivery": d.name },
            h("input", {
              class: "delivery-name",
              name: "delivery-name",
              placeholder: "Name",
              value: d.name,
              onChange: (e: Event) => patch(i, { name: (e.target as HTMLInputElement).value }),
            }),
            h("input", {
              class: "delivery-how grow",
              name: "delivery-how",
              placeholder: "How it's said: e.g. angry, sharp and raised",
              value: d.how,
              onChange: (e: Event) => patch(i, { how: (e.target as HTMLInputElement).value }),
            }),
            !d.clip
              ? h("span", { class: "muted small clip-state" }, "no clip")
              : !c || c.status === "requested"
                ? h("span", { class: "muted small clip-state" }, "making…")
                : c.status === "failed"
                  ? h("span", { class: "small clip-state danger" }, "failed")
                  : h("audio", {
                      class: "clip",
                      controls: true,
                      preload: "none",
                      src: src.mediaUrl(d.clip),
                    }),
            d.clip && c?.status === "ready"
              ? h("input", {
                  class: "delivery-words",
                  name: "delivery-words",
                  placeholder: "What the clip says",
                  title: "What the clip says (with it, its delivery carries over too)",
                  value: d.words,
                  onChange: (e: Event) => patch(i, { words: (e.target as HTMLInputElement).value }),
                })
              : null,
            action(d.clip ? "Make again" : "Make", () => make([i]), {
              class: "link",
              name: "delivery-make",
              disabled: () => !described(),
            }),
            h(
              "label",
              { class: "button link" },
              "Upload",
              h("input", {
                type: "file",
                name: "delivery-upload",
                accept: "audio/*",
                hidden: true,
                onChange: async (e: Event) => {
                  const input = e.target as HTMLInputElement;
                  try {
                    await upload(i, input.files);
                  } catch (err) {
                    toast((err as Error).message, "error");
                  }
                  input.value = "";
                },
              }),
            ),
            h(
              "button",
              {
                class: "icon",
                title: "Remove this delivery",
                onClick: () =>
                  voice.update((v) => ({
                    ...v,
                    deliveries: v.deliveries.filter((_, j) => j !== i),
                  })),
              },
              "×",
            ),
          ),
        );
      }),
    ),
    h(
      "div",
      { class: "row wrap" },
      h(
        "button",
        {
          class: "link",
          name: "delivery-add",
          onClick: () =>
            voice.update((v) => ({
              ...v,
              deliveries: [...v.deliveries, { name: "", how: "", clip: null, words: "" }],
            })),
        },
        "+ Add a delivery",
      ),
      h("span", { class: "grow" }),
      action("Make the voice", () => make(voice().deliveries.map((_, i) => i)), {
        name: "voice-make",
        title: "Make a clip of every delivery: one voice, the same words said each way",
        disabled: () => !described() || !voice().deliveries.length,
      }),
    ),
  );
}

function refTile(
  src: Source,
  r: RefImage,
  m: MediaView | null,
  change: (patch: Partial<RefImage>) => void,
  remove: () => Promise<unknown>,
): Node {
  const caption = signal(r.caption);
  const useFor = signal(r.use_for ?? "");
  return h(
    "figure",
    { class: "ref" },
    m?.status === "ready"
      ? h("img", {
          src: src.mediaUrl(r.media),
          alt: r.caption,
          loading: "lazy",
        })
      : h("div", { class: "painting-tile" }, m?.status === "failed" ? "failed" : "painting…"),
    h("input", {
      name: "ref-caption",
      placeholder: "Caption",
      ...bind(caption),
      onInput: (e: Event) => {
        caption.set((e.target as HTMLInputElement).value);
        change({ caption: caption() });
      },
    }),
    // Optional: what pictures take from it. Left empty, the storyteller decides. Two lines,
    // so the example fits.
    h("textarea", {
      name: "ref-use",
      rows: 2,
      placeholder: "Use for (optional): e.g. face only, the outfit",
      title: "What pictures drawn from this one should take from it",
      ...bind(useFor),
      onInput: (e: Event) => {
        useFor.set((e.target as HTMLTextAreaElement).value);
        change({ use_for: useFor() });
      },
    }),
    action("×", remove, {
      class: "icon ref-remove",
      title: "Remove this picture",
    }),
  );
}

/**
 * LoRA adapters per picture/video model the story uses: the vault's, each switched on or
 * off with a weight (only those on are used). The vault is opened on the stories screen
 * (adapters are added in thinfer).
 */
function loraPicker(s: Signal<Settings>, models: () => ModelCaps[]): Node {
  const vault = signal<boolean | null>(null);
  vaultState().then(
    (v) => vault.set(v.set),
    () => vault.set(false),
  );
  const lists = signal<Record<string, Adapter[] | null>>({});
  const used = computed(() => {
    const x = s();
    const ids = [x.image_model, x.image_edit_model];
    if (x.video) ids.push(x.video_model, x.video_ref_model);
    return [...new Set(ids)].join("\n");
  });
  effect(() => {
    if (!vault()) return;
    for (const id of used().split("\n")) {
      if (id in lists.peek()) continue;
      vaultAdapters(id).then(
        (r) => lists.update((l) => ({ ...l, [id]: r.adapters })),
        () => lists.update((l) => ({ ...l, [id]: null })),
      );
    }
  });
  const picks = (model: string) => s().loras?.[model] ?? [];
  const pick = (model: string, id: string, on: boolean, weight: number) =>
    s.update((x) => {
      const now = (x.loras?.[model] ?? []).map((l) => (l.id === id ? { id, weight } : l));
      const next = on
        ? now.some((l) => l.id === id)
          ? now
          : [...now, { id, weight }]
        : now.filter((l) => l.id !== id);
      const loras = { ...x.loras, [model]: next };
      if (!next.length) delete loras[model];
      const { loras: _, ...rest } = x;
      return Object.keys(loras).length ? { ...rest, loras } : rest;
    });
  return h("div", { class: "loras" }, () => {
    const open = vault();
    if (open === null) return null;
    const head = h("h3", null, "LoRA adapters");
    if (!open)
      return [
        head,
        h(
          "p",
          { class: "muted small" },
          "Open the LoRA vault on the stories screen to pick adapters here (add them in thinfer).",
        ),
      ];
    return [
      head,
      ...used()
        .split("\n")
        .map((id) => {
          const list = lists()[id];
          const title = models().find((m) => m.id === id)?.title || id;
          return h(
            "div",
            { class: "lora-model", "data-model": id },
            h("div", { class: "small" }, title),
            list === undefined
              ? h("p", { class: "muted small" }, "…")
              : !list?.length
                ? h("p", { class: "muted small" }, "None in the vault for this model.")
                : list.map((a) => {
                    const p = picks(id).find((l) => l.id === a.id);
                    const w = p?.weight ?? a.weight ?? 1;
                    return h(
                      "div",
                      { class: "row lora" },
                      h(
                        "label",
                        { class: "check grow" },
                        h("input", {
                          type: "checkbox",
                          name: "lora-on",
                          checked: !!p,
                          onChange: (e: Event) =>
                            pick(id, a.id, (e.target as HTMLInputElement).checked, w),
                        }),
                        h("span", null, a.name || a.id),
                      ),
                      h("input", {
                        type: "number",
                        name: "lora-weight",
                        class: "lora-weight",
                        step: 0.05,
                        min: -2,
                        max: 2,
                        value: String(Math.round(w * 100) / 100),
                        disabled: !p,
                        title: "Weight",
                        onChange: (e: Event) => {
                          const v = Number((e.target as HTMLInputElement).value);
                          if (Number.isFinite(v)) pick(id, a.id, true, v);
                        },
                      }),
                    );
                  }),
          );
        }),
    ];
  });
}

function settingsCard(ed: Editor, initial: Settings, objects: () => ObjectView[]): Node {
  for (const [k, v] of Object.entries(MODEL_DEFAULTS))
    if ((initial as unknown as Record<string, unknown>)[k] == null)
      (initial as unknown as Record<string, unknown>)[k] = v;
  const s = signal<Settings>({ ...initial });
  const models = signal<ModelCaps[]>([]);
  listModels().then(models.set, () => {});
  const shape = () => s().orientation ?? "landscape";
  const set = <K extends keyof Settings>(k: K, v: Settings[K]) =>
    s.update((x) => ({ ...x, [k]: v }));
  const dirty = () => JSON.stringify(s()) !== JSON.stringify(initial);
  return h(
    "section",
    { class: "card" },
    h("h2", null, "Settings"),
    h(
      "div",
      { class: "grid2" },
      MODEL_FIELDS.map(([k, label, fits]) =>
        h("label", null, label, () => {
          const list = models().filter(fits);
          const current = String(s()[k]);
          return h(
            "select",
            {
              name: k,
              onChange: (e: Event) => set(k, (e.target as HTMLSelectElement).value as never),
            },
            // What the story uses now, even if this generator doesn't have it.
            list.some((m) => m.id === current)
              ? null
              : h("option", { value: current, selected: true }, `${current} (not available)`),
            list.map((m) =>
              h(
                "option",
                { value: m.id, selected: m.id === current },
                [
                  m.title || m.id,
                  m.max_refs ? `${m.max_refs} references` : "",
                  m.downloaded ? "" : "downloads on first use",
                ]
                  .filter(Boolean)
                  .join(" · "),
              ),
            ),
          );
        }),
      ),
    ),
    () =>
      models().length
        ? null
        : h(
            "p",
            { class: "muted small" },
            "No generator is connected, so its models can't be listed.",
          ),
    loraPicker(s, models),
    h(
      "div",
      { class: "row wrap" },
      h(
        "label",
        null,
        "Shape",
        h(
          "select",
          {
            name: "orientation",
            onChange: (e: Event) => {
              const o = (e.target as HTMLSelectElement).value as "landscape" | "portrait";
              const [w, hh] = RESOLUTIONS[o][0];
              s.update((x) => ({
                ...x,
                orientation: o,
                width: w,
                height: hh,
                video_size: undefined,
              }));
            },
          },
          h("option", { value: "landscape", selected: () => shape() === "landscape" }, "Landscape"),
          h("option", { value: "portrait", selected: () => shape() === "portrait" }, "Portrait"),
        ),
      ),
      h("label", null, "Resolution", () =>
        h(
          "select",
          {
            name: "resolution",
            onChange: (e: Event) => {
              const [w, hh] = (e.target as HTMLSelectElement).value.split("x").map(Number);
              if (w) s.update((x) => ({ ...x, width: w, height: hh }));
            },
          },
          RESOLUTIONS[shape()].some(([w, hh]) => s().width === w && s().height === hh)
            ? null
            : h(
                "option",
                { value: `${s().width}x${s().height}`, selected: true },
                `${s().width} × ${s().height}`,
              ),
          RESOLUTIONS[shape()].map(([w, hh, name]) =>
            h(
              "option",
              {
                value: `${w}x${hh}`,
                selected: s().width === w && s().height === hh,
              },
              `${name} (${w} × ${hh})`,
            ),
          ),
        ),
      ),
    ),
    h("label", null, "Video size", () => {
      const [aw, ah] = [s().width, s().height];
      const vs = s().video_size;
      const presets = RESOLUTIONS[shape()].filter(([w, hh]) => w !== aw || hh !== ah);
      return h(
        "select",
        {
          name: "video-size",
          onChange: (e: Event) => {
            const v = (e.target as HTMLSelectElement).value;
            const [w, hh] = v.split("x").map(Number);
            s.update((x) => ({
              ...x,
              video_size: v === "auto" || !w ? undefined : [w, hh],
            }));
          },
        },
        h("option", { value: "auto", selected: !vs }, `Same as pictures (${aw} × ${ah})`),
        vs && !presets.some(([w, hh]) => w === vs[0] && hh === vs[1])
          ? h("option", { value: `${vs[0]}x${vs[1]}`, selected: true }, `${vs[0]} × ${vs[1]}`)
          : null,
        presets.map(([w, hh, name]) =>
          h(
            "option",
            {
              value: `${w}x${hh}`,
              selected: !!vs && vs[0] === w && vs[1] === hh,
            },
            `${name} (${w} × ${hh})`,
          ),
        ),
      );
    }),
    h(
      "p",
      { class: "muted small" },
      "Videos cost far more per pixel than pictures: pick a smaller video size to make them faster. Higher resolution makes every picture and video slower to generate. Characters only stay recognisable from picture to picture with models that take references.",
    ),
    h(
      "label",
      { class: "check" },
      h("input", {
        type: "checkbox",
        name: "video",
        checked: () => s().video,
        onChange: (e: Event) => set("video", (e.target as HTMLInputElement).checked),
      }),
      h("span", null, "Allow video scenes"),
    ),
    h(
      "label",
      null,
      "Beats between new pictures (at least)",
      h("input", {
        type: "number",
        name: "picture_gap",
        min: 0,
        max: 12,
        value: () => String(s().picture_gap ?? 0),
        onChange: (e: Event) =>
          set("picture_gap", Math.max(0, Number((e.target as HTMLInputElement).value) || 0)),
      }),
    ),
    h(
      "label",
      null,
      "Most beats on one picture (0 = no limit)",
      h("input", {
        type: "number",
        name: "picture_max",
        min: 0,
        max: 30,
        value: () => String(s().picture_max ?? 8),
        onChange: (e: Event) =>
          set("picture_max", Math.max(0, Number((e.target as HTMLInputElement).value) || 0)),
      }),
    ),
    h(
      "label",
      null,
      "Most beats without a choice (0 = no limit)",
      h("input", {
        type: "number",
        name: "choice_max",
        min: 0,
        max: 30,
        value: () => String(s().choice_max ?? 6),
        onChange: (e: Event) =>
          set("choice_max", Math.max(0, Number((e.target as HTMLInputElement).value) || 0)),
      }),
    ),
    h(
      "label",
      null,
      "Earliest ending, in beats (0 = any time)",
      h("input", {
        type: "number",
        name: "min_beats",
        min: 0,
        max: 500,
        value: () => String(s().min_beats ?? 40),
        onChange: (e: Event) =>
          set("min_beats", Math.max(0, Number((e.target as HTMLInputElement).value) || 0)),
      }),
    ),
    h(
      "label",
      { class: "check" },
      h("input", {
        type: "checkbox",
        name: "think",
        checked: () => s().think,
        onChange: (e: Event) => set("think", (e.target as HTMLInputElement).checked),
      }),
      h("span", null, "Let the text model think before each beat (much slower)"),
    ),
    h("h3", null, "Voices and sounds"),
    h(
      "label",
      { class: "check" },
      h("input", {
        type: "checkbox",
        name: "voices",
        checked: () => s().voices !== false,
        onChange: (e: Event) => set("voices", (e.target as HTMLInputElement).checked),
      }),
      h("span", null, "Speak the lines of talking characters aloud"),
    ),
    h(
      "label",
      null,
      "Narration read aloud by",
      h(
        "select",
        {
          name: "narrator",
          onChange: (e: Event) => {
            const v = (e.target as HTMLSelectElement).value;
            set("narrator", v || null);
          },
        },
        h("option", { value: "", selected: () => !s().narrator }, "Nobody (narration is silent)"),
        () =>
          objects()
            .filter((o) => o.talks)
            .map((o) =>
              h("option", { value: o.id, selected: () => s().narrator === o.id }, o.name),
            ),
      ),
    ),
    h(
      "p",
      { class: "muted small" },
      "To read the narration aloud, add a talking object for the narrator (its name and description are only for its voice: the storyteller isn't told about it) and pick it here.",
    ),
    h(
      "label",
      { class: "check" },
      h("input", {
        type: "checkbox",
        name: "sounds",
        checked: () => s().sounds !== false,
        onChange: (e: Event) => set("sounds", (e.target as HTMLInputElement).checked),
      }),
      h(
        "span",
        null,
        "Sound effects and background sounds (each is slow to make, so they're rare)",
      ),
    ),

    action(
      "Save settings",
      async () => {
        await ed.setSettings(s(), null);
        Object.assign(initial, s());
        s.update((x) => ({ ...x }));
        toast("Settings saved.");
      },
      { class: "primary", disabled: () => !dirty() },
    ),
  );
}
