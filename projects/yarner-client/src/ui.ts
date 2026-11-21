// A tiny reactive core (signals, effects, scopes) and a DOM builder on top of it.
//
// - `signal(v)` is a getter with `.set`, `.update` and `.peek`. Reading it inside an effect
//   subscribes that effect.
// - `effect(fn)` runs now and again whenever a signal it read changes. Effects created while
//   another effect (or a `root`) runs are owned by it and disposed when it re-runs or ends.
// - `h(tag, props, ...children)` builds elements. A function prop or child is reactive.

type Fn = () => void;

interface Computation {
  run: Fn;
  deps: Set<Set<Computation>>;
  cleanups: Fn[];
  disposed: boolean;
}

let observer: Computation | null = null;
let owner: Fn[] | null = null;
let batching = 0;
let flushing = false;
const pending = new Set<Computation>();

function flush() {
  if (flushing) return;
  flushing = true;
  try {
    while (pending.size) {
      const [c] = pending;
      pending.delete(c);
      c.run();
    }
  } finally {
    flushing = false;
  }
}

export interface Signal<T> {
  (): T;
  set(v: T): void;
  update(f: (v: T) => T): void;
  peek(): T;
}

export function signal<T>(value: T, equals: (a: T, b: T) => boolean = Object.is): Signal<T> {
  const subs = new Set<Computation>();
  const read = (() => {
    if (observer) {
      subs.add(observer);
      observer.deps.add(subs);
    }
    return value;
  }) as Signal<T>;
  read.peek = () => value;
  read.set = (v: T) => {
    if (equals(v, value)) return;
    value = v;
    for (const c of subs) pending.add(c);
    if (!batching) flush();
  };
  read.update = (f) => read.set(f(value));
  return read;
}

export function batch(fn: Fn) {
  batching++;
  try {
    fn();
  } finally {
    batching--;
    if (!batching) flush();
  }
}

export function effect(fn: () => void | Fn): Fn {
  const c: Computation = {
    deps: new Set(),
    cleanups: [],
    disposed: false,
    run,
  };
  function cleanup() {
    for (const d of c.deps) d.delete(c);
    c.deps.clear();
    for (const f of c.cleanups.splice(0).reverse()) f();
  }
  function run() {
    if (c.disposed) return;
    cleanup();
    const [o, w] = [observer, owner];
    observer = c;
    owner = c.cleanups;
    try {
      const r = fn();
      if (typeof r === "function") c.cleanups.push(r);
    } finally {
      observer = o;
      owner = w;
    }
  }
  const dispose = () => {
    c.disposed = true;
    pending.delete(c);
    cleanup();
  };
  owner?.push(dispose);
  run();
  return dispose;
}

/** A derived value, recomputed eagerly when its inputs change. */
export function computed<T>(fn: () => T): () => T {
  const s = signal<T>(undefined as T);
  effect(() => s.set(fn()));
  return s;
}

/** Run without subscribing the current effect (ownership is kept). */
export function untrack<T>(fn: () => T): T {
  const o = observer;
  observer = null;
  try {
    return fn();
  } finally {
    observer = o;
  }
}

/** An ownership scope: everything created inside is disposed by the returned function. */
export function root<T>(fn: (dispose: Fn) => T): T {
  const list: Fn[] = [];
  const [o, w] = [observer, owner];
  observer = null;
  owner = list;
  const dispose = () =>
    list
      .splice(0)
      .reverse()
      .forEach((f) => f());
  try {
    return fn(dispose);
  } finally {
    observer = o;
    owner = w;
  }
}

/** Run `fn` when the current scope ends. */
export function onCleanup(fn: Fn) {
  owner?.push(fn);
}

// DOM.

export type Child = Node | string | number | null | undefined | false | (() => Child) | Child[];
type Props = Record<string, unknown>;

const PROPS = new Set(["value", "checked", "disabled", "hidden", "selected", "textContent"]);

function setProp(el: HTMLElement, k: string, v: unknown) {
  if (k === "class") el.className = v == null || v === false ? "" : String(v);
  else if (k.startsWith("class:")) el.classList.toggle(k.slice(6), !!v);
  else if (k.startsWith("style:")) el.style.setProperty(k.slice(6), v == null ? null : String(v));
  else if (PROPS.has(k)) (el as unknown as Record<string, unknown>)[k] = v;
  else if (v == null || v === false) el.removeAttribute(k);
  else el.setAttribute(k, v === true ? "" : String(v));
}

function toNodes(c: Child, out: Node[]) {
  if (c == null || c === false) return;
  if (Array.isArray(c)) c.forEach((x) => toNodes(x, out));
  else if (typeof c === "function") out.push(dynamic(c));
  else if (c instanceof Node) out.push(c);
  else out.push(document.createTextNode(String(c)));
}

/** A reactive region between two comment markers. */
function dynamic(fn: () => Child): Node {
  const frag = document.createDocumentFragment();
  const start = document.createComment("");
  const end = document.createComment("");
  frag.append(start, end);
  effect(() => {
    const v = fn();
    const parent = end.parentNode!;
    while (start.nextSibling && start.nextSibling !== end) parent.removeChild(start.nextSibling);
    const nodes: Node[] = [];
    toNodes(v, nodes);
    for (const n of nodes) parent.insertBefore(n, end);
  });
  return frag;
}

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props?: Props | null,
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props ?? {})) {
    if (k === "ref") (v as (e: HTMLElement) => void)(el);
    else if (k.startsWith("on") && typeof v === "function")
      el.addEventListener(k.slice(2).toLowerCase(), v as EventListener);
    else if (typeof v === "function") effect(() => setProp(el, k, (v as () => unknown)()));
    else setProp(el, k, v);
  }
  const nodes: Node[] = [];
  toNodes(children, nodes);
  el.append(...nodes);
  return el;
}

/** Render `then()` while `cond()` is truthy (rebuilt only when truthiness flips). */
export function when(cond: () => unknown, then: () => Child, otherwise?: () => Child): () => Child {
  const b = computed(() => !!cond());
  return () => (b() ? untrack(then) : otherwise ? untrack(otherwise) : null);
}

/** Render a list, rebuilt whenever `items()` changes. */
export function each<T>(
  items: () => readonly T[],
  render: (t: T, i: number) => Child,
): () => Child {
  return () => {
    const xs = items();
    return untrack(() => xs.map(render));
  };
}

// Layers: modal overlays with an Escape stack.

const layers: { close: Fn }[] = [];
export const layerCount = signal(0);

addEventListener("keydown", (e) => {
  if (e.key === "Escape" && layers.length) {
    e.preventDefault();
    e.stopImmediatePropagation();
    layers[layers.length - 1].close();
  }
});

/** Mount an overlay on top of everything. `render` gets `close`. */
export function layer(render: (close: Fn) => Node, onClose?: Fn): Fn {
  let el: Node | null = null;
  let closed = false;
  const entry = { close: () => {} };
  root((dispose) => {
    entry.close = () => {
      if (closed) return;
      closed = true;
      layers.splice(layers.indexOf(entry), 1);
      layerCount.set(layers.length);
      dispose();
      (el as ChildNode | null)?.remove();
      onClose?.();
    };
    el = render(entry.close);
  });
  layers.push(entry);
  layerCount.set(layers.length);
  document.body.append(el!);
  return entry.close;
}

/** A centered dialog layer with a title bar. */
export function modal(title: string, body: (close: Fn) => Child, cls = ""): Fn {
  return layer((close) =>
    h(
      "div",
      {
        class: "modal-back",
        onClick: (e: Event) => e.target === e.currentTarget && close(),
      },
      h(
        "div",
        { class: `modal ${cls}`, role: "dialog", "aria-label": title },
        h(
          "header",
          null,
          h("h2", null, title),
          h("button", { class: "icon close", title: "Close (Esc)", onClick: close }, "×"),
        ),
        h("div", { class: "modal-body" }, body(close)),
      ),
    ),
  );
}

// Toasts.

export function toast(msg: string, kind: "info" | "error" = "info") {
  let box = document.getElementById("toasts");
  if (!box) document.body.append((box = h("div", { id: "toasts" })));
  const t = h("div", { class: `toast ${kind}` }, msg);
  box.append(t);
  setTimeout(() => t.classList.add("out"), 4200);
  setTimeout(() => t.remove(), 4800);
}

// Small helpers.

export function fmtDate(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

/** A button that disables itself while its async action runs. */
export function action(
  label: Child,
  run: () => Promise<unknown> | unknown,
  props: Props = {},
): HTMLButtonElement {
  const busy = signal(false);
  const outer = props.disabled as (() => boolean) | undefined;
  return h(
    "button",
    {
      ...props,
      disabled: () => busy() || !!outer?.(),
      "class:busy": busy,
      onClick: async (e: Event) => {
        e.stopPropagation();
        busy.set(true);
        try {
          await run();
        } catch (err) {
          toast(err instanceof Error ? err.message : String(err), "error");
        } finally {
          busy.set(false);
        }
      },
    },
    label,
  );
}

/** Two-way bind an input's value to a signal. */
export function bind(s: Signal<string>): Props {
  return {
    value: s,
    onInput: (e: Event) => s.set((e.target as HTMLInputElement).value),
  };
}
