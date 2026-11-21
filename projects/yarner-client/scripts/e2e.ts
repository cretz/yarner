// End-to-end test: starts the real server (mock generator, instant) on a temp data dir and a
// free port, serves the built client, and drives a full session in headless Chromium.
//
//   pnpm test:e2e                 (builds the client first)
//   YARNER_BIN=path/to/yarner     (default: ../target/debug/yarner[.exe])
//   SHOTS=dir                     (save screenshots there)

import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer as createHttpServer } from "node:http";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { crc32, deflateSync } from "node:zlib";
import { chromium, type Page } from "playwright";

const here = dirname(fileURLToPath(import.meta.url));
const client = resolve(here, "..");
const exe = process.platform === "win32" ? "yarner.exe" : "yarner";
const bin = process.env.YARNER_BIN ?? resolve(client, "..", "target", "debug", exe);
const shots = process.env.SHOTS;
const PASS = "e2e passphrase";
const TITLE = "The Lighthouse";

/** A small solid-color PNG for the uploaded reference picture. */
function png(w: number, h: number, rgb: [number, number, number]): Buffer {
  const chunk = (type: string, body: Buffer) => {
    const len = Buffer.alloc(4);
    len.writeUInt32BE(body.length);
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(Buffer.concat([Buffer.from(type), body])));
    return Buffer.concat([len, Buffer.from(type), body, crc]);
  };
  const head = Buffer.alloc(13);
  head.writeUInt32BE(w, 0);
  head.writeUInt32BE(h, 4);
  head.set([8, 2, 0, 0, 0], 8);
  const row = Buffer.concat([
    Buffer.from([0]),
    Buffer.from(Array.from({ length: w }, () => rgb).flat()),
  ]);
  const raw = Buffer.concat(Array.from({ length: h }, () => row));
  return Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk("IHDR", head),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}
const PNG = png(64, 48, [180, 120, 60]);

function freePort(): Promise<number> {
  return new Promise((ok, fail) => {
    const s = createServer();
    s.once("error", fail);
    s.listen(0, "127.0.0.1", () => {
      const port = (s.address() as { port: number }).port;
      s.close(() => ok(port));
    });
  });
}

async function waitForServer(url: string, proc: ChildProcess) {
  for (let i = 0; i < 200; i++) {
    if (proc.exitCode !== null) throw new Error(`server exited with ${proc.exitCode}`);
    try {
      if ((await fetch(url)).ok) return;
    } catch {
      // Not up yet.
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("server didn't start");
}

let step = 0;
async function check(name: string, page: Page, f: () => Promise<void>) {
  step++;
  process.stdout.write(`${String(step).padStart(2)}. ${name} ... `);
  try {
    await f();
  } catch (e) {
    if (shots) await page.screenshot({ path: join(shots, "FAILED.png") });
    if (process.env.E2E_DEBUG)
      console.log(
        await page.evaluate(() => document.querySelector(".play")?.outerHTML.slice(0, 3000)),
      );
    console.log(
      await page
        .locator("#toasts")
        .textContent()
        .catch(() => ""),
    );
    throw e;
  }
  if (shots)
    await page.screenshot({
      path: join(shots, `${String(step).padStart(2, "0")}-${name.replace(/\W+/g, "-")}.png`),
    });
  console.log("ok");
}

/** Serve the built client as plain static files: the player with no yarner server. */
async function staticHost(): Promise<{ url: string; close: () => void }> {
  const types: Record<string, string> = {
    html: "text/html",
    js: "text/javascript",
    css: "text/css",
    wasm: "application/wasm",
  };
  const dist = join(client, "dist");
  const srv = createHttpServer((req, res) => {
    const path = decodeURIComponent((req.url ?? "/").split("?")[0]);
    const file = join(dist, path === "/" ? "index.html" : path);
    if (!file.startsWith(dist) || !existsSync(file) || (path.endsWith("/") && path !== "/")) {
      res.writeHead(404, { "content-type": "text/html" }).end("<h1>not found</h1>");
      return;
    }
    const ext = file.split(".").pop() ?? "";
    res.writeHead(200, {
      "content-type": types[ext] ?? "application/octet-stream",
    });
    res.end(readFileSync(file));
  });
  const port = await freePort();
  await new Promise<void>((ok) => srv.listen(port, "127.0.0.1", ok));
  return { url: `http://127.0.0.1:${port}/`, close: () => srv.close() };
}

const attr = async (page: Page, name: string) => (await page.getAttribute(".play", name)) ?? "";
const depth = async (page: Page) => Number(await attr(page, "data-depth"));

/** Click through lines until the beat changes (or choices appear). */
async function nextBeat(page: Page) {
  const before = await attr(page, "data-beat");
  for (let i = 0; i < 30; i++) {
    if (await page.locator(".choices").isVisible()) return "choices";
    if (await page.locator(".endcard").isVisible()) return "end";
    await page.locator(".dialog").click();
    await page.waitForTimeout(60);
    if ((await attr(page, "data-beat")) !== before) return "moved";
  }
  throw new Error("the story didn't move on");
}

async function main() {
  if (!existsSync(bin)) throw new Error(`no server binary at ${bin}: cargo build -p yarner-server`);
  if (!existsSync(join(client, "dist", "index.html"))) throw new Error("build the client first");
  if (shots) mkdirSync(shots, { recursive: true });
  const data = mkdtempSync(join(tmpdir(), "yarner-e2e-"));
  const port = await freePort();
  const base = `http://127.0.0.1:${port}`;
  const server = spawn(
    bin,
    [
      "--mock",
      "--mock-scale",
      "0",
      "--data",
      data,
      "--client",
      join(client, "dist"),
      "--bind",
      `127.0.0.1:${port}`,
      "--diag",
      join(data, "diag.log"),
    ],
    { stdio: ["ignore", "ignore", "pipe"] },
  );
  let log = "";
  server.stderr?.on("data", (d: Buffer) => (log += d.toString()));
  const browser = await chromium.launch({ headless: true });
  try {
    await waitForServer(base, server);
    const ctx = await browser.newContext({
      viewport: { width: 1280, height: 720 },
      acceptDownloads: true,
    });
    // Every clip the page starts playing (voices, sounds), by URL.
    await ctx.addInitScript(() => {
      const played: string[] = [];
      (globalThis as { __yarnerPlayed?: string[] }).__yarnerPlayed = played;
      const play = HTMLMediaElement.prototype.play;
      HTMLMediaElement.prototype.play = function (this: HTMLMediaElement) {
        if (this instanceof HTMLAudioElement) played.push(this.src);
        return play.call(this);
      };
    });
    const page = await ctx.newPage();
    if (process.env.E2E_DEBUG)
      page.on("response", async (r) => {
        if (/\/plays\/[^/]+(\/advance)?$/.test(r.url()))
          console.log(
            `[api] ${r.url().replace(/.*\/plays/, "plays")} ${(await r.text().catch(() => "")).slice(0, 4000)}`,
          );
      });
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(base);

    await check("the LoRA vault: set, kept by the server only, forgotten", page, async () => {
      // A password the vault refuses isn't taken.
      await page.click(".vault-open");
      await page.fill("input[name=vault-password]", "wrong pass");
      await page.click('.modal button:has-text("Use it")');
      await page.waitForSelector('.toast:has-text("doesn\'t open the vault")');
      await page.keyboard.press("Escape");
      await page.waitForSelector('.vault-open:has-text("LoRA vault…")');
      await page.click(".vault-open");
      await page.fill("input[name=vault-password]", "vault pass");
      await page.click('.modal button:has-text("Use it")');
      await page.waitForSelector('.vault-open:has-text("LoRA vault: on")');
      // The server holds it (memory only); the page keeps nothing.
      await page.reload();
      await page.waitForSelector('.vault-open:has-text("LoRA vault: on")');
      const stored = await page.evaluate(() =>
        JSON.stringify({ ...localStorage, ...sessionStorage }),
      );
      assert.ok(!stored.includes("vault pass"), stored);
      await page.click(".vault-open");
      await page.click('.modal button:has-text("Forget it")');
      await page.waitForSelector('.vault-open:has-text("LoRA vault…")');
    });

    await check("create story", page, async () => {
      await page.click('[data-tab="new"]');
      await page.fill("input[name=title]", TITLE);
      await page.fill("input[name=new-passphrase]", PASS);
      await page.fill("input[name=confirm]", PASS);
      await page.click('button:has-text("Create story")');
      await page.waitForSelector(".setup");
    });

    await check("setup prompts", page, async () => {
      await page.fill(
        "textarea[name=prompt-story]",
        "A lighthouse keeper finds a door in the rock.",
      );
      await page.locator("textarea[name=prompt-style]").focus();
      await page.waitForSelector(
        '.prompt-field:has(textarea[name=prompt-story]) .save-state:has-text("Saved")',
      );
      await page.fill("textarea[name=prompt-style]", "Painterly, moody, film grain");
      await page.locator("input[name=story-title]").focus();
      await page.waitForSelector(
        '.prompt-field:has(textarea[name=prompt-style]) .save-state:has-text("Saved")',
      );
    });

    await check("extra direction: add one, remove it, it stays gone", page, async () => {
      const dir = page.locator('.prompt-field:has(textarea[name^="prompt-d"])');
      await page.click('button:has-text("+ Add direction")');
      await dir.locator("textarea").fill("Keep chapters short.");
      await page.locator("input[name=story-title]").focus();
      await dir.locator('.save-state:has-text("Saved")').waitFor();
      await dir.locator('button:has-text("Remove")').click();
      await dir.waitFor({ state: "detached" });
      await page.reload();
      await page.waitForSelector("textarea[name=prompt-story]");
      assert.equal(await dir.count(), 0);
    });

    await check("LoRA adapters picked per model in Settings, kept", page, async () => {
      const picker = page.locator(".loras");
      await picker.locator("text=Open the LoRA vault").waitFor();
      // Opened (as from the stories screen): the vault's adapters for each model show.
      await page.evaluate(() =>
        fetch("/api/vault", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ password: "vault pass" }),
        }),
      );
      await page.reload();
      const first = page.locator(".lora-model").first();
      await first.locator('.lora:has-text("Ink wash") input[name=lora-on]').check();
      await first.locator('.lora:has-text("Ink wash") input[name=lora-weight]').fill("0.6");
      await first.locator('.lora:has-text("Ink wash") input[name=lora-weight]').blur();
      await page.click('button:has-text("Save settings")');
      await page.waitForSelector('.toast:has-text("Settings saved")');
      await page.reload();
      const again = page.locator(".lora-model").first();
      await again.locator('.lora:has-text("Ink wash") input[name=lora-on]:checked').waitFor();
      assert.equal(
        await again.locator('.lora:has-text("Ink wash") input[name=lora-weight]').inputValue(),
        "0.6",
      );
      assert.ok(
        !(await again.locator('.lora:has-text("Film grain") input[name=lora-on]').isChecked()),
      );
      await page.evaluate(() =>
        fetch("/api/vault", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ password: null }),
        }),
      );
    });

    await check("object with uploaded and painted pictures", page, async () => {
      await page.fill("input[name=new-object-name]", "Mara");
      await page.fill("textarea[name=new-object-description]", "The keeper. Tired, stubborn.");
      await page.click(".new-object button[type=submit]");
      const card = page.locator(".object-card", { hasText: "" }).first();
      await card.waitFor();
      await card.locator("input[name=ref-upload]").setInputFiles({
        name: "mara.png",
        mimeType: "image/png",
        buffer: PNG,
      });
      await card.locator(".ref img").first().waitFor();
      await card.locator("textarea[name=gen-prompt]").fill("portrait in oils");
      await card.locator('button:has-text("Paint")').click();
      await page.waitForFunction(
        () => document.querySelectorAll(".object-card .ref img").length === 2,
      );
      const loaded = await card
        .locator(".ref img")
        .first()
        .evaluate((i: HTMLImageElement) => i.complete && i.naturalWidth > 0);
      assert.ok(loaded, "uploaded reference picture should load");
      // A picture's "use for" is an edit like any other: Save lights up and keeps it.
      const save = card.locator('button:has-text("Save changes")');
      assert.ok(await save.isDisabled());
      await card.locator("textarea[name=ref-use]").first().fill("face only");
      await page.waitForFunction(
        () => !document.querySelector<HTMLButtonElement>(".object-card button.primary")?.disabled,
      );
      await save.click();
      await page.waitForFunction(
        () => document.querySelector<HTMLButtonElement>(".object-card button.primary")?.disabled,
      );
      await page.reload();
      await page.waitForFunction(() =>
        [...document.querySelectorAll<HTMLTextAreaElement>("textarea[name=ref-use]")].some(
          (t) => t.value === "face only",
        ),
      );
    });

    await check(
      "a talking object: its voice made in setup, a clip per delivery",
      page,
      async () => {
        const card = page.locator(".object-card").first();
        await card.locator("input[name=object-talks]").check();
        await card.locator(".voice").waitFor();
        assert.equal(await card.locator(".delivery").count(), 6, "the default deliveries");
        assert.ok(
          await card.locator("button[name=voice-make]").isDisabled(),
          "needs a description",
        );
        await card
          .locator("textarea[name=voice-description]")
          .fill("a low, tired woman's voice, slow and dry");
        await card.locator("button[name=voice-make]").click();
        await page.waitForFunction(
          () => document.querySelectorAll(".object-card .delivery audio.clip").length === 6,
        );
        // Made and saved together: nothing left to save, and it's there after a reload.
        await page.waitForFunction(
          () => document.querySelector<HTMLButtonElement>(".object-card button.primary")?.disabled,
        );
        // One made again alone: a new take of that one.
        const firstClip = await card.locator(".delivery audio.clip").first().getAttribute("src");
        await card.locator(".delivery").first().locator("button[name=delivery-make]").click();
        await page.waitForFunction(
          (old) => {
            const a = document.querySelector<HTMLAudioElement>(".object-card .delivery audio.clip");
            return !!a && !a.src.endsWith(old ?? "-");
          },
          firstClip?.split("/").pop() ?? "",
        );
        // A delivery of the author's own.
        await card.locator("button[name=delivery-add]").click();
        await card.locator(".delivery").nth(6).locator("input[name=delivery-name]").fill("wry");
        await card
          .locator(".delivery")
          .nth(6)
          .locator("input[name=delivery-how]")
          .fill("dry and amused, half a smile");
        await card.locator('button:has-text("Save changes")').click();
        await page.reload();
        await page.waitForFunction(
          () => document.querySelectorAll(".object-card .delivery").length === 7,
        );
        assert.ok(await page.locator("input[name=object-talks]").first().isChecked());
        assert.equal(await page.locator(".object-card .delivery audio.clip").count(), 6);
        // The clip is real audio, served from the story.
        const src = await page
          .locator(".object-card .delivery audio.clip")
          .first()
          .getAttribute("src");
        const r = await page.evaluate(async (u) => {
          const res = await fetch(u!);
          return {
            type: res.headers.get("content-type"),
            head: [...new Uint8Array(await res.arrayBuffer()).slice(0, 4)],
          };
        }, src);
        assert.equal(r.type, "audio/wav");
        assert.deepEqual(r.head, [82, 73, 70, 70], "RIFF");
        // Settings: the generator's voice and sound models, and who reads the narration.
        await page.waitForFunction(() =>
          [...document.querySelectorAll<HTMLOptionElement>("select[name=voice_model] option")].some(
            (o) => o.value === "qwen3-tts",
          ),
        );
        const narrators = await page.locator("select[name=narrator] option").allTextContents();
        assert.ok(narrators.includes("Mara"), narrators.join("|"));
        assert.ok(await page.locator("input[name=voices]").isChecked());
      },
    );

    await check("start playing", page, async () => {
      await page.click("button[name=start]");
      await page.waitForSelector(".play");
      await page.waitForFunction(() => document.querySelector(".play")?.getAttribute("data-beat"));
      await page.waitForSelector(".scene img.layer.on");
      assert.equal(await depth(page), 1);
    });

    let chose = "";
    await check("advance through beats and a choice", page, async () => {
      for (let i = 0; i < 20 && (!chose || (await depth(page)) < 4); i++) {
        if ((await nextBeat(page)) !== "choices") continue;
        const c = page.locator(".choice").nth(chose ? 0 : 1);
        if (!chose) {
          chose = (await c.locator(".label").textContent()) ?? "";
          await c.hover();
          await page.waitForTimeout(700);
        }
        const before = await attr(page, "data-beat");
        await c.click();
        await page.waitForFunction(
          (b) => document.querySelector(".play")?.getAttribute("data-beat") !== b,
          before,
        );
      }
      assert.ok(chose, "expected a choice");
      assert.ok((await depth(page)) >= 3, "should be a few beats in");
    });

    await check("her lines are spoken as they show", page, async () => {
      // Read on until a screen where she speaks (the mock picks speakers at random).
      for (let i = 0; i < 30; i++) {
        const played = await page.evaluate(
          () => (globalThis as { __yarnerPlayed?: string[] }).__yarnerPlayed ?? [],
        );
        if (played.some((u) => u.includes("/media/"))) break;
        if ((await nextBeat(page)) === "choices") {
          const before = await attr(page, "data-beat");
          await page.locator(".choice").first().click();
          await page.waitForFunction(
            (b) => document.querySelector(".play")?.getAttribute("data-beat") !== b,
            before,
          );
        }
      }
      const played = await page.evaluate(
        () => (globalThis as { __yarnerPlayed?: string[] }).__yarnerPlayed ?? [],
      );
      const clips = played.filter((u) => u.includes("/media/"));
      assert.ok(clips.length > 0, "no voice or sound was played");
      const types = await page.evaluate(
        async (us) =>
          Promise.all(us.map(async (u) => (await fetch(u)).headers.get("content-type"))),
        clips.slice(0, 5),
      );
      assert.ok(
        types.every((t) => t === "audio/wav"),
        types.join("|"),
      );
    });

    await check("back", page, async () => {
      const d = await depth(page);
      for (let i = 0; i < 10 && (await depth(page)) === d; i++) {
        await page.click('.quick button:has-text("Back")');
        await page.waitForTimeout(80);
      }
      assert.equal(await depth(page), d - 1);
      await nextBeat(page);
    });

    let savedBeat = "";
    await check("save", page, async () => {
      savedBeat = await attr(page, "data-beat");
      // A grid of slots: pick one; no name needed.
      await page.click('.quick button:has-text("Save")');
      await page.click('.save-slot[data-slot="2"] .save-open');
      await page.waitForSelector('.toast:has-text("Saved")');
      // Saving over a taken slot asks first.
      page.once("dialog", (d) => void d.accept());
      await page.click('.quick button:has-text("Save")');
      await page.click('.save-slot[data-slot="2"] .save-open');
      await page.waitForSelector('.toast:has-text("Saved")');
    });

    await check("load", page, async () => {
      const r = await nextBeat(page);
      if (r === "choices") await page.locator(".choice").first().click();
      await page.waitForFunction(
        (b) => document.querySelector(".play")?.getAttribute("data-beat") !== b,
        savedBeat,
      );
      await page.click('.quick button:has-text("Load")');
      await page.click('.save-slot[data-slot="2"] .save-open');
      await page.waitForFunction(
        (b) => document.querySelector(".play")?.getAttribute("data-beat") === b,
        savedBeat,
      );
    });

    await check("a save at a decision shows its buttons", page, async () => {
      for (let i = 0; i < 20; i++) {
        const r = await nextBeat(page);
        if (r === "choices") break;
        if (r === "end") throw new Error("ended before a decision");
      }
      const shown = await page.locator(".choice[data-exit] .label").allTextContents();
      await page.click('.quick button:has-text("Save")');
      await page.click('.save-slot[data-slot="3"] .save-open');
      await page.waitForSelector('.toast:has-text("Saved")');
      await page.click('.quick button:has-text("Load")');
      const slot = page.locator('.save-slot[data-slot="3"]');
      await slot.locator(".save-choice").first().waitFor();
      assert.deepEqual(await slot.locator(".save-choice").allTextContents(), shown);
      // Loading it comes back to the decision, buttons up.
      await slot.locator(".save-open").click();
      await page.locator(".choices").waitFor();
    });

    await check("history and rewind", page, async () => {
      await page.click('.quick button:has-text("History")');
      await page.waitForSelector(".history-entry");
      assert.ok(
        await page.locator(".chose", { hasText: `You chose: ${chose}` }).count(),
        "history shows the choice",
      );
      await page.locator(".history-entry").first().click();
      await page.waitForFunction(
        () => document.querySelector(".play")?.getAttribute("data-depth") === "1",
      );
    });

    await check("prefs persist through the API", page, async () => {
      await page.click('.quick button:has-text("Prefs")');
      await page.locator("input[name=skip_unseen]").check();
      await page.keyboard.press("Escape");
      const handle = await page.evaluate(() => sessionStorage.getItem("yarner.handle"));
      await page.waitForTimeout(200);
      const prefs = (await (await fetch(`${base}/api/s/${handle}/prefs`)).json()) as {
        skip_unseen?: boolean;
      };
      assert.equal(prefs.skip_unseen, true);
    });

    await check("skip stops at a choice; auto moves on; hide", page, async () => {
      if (await page.locator(".choices").isVisible()) await page.locator(".choice").first().click();
      await page.waitForTimeout(150);
      await page.click('.quick button:has-text("Skip")');
      await page.waitForSelector(".choices", { timeout: 15_000 });
      await page.waitForSelector(".status .chip", { state: "detached" });
      await page.locator(".choice").first().click();
      await page.waitForTimeout(150);
      const where = `${await attr(page, "data-beat")}/${await attr(page, "data-line")}`;
      await page.keyboard.press("a");
      await page.waitForFunction(
        (w) => {
          const p = document.querySelector(".play");
          return `${p?.getAttribute("data-beat")}/${p?.getAttribute("data-line")}` !== w;
        },
        where,
        { timeout: 15_000 },
      );
      await page.keyboard.press("a");
      await page.keyboard.press("h");
      await page.waitForSelector(".play.hide-ui");
      await page.mouse.click(640, 200);
      await page.waitForSelector(".play:not(.hide-ui)");
    });

    await check("map in play mode: rewind along the path", page, async () => {
      for (let i = 0; i < 3; i++) {
        const r = await nextBeat(page);
        if (r === "choices") await page.locator(".choice").first().click();
        await page.waitForTimeout(100);
      }
      await page.keyboard.press("m");
      await page.waitForSelector('.map[data-mode="play"]');
      await page.waitForFunction(() => {
        const c = document.querySelector(".map-canvas") as unknown as {
          yarnerNodes?: () => unknown[];
        };
        return (c?.yarnerNodes?.().length ?? 0) > 3;
      });
      type N = { id: string; pathIndex: number | null; x: number; y: number };
      const onScreenEarlier = async () => {
        const nodes = await page.evaluate(() =>
          (
            document.querySelector(".map-canvas") as unknown as {
              yarnerNodes: () => N[];
            }
          ).yarnerNodes(),
        );
        const last = Math.max(...nodes.map((x) => x.pathIndex ?? 0));
        // The latest earlier beat on the path that is visible, never the opening (the
        // next steps re-run where this lands: a re-run opening is a new one nobody has
        // played past, and the export then ends at it).
        return nodes
          .filter((n) => n.pathIndex !== null && n.pathIndex >= 1 && n.pathIndex < last)
          .filter((n) => n.x > 20 && n.x < 1260 && n.y > 80 && n.y < 680)
          .sort((a, b) => b.pathIndex! - a.pathIndex!)[0];
      };
      let target = await onScreenEarlier();
      if (!target) {
        await page.click('.map button:has-text("Fit")');
        await page.waitForTimeout(300);
        target = await onScreenEarlier();
      }
      assert.ok(target, "a beat on the path should be on screen");
      await page.mouse.click(target.x, target.y);
      await page.waitForSelector(".map", { state: "detached" });
      await page.waitForFunction(
        (id) => document.querySelector(".play")?.getAttribute("data-beat") === id,
        target.id,
      );
      assert.equal(await depth(page), target.pathIndex! + 1);
    });

    await check("edit mode: re-run", page, async () => {
      await page.keyboard.press("e");
      await page.waitForSelector(".edit-panel");
      // A full-screen dialog with tabs (portrait or landscape alike).
      const box = await page.locator(".edit-panel").boundingBox();
      assert.ok(box && box.x <= 0 && box.y <= 0 && box.width >= 1280 && box.height >= 720);
      await page.click('.edit-tabs [data-tab="setup"]');
      await page.waitForSelector('.edit-panel .card[data-prompt="story"]');
      // The queue tab says how writing has gone (pushed with the queue ticks).
      await page.click('.edit-tabs [data-tab="queue"]');
      await page.waitForSelector(".edit-panel [data-writing]");
      await page.click('.edit-tabs [data-tab="screen"]');
      const epoch = Number(await attr(page, "data-epoch"));
      await page.click('.edit-panel button:has-text("Rewrite this beat now")');
      await page.waitForFunction(
        (e) => Number(document.querySelector(".play")?.getAttribute("data-epoch")) > e,
        epoch,
      );
    });

    await check("edit mode: re-run with direction", page, async () => {
      const epoch = Number(await attr(page, "data-epoch"));
      await page.click('.edit-panel button:has-text("Rewrite it with a direction")');
      await page.fill("textarea[name=direction]", "Make it rain.");
      await page.click('.edit-panel .subform button:has-text("Rewrite this beat now")');
      await page.waitForFunction(
        (e) => Number(document.querySelector(".play")?.getAttribute("data-epoch")) > e,
        epoch,
      );
    });

    await check("edit mode: edit the story prompt from here", page, async () => {
      await page.click('.edit-tabs [data-tab="setup"]');
      const card = page.locator('.edit-panel .card[data-prompt="story"]');
      await card.locator('button:has-text("Edit")').click();
      await card.locator("textarea").fill("A lighthouse keeper finds a door. It is raining now.");
      await card.locator('button:has-text("Save")').click();
      await page.waitForFunction(() =>
        document
          .querySelector('.edit-panel .card[data-prompt="story"] .prompt-text')
          ?.textContent?.includes("raining"),
      );
      await card.locator('button:has-text("History")').click();
      await page.waitForFunction(
        () =>
          document.querySelectorAll('.edit-panel .card[data-prompt="story"] .timeline li').length >=
          2,
      );
    });

    await check("map in edit mode: jump anywhere", page, async () => {
      await page.keyboard.press("m");
      await page.waitForSelector('.map[data-mode="edit"]');
      await page.waitForFunction(() => {
        const c = document.querySelector(".map-canvas") as unknown as {
          yarnerNodes?: () => unknown[];
        };
        return (c?.yarnerNodes?.().length ?? 0) > 3;
      });
      const nodes = await page.evaluate(() =>
        (
          document.querySelector(".map-canvas") as unknown as {
            yarnerNodes: () => {
              id: string;
              pathIndex: number | null;
              x: number;
              y: number;
            }[];
          }
        ).yarnerNodes(),
      );
      const target = nodes.find(
        (n) => n.pathIndex === null && n.x > 0 && n.x < 1280 && n.y > 60 && n.y < 700,
      );
      assert.ok(target, "a beat off the path should be on screen");
      await page.mouse.click(target.x, target.y);
      await page.waitForSelector(".map", { state: "detached" });
      await page.waitForFunction(
        (id) => document.querySelector(".play")?.getAttribute("data-beat") === id,
        target.id,
      );
      await page.keyboard.press("e");
      await page.waitForSelector(".edit-panel", { state: "detached" });
    });

    let bundle = "";
    await check("export", page, async () => {
      await page.click('.quick button:has-text("Home")');
      await page.waitForSelector(".home-card .story-title");
      await page.click('.menu-item:has-text("Export")');
      await page.fill("input[name=export-passphrase]", "bundle passphrase");
      await page.fill("input[name=export-confirm]", "bundle passphrase");
      const [download] = await Promise.all([
        page.waitForEvent("download"),
        page.click('.export button:has-text("Export")'),
      ]);
      assert.equal(download.suggestedFilename(), `${TITLE}.yarner`);
      bundle = join(data, "export.yarner");
      await download.saveAs(bundle);
      await page.waitForSelector('.report:has-text("beats")');
    });

    await check("lock and import the bundle", page, async () => {
      await page.keyboard.press("Escape");
      await page.click('.menu-item:has-text("Lock")');
      await page.waitForSelector(".title-screen");
      await page.click('[data-tab="import"]');
      await page.setInputFiles("input[name=bundle]", bundle);
      await page.fill("input[name=bundle-passphrase]", "bundle passphrase");
      await page.fill("input[name=import-passphrase]", "another passphrase");
      await page.fill("input[name=import-confirm]", "another passphrase");
      await page.click('button:has-text("Import story")');
      await page.waitForSelector(".home-card .story-title");
      assert.equal(await page.textContent(".story-title"), TITLE);
    });

    await check("write your own choice at a decision", page, async () => {
      await page.click('.menu-item:has-text("Start")');
      await page.waitForSelector(".play .dialog");
      for (let i = 0; i < 40; i++) {
        const r = await nextBeat(page);
        if (r === "choices") break;
        assert.notEqual(r, "end", "reached an ending before any decision");
      }
      const before = await attr(page, "data-beat");
      await page.click(".choice.own-open");
      await page.fill("textarea[name=own-choice]", "I climb onto the roof and shout for help");
      await page.press("textarea[name=own-choice]", "Enter");
      // Taken at once: the box closes and the choice shows as yours.
      await page.waitForSelector('.choice.own:has-text("I climb onto the roof")', {
        timeout: 3000,
      });
      assert.equal(await page.locator("textarea[name=own-choice]").count(), 0);
      // It never moves on by itself: once its screen is ready, a tap takes it.
      const mine = page.locator(
        '.choice.own[data-state="ready"]:has-text("I climb onto the roof")',
      );
      await mine.waitFor({ timeout: 30000 }).catch(async (e) => {
        console.log(
          "own choice never got ready; choices:",
          await page.evaluate(() =>
            document.querySelector(".choices")?.outerHTML.replace(/<svg.*?<\/svg>/g, ""),
          ),
        );
        const diag = readFileSync(join(data, "diag.log"), "utf8").split("\n");
        console.log("server diag tail:\n" + diag.slice(-40).join("\n"));
        throw e;
      });
      await page.waitForTimeout(400);
      assert.equal(await attr(page, "data-beat"), before, "no auto-advance");
      await mine.click();
      await page.waitForFunction(
        (b) => document.querySelector(".play")?.getAttribute("data-beat") !== b,
        before,
        { timeout: 30000 },
      );
      // It's in the history as what I chose.
      await page.click('.quick button:has-text("History")');
      await page.waitForSelector('.history .chose:has-text("I climb onto the roof")');
      await page.keyboard.press("Escape");
      await page.click('.quick button:has-text("Home")');
      await page.waitForSelector(".home-card .story-title");
    });

    await check(
      "on a phone in portrait: dialogue at the bottom, choices never over it",
      page,
      async () => {
        await page.setViewportSize({ width: 390, height: 844 });
        try {
          page.once("dialog", (d) => void d.accept());
          await page.click('.menu-item:has-text("Start")');
          await page.waitForSelector(".play .dialog");
          for (let i = 0; i < 40; i++) {
            const r = await nextBeat(page);
            if (r === "choices") break;
            assert.notEqual(r, "end", "reached an ending before any decision");
          }
          // A long list of long choices (more than the storyteller ever offers).
          await page.evaluate(() => {
            const list = document.querySelector(".choices")!;
            const first = list.querySelector(".choice")!;
            for (let i = 0; i < 4; i++) {
              const c = first.cloneNode(true) as HTMLElement;
              c.querySelector(".label")!.textContent =
                "Climb down the rotten ladder into the flooded cellar and look for the ledger, whatever the cost";
              list.insertBefore(c, first);
            }
          });
          const geo = await page.evaluate(() => {
            const r = (s: string) => document.querySelector(s)!.getBoundingClientRect();
            return {
              dialog: r(".dialog"),
              choices: r(".choices"),
              h: innerHeight,
              w: innerWidth,
            };
          });
          assert.ok(
            geo.dialog.bottom > geo.h - 90,
            `dialogue at the bottom (${geo.dialog.bottom} of ${geo.h})`,
          );
          assert.ok(geo.choices.bottom <= geo.dialog.top + 1, "choices sit above the dialogue");
          assert.ok(geo.choices.top >= 0, "choices stay on screen (they scroll)");
          assert.ok(geo.dialog.right <= geo.w + 1 && geo.dialog.left >= -1, "no sideways overflow");
        } finally {
          await page.setViewportSize({ width: 1280, height: 720 });
        }
        await page.click('.quick button:has-text("Home")');
        await page.waitForSelector(".home-card .story-title");
      },
    );

    await check("the story list is home, and stays put on a refresh", page, async () => {
      await page.click('.home-card button:has-text("All stories")');
      await page.waitForSelector(".title-screen .story-pick");
      await page.reload();
      await page.waitForSelector(".title-screen .story-pick");
      assert.equal(await page.evaluate(() => location.hash), "#/");
      await page.click(".title-screen .story-pick");
      await page.waitForSelector(".home-card .story-title");
      await page.reload();
      await page.waitForSelector(".home-card .story-title");
    });

    await check("a locked story sends you back to the title screen", page, async () => {
      await page.evaluate(async () => {
        await fetch(`/api/s/${sessionStorage.getItem("yarner.handle")}/lock`, { method: "POST" });
      });
      page.once("dialog", (d) => void d.accept());
      await page.click('.menu-item:has-text("Start")');
      await page.waitForSelector('.title-screen .notice:has-text("locked")');
      assert.equal(await page.evaluate(() => sessionStorage.length), 0);
      await page.fill("input[name=passphrase]", "another passphrase");
      await page.click(".title-screen form button[type=submit]");
      // The list is home, even for one story.
      await page.click(".title-screen .story-pick");
      await page.waitForSelector(".home-card .story-title");
    });

    await check("leave mid-story and Continue where you were", page, async () => {
      await page.evaluate(() => ((globalThis as { __yarnerMoves?: string[] }).__yarnerMoves = []));
      // "Start", or "Start over" if an earlier step played as a reader.
      page.once("dialog", (d) => void d.accept());
      await page.click('.menu-item:text-is("Start"), .menu-item:text-is("Start over")');
      await page.waitForFunction(() => document.querySelector(".play")?.getAttribute("data-beat"));
      for (let i = 0; i < 3; i++) {
        const before = await attr(page, "data-beat");
        if ((await nextBeat(page)) === "choices") {
          await page.locator('.choice[data-state="ready"]').first().click();
          await page.waitForFunction(
            (b) => document.querySelector(".play")?.getAttribute("data-beat") !== b,
            before,
          );
        }
      }
      // Partway through a beat's lines (the click may move on to the next beat: settle).
      await page.locator(".dialog").click();
      await page.waitForTimeout(500);
      const beat = await attr(page, "data-beat");
      const line = await attr(page, "data-line");
      const was = `depth ${await attr(page, "data-depth")} next ${await attr(page, "data-next")}`;
      await page.click('.quick button:has-text("Home")');
      await page.waitForSelector('.menu-item:text-is("Start over")');
      await page.click('.menu-item:text-is("Continue")');
      await page.waitForFunction(() => document.querySelector(".play")?.getAttribute("data-beat"));
      const now = `depth ${await attr(page, "data-depth")} next ${await attr(page, "data-next")}`;
      if ((await attr(page, "data-beat")) !== beat)
        console.log(
          `moves: ${JSON.stringify(await page.evaluate(() => (globalThis as { __yarnerMoves?: string[] }).__yarnerMoves))} left ${beat}`,
        );
      assert.equal(await attr(page, "data-beat"), beat, `left at ${was}, back at ${now}`);
      assert.equal(await attr(page, "data-line"), line);
      await page.click('.quick button:has-text("Home")');
      await page.waitForSelector(".home-card .story-title");
    });

    await check("keep generating: start and stop a block from Setup", page, async () => {
      await page.click('.menu-item:has-text("Setup")');
      await page.selectOption('select[name="keep-hours"]', "2");
      await page.click('button[name="keep-start"]');
      await page.waitForSelector('.keep-state:has-text("left")');
      await page.click('button[name="keep-stop"]');
      await page.waitForSelector('.keep-state:has-text("Keep generating")');
      await page.click('button:has-text("Story home")');
      await page.waitForSelector(".home-card .story-title");
    });

    await check(
      "new story from this setup: same setup, nothing played, old one kept",
      page,
      async () => {
        await page.click('.menu-item:has-text("New story from this setup")');
        await page.fill("input[name=copy-title]", "Copied lighthouse");
        await page.fill("input[name=copy-passphrase]", "copied setup pass");
        await page.fill("input[name=copy-confirm]", "copied setup pass");
        await page.click('.modal button:has-text("Create story")');
        await page.waitForSelector(".setup");
        await page.waitForFunction(
          () =>
            document.querySelector<HTMLTextAreaElement>("textarea[name=prompt-story]")?.value ===
            "A lighthouse keeper finds a door in the rock.",
        );
        assert.equal(await page.inputValue("input[name=story-title]"), "Copied lighthouse");
        await page.waitForFunction(() =>
          [...document.querySelectorAll<HTMLTextAreaElement>("textarea[name=ref-use]")].some(
            (t) => t.value === "face only",
          ),
        );
        const loaded = await page
          .locator(".object-card .ref img")
          .first()
          .evaluate(
            (i: HTMLImageElement) =>
              new Promise<boolean>((ok) =>
                i.complete ? ok(i.naturalWidth > 0) : (i.onload = () => ok(i.naturalWidth > 0)),
              ),
          );
        assert.ok(loaded, "the copied reference picture loads");
        // Nothing played in it yet.
        assert.match(await page.locator('button[name="start"]').innerText(), /Start the story/);
        // Back to the original, which is as it was.
        await page.click('button:has-text("Story home")');
        await page.click('.menu-item:has-text("All stories")');
        await page.click(`.title-screen .story-pick:has-text("${TITLE}")`);
        await page.waitForSelector(".home-card .story-title");
        assert.equal(await page.locator(".home-card .story-title").innerText(), TITLE);
      },
    );

    await check("Load from the story home keeps the editing tools", page, async () => {
      await page.click('.menu-item:text-is("Load")');
      await page.click('.save-slot[data-slot="2"] .save-open');
      await page.waitForFunction(() => document.querySelector(".play")?.getAttribute("data-beat"));
      await page.locator('.quick button:has-text("Edit")').waitFor();
      await page.click('.quick button:has-text("Home")');
      await page.waitForSelector(".home-card .story-title");
    });

    await check("the telemetry log holds nothing of the story", page, async () => {
      const diag = readFileSync(join(data, "diag.log"), "utf8");
      assert.match(diag, /event="?beat/, "beats are logged");
      assert.match(diag, /event="?text/, "text timings are logged");
      // Story words: the title, the passphrase, the reader's own words, the mock's
      // lines, places and speakers, the style.
      for (const s of [
        TITLE,
        PASS,
        "I climb onto the roof",
        "eleven winters",
        "the tide does not knock",
        "old pier",
        "clifftop",
        "Mara",
        "watercolor",
      ])
        assert.ok(!diag.toLowerCase().includes(s.toLowerCase()), `diag log mentions "${s}"`);
    });

    await check("nothing kept in browser storage", page, async () => {
      const s = await page.evaluate(async () => ({
        local: localStorage.length,
        session: Object.keys(sessionStorage),
        dbs: (await indexedDB.databases()).length,
        caches: (await caches.keys()).length,
      }));
      // Only opaque handles: the open story's, and the story list's.
      const extra = s.session.filter(
        (k) => k !== "yarner.handle" && k !== "yarner.opened" && !k.startsWith("yarner.line."),
      );
      assert.deepEqual({ ...s, session: extra }, { local: 0, session: [], dbs: 0, caches: 0 });
    });

    // The read-only player: the same client on a static host, no server, the export file.
    const host = await staticHost();
    const player = await browser.newPage({
      viewport: { width: 1280, height: 720 },
    });
    const playerErrors: string[] = [];
    player.on("pageerror", (e) => playerErrors.push(e.message));
    player.on("console", (m) => process.env.DEBUG_PLAYER && console.log(`[player] ${m.text()}`));
    player.on(
      "requestfailed",
      (r) => process.env.DEBUG_PLAYER && console.log(`[player] failed ${r.url()}`),
    );
    try {
      await check("static player: only 'Play a file' without a server", player, async () => {
        await player.goto(host.url);
        await player.waitForSelector('[data-tab="file"].on');
        assert.equal(await player.locator(".tabs button").count(), 1);
      });

      await check("static player: wrong passphrase is refused", player, async () => {
        await player.setInputFiles("input[name=story-file]", bundle);
        await player.fill("input[name=file-passphrase]", "not the passphrase");
        await player.click("form button[type=submit]");
        await player.waitForSelector("#toasts .toast", { timeout: 60000 });
        const msg = await player.textContent("#toasts");
        assert.ok(msg?.includes("wrong passphrase"), `toast: ${msg}`);
      });

      await check("static player: plays the exported story", player, async () => {
        await player.fill("input[name=file-passphrase]", "bundle passphrase");
        await player.click("form button[type=submit]");
        await player.waitForSelector(".home-card .story-title");
        assert.equal(await player.textContent(".story-title"), TITLE);
        assert.equal(await player.locator('.menu-item:has-text("Setup")').count(), 0);
        await player.click('.menu-item:has-text("Start")');
        await player.waitForSelector(".play .dialog");
        let moved = 0;
        for (let i = 0; i < 40 && moved < 4; i++) {
          const r = await nextBeat(player);
          if (r === "moved") moved++;
          else if (r === "choices") {
            const open = player.locator(".choices button.choice:not([disabled])");
            if ((await open.count()) === 0) {
              // Past where anyone played before exporting: the copy says so.
              await player.waitForSelector('.choices-note:has-text("ends here")');
              break;
            }
            await open.first().click();
            moved++;
            await player.waitForTimeout(100);
          } else break;
        }
        assert.ok(moved >= 2, `moved ${moved}`);
        assert.equal(await player.locator('.quick button:has-text("Edit")').count(), 0);
        const img = await player.locator(".scene img").first().getAttribute("src");
        assert.ok(
          !img || img.startsWith("blob:") || img.startsWith("data:"),
          `scene from memory: ${img}`,
        );
      });

      await check("static player: nothing kept in browser storage", player, async () => {
        const st = await player.evaluate(async () => ({
          local: localStorage.length,
          session: sessionStorage.length,
          dbs: (await indexedDB.databases()).length,
          caches: (await caches.keys()).length,
        }));
        assert.deepEqual(st, { local: 0, session: 0, dbs: 0, caches: 0 });
      });
      assert.deepEqual(playerErrors, [], "no player page errors");
    } finally {
      await player.close();
      host.close();
    }

    // Slow generation: screens come whole (text and picture together).
    const slowData = mkdtempSync(join(tmpdir(), "yarner-e2e-slow-"));
    const slowPort = await freePort();
    const slow = spawn(
      bin,
      [
        "--mock",
        "--mock-scale",
        "0.2",
        "--data",
        slowData,
        "--client",
        join(client, "dist"),
        "--bind",
        `127.0.0.1:${slowPort}`,
      ],
      { stdio: ["ignore", "ignore", "ignore"] },
    );
    const sp = await browser.newPage({
      viewport: { width: 1280, height: 720 },
    });
    try {
      await waitForServer(`http://127.0.0.1:${slowPort}`, slow);
      await check("the next screen opens only once written and painted", sp, async () => {
        await sp.goto(`http://127.0.0.1:${slowPort}/`);
        await sp.click('[data-tab="new"]');
        await sp.fill("input[name=title]", "Slow");
        await sp.fill("input[name=new-passphrase]", PASS);
        await sp.fill("input[name=confirm]", PASS);
        await sp.click('button:has-text("Create story")');
        await sp.waitForSelector(".setup");
        await sp.click("button[name=start]");
        await sp.waitForSelector(".play");
        // Every screen comes whole: never a beat over a blank or unpainted picture, and
        // at the end of a beat the way on stays shut (saying why) until the next screen
        // is written and painted.
        await sp.waitForFunction(
          () => document.querySelector(".play")?.getAttribute("data-beat"),
          null,
          { timeout: 60000 },
        );
        const shown = () =>
          sp.evaluate(() => {
            const img = document.querySelector<HTMLImageElement>(".scene .layer.on, .scene img");
            return !!img && (img.complete || img instanceof HTMLVideoElement);
          });
        let sawWait = false;
        const deadline = Date.now() + 240_000;
        // Beats moved through; the move lands after the click's request returns.
        let last: string | null = null;
        for (let screens = 0; screens < 6; ) {
          assert.ok(Date.now() < deadline, `stuck after ${screens} screens`);
          if (await sp.locator(".endcard").isVisible()) break;
          await sp.waitForFunction(() => !!document.querySelector(".scene .layer"), null, {
            timeout: 10000,
          });
          assert.ok(await shown(), `screen ${screens} has its picture`);
          const beat = await sp.getAttribute(".play", "data-beat");
          if (last !== null && beat !== last) screens++;
          last = beat;
          if (await sp.locator(".choices").isVisible()) {
            const ready = sp.locator('.choice[data-state="ready"]');
            // A shut choice doesn't move the story (checked and clicked in one go, so
            // it can't turn ready in between).
            const clickedShut = await sp.evaluate(() => {
              const b = document.querySelector<HTMLElement>('.choice[aria-disabled="true"]');
              b?.click();
              return !!b;
            });
            // A choice's wait note ("painting…" and its ring) never covers its words, and
            // the list never covers the dialogue.
            const overlaps = await sp.evaluate(() => {
              const hit = (a: DOMRect, b: DOMRect) =>
                a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
              const bad: string[] = [];
              for (const c of document.querySelectorAll(".choice")) {
                const l = c.querySelector(".label")?.getBoundingClientRect();
                const n = c.querySelector(".wait-note")?.getBoundingClientRect();
                if (l && n && hit(l, n)) bad.push("note over a label");
              }
              const cs = document.querySelector(".choices")?.getBoundingClientRect();
              const d = document.querySelector(".dialog")?.getBoundingClientRect();
              if (cs && d && hit(cs, d)) bad.push("choices over the dialogue");
              return bad;
            });
            assert.deepEqual(overlaps, []);
            if (clickedShut) {
              sawWait = true;
              // Clicking a shut choice puts it first, but never moves on by itself: once
              // its screen is ready it takes another tap.
              const picked = Number(
                await sp.getAttribute(".choice.favored", "data-exit", {
                  timeout: 5000,
                }),
              );
              const one = sp.locator(`.choice[data-exit="${picked}"][data-state="ready"]`);
              await one.waitFor({ timeout: 90000 });
              await sp.waitForTimeout(400);
              assert.equal(await sp.getAttribute(".play", "data-beat"), beat, "no auto-advance");
              await one.click();
            } else {
              await ready.first().waitFor({ timeout: 90000 });
              await ready.first().click();
            }
          } else {
            await sp.click(".play", { position: { x: 640, y: 200 } });
            const state = await sp.getAttribute(".play", "data-next");
            if (state && state !== "ready") {
              const atEnd = await sp.locator(".next-wait").isVisible();
              if (atEnd) {
                sawWait = true;
                await sp.waitForTimeout(200);
                assert.equal(await sp.getAttribute(".play", "data-beat"), beat, "waits whole");
                await sp.waitForFunction(
                  () => document.querySelector(".play")?.getAttribute("data-next") === "ready",
                  null,
                  { timeout: 90000 },
                );
              }
            }
          }
          // A fast reader: clicks on at once, so it meets screens still being made.
          await sp.waitForTimeout(150);
        }
        assert.ok(sawWait, "the next screen was waited on at least once");
      });

      // The same story with no generator connected (no GPU): it plays as far as it's
      // written, Play and Play & edit both work, and nothing that needs writing hangs.
      slow.kill();
      await new Promise((r) => setTimeout(r, 1000));
      const offPort = await freePort();
      const off = spawn(
        bin,
        ["--data", slowData, "--client", join(client, "dist"), "--bind", `127.0.0.1:${offPort}`],
        { stdio: ["ignore", "ignore", "ignore"] },
      );
      try {
        await waitForServer(`http://127.0.0.1:${offPort}`, off);
        await check("no generator: Start and Play & edit, as far as it's written", sp, async () => {
          const deadline = Date.now() + 120_000;
          await sp.goto(`http://127.0.0.1:${offPort}/`);
          await sp.fill("input[name=passphrase]", PASS);
          await sp.click(".title-screen form button[type=submit]");
          // The list is home, even for one story.
          await sp.click(".title-screen .story-pick");
          await sp.waitForSelector(".home-card .story-title");
          assert.equal(await sp.locator('.menu-item:has-text("Play & edit")').count(), 1);
          // Play: the player, without the editing tools.
          await sp.click('.menu-item:has-text("Start")');
          await sp.waitForSelector(".play .dialog");
          await sp.waitForSelector(".scene .layer.on", { timeout: 10000 });
          assert.equal(await sp.locator('.quick button:has-text("Edit")').count(), 0);
          // Read on to where the writing stopped: it ends (or its choices are shut with
          // the reason), never a wait for writing that can't happen.
          let ended = false;
          while (!ended) {
            assert.ok(Date.now() < deadline, "never reached the end of what's written");
            if (await sp.locator(".endcard").isVisible()) break;
            const open = sp.locator(".choices button.choice:not([disabled])");
            if (await sp.locator(".choices").isVisible()) {
              if ((await open.count()) === 0) {
                const title = await sp.getAttribute(".choices button.choice", "title");
                assert.ok(title?.includes("no generator"), `shut choice says why: ${title}`);
                ended = true;
                break;
              }
              await open.first().click();
            } else {
              await sp.click(".play", { position: { x: 640, y: 200 } });
              assert.equal(await sp.locator(".next-wait").count(), 0, "no wait for writing");
            }
            await sp.waitForTimeout(80);
          }
          // Play & edit: the editing tools are there; writing is refused, not hung.
          // ("/" is the story list: the story is picked from it.)
          await sp.goto(`http://127.0.0.1:${offPort}/`);
          await sp.click(".title-screen .story-pick");
          await sp.waitForSelector(".home-card .story-title");
          await sp.click('.menu-item:has-text("Play & edit")');
          await sp.waitForSelector('.quick button:has-text("Edit")');
          const refused = await sp.evaluate(async () => {
            const h = sessionStorage.getItem("yarner.handle");
            const plays = await (await fetch(`/api/s/${h}/plays`)).json();
            const p = (plays.plays ?? plays)[0].id;
            const r = await fetch(`/api/s/${h}/plays/${p}/rerun`, {
              method: "POST",
              headers: { "content-type": "application/json" },
              body: "{}",
            });
            return [r.status, (await r.json()).error];
          });
          assert.equal(refused[0], 503, `rerun refused: ${refused}`);
        });

        await check("delete a story", sp, async () => {
          await sp.goto(`http://127.0.0.1:${offPort}/`);
          await sp.click(".title-screen .story-pick");
          await sp.waitForSelector(".home-card .story-title");
          await sp.click('.menu-item:has-text("Delete story")');
          // The wrong passphrase doesn't delete it.
          await sp.fill("input[name=delete-passphrase]", "not the passphrase");
          await sp.click('button:has-text("Delete for good")');
          await sp.waitForSelector('#toasts .toast:has-text("doesn\'t open this story")');
          await sp.fill("input[name=delete-passphrase]", PASS);
          await sp.click('button:has-text("Delete for good")');
          await sp.waitForSelector(".title-screen");
          // Gone: its passphrase opens nothing now.
          await sp.fill("input[name=passphrase]", PASS);
          await sp.click(".title-screen form button[type=submit]");
          await sp.waitForSelector('.title-screen :text("No stories open with that passphrase")');
        });

        await check("clear all stories (asks first, no passphrase)", sp, async () => {
          // One story to clear.
          await sp.click('[data-tab="new"]');
          await sp.fill("input[name=title]", "To clear");
          await sp.fill("input[name=new-passphrase]", "clear me please");
          await sp.fill("input[name=confirm]", "clear me please");
          await sp.click('button:has-text("Create story")');
          await sp.waitForSelector(".setup");
          await sp.evaluate(() => {
            sessionStorage.clear();
            location.hash = "#/";
          });
          await sp.reload();
          await sp.waitForSelector(".title-screen");
          // Saying no keeps it.
          sp.once("dialog", (d) => void d.dismiss());
          await sp.click("button.clear-all");
          await sp.fill("input[name=passphrase]", "clear me please");
          await sp.click(".title-screen form button[type=submit]");
          // The list is home, even for one story.
          await sp.click(".title-screen .story-pick");
          await sp.waitForSelector(".home-card .story-title");
          await sp.evaluate(() => {
            sessionStorage.clear();
            location.hash = "#/";
          });
          await sp.reload();
          await sp.waitForSelector(".title-screen");
          sp.once("dialog", (d) => void d.accept());
          // The page reloads once everything is gone (the title screen is already
          // showing, so waiting for it alone would race the clearing).
          await Promise.all([sp.waitForEvent("load"), sp.click("button.clear-all")]);
          await sp.waitForSelector(".title-screen");
          await sp.fill("input[name=passphrase]", "clear me please");
          await sp.click(".title-screen form button[type=submit]");
          await sp.waitForSelector('.title-screen :text("No stories open with that passphrase")');
        });
      } finally {
        off.kill();
      }
    } finally {
      await sp.close();
      slow.kill();
      rmSync(slowData, { recursive: true, force: true });
    }

    assert.deepEqual(errors, [], "no page errors");
    console.log(`\ne2e passed (${step} steps)`);
  } catch (e) {
    if (log) console.error(`\nserver log:\n${log.slice(-3000)}`);
    throw e;
  } finally {
    await browser.close();
    server.kill();
    await new Promise((r) => setTimeout(r, 300));
    rmSync(data, { recursive: true, force: true });
  }
}

main().catch((e: unknown) => {
  console.error(e);
  process.exit(1);
});
