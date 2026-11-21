// The story map: a canvas-drawn, zoomable, draggable left-to-right graph of every beat.
// Layout: depth by BFS from the root over ordinary edges (rejoins don't move anything),
// rows by a tidy-tree pass over the BFS tree. Only what's in view is drawn.

import type { MapEdge, MapNode, MapView } from "./api.ts";
import type { Source } from "./source.ts";
import { h, layer, onCleanup, toast } from "./ui.ts";

// Tile size and spacing: the story's shape (set when the map opens; one map at a time).
let W = 128;
let H = 72;
let COL = 190;
let ROW = 92;
function shapeTiles(frame?: [number, number]) {
  const portrait = !!frame && frame[1] > frame[0];
  [W, H] = portrait ? [72, 128] : [128, 72];
  // Room between columns for the choices' labels (narrow tiles leave more to spare).
  COL = W + (portrait ? 110 : 62);
  ROW = H + 20;
}
const GOLD = "#e8c27a";

// Decoded thumbnails, in memory only (a video's is a frame drawn from it; null: none).
const thumbs = new Map<string, HTMLImageElement | HTMLCanvasElement | null>();

/** What a video shows now, drawn into a canvas (null: nothing decoded yet). */
function drawFrame(v: HTMLVideoElement): HTMLCanvasElement | null {
  if (!v.videoWidth || !v.videoHeight) return null;
  const c = document.createElement("canvas");
  c.width = v.videoWidth;
  c.height = v.videoHeight;
  c.getContext("2d")?.drawImage(v, 0, 0);
  return c;
}

/**
 * A video on screen got to its end: its last frame is the map tile for it (the frame the
 * reader saw last; no extra loading).
 */
export function keepVideoFrame(url: string, v: HTMLVideoElement) {
  const c = drawFrame(v);
  if (!c) return;
  thumbs.set(url, c);
  onThumb?.();
}

/**
 * A video's last frame, drawn into a canvas (a map tile for a video screen). Played muted
 * and paused at the end: iOS Safari won't load or seek a video nobody plays (tiles stayed
 * gray; 2026-11-22 alpha).
 */
function videoFrame(url: string): Promise<HTMLCanvasElement> {
  return new Promise((ok, fail) => {
    const v = document.createElement("video");
    v.muted = true;
    v.defaultMuted = true;
    v.setAttribute("muted", "");
    v.setAttribute("playsinline", "");
    v.playsInline = true;
    v.preload = "auto";
    let settled = false;
    // Given up on, or done: stop loading it (a late seek draws nothing).
    const release = () => {
      settled = true;
      v.onseeked = v.onerror = v.onloadedmetadata = v.onplaying = null;
      v.pause();
      v.removeAttribute("src");
      v.load();
    };
    const done = setTimeout(() => {
      release();
      fail(new Error("timeout"));
    }, 20000);
    const toEnd = () => {
      if (settled) return;
      v.pause();
      v.currentTime = Math.max(0, (v.duration || 0) - 0.05);
    };
    v.onloadedmetadata = () => {
      // Seeking alone works where data loads unplayed; else playing makes it load.
      toEnd();
      v.play().then(
        () => undefined,
        () => undefined,
      );
    };
    v.onplaying = toEnd;
    v.onseeked = () => {
      const c = drawFrame(v);
      if (!c) return;
      clearTimeout(done);
      release();
      ok(c);
    };
    v.onerror = () => {
      clearTimeout(done);
      release();
      fail(new Error("no video"));
    };
    v.src = url;
  });
}
const inflight = new Set<string>();
const queued: { url: string; video: boolean }[] = [];
/** Videos whose frame didn't come: how often, and when to try again. */
const missed = new Map<string, { n: number; at: number }>();
let videoBusy = false;
let onThumb: (() => void) | null = null;
const VIDEO_TRIES = 4;

export function clearThumbs() {
  thumbs.clear();
  missed.clear();
  queued.length = 0;
}

function settle(url: string) {
  inflight.delete(url);
  onThumb?.();
  pumpThumbs();
}

/** A video's frame, one at a time: iOS Safari decodes only one video at once, so tiles
 * fetched together timed out and stayed gray for good (2026-11-22 alpha). A miss is tried
 * again a little later, a few times. */
function loadVideo(url: string) {
  videoBusy = true;
  videoFrame(url)
    .then(
      (c) => {
        missed.delete(url);
        thumbs.set(url, c);
      },
      () => {
        const n = (missed.get(url)?.n ?? 0) + 1;
        missed.set(url, { n, at: performance.now() + 5000 * n });
        if (n >= VIDEO_TRIES) thumbs.set(url, null);
        // Drawn again then, which asks again.
        else setTimeout(() => onThumb?.(), 5000 * n + 50);
      },
    )
    .finally(() => {
      videoBusy = false;
      settle(url);
    });
}

function pumpThumbs() {
  for (let i = 0; i < queued.length && inflight.size < 6; ) {
    const { url, video } = queued[i];
    if (thumbs.has(url) || inflight.has(url)) {
      queued.splice(i, 1);
      continue;
    }
    if (video && videoBusy) {
      i++;
      continue;
    }
    queued.splice(i, 1);
    inflight.add(url);
    if (video) {
      loadVideo(url);
      continue;
    }
    const img = new Image();
    img.decoding = "async";
    img.src = url;
    img.decode().then(
      () => {
        thumbs.set(url, img);
        settle(url);
      },
      // Not a picture after all: a video's frame, in the video line.
      () => {
        inflight.delete(url);
        queued.push({ url, video: true });
        pumpThumbs();
      },
    );
  }
}

function wantThumb(url: string, video: boolean) {
  if (thumbs.has(url) || inflight.has(url) || queued.some((q) => q.url === url)) return;
  const m = missed.get(url);
  if (m && performance.now() < m.at) return;
  queued.push({ url, video });
  pumpThumbs();
}

/** A small film mark in a tile's corner: this screen is a video. */
function videoMark(ctx: CanvasRenderingContext2D, x: number, y: number) {
  ctx.fillStyle = "rgba(8,9,13,0.75)";
  ctx.beginPath();
  ctx.roundRect(x, y, 22, 16, 4);
  ctx.fill();
  ctx.fillStyle = "#e9e5dc";
  ctx.beginPath();
  ctx.moveTo(x + 8, y + 4);
  ctx.lineTo(x + 16, y + 8);
  ctx.lineTo(x + 8, y + 12);
  ctx.closePath();
  ctx.fill();
}

interface Laid {
  n: MapNode;
  x: number;
  y: number;
}

interface Layout {
  nodes: Laid[];
  byId: Map<string, Laid>;
  edges: { a: Laid; b: Laid; e: MapEdge; path: boolean }[];
  ghosts: { from: Laid; exit: number }[];
  current: Laid | null;
}

export function layout(m: MapView): Layout {
  const byId = new Map<string, Laid>();
  for (const n of m.nodes) byId.set(n.id, { n, x: 0, y: 0 });
  const out = new Map<string, MapEdge[]>();
  for (const e of m.edges) {
    if (!byId.has(e.from) || !byId.has(e.to)) continue;
    if (!out.has(e.from)) out.set(e.from, []);
    out.get(e.from)!.push(e);
  }
  for (const list of out.values()) list.sort((a, b) => a.exit - b.exit);

  // BFS: ordinary edges first, then rejoins only for nodes nothing else reaches.
  const depth = new Map<string, number>();
  const kids = new Map<string, string[]>();
  const roots: string[] = [];
  const bfs = (starts: string[], rejoins: boolean) => {
    const q = [...starts];
    for (let i = 0; i < q.length; i++) {
      const id = q[i];
      for (const e of out.get(id) ?? []) {
        if (depth.has(e.to) || (e.rejoin && !rejoins)) continue;
        depth.set(e.to, depth.get(id)! + 1);
        if (!kids.has(id)) kids.set(id, []);
        kids.get(id)!.push(e.to);
        q.push(e.to);
      }
    }
  };
  // Every version of the opening starts a tree of its own (the root first).
  const openings = [...new Set([...(m.root ? [m.root] : []), ...(m.openings ?? [])])].filter((id) =>
    byId.has(id),
  );
  for (const o of openings) {
    depth.set(o, 0);
    roots.push(o);
  }
  bfs(openings, false);
  bfs([...depth.keys()], true);
  for (const n of m.nodes) {
    if (depth.has(n.id)) continue;
    depth.set(n.id, 0);
    roots.push(n.id);
    bfs([n.id], true);
  }

  // Tidy rows: leaves take the next row, parents sit midway over their children.
  let slot = 0;
  for (const r of roots) {
    const stack: [string, number][] = [[r, 0]];
    while (stack.length) {
      const top = stack[stack.length - 1];
      const ch = kids.get(top[0]) ?? [];
      if (top[1] < ch.length) {
        stack.push([ch[top[1]++], 0]);
        continue;
      }
      stack.pop();
      const l = byId.get(top[0])!;
      l.x = depth.get(top[0])! * COL;
      l.y = ch.length ? (byId.get(ch[0])!.y + byId.get(ch[ch.length - 1])!.y) / 2 : slot++ * ROW;
    }
    slot++;
  }

  const nodes = [...byId.values()];
  const edges = m.edges
    .filter((e) => byId.has(e.from) && byId.has(e.to))
    .map((e) => {
      const a = byId.get(e.from)!;
      const b = byId.get(e.to)!;
      const path =
        a.n.path_index != null && b.n.path_index != null && b.n.path_index === a.n.path_index + 1;
      return { a, b, e, path };
    });
  const ghosts = m.generating
    .filter(([from]) => byId.has(from))
    .map(([from, exit]) => ({ from: byId.get(from)!, exit }));
  let current: Laid | null = null;
  for (const l of nodes)
    if (l.n.path_index != null && (!current || l.n.path_index > current.n.path_index!)) current = l;
  return { nodes, byId, edges, ghosts, current };
}

export interface MapOpts {
  editing: boolean;
  /** Seen beats off the current path can be gone to too. */
  seenJump?: boolean;
  /** The story's picture shape (width, height): tiles take it. */
  frame?: [number, number];
  pick: (n: MapNode) => Promise<void>;
}

export function openMap(src: Source, play: string, opts: MapOpts) {
  shapeTiles(opts.frame);
  layer((close) => mapView(src, play, opts, close));
}

function mapView(src: Source, play: string, opts: MapOpts, close: () => void): Node {
  const canvas = h("canvas", { class: "map-canvas" });
  const tip = h("div", { class: "map-tip", hidden: true });
  const count = h("span", { class: "muted small" }, "Loading…");
  const ctx = canvas.getContext("2d")!;
  let L: Layout | null = null;
  let s = 1;
  let tx = 0;
  let ty = 0;
  let dirty = true;
  let hover: Laid | null = null;
  let placed = false;
  let closed = false;
  onThumb = () => (dirty = true);
  // Where each beat is on screen (lets scripted tests click one).
  Object.assign(canvas, {
    yarnerNodes: () =>
      (L?.nodes ?? []).map((l) => ({
        id: l.n.id,
        pathIndex: l.n.path_index,
        x: tx + (l.x + W / 2) * s,
        y: ty + (l.y + H / 2) * s,
      })),
  });

  // Anything you've seen can be gone back to, from any route (2026-11-21 alpha: after
  // going back and taking another way, the old route couldn't be clicked).
  const clickable = (n: MapNode) =>
    opts.editing || n.path_index != null || (!!opts.seenJump && n.seen);
  // Spoilers: in play, what's ahead of the reader or on paths they didn't take (and
  // haven't seen) stays blurred until they say they're fine seeing it.
  let reveal = opts.editing;
  const spoiler = (n: MapNode) => !reveal && !n.on_path && !n.seen;
  const askReveal = () => {
    if (reveal) return true;
    if (
      !confirm(
        "Show the rest of the story map? It will spoil what's ahead and paths you haven't taken.",
      )
    )
      return false;
    reveal = true;
    dirty = true;
    return true;
  };

  const center = (l: Laid | null) => {
    const w = canvas.clientWidth;
    const hh = canvas.clientHeight;
    if (!l) return;
    tx = w / 2 - (l.x + W / 2) * s;
    ty = hh / 2 - (l.y + H / 2) * s;
    dirty = true;
  };

  const fit = () => {
    if (!L?.nodes.length) return;
    const xs = L.nodes.map((l) => l.x);
    const ys = L.nodes.map((l) => l.y);
    const [x0, x1, y0, y1] = [
      Math.min(...xs),
      Math.max(...xs) + W,
      Math.min(...ys),
      Math.max(...ys) + H,
    ];
    const w = canvas.clientWidth;
    const hh = canvas.clientHeight - 100;
    s = Math.min(1.2, Math.max(0.05, Math.min(w / (x1 - x0 + 80), hh / (y1 - y0 + 80))));
    tx = (w - (x1 - x0) * s) / 2 - x0 * s;
    ty = 60 + (hh - (y1 - y0) * s) / 2 - y0 * s;
    dirty = true;
  };

  const fetchMap = async () => {
    try {
      const m = await src.map(play);
      L = layout(m);
      count.textContent = `${m.nodes.length} beats`;
      if (!placed) {
        placed = true;
        center(L.current ?? (m.root ? (L.byId.get(m.root) ?? null) : null));
      }
      dirty = true;
    } catch (e) {
      toast((e as Error).message, "error");
    }
  };
  void fetchMap();
  let refetch = 0;
  onCleanup(
    src.subscribe(play, (m) => {
      if (m.type === "queue" || m.type === "error") return;
      clearTimeout(refetch);
      refetch = window.setTimeout(fetchMap, 500);
    }),
  );

  // Drawing.

  const curve = (p: Path2D, a: Laid, b: Laid) => {
    const x1 = a.x + W;
    const y1 = a.y + H / 2;
    const x2 = b.x;
    const y2 = b.y + H / 2;
    const dx = Math.max(40, Math.abs(x2 - x1) / 2);
    p.moveTo(x1, y1);
    p.bezierCurveTo(x1 + dx, y1, x2 - dx, y2, x2, y2);
  };

  const drawNode = (l: Laid, detail: boolean) => {
    const n = l.n;
    const isCur = L!.current === l;
    ctx.globalAlpha = n.pending ? 0.45 : n.seen ? 1 : 0.75;
    ctx.fillStyle = "#181b24";
    ctx.beginPath();
    ctx.roundRect(l.x, l.y, W, H, 8);
    ctx.fill();
    const hide = spoiler(n);
    if (detail && n.thumb) {
      const url = src.mediaUrl(n.thumb);
      const img = thumbs.get(url);
      if (img === undefined) wantThumb(url, !!n.video);
      else if (img) {
        ctx.save();
        ctx.clip();
        if (hide) ctx.filter = "blur(7px) brightness(0.7)";
        const w0 = img instanceof HTMLImageElement ? img.naturalWidth : img.width;
        const h0 = img instanceof HTMLImageElement ? img.naturalHeight : img.height;
        const r = Math.max(W / w0, H / h0);
        const iw = w0 * r;
        const ih = h0 * r;
        ctx.drawImage(img, l.x + (W - iw) / 2, l.y + (H - ih) / 2, iw, ih);
        ctx.restore();
      }
    }
    if (detail && s > 0.6 && !hide) {
      const text = n.label || n.excerpt;
      if (text) {
        ctx.fillStyle = "rgba(8,9,13,0.72)";
        ctx.fillRect(l.x, l.y + H - 20, W, 20);
        ctx.fillStyle = "#e9e5dc";
        ctx.font = "11px system-ui, sans-serif";
        ctx.textBaseline = "middle";
        ctx.fillText(clip(text, 22), l.x + 6, l.y + H - 10, W - 12);
      }
    }
    ctx.globalAlpha = 1;
    ctx.beginPath();
    ctx.roundRect(l.x, l.y, W, H, 8);
    ctx.setLineDash(n.pending ? [5, 4] : []);
    ctx.lineWidth = isCur ? 3.5 : n.on_path ? 2 : hover === l ? 2 : 1;
    ctx.strokeStyle = n.on_path
      ? GOLD
      : hover === l
        ? "#cfd6e4"
        : n.pending
          ? "#5b6170"
          : "#3a3f4d";
    ctx.stroke();
    ctx.setLineDash([]);
    if (detail && n.video) videoMark(ctx, l.x + 5, l.y + 5);
    if (n.ending && !hide) {
      ctx.beginPath();
      ctx.roundRect(l.x + 4, l.y + 4, W - 8, H - 8, 5);
      ctx.lineWidth = 1;
      ctx.strokeStyle = "rgba(232,194,122,0.55)";
      ctx.stroke();
      if (s > 0.4) {
        ctx.fillStyle = GOLD;
        ctx.font = "bold 9px system-ui, sans-serif";
        ctx.textBaseline = "top";
        ctx.fillText("END", l.x + W - 28, l.y + 8);
      }
    }
    if (isCur) {
      ctx.shadowColor = GOLD;
      ctx.shadowBlur = 18;
      ctx.beginPath();
      ctx.roundRect(l.x, l.y, W, H, 8);
      ctx.stroke();
      ctx.shadowBlur = 0;
    }
  };

  const draw = (now: number) => {
    const t0 = performance.now();
    const dpr = devicePixelRatio || 1;
    const w = canvas.clientWidth;
    const hh = canvas.clientHeight;
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(hh * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(hh * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = "#0a0b0f";
    ctx.fillRect(0, 0, w, hh);
    if (!L) return;
    ctx.setTransform(dpr * s, 0, 0, dpr * s, dpr * tx, dpr * ty);
    const x0 = -tx / s - W;
    const x1 = (w - tx) / s + W;
    const y0 = -ty / s - H;
    const y1 = (hh - ty) / s + H;
    const inView = (ax: number, ay: number, bx: number, by: number) =>
      Math.max(ax, bx) >= x0 &&
      Math.min(ax, bx) <= x1 &&
      Math.max(ay, by) >= y0 &&
      Math.min(ay, by) <= y1;

    const plain = new Path2D();
    const onPath = new Path2D();
    const rejoin = new Path2D();
    for (const ed of L.edges) {
      if (!inView(ed.a.x, ed.a.y, ed.b.x + W, ed.b.y + H)) continue;
      curve(ed.e.rejoin ? rejoin : ed.path ? onPath : plain, ed.a, ed.b);
    }
    ctx.lineWidth = 1.5;
    ctx.strokeStyle = "#353a48";
    ctx.stroke(plain);
    ctx.setLineDash([6, 6]);
    ctx.strokeStyle = "#7c8fb8";
    ctx.stroke(rejoin);
    ctx.setLineDash([]);
    ctx.lineWidth = 3;
    ctx.strokeStyle = GOLD;
    ctx.stroke(onPath);

    if (s > 0.8) {
      ctx.fillStyle = "#9aa3b5";
      ctx.font = "10px system-ui, sans-serif";
      ctx.textBaseline = "bottom";
      for (const ed of L.edges) {
        if (!ed.e.label || !inView(ed.a.x, ed.a.y, ed.b.x, ed.b.y)) continue;
        ctx.fillText(
          clip(ed.e.label, 26),
          ed.a.x + W + 8,
          ed.a.y + H / 2 + (ed.b.y - ed.a.y) * 0.2 - 3,
          COL - W - 10,
        );
      }
    }

    const detail = s > 0.28;
    for (const l of L.nodes) if (inView(l.x, l.y, l.x + W, l.y + H)) drawNode(l, detail);

    // Exits being written: an animated dashed stub.
    ctx.lineWidth = 2;
    ctx.strokeStyle = GOLD;
    ctx.setLineDash([4, 5]);
    ctx.lineDashOffset = -now / 40;
    for (const g of L.ghosts) {
      const { x, y } = g.from;
      if (!inView(x, y, x + COL + W, y + H)) continue;
      const gy = y + H / 2 + (g.exit - 0.5) * 18;
      ctx.beginPath();
      ctx.moveTo(x + W, y + H / 2);
      ctx.quadraticCurveTo(x + W + 30, gy, x + W + 52, gy);
      ctx.stroke();
      ctx.beginPath();
      ctx.globalAlpha = 0.5 + 0.5 * Math.sin(now / 300);
      ctx.arc(x + W + 58, gy, 5, 0, Math.PI * 2);
      ctx.fillStyle = GOLD;
      ctx.fill();
      ctx.globalAlpha = 1;
    }
    ctx.setLineDash([]);
    ctx.lineDashOffset = 0;
    canvas.dataset.drawMs = (performance.now() - t0).toFixed(1);
  };

  let raf = 0;
  const frame = (t: number) => {
    if (closed) return;
    if (dirty || (L?.ghosts.length ?? 0) > 0) {
      dirty = false;
      draw(t);
    }
    raf = requestAnimationFrame(frame);
  };
  raf = requestAnimationFrame(frame);
  const ro = new ResizeObserver(() => (dirty = true));
  ro.observe(canvas);
  onCleanup(() => {
    closed = true;
    cancelAnimationFrame(raf);
    ro.disconnect();
    onThumb = null;
    clearTimeout(refetch);
  });

  // Interaction.

  const hit = (px: number, py: number): Laid | null => {
    if (!L) return null;
    const wx = (px - tx) / s;
    const wy = (py - ty) / s;
    for (const l of L.nodes) if (wx >= l.x && wx <= l.x + W && wy >= l.y && wy <= l.y + H) return l;
    return null;
  };
  const zoomAt = (cx: number, cy: number, f: number) => {
    const ns = Math.min(3, Math.max(0.05, s * f));
    tx = cx - (cx - tx) * (ns / s);
    ty = cy - (cy - ty) * (ns / s);
    s = ns;
    dirty = true;
  };
  const local = (e: PointerEvent | WheelEvent) => {
    const r = canvas.getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  const showTip = (l: Laid | null, x: number, y: number) => {
    if (hover !== l) dirty = true;
    hover = l;
    canvas.style.cursor = l ? (clickable(l.n) ? "pointer" : "not-allowed") : "grab";
    if (!l) return void (tip.hidden = true);
    const n = l.n;
    if (spoiler(n)) {
      tip.replaceChildren(
        h("small", null, "Hidden so it isn't spoiled. Tap to show the whole map."),
      );
      tip.hidden = false;
      const r = canvas.getBoundingClientRect();
      tip.style.left = `${Math.min(x + 16, r.width - 280)}px`;
      tip.style.top = `${Math.min(y + 16, r.height - 120)}px`;
      return;
    }
    const state = [
      n.pending ? "written ahead, not reached yet" : n.seen ? "seen" : "not seen",
      n.ending ? "an ending" : "",
      n.on_path ? "on your path" : "",
    ].filter(Boolean);
    tip.replaceChildren(
      ...[
        n.label ? h("strong", null, n.label) : null,
        h("p", null, n.excerpt || "…"),
        h("small", null, state.join(", ")),
        clickable(n)
          ? null
          : h(
              "small",
              { class: "muted" },
              opts.seenJump
                ? "Not seen yet: play on to reach it (or use edit mode to jump anywhere)."
                : "In play mode you can go back along your path only. Use edit mode to jump anywhere.",
            ),
      ].filter((x) => x !== null),
    );
    tip.hidden = false;
    const r = canvas.getBoundingClientRect();
    tip.style.left = `${Math.min(x + 16, r.width - 280)}px`;
    tip.style.top = `${Math.min(y + 16, r.height - 120)}px`;
  };

  const pointers = new Map<number, { x: number; y: number }>();
  let down = { x: 0, y: 0 };
  let moved = false;
  let pinch: { d: number } | null = null;
  const dist = () => {
    const [a, b] = [...pointers.values()];
    return Math.hypot(a.x - b.x, a.y - b.y);
  };

  canvas.addEventListener("pointerdown", (e) => {
    canvas.setPointerCapture(e.pointerId);
    const p = local(e);
    pointers.set(e.pointerId, p);
    if (pointers.size === 1) {
      down = p;
      moved = false;
    } else if (pointers.size === 2) {
      pinch = { d: dist() };
      moved = true;
    }
  });
  canvas.addEventListener("pointermove", (e) => {
    const p = local(e);
    const prev = pointers.get(e.pointerId);
    if (!prev) return showTip(hit(p.x, p.y), p.x, p.y);
    pointers.set(e.pointerId, p);
    if (pointers.size === 1) {
      tx += p.x - prev.x;
      ty += p.y - prev.y;
      if (Math.hypot(p.x - down.x, p.y - down.y) > 5) moved = true;
      tip.hidden = true;
      dirty = true;
    } else if (pinch && pointers.size === 2) {
      const d = dist();
      const [a, b] = [...pointers.values()];
      zoomAt((a.x + b.x) / 2, (a.y + b.y) / 2, d / pinch.d);
      pinch.d = d;
    }
  });
  const up = (e: PointerEvent) => {
    pointers.delete(e.pointerId);
    if (pointers.size < 2) pinch = null;
  };
  canvas.addEventListener("pointerup", up);
  canvas.addEventListener("pointercancel", (e) => {
    up(e);
    moved = true;
  });
  // The tap itself: on click, so it's fully handled here and closing the map can't pass
  // it on to the screen underneath.
  canvas.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (moved) return;
    const r = canvas.getBoundingClientRect();
    const l = hit(e.clientX - r.left, e.clientY - r.top);
    if (!l) return;
    if (spoiler(l.n)) return void askReveal();
    if (!clickable(l.n)) return;
    try {
      await opts.pick(l.n);
      close();
    } catch (err) {
      toast((err as Error).message, "error");
    }
  });
  canvas.addEventListener("pointerleave", () => {
    if (!pointers.size) showTip(null, 0, 0);
  });
  canvas.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const p = local(e);
      zoomAt(p.x, p.y, Math.exp(-e.deltaY * (e.ctrlKey ? 0.01 : 0.0015)));
    },
    { passive: false },
  );

  const legend = (cls: string, label: string) =>
    h("span", { class: "legend-item" }, h("i", { class: `lg ${cls}` }), label);

  return h(
    "div",
    {
      class: "map",
      "data-mode": opts.editing ? "edit" : "play",
      // Clicks never reach the screen underneath.
      onClick: (e: Event) => e.stopPropagation(),
    },
    canvas,
    tip,
    h("button", { class: "icon close map-close", title: "Close (Esc)", onClick: close }, "×"),
    h(
      "header",
      { class: "map-head" },
      h("h2", null, "Story map"),
      count,
      h("span", { class: "grow" }),
      h(
        "div",
        { class: "legend" },
        legend("path", "your path"),
        legend("pending", "written ahead"),
        legend("rejoin", "rejoins"),
        legend("gen", "being written"),
      ),
      h("button", { onClick: fit, title: "Show the whole story" }, "Fit"),
      h(
        "button",
        {
          onClick: () => center(L?.current ?? null),
          title: "Back to where you are",
        },
        "Center",
      ),
      opts.editing
        ? null
        : h(
            "button",
            {
              class: "map-spoilers",
              title: "Show beats ahead and paths you didn't take",
              onClick: () => askReveal(),
            },
            "Show all",
          ),
    ),
    h(
      "p",
      { class: "map-hint" },
      opts.editing
        ? "Edit mode: click any beat to continue from there. Scroll or pinch to zoom, drag to move."
        : opts.seenJump
          ? "Tap any beat you've seen to go there. Blurred beats you haven't seen yet. Pinch to zoom, drag to move."
          : "Tap a beat on your path to go back there. Blurred beats are ahead or on paths you didn't take. Pinch to zoom, drag to move.",
    ),
  );
}

function clip(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}
