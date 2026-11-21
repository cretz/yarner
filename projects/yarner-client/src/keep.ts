// "Keep generating while I'm away": a block of hours (up to 12) in which the story is
// filled out in the background, started or stopped from the story home, Setup or the
// Queue. A block ends when its time is up (start another) or yarner restarts (the
// story's key is only ever in memory).

import type { StoryInfo } from "./api.ts";
import type { Editor } from "./source.ts";
import { h, onCleanup, signal, toast } from "./ui.ts";

/** Block lengths offered (hours); 12 at most. */
export const KEEP_HOURS = [1, 2, 4, 8, 12];

function left(secs: number): string {
  const m = Math.max(1, Math.round(secs / 60));
  return m < 60 ? `${m} min` : `${Math.floor(m / 60)} h ${String(m % 60).padStart(2, "0")} min`;
}

/** The control: what's running (time left, Stop), and a block to start. */
export function keepControl(edit: Editor, info: () => Promise<StoryInfo>): Node {
  // Seconds left when last asked, and when that was (it counts down here meanwhile).
  const until = signal<number | null>(null);
  const now = signal(Date.now());
  const hours = signal(KEEP_HOURS[KEEP_HOURS.length - 1]);
  const refresh = () =>
    info().then(
      (i) => until.set(i.keep_secs ? Date.now() + i.keep_secs * 1000 : null),
      () => {},
    );
  void refresh();
  const tick = window.setInterval(() => {
    now.set(Date.now());
    const u = until.peek();
    if (u !== null && Date.now() >= u) void refresh();
  }, 30_000);
  onCleanup(() => clearInterval(tick));
  const set = async (hrs: number) => {
    try {
      await edit.keep(hrs);
      toast(hrs ? `Generating in the background for ${hrs} h.` : "Background generation stopped.");
      await refresh();
    } catch (err) {
      toast((err as Error).message, "error");
    }
  };
  return h("div", { class: "keep" }, () => {
    const u = until();
    const running = u !== null && u > now();
    return [
      h(
        "span",
        { class: "keep-state" },
        running
          ? `Generating in the background: ${left((u - now()) / 1000)} left.`
          : "Keep generating while I'm away:",
      ),
      h(
        "select",
        {
          name: "keep-hours",
          onChange: (e: Event) => hours.set(Number((e.target as HTMLSelectElement).value)),
        },
        KEEP_HOURS.map((n) =>
          h("option", { value: n, selected: n === hours.peek() }, `${n} hour${n === 1 ? "" : "s"}`),
        ),
      ),
      h(
        "button",
        { name: "keep-start", onClick: () => void set(hours.peek()) },
        // Not just "Start": next to the story's own Start, that read as starting the story.
        running ? "Restart the clock" : "Generate in the background",
      ),
      running
        ? h("button", { name: "keep-stop", onClick: () => void set(0) }, "Stop generating")
        : null,
      running
        ? null
        : h(
            "small",
            { class: "muted" },
            "Fills the story out while you're away. Ends when the time is up or yarner restarts.",
          ),
    ];
  });
}
