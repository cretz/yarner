//! A synthetic player: reads at a human pace, picks choices, and measures how often the
//! story made them wait.

use std::time::{Duration, Instant};

use serde::Serialize;
use yarner_engine::view::NextView;
use yarner_engine::{Advance, Engine};

#[derive(Clone, Debug)]
pub enum Picker {
    First,
    /// Deterministic pseudo-random from a seed.
    Seeded(u64),
}

#[derive(Clone, Debug)]
pub struct Player {
    /// Characters read per second (a human is ~15-25).
    pub chars_per_sec: f64,
    /// Fixed pause per line (clicking next).
    pub per_line: Duration,
    pub picker: Picker,
    pub beats: usize,
    /// Give up on one wait after this long.
    pub max_wait: Duration,
}

impl Default for Player {
    fn default() -> Self {
        Self {
            chars_per_sec: 20.0,
            per_line: Duration::from_millis(300),
            picker: Picker::Seeded(1),
            beats: 30,
            max_wait: Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub beats: usize,
    pub waits: usize,
    pub wait_total_ms: u128,
    pub wait_max_ms: u128,
    pub read_total_ms: u128,
    pub images_new: usize,
    pub images_reused: usize,
    pub screen_changes: usize,
    /// Beats shown while their own picture was still being made.
    pub media_late: usize,
    pub choices: usize,
    pub rejoins: usize,
    pub gave_up: bool,
    /// Every wait: where, why it might be, how long.
    pub wait_log: Vec<WaitNote>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WaitNote {
    /// Index of the beat waited for.
    pub beat: usize,
    pub after_choice: bool,
    pub ms: u128,
    /// The backend's line when the wait began, most urgent first: the running job marked
    /// `*`, each as kind@priority and what it's for.
    pub running: Vec<String>,
}

impl Report {
    /// Share of the session spent waiting.
    pub fn stall_ratio(&self) -> f64 {
        let total = self.wait_total_ms + self.read_total_ms;
        if total == 0 {
            0.0
        } else {
            self.wait_total_ms as f64 / total as f64
        }
    }
}

/// Play `player.beats` beats of `play`.
pub async fn run(engine: &Engine, play: &str, player: &Player) -> Report {
    let mut r = Report::default();
    let mut rng = match player.picker {
        Picker::Seeded(s) => s,
        Picker::First => 0,
    };
    let mut last_screen: Option<String> = None;
    engine.activate(play);
    let mut events = engine.subscribe();
    while r.beats < player.beats {
        let view = engine.play_view(play).expect("play");
        let exit = match view.beat.as_ref().map(|b| &b.next) {
            Some(NextView::End) => break,
            Some(NextView::Choices { choices }) => {
                r.choices += 1;
                Some(match player.picker {
                    Picker::First => 0,
                    Picker::Seeded(_) => {
                        rng = rng
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        ((rng >> 33) % choices.len() as u64) as usize
                    }
                })
            }
            _ => None,
        };
        let t0 = Instant::now();
        let mut waited = false;
        let mut running: Vec<String> = vec![];
        loop {
            match engine.advance(play, exit).expect("advance") {
                Advance::Moved => break,
                Advance::Ended => return r,
                Advance::Waiting => {
                    if !waited {
                        let mut line = engine.queue().await;
                        line.sort_by_key(|q| (!q.running, std::cmp::Reverse(q.priority)));
                        running = line
                            .into_iter()
                            .take(6)
                            .map(|q| {
                                format!(
                                    "{}{:?}@{} {}",
                                    if q.running { "*" } else { "" },
                                    q.kind.unwrap_or(yarner_engine::backend::JobKind::Text),
                                    q.priority,
                                    q.what.unwrap_or_default()
                                )
                            })
                            .collect();
                    }
                    waited = true;
                    if t0.elapsed() > player.max_wait {
                        r.gave_up = true;
                        return r;
                    }
                    let _ = tokio::time::timeout(Duration::from_millis(100), events.recv()).await;
                }
            }
        }
        if waited {
            let ms = t0.elapsed().as_millis();
            r.waits += 1;
            r.wait_total_ms += ms;
            r.wait_max_ms = r.wait_max_ms.max(ms);
            r.wait_log.push(WaitNote {
                beat: r.beats,
                after_choice: exit.is_some(),
                ms,
                running,
            });
        }
        r.beats += 1;
        let view = engine.play_view(play).expect("play");
        let b = view.beat.expect("moved onto a beat");
        if let Some(m) = &b.shows {
            if m.status != yarner_engine::model::MediaStatus::Ready {
                r.media_late += 1;
            }
            let fresh = engine.with_graph(|g| {
                g.beats
                    .get(&b.id)
                    .is_some_and(|x| matches!(x.show, yarner_engine::model::Show::New { .. }))
            });
            if fresh {
                r.images_new += 1;
            } else {
                r.images_reused += 1;
            }
        }
        let screen = b.screen.as_ref().map(|m| m.id.clone());
        if screen != last_screen {
            r.screen_changes += 1;
            last_screen = screen;
        }
        if engine.with_graph(|g| {
            g.beats
                .get(&b.id)
                .is_some_and(|x| matches!(x.next, yarner_engine::model::Next::Rejoin { .. }))
        }) {
            r.rejoins += 1;
        }
        let chars: usize = b.lines.iter().map(|l| l.text.len()).sum();
        let read = Duration::from_secs_f64(chars as f64 / player.chars_per_sec)
            + player.per_line * b.lines.len() as u32;
        r.read_total_ms += read.as_millis();
        tokio::time::sleep(read).await;
    }
    r
}
