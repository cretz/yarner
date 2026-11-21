// Edit mode: a side panel for the current beat. Re-run, fork, versions, the setup as it
// applies here (edit from here or everywhere, with history), state notes and the queue.

import { gpuState, setGpuPaused, type GpuState, type StoryInfo } from "../api.ts";
import { keepControl } from "../keep.ts";
import type {
  Anchor,
  ObjectRev,
  ObjectView,
  PlayView,
  PromptRev,
  PromptView,
  QueueEntry,
  Scope,
  SetupView,
  Writing,
} from "../api.ts";
import type { Editor } from "../source.ts";
import {
  action,
  bind,
  computed,
  each,
  effect,
  fmtDate,
  h,
  signal,
  toast,
  untrack,
  when,
  type Child,
} from "../ui.ts";
import { keyOf } from "./play.ts";

export interface EditCtx {
  edit: Editor;
  /** The story's info (background generation's time left). */
  info: () => Promise<StoryInfo>;
  play: string;
  view: () => PlayView | null;
  load: (p: Promise<PlayView>, toEnd?: boolean) => Promise<PlayView>;
  queue: () => QueueEntry[];
  writing: () => Writing | null;
  setupTick: () => number;
  /** The story map (in edit mode: jump anywhere). */
  map?: () => void;
  close: () => void;
}

type Tab = "screen" | "setup" | "notes" | "queue";
const TABS: [Tab, string][] = [
  ["screen", "This screen"],
  ["setup", "Setup"],
  ["notes", "Notes"],
  ["queue", "Queue"],
];
/** The tab Edit last showed: it opens there again. */
const lastTab = signal<Tab>("screen");

interface Work {
  kind: "rerun" | "fork";
  key: string;
  versions: number;
}

export function promptTitle(id: string): string {
  if (id === "story") return "Story";
  if (id === "style") return "Visual style";
  if (id.startsWith("here:")) return "Direction for one beat";
  if (id === "media") return "Pictures and video";
  if (id === "steer") return "Steer (from here on)";
  if (id === "pictures") return "Steer for pictures and video (from here on)";
  if (id.startsWith("next:")) return "Steer (the next beat)";
  return "Direction";
}

function whereText(anchor: Anchor, scope: Scope, beat: string | undefined): string {
  if (!anchor) return "everywhere";
  const at = anchor === beat ? "this beat" : "an earlier beat";
  return scope === "here" ? `${at} only` : `from ${at} on`;
}

export function editPanel(ctx: EditCtx): Node {
  const beat = () => ctx.view()?.beat ?? null;
  const beatId = computed(() => beat()?.id);
  const setup = signal<SetupView | null>(null);
  const work = signal<Work | null>(null);

  effect(() => {
    ctx.setupTick();
    beatId();
    ctx.edit.setup(ctx.play).then(setup.set, (e: Error) => toast(e.message, "error"));
  });

  // A re-run is done when the beat was re-written; a fork when a new version exists.
  effect(() => {
    const v = ctx.view();
    const w = work();
    if (!v || !w) return;
    if (w.kind === "rerun" && !v.regenerating && keyOf(v) !== w.key) {
      work.set(null);
      toast("The beat was re-written.");
    }
    const vs = v.beat?.versions ?? [];
    if (w.kind === "fork" && vs.length > w.versions) {
      work.set(null);
      toast("A new version is ready.");
      void ctx.load(ctx.edit.version(ctx.play, vs[vs.length - 1]));
    }
  });
  const busy = () => !!work() || !!ctx.view()?.regenerating;

  const direction = signal("");
  const scope = signal<Scope>("here");
  const forkDirection = signal("");
  const openPart = signal<"" | "direct" | "fork">("");

  const rerun = async (prompt?: string) => {
    const v = ctx.view();
    if (!v) return;
    work.set({ kind: "rerun", key: keyOf(v), versions: 0 });
    try {
      await ctx.load(ctx.edit.rerun(ctx.play, prompt, scope()));
      direction.set("");
      openPart.set("");
    } catch (e) {
      work.set(null);
      throw e;
    }
  };
  const fork = async () => {
    const v = ctx.view();
    if (!v?.beat) return;
    work.set({ kind: "fork", key: keyOf(v), versions: v.beat.versions.length });
    try {
      await ctx.load(ctx.edit.fork(ctx.play, forkDirection().trim() || undefined));
      forkDirection.set("");
      openPart.set("");
    } catch (e) {
      work.set(null);
      throw e;
    }
  };

  const beatSection = h(
    "section",
    null,
    h(
      "div",
      { class: "badges" },
      () => (beat()?.stale ? h("span", { class: "badge warn" }, "made with older setup") : null),
      () => (beat()?.pending ? h("span", { class: "badge" }, "not yet taken") : null),
      () => {
        const b = beat();
        return b?.label ? h("span", { class: "badge soft" }, b.label) : null;
      },
    ),
    () => {
      const b = beat();
      if (!b || b.versions.length < 2) return null;
      const i = b.version;
      return untrack(() =>
        h(
          "div",
          { class: "versions" },
          action("‹", () => ctx.load(ctx.edit.version(ctx.play, b.versions[i - 1])), {
            class: "icon",
            disabled: () => i === 0 || busy(),
            title: "Previous version",
          }),
          h("span", null, `Version ${i + 1} of ${b.versions.length}`),
          action("›", () => ctx.load(ctx.edit.version(ctx.play, b.versions[i + 1])), {
            class: "icon",
            disabled: () => i === b.versions.length - 1 || busy(),
            title: "Next version",
          }),
        ),
      );
    },
    when(busy, () =>
      h(
        "div",
        { class: "progress" },
        h("div", { class: "bar" }),
        h("span", null, () =>
          work()?.kind === "fork" ? "Writing a new version…" : "Re-writing this beat…",
        ),
      ),
    ),
    h(
      "div",
      { class: "lines" },
      each(lines(beat), (l) =>
        h(
          "p",
          { class: l.speaker ? "said" : "narration" },
          l.speaker ? h("b", null, l.speaker) : null,
          l.text,
        ),
      ),
    ),
    h(
      "div",
      { class: "row wrap" },
      action("Rewrite this beat now", () => rerun(), {
        disabled: busy,
        title: "Write the beat on screen again, right away (its picture too, if it asked for one)",
      }),
      h(
        "button",
        {
          "class:on": () => openPart() === "direct",
          disabled: busy,
          onClick: () => openPart.update((p) => (p === "direct" ? "" : "direct")),
        },
        "Rewrite it with a direction…",
      ),
      h(
        "button",
        {
          "class:on": () => openPart() === "fork",
          disabled: busy,
          // Says why when it's off.
          title: () =>
            busy()
              ? "Wait for the rewrite under way to finish."
              : "A new version of this beat. The current one is kept.",
          onClick: () => openPart.update((p) => (p === "fork" ? "" : "fork")),
        },
        "Fork from here…",
      ),
    ),
    () =>
      openPart() === "direct"
        ? untrack(() =>
            h(
              "div",
              { class: "subform" },
              h("textarea", {
                name: "direction",
                rows: 3,
                placeholder: 'What should change? (e.g. "she refuses, angrily")',
                ...bind(direction),
              }),
              h(
                "div",
                { class: "radios" },
                radio("scope", "here", scope, "This beat only"),
                radio("scope", "from_here", scope, "This beat and everything after"),
              ),
              h(
                "p",
                { class: "muted small" },
                "The beat on screen is rewritten now. To steer where the story goes from here instead, close Edit and use Steer… in the bar.",
              ),
              action("Rewrite this beat now", () => rerun(direction().trim()), {
                class: "primary",
                disabled: () => busy() || !direction().trim(),
              }),
            ),
          )
        : openPart() === "fork"
          ? untrack(() =>
              h(
                "div",
                { class: "subform" },
                h("textarea", {
                  name: "fork-direction",
                  rows: 3,
                  placeholder: "Optional direction for the new version",
                  ...bind(forkDirection),
                }),
                h("p", { class: "muted small" }, "The current version stays; you can switch back."),
                action("Fork", fork, { class: "primary", disabled: busy }),
              ),
            )
          : null,
  );

  const promptIds = computed(() => (setup()?.prompts ?? []).map((p) => p.id).join("\n"));
  const objectIds = computed(() => (setup()?.objects ?? []).map((o) => o.id).join("\n"));
  const prompt = (id: string) => () => setup()?.prompts.find((p) => p.id === id) ?? null;
  const object = (id: string) => () => setup()?.objects.find((o) => o.id === id) ?? null;

  const setupSection = h(
    "section",
    null,
    h("p", { class: "muted small" }, "The setup as it stands at this screen."),
    () => {
      const ids = promptIds();
      return untrack(() =>
        ids ? ids.split("\n").map((id) => promptCard(ctx, id, prompt(id), beatId)) : null,
      );
    },
    () => {
      const ids = objectIds();
      return untrack(() =>
        ids ? ids.split("\n").map((id) => objectCard(ctx, object(id), beatId)) : null,
      );
    },
  );

  const panes: Record<Tab, Node> = {
    screen: beatSection,
    setup: setupSection,
    notes: stateSection(ctx),
    queue: queueSection(ctx),
  };
  const working = () => ctx.queue().filter((j) => !j.lined).length;
  return h(
    "div",
    {
      class: "edit-panel",
      role: "dialog",
      "aria-label": "Edit",
      onClick: (e: Event) => e.stopPropagation(),
    },
    h(
      "header",
      null,
      h(
        "div",
        { class: "grow" },
        h("h2", null, "Edit"),
        h("small", { class: "muted" }, "Play is paused"),
      ),
      ctx.map
        ? h(
            "button",
            {
              class: "map-open",
              title: "Story map: jump anywhere (M)",
              onClick: ctx.map,
            },
            "Map",
          )
        : null,
      h(
        "button",
        {
          class: "icon close",
          title: "Leave edit mode (E)",
          onClick: ctx.close,
        },
        "×",
      ),
    ),
    h(
      "nav",
      { class: "edit-tabs", role: "tablist" },
      TABS.map(([t, name]) =>
        h(
          "button",
          {
            role: "tab",
            "data-tab": t,
            "aria-selected": () => String(lastTab() === t),
            "class:on": () => lastTab() === t,
            onClick: () => lastTab.set(t),
          },
          name,
          t === "queue"
            ? h("span", { class: "count" }, () => (working() ? String(working()) : ""))
            : null,
        ),
      ),
    ),
    h(
      "div",
      { class: "edit-scroll" },
      h(
        "div",
        { class: "edit-body" },
        TABS.map(([t]) =>
          h(
            "div",
            {
              class: "edit-pane",
              "data-pane": t,
              hidden: () => lastTab() !== t,
            },
            panes[t],
          ),
        ),
      ),
    ),
  );
}

const lines = (beat: () => PlayView["beat"]) => () => beat()?.lines ?? [];

function radio<T extends string>(
  name: string,
  value: T,
  s: { (): T; set(v: T): void },
  label: string,
) {
  return h(
    "label",
    { class: "radio" },
    h("input", {
      type: "radio",
      name,
      value,
      checked: () => s() === value,
      onChange: () => s.set(value),
    }),
    label,
  );
}

type Where = "here" | "everywhere";

function whereRadios(name: string, where: { (): Where; set(v: Where): void }) {
  return h(
    "div",
    { class: "radios" },
    radio(name, "here", where, "From here on"),
    radio(name, "everywhere", where, "Everywhere"),
  );
}

function timeline<T extends { rev: string; at: number; anchor: Anchor }>(
  ctx: EditCtx,
  items: T[],
  current: string | undefined,
  beat: string | undefined,
  scopeOf: (t: T) => Scope,
  body: (t: T) => Child,
): Node {
  return h(
    "ol",
    { class: "timeline" },
    items.map((t) =>
      h(
        "li",
        { "class:current": t.rev === current },
        h(
          "div",
          { class: "tl-head" },
          h("time", null, fmtDate(t.at)),
          h("small", { class: "muted" }, whereText(t.anchor, scopeOf(t), beat)),
        ),
        body(t),
        t.anchor || items.length > 1
          ? action("Make this the version everywhere", () => ctx.edit.everywhere(t.rev), {
              class: "link",
            })
          : null,
      ),
    ),
  );
}

function promptCard(
  ctx: EditCtx,
  id: string,
  p: () => PromptView | null,
  beatId: () => string | undefined,
): Node {
  const editing = signal(false);
  const draft = signal("");
  const where = signal<Where>("here");
  const showHistory = signal(false);
  return h(
    "div",
    { class: "card", "data-prompt": id },
    h(
      "div",
      { class: "card-head" },
      h("strong", null, promptTitle(id)),
      h("small", { class: "muted" }, () => {
        const x = p();
        return x ? whereText(x.anchor, x.scope, beatId()) : "";
      }),
      h("span", { class: "grow" }),
      h(
        "button",
        { class: "link", onClick: () => showHistory.update((x) => !x) },
        () => `History (${p()?.history.length ?? 0})`,
      ),
      h(
        "button",
        {
          class: "link",
          onClick: () => {
            draft.set(p()?.text ?? "");
            editing.update((x) => !x);
          },
        },
        () => (editing() ? "Cancel" : "Edit"),
      ),
    ),
    () =>
      editing()
        ? untrack(() =>
            h(
              "div",
              { class: "subform" },
              h("textarea", { name: `prompt-${id}`, rows: 5, ...bind(draft) }),
              whereRadios(`where-${id}`, where),
              action(
                "Save",
                async () => {
                  const anchor = where() === "here" ? (beatId() ?? null) : null;
                  await ctx.edit.setPrompt(id, draft(), anchor, "from_here");
                  editing.set(false);
                },
                { class: "primary" },
              ),
            ),
          )
        : h("p", { class: "prompt-text" }, () => p()?.text ?? ""),
    () =>
      showHistory()
        ? (() => {
            const x = p();
            return untrack(() =>
              x
                ? timeline<PromptRev>(
                    ctx,
                    x.history,
                    x.rev,
                    beatId(),
                    (t) => t.scope,
                    (t) => h("p", { class: "tl-text" }, t.text || h("em", null, "(cleared)")),
                  )
                : null,
            );
          })()
        : null,
  );
}

function objectCard(
  ctx: EditCtx,
  o: () => ObjectView | null,
  beatId: () => string | undefined,
): Node {
  const editing = signal(false);
  const name = signal("");
  const desc = signal("");
  const where = signal<Where>("here");
  const showHistory = signal(false);
  return h(
    "div",
    { class: "card object" },
    h(
      "div",
      { class: "card-head" },
      h("strong", null, () => o()?.name ?? ""),
      h("small", { class: "muted" }, () => {
        const x = o();
        return x ? whereText(x.anchor, "from_here", beatId()) : "";
      }),
      h("span", { class: "grow" }),
      h(
        "button",
        { class: "link", onClick: () => showHistory.update((x) => !x) },
        () => `History (${o()?.history.length ?? 0})`,
      ),
      h(
        "button",
        {
          class: "link",
          onClick: () => {
            name.set(o()?.name ?? "");
            desc.set(o()?.description ?? "");
            editing.update((x) => !x);
          },
        },
        () => (editing() ? "Cancel" : "Edit"),
      ),
    ),
    () =>
      editing()
        ? untrack(() =>
            h(
              "div",
              { class: "subform" },
              h("input", { name: "object-name", ...bind(name) }),
              h("textarea", {
                name: "object-description",
                rows: 4,
                ...bind(desc),
              }),
              whereRadios(`where-o-${o()?.id}`, where),
              action(
                "Save",
                async () => {
                  const x = o();
                  if (!x) return;
                  await ctx.edit.putObject({
                    id: x.id,
                    name: name(),
                    description: desc(),
                    refs: x.refs.map(([r]) => r),
                    talks: x.talks,
                    voice: x.voice,
                    anchor: where() === "here" ? (beatId() ?? null) : null,
                  });
                  editing.set(false);
                },
                { class: "primary" },
              ),
            ),
          )
        : h("p", { class: "prompt-text" }, () => o()?.description ?? ""),
    () =>
      showHistory()
        ? (() => {
            const x = o();
            return untrack(() =>
              x
                ? timeline<ObjectRev>(
                    ctx,
                    x.history,
                    x.rev,
                    beatId(),
                    () => "from_here",
                    (t) =>
                      h(
                        "p",
                        { class: "tl-text" },
                        t.removed ? h("em", null, "(removed)") : `${t.name}: ${t.description}`,
                      ),
                  )
                : null,
            );
          })()
        : null,
  );
}

function stateSection(ctx: EditCtx): Node {
  return h(
    "section",
    null,
    h("h3", null, "Story notes"),
    () =>
      (ctx.view()?.state.length ?? 0) === 0
        ? h("p", { class: "muted small" }, "No notes at this point.")
        : null,
    h(
      "dl",
      { class: "notes" },
      each(
        () => ctx.view()?.state ?? [],
        ([k, v]) => [h("dt", null, k), h("dd", null, v)],
      ),
    ),
  );
}

/** Pause: give the GPU back (every story stops, the alpha's thinfer is stopped and its
 * memory freed; cancelled work is made again on resume). Hidden without a server. */
function gpuControl(): Node {
  const state = signal<GpuState | null>(null);
  gpuState().then(state.set, () => {});
  const toggle = () => {
    const pause = state() === "running";
    state.set(pause ? "pausing" : "resuming");
    setGpuPaused(pause).then(state.set, (e: Error) => {
      toast(e.message, "error");
      gpuState().then(state.set, () => {});
    });
  };
  return h("div", { class: "gpu-control" }, () => {
    const s = state();
    if (!s) return null;
    const busy = s === "pausing" || s === "resuming";
    return [
      h(
        "button",
        {
          class: s === "paused" ? "primary" : "",
          disabled: busy,
          onClick: toggle,
        },
        s === "running"
          ? "⏸ Pause: give the GPU back"
          : s === "paused"
            ? "▶ Resume"
            : s === "pausing"
              ? "Pausing…"
              : "Resuming…",
      ),
      h(
        "p",
        { class: "muted small" },
        s === "paused"
          ? "Paused: nothing is being made and the GPU is free. Resume to carry on where it left off."
          : s === "resuming"
            ? "Starting the model server again…"
            : s === "pausing"
              ? "Stopping the work and freeing the GPU…"
              : "Pausing stops all background work and frees the GPU for you; what was being made is made again on resume.",
      ),
    ];
  });
}

function queueSection(ctx: EditCtx): Node {
  const clock = (x: number) => {
    const t = Math.round(x);
    return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, "0")}`;
  };
  const kindWord = (k: QueueEntry["kind"]) =>
    k === "image" ? "picture" : k === "video" ? "video" : k === "text" ? "writing" : "job";
  const row = (j: QueueEntry) =>
    h(
      "li",
      { "class:running": j.running, "class:lined": !!j.lined },
      h(
        "div",
        { class: "q-head" },
        h("span", { class: "q-label" }, j.label ?? kindWord(j.kind)),
        h(
          "span",
          { class: "q-time" },
          j.running
            ? (j.progress != null ? `${Math.round(j.progress * 100)}% · ` : "") +
                (j.secs != null ? clock(j.secs) : "")
            : j.lined
              ? "waiting for a slot"
              : `in line${j.secs != null ? ` · ${clock(j.secs)}` : ""}`,
        ),
      ),
      h(
        "div",
        { class: "q-meta muted" },
        `${kindWord(j.kind)} · priority ${j.priority}`,
        j.priority >= 1000 ? h("span", { class: "badge warn" }, "you're waiting") : null,
      ),
      j.running
        ? h(
            "div",
            { class: "q-bar", "class:unknown": j.progress == null },
            h("div", {
              style: `width:${Math.round((j.progress ?? 0) * 100)}%`,
            }),
          )
        : null,
    );
  const sent = () => ctx.queue().filter((j) => !j.lined);
  const lined = () => ctx.queue().filter((j) => j.lined);
  return h(
    "section",
    null,
    gpuControl(),
    keepControl(ctx.edit, ctx.info),
    () => (ctx.queue().length ? null : h("p", { class: "muted small" }, "Idle: nothing to make.")),
    () => (sent().length ? h("h3", null, "Being made") : null),
    h("ul", { class: "queue" }, each(sent, row)),
    () => (lined().length ? h("h3", null, "Lined up next") : null),
    h("ul", { class: "queue" }, each(lined, row)),
    () => {
      const w = ctx.writing();
      if (!w) return null;
      const s = (x: number) => `${x < 10 ? x.toFixed(1) : Math.round(x)}s`;
      return h(
        "p",
        { class: "muted small", "data-writing": "" },
        `Beats take about ${s(w.wall_secs)}, first line after ${s(w.first_line_secs)}; ` +
          `${Math.round(w.resumed * 100)}% continued from cache (last ${w.samples}).`,
      );
    },
    h("p", { class: "muted small" }, "Updates every couple of seconds."),
  );
}
