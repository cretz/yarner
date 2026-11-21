// The story websocket, reconnecting with backoff. After a reconnect it reports a
// `resync` so listeners refetch whatever they show.

import type { ServerMsg } from "./api.ts";

export function connect(url: string, on: (m: ServerMsg) => void): () => void {
  let ws: WebSocket | null = null;
  let closed = false;
  let delay = 500;
  let timer = 0;
  let opened = false;

  const open = () => {
    if (closed) return;
    ws = new WebSocket(url);
    ws.onopen = () => {
      delay = 500;
      if (opened) on({ type: "resync" });
      opened = true;
    };
    ws.onmessage = (e) => {
      try {
        on(JSON.parse(String(e.data)) as ServerMsg);
      } catch {
        // Ignore malformed messages.
      }
    };
    ws.onclose = () => {
      ws = null;
      if (closed) return;
      timer = window.setTimeout(open, delay);
      delay = Math.min(delay * 2, 10_000);
    };
  };
  open();
  return () => {
    closed = true;
    clearTimeout(timer);
    ws?.close();
  };
}
