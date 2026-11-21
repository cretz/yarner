# yarner-client

The web client for yarner: build and play interactive adventure stories. Plain TypeScript,
hand-written HTML and CSS, no UI framework. `src/ui.ts` is a tiny in-house reactive core
(signals, effects) and DOM builder that everything else is built on.

## Layout

| File | What |
| --- | --- |
| `src/ui.ts` | signals, effects, `h()` DOM builder, overlays (layers / modals), toasts |
| `src/api.ts` | API types (mirroring the Rust views) and typed wrappers for every endpoint |
| `src/source.ts` | `Source` / `Editor` interfaces: where a story comes from; edit features only when `edit` is set |
| `src/ws.ts` | story websocket with reconnect backoff |
| `src/main.ts` | app shell and hash router |
| `src/screens/title.ts` | unlock, create, import |
| `src/screens/home.ts` | story home: continue, new playthrough, load, setup, export, lock, keep generating |
| `src/screens/setup.ts` | story prompt, style, direction, objects with reference pictures, settings |
| `src/screens/export.ts` | export a passphrase-protected bundle |
| `src/play/play.ts` | the play screen: scene, dialog, choices, quick menu, skip / auto, keys |
| `src/play/scene.ts` | full-bleed picture or video with crossfades |
| `src/play/overlays.ts` | history, save, load, preferences |
| `src/play/edit.ts` | edit mode side panel |
| `src/map.ts` | canvas story map |

## Privacy

The browser keeps no story content: no localStorage, IndexedDB, Cache API or service worker.
The open story's handle lives in `sessionStorage` only; preferences are saved through the API;
media loads straight from API URLs, which the server sends with `Cache-Control: no-store`.

## Develop

Needs Node 24+ and pnpm 11.

```
pnpm install
pnpm dev          # Vite on :5173, proxies /api (and websockets) to http://127.0.0.1:7878
```

Run the server next to it, for example with the mock generator:

```
cd ..
cargo build -j 4 -p yarner-server
target/debug/yarner --mock --mock-scale 0.05 --data some-dir
```

## Build and check

```
pnpm typecheck
pnpm lint
pnpm fmt:check    # pnpm fmt to fix
pnpm build        # -> dist/, served by `yarner --client dist`
```

## End-to-end test

```
pnpm test:e2e
```

Builds the client, starts `../target/debug/yarner` (override with `YARNER_BIN`) with
`--mock --mock-scale 0` on a temporary data dir and a free port, and drives a whole session in
headless Chromium (Playwright): create a story, set it up (prompts, an object with an uploaded
and a painted picture), play through beats and a choice, back, save, load, history, skip / auto /
hide, the map in play and edit mode, edit-mode re-runs and prompt edits, export, import, and the
locked-story path. Set `SHOTS=dir` to save a screenshot after each step.

Playwright is pinned to 1.59.1; if its Chromium isn't installed yet, run
`pnpm exec playwright install chromium-headless-shell`.
