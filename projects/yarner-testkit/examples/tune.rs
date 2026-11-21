//! Playability tuning: play a story with a human-paced reader against realistic (scaled)
//! generation times, across scheduler settings, and report stalls and waste. Each config
//! runs several seeds; numbers are averaged (max wait is the worst seen).
//!
//! `cargo run -p yarner-testkit --example tune --release -- [scale] [beats] [seeds] [cps]`

use std::time::Duration;

use yarner_engine::model::{STORY_PROMPT, Scope};
use yarner_engine::{Config, Engine};
use yarner_format::{Install, KdfParams};
use yarner_testkit::mock::{MockThinfer, Timing};
use yarner_testkit::playtest::{self, Picker, Player};

#[derive(Default)]
struct Sum {
    stall: f64,
    waits: usize,
    max_wait: u128,
    wait_ms: u128,
    late: usize,
    imgs: usize,
    reuse: usize,
    util: f64,
    pending: f64,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: f64| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    let measured = args.get(5).is_some_and(|a| a == "measured");
    // `text=<secs>`: measured timings, but a beat takes this long.
    let text_secs: Option<f64> = args
        .get(5)
        .and_then(|a| a.strip_prefix("text="))
        .and_then(|v| v.parse().ok());
    let (scale, beats, seeds, cps) = (
        arg(1, 0.01),
        arg(2, 40.0) as usize,
        arg(3, 3.0) as u64,
        arg(4, 20.0),
    );
    println!(
        "scale {scale}: beat ~4 s, image ~30 s, video ~200 s (times below are real-world), \
         {beats} beats x {seeds} seeds, reader {cps} chars/s"
    );
    println!(
        "{:<24} {:>7} {:>6} {:>8} {:>8} {:>6} {:>5} {:>6} {:>6} {:>8}",
        "config",
        "stall%",
        "waits",
        "maxwait",
        "avgwait",
        "late",
        "imgs",
        "reuse",
        "util%",
        "pending%"
    );
    let base = Config {
        read_chars_per_sec: cps / scale,
        read_secs_per_line: 0.4 * scale,
        away_secs: 90.0 * scale,
        // In real seconds, like everything else.
        preempt_secs: Config::default().preempt_secs.map(|s| s * scale),
        ..Config::default()
    };
    // (name, config, pictures/video can pause)
    let configs: Vec<(&str, Config, bool)> = vec![
        ("default (pause)", base.clone(), true),
        ("no pause (cancel 1s)", base.clone(), false),
        (
            "no pause, no cancel",
            Config {
                preempt_secs: None,
                ..base.clone()
            },
            false,
        ),
        (
            "pause, depth 1",
            Config {
                speculate_depth: 1,
                ..base.clone()
            },
            true,
        ),
    ];
    let to_real = |ms: f64| ms / scale / 1000.0;
    for (name, cfg, pause) in configs {
        let mut s = Sum::default();
        for seed in 0..seeds {
            let dir = tempfile::tempdir().unwrap();
            let inst = Install::open_with(dir.path(), KdfParams::insecure_fast()).unwrap();
            let story = inst.create_story("pw", b"{}").unwrap();
            let timing = match text_secs {
                Some(t) => Timing {
                    text: Duration::from_secs_f64(t * scale),
                    ..Timing::measured(scale)
                },
                None if measured => Timing::measured(scale),
                None => Timing::realistic(scale),
            };
            let mock = MockThinfer::new(timing);
            if !pause {
                mock.no_pause();
            }
            let e = Engine::open(story, mock.clone(), cfg.clone()).unwrap();
            let prompt = format!("A lighthouse mystery, take {seed}.");
            e.set_prompt(STORY_PROMPT, &prompt, None, Scope::FromHere)
                .unwrap();
            e.put_object(None, "Mira", "The keeper.", vec![], false, None, None)
                .unwrap();
            e.put_object(None, "Tomas", "A fisherman.", vec![], false, None, None)
                .unwrap();
            let play = e.new_play("p").unwrap();
            let player = Player {
                chars_per_sec: cps / scale,
                per_line: Duration::from_secs_f64(0.4 * scale),
                picker: Picker::Seeded(3 + seed),
                beats,
                max_wait: Duration::from_secs_f64(600.0 * scale),
            };
            let r = playtest::run(&e, &play, &player).await;
            let (total, pending) = e.with_graph(|g| {
                (
                    g.beats.len(),
                    g.beats.values().filter(|b| b.pending).count(),
                )
            });
            s.stall += r.stall_ratio();
            s.waits += r.waits;
            s.max_wait = s.max_wait.max(r.wait_max_ms);
            // `TUNE_WHY=<secs>`: what each wait at least that long was behind.
            if let Some(min) = std::env::var("TUNE_WHY")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
            {
                for w in r.wait_log.iter().filter(|w| to_real(w.ms as f64) >= min) {
                    println!(
                        "  {name} seed {seed}: beat {} waited {:.0}s{} behind {:?}",
                        w.beat,
                        to_real(w.ms as f64),
                        if w.after_choice {
                            " (after a choice)"
                        } else {
                            ""
                        },
                        w.running
                    );
                }
            }
            s.wait_ms += r.wait_total_ms;
            s.late += r.media_late;
            s.imgs += r.images_new;
            s.reuse += r.images_reused;
            s.util += mock.stats().utilization();
            s.pending += pending as f64 / total.max(1) as f64;
        }
        let n = seeds as f64;
        let avg_wait = if s.waits > 0 {
            to_real(s.wait_ms as f64) / s.waits as f64
        } else {
            0.0
        };
        println!(
            "{:<24} {:>6.1}% {:>6.1} {:>7.1}s {:>7.1}s {:>6.1} {:>5.1} {:>6.1} {:>5.0}% {:>7.0}%",
            name,
            s.stall / n * 100.0,
            s.waits as f64 / n,
            to_real(s.max_wait as f64),
            avg_wait,
            s.late as f64 / n,
            s.imgs as f64 / n,
            s.reuse as f64 / n,
            s.util / n * 100.0,
            s.pending / n * 100.0,
        );
    }
}
