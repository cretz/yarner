//! A long story on the instant mock: does each beat stay as cheap as the first? Reports,
//! per stretch of beats, wall time per beat, the context the storyteller sees, and the
//! story's size.
//!
//! `cargo run -p yarner-testkit --example long_play --release -- [beats]`

use std::time::{Duration, Instant};

use yarner_engine::model::{STORY_PROMPT, Scope};
use yarner_engine::{Config, Engine};
use yarner_format::{Install, KdfParams};
use yarner_testkit::mock::{MockThinfer, Timing};
use yarner_testkit::playtest::{self, Picker, Player};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let beats: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);
    let dir = tempfile::tempdir().unwrap();
    let inst = Install::open_with(dir.path(), KdfParams::insecure_fast()).unwrap();
    let story = inst.create_story("pw", b"{}").unwrap();
    let mock = MockThinfer::new(Timing::instant());
    let e = Engine::open(story, mock.clone(), Config::default()).unwrap();
    e.set_prompt(STORY_PROMPT, "A lighthouse mystery.", None, Scope::FromHere)
        .unwrap();
    e.put_object(None, "Mira", "The keeper.", vec![], false, None, None)
        .unwrap();
    let play = e.new_play("p").unwrap();
    let step = 25;
    println!(
        "{:>6} {:>9} {:>10} {:>10} {:>8} {:>8}",
        "beats", "ms/beat", "ctx avg", "ctx max", "graph", "media"
    );
    let mut done = 0;
    let mut seen = 0;
    while done < beats {
        let t0 = Instant::now();
        let r = playtest::run(
            &e,
            &play,
            &Player {
                chars_per_sec: 1e9,
                per_line: Duration::ZERO,
                picker: Picker::Seeded(done as u64 + 1),
                beats: step,
                max_wait: Duration::from_secs(60),
            },
        )
        .await;
        if r.gave_up {
            println!("gave up waiting at {done} beats");
            break;
        }
        done += r.beats;
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / r.beats.max(1) as f64;
        let s = mock.stats();
        let ctx = &s.context_chars[seen..];
        seen = s.context_chars.len();
        let avg = ctx.iter().sum::<usize>() / ctx.len().max(1);
        let max = ctx.iter().max().copied().unwrap_or(0);
        let (nodes, media) = e.with_graph(|g| (g.beats.len(), g.media.len()));
        println!("{done:>6} {ms:>9.1} {avg:>10} {max:>10} {nodes:>8} {media:>8}");
        if r.beats == 0 {
            println!("the story ended");
            break;
        }
    }
}
