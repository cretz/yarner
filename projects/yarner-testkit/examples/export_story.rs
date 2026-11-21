//! Export a story to a read-only bundle (for the static player) without any generator:
//! the engine opens against a backend that refuses every job, so nothing new is made.
//!
//! `cargo run -p yarner-testkit --example export_story -- <data-dir> <passphrase> <out.yarner> <bundle-passphrase>`

use std::sync::Arc;

use yarner_engine::{Config, Engine};
use yarner_format::{Install, KdfParams};
use yarner_testkit::mock::{MockThinfer, Timing};

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let inst = Install::open_with(std::path::Path::new(&a[1]), KdfParams::insecure_fast()).unwrap();
    let story = inst
        .unlock(&a[2])
        .unwrap()
        .pop()
        .expect("no story for that passphrase");
    let mock = MockThinfer::new(Timing::instant());
    mock.set_down(true);
    let backend: Arc<MockThinfer> = mock;
    let e = Engine::open(story, backend, Config::default()).unwrap();
    e.set_paused(true);
    let report = e.export_report();
    let file = std::fs::File::create(&a[3]).unwrap();
    e.export(&inst, &a[4], file).unwrap();
    println!(
        "wrote {}: {} beats ({} unseen dropped), {} choices nobody took, {} unfinished media left out",
        a[3],
        report.beats,
        report.pending_dropped,
        report.untaken.len(),
        report.media_unfinished
    );
}
