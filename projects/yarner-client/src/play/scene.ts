// The full-bleed scene: the picture (or video) on screen, crossfading when it changes.
// Until the new media is ready (and decoded), the previous one stays up.

import type { MediaView } from "../api.ts";
import { keepVideoFrame } from "../map.ts";
import { effect, h, signal } from "../ui.ts";

const FADE_MS = 700;

export function scene(
  url: (id: string) => string,
  media: () => MediaView | null,
  loop: () => boolean = () => false,
  /** Play a new video's own sound (else it's muted). */
  sound: () => boolean = () => false,
) {
  const painting = signal(false);
  /** The video on screen, if the screen is a video (for its controls). */
  const playing = signal<HTMLVideoElement | null>(null);
  const el = h("div", { class: "scene" });
  let shown: string | null = null;
  let loading: string | null = null;
  // Pictures decoded ahead (the next screen), so they appear with their text. Media is
  // served no-store, so the decoded element itself is kept and used.
  const prepared = new Map<string, HTMLImageElement>();

  /** Fetch and decode a picture before it's needed (bounded wait). */
  async function prepare(m: MediaView | null | undefined): Promise<void> {
    if (!m || m.status !== "ready" || m.id === shown || prepared.has(m.id)) return;
    if (m.kind === "video" || (m.mime ?? "").startsWith("video/")) return;
    const img = h("img", {
      class: "layer",
      alt: m.caption,
      decoding: "async",
    }) as HTMLImageElement;
    img.src = url(m.id);
    const ok = await Promise.race([
      img.decode().then(
        () => true,
        () => false,
      ),
      new Promise<boolean>((r) => setTimeout(() => r(false), 4000)),
    ]);
    if (ok) {
      prepared.set(m.id, img);
      // Keep a few at most.
      while (prepared.size > 4) prepared.delete(prepared.keys().next().value!);
    }
  }

  const swap = (m: MediaView) => {
    loading = m.id;
    const ready = prepared.get(m.id);
    prepared.delete(m.id);
    const video = m.kind === "video" || (m.mime ?? "").startsWith("video/");
    const node: HTMLElement =
      ready ??
      (video
        ? // Once, then it holds its last frame (the controls replay it), unless the
          // reader chose to loop videos.
          h("video", {
            class: "layer",
            muted: !sound(),
            autoplay: true,
            loop: loop(),
            playsinline: true,
          })
        : h("img", { class: "layer", alt: m.caption, decoding: "async" }));
    const done = () => {
      if (loading !== m.id) return;
      loading = null;
      shown = m.id;
      painting.set(false);
      playing.set(node instanceof HTMLVideoElement ? node : null);
      el.append(node);
      requestAnimationFrame(() => requestAnimationFrame(() => node.classList.add("on")));
      const old = [...el.children].filter((c) => c !== node);
      setTimeout(() => old.forEach((c) => c.remove()), FADE_MS + 50);
    };
    const failed = () => {
      if (loading === m.id) loading = null;
      painting.set(false);
    };
    if (node instanceof HTMLVideoElement) {
      // Muted unless its sound is wanted (autoplay with sound needs a tap first: the
      // reader has tapped to get here).
      node.muted = !sound();
      // Its last frame is its map tile (once played through; a looping one never "ends").
      let kept = false;
      const keep = () => {
        if (kept) return;
        kept = true;
        keepVideoFrame(url(m.id), node);
      };
      node.addEventListener("ended", keep);
      node.addEventListener("timeupdate", () => {
        if (node.loop && node.duration && node.duration - node.currentTime < 0.25) keep();
      });
      node.addEventListener("loadeddata", done, { once: true });
      node.addEventListener("error", failed, { once: true });
      node.src = url(m.id);
    } else if (ready) {
      done();
    } else {
      (node as HTMLImageElement).src = url(m.id);
      (node as HTMLImageElement).decode().then(done, failed);
    }
  };

  effect(() => {
    const m = media();
    if (!m || m.kind === "audio") return painting.set(false);
    if (m.id === shown || m.id === loading) return;
    if (m.status === "requested") return painting.set(true);
    if (m.status === "failed") return painting.set(false);
    painting.set(true);
    swap(m);
  });

  return { el, painting, prepare, video: playing };
}
