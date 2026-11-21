# yarner

Build interactive adventure stories, or just play them. Describe a world and the things in
it, and a language model on your own GPU tells the story: who speaks, what they say, which
choices you get, and the pictures and video you see. Steer it by adjusting prompts
anywhere in the story, rewind, re-roll a beat you don't like, or fork it into another
version.

Generation runs on [thinfer](https://github.com/cretz/thinfer). Everything yarner stores
is encrypted with a passphrase only you know.

Early days: it plays end to end on thinfer, but expect rough edges.

## Run it

Install [Rust](https://rustup.rs/) (with `rustup target add wasm32-unknown-unknown`),
[wasm-bindgen](https://github.com/wasm-bindgen/wasm-bindgen) 0.2.126 and
[Node](https://nodejs.org/) 24 with pnpm, then:

    cd projects/yarner-client && pnpm install && pnpm build:wasm && pnpm build && cd ..
    cargo run --release -p yarner-server -- --client yarner-client/dist       --thinfer-managed --thinfer-bin path/to/thinfer-serve

Open <http://127.0.0.1:7878>. `--thinfer-managed` starts yarner's own private
thinfer-serve (loopback only, its own token, config and directories under the data
directory), restarts it if it dies, and stops it on exit. To use a thinfer-serve you run
yourself, pass `--thinfer <url> --thinfer-token <token>` instead; yarner never starts or
stops that one. Add `--thinfer-keep` to leave the private thinfer-serve running when
yarner exits and take it over on the next start (when started the same way), so
restarting yarner doesn't reload models; it holds the GPU until ended.

LoRA adapters come from thinfer's encrypted adapter vault: add them in thinfer, and give
`--thinfer-vault-dir <folder>` to have yarner's private thinfer-serve use the same vault
(thinfer logs its folder at startup: `adapter vault vault_dir=...`). Open the vault with
its password on the stories screen (kept in memory only), then switch adapters on per
picture and video model, each with a weight, in a story's Settings. Only those are used.

Voices: mark an object "Talks" in setup and describe how it sounds, then make its voice:
a clip per delivery (natural, angry, whispering, ... change them, add your own), the same
words said each way, so they're one voice. Listen and make any again until it's right, or
upload your own clip of each. Every line it says is spoken in the voice of its delivery's
clip; with none made, from its description alone. Pick a talking object in Settings to
read the narration aloud (it's only a voice: the storyteller isn't told about it). The
storyteller can also ask for sound effects and background sounds, which are slow to make,
so they're rare. A video whose model makes sound says its own lines, in its speakers'
voices. Voices and sounds need thinfer's speech and sound models (qwen3-tts,
qwen3-tts-design, moss-sfx); without them stories are silent.

Other ways to run:

- `--mock`: placeholder pictures and a scripted storyteller, to try everything without a
  GPU.
- No generator flag at all: your stories play as far as they're written (choices nobody
  wrote are shut, with the reason); nothing new is made and the GPU isn't used.

The same built client is also the read-only player: put `yarner-client/dist` on any static
web host, open it, and pick "Play a file" with an exported story. No server or GPU needed.

Serving beyond this machine needs TLS: add `--bind <your LAN address>:7878 --self-signed`
(or `--tls-cert`/`--tls-key`) to play from a phone on the same network; it will warn about
the self-signed certificate once.

Options can live in a TOML file instead: `yarner --config yarner.toml`, with the option
names as keys and relative paths taken from the file's folder (anything also given on the
command line wins):

    data = "data"
    bind = "192.168.1.20:7878"
    self-signed = true
    client = "client"
    thinfer-managed = true
    thinfer-bin = "thinfer-serve"

`--diag <file>` appends content-free telemetry (ids, counts, kinds, timings, error classes;
never story text, prompts, names or error messages), for tuning and bug reports.

## How it works

- A story is a graph of beats the model writes. Every beat is kept, so going back and
  replaying is instant and the story can be exported with all its pictures and video.
- While you read, yarner generates ahead: the next beat, every choice you might pick, and
  the pictures they need. A screen only appears once its text and picture (or video) are
  both ready (and its voices and sounds, when it has them); until then the way on says it's
  writing, painting, filming or voicing. When you're
  waiting, what you need cuts in: a picture or video being made pauses and resumes after.
- Prompts and objects (characters, places, anything) are versioned. Change one "from here"
  or "everywhere"; beats made with an older version are marked, never silently redone.
- Playing keeps no hidden place: Start plays from the beginning, and saves (a grid of slots,
  like any game) are how you come back. Building is "Play & edit", which resumes where you
  were. Exports never include anyone's saves.
- An exported story plays in the browser with no server and no AI: choices nobody took
  before exporting are disabled.

## Encryption

Nothing is written unencrypted: stories, prompts, pictures, saves, logs. Each story has
its own passphrase. The passphrase unlocks a random key that encrypts the content, and
keys are kept out of swap. Forget the passphrase and the story is gone; there is no
recovery. The server keeps an unlocked story's key in memory only while you are working
with it, and locks it after an idle timeout (or later, if you ask it to keep generating
overnight).

## Layout

- `projects/yarner-format`: encrypted storage and export bundles.
- `projects/yarner-engine`: the story graph, what the model sees and returns, scheduling.
- `projects/yarner-server`: the `yarner` binary and its HTTP API.
- `projects/yarner-client`: the web client (plain TypeScript).
- `projects/yarner-wasm`: the read-only player for exported stories, in the browser.
- `projects/yarner-testkit`: a mock generator and a synthetic player for tests and tuning.
