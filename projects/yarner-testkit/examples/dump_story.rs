//! Print a story's played path from its encrypted store (no generation).
//!
//! `cargo run -p yarner-testkit --example dump_story -- <data-dir> <passphrase>`
//!
//! `MEDIA=<id> OUT=<file>` writes that one picture/video, decrypted, to `<file>` (to look
//! at it; delete it after). `RAW=1` prints each beat's raw reply.

use yarner_engine::model::{Graph, Next, Record};
use yarner_engine::plays::{PlayRecord, Plays};
use yarner_format::{Install, KdfParams};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let inst =
        Install::open_with(std::path::Path::new(&args[1]), KdfParams::insecure_fast()).unwrap();
    for story in inst.unlock(&args[2]).unwrap() {
        let mut g = Graph::default();
        for r in story.open_graph().unwrap().1 {
            if let Ok(rec) = serde_json::from_slice::<Record>(&r) {
                g.apply(rec);
            }
        }
        let mut plays = Plays::default();
        for r in story.open_plays().unwrap().1 {
            if let Ok(rec) = serde_json::from_slice::<PlayRecord>(&r) {
                plays.apply(rec);
            }
        }
        let pending = g.beats.values().filter(|b| b.pending).count();
        println!(
            "beats {} ({} pending), media {} ({} ready)",
            g.beats.len(),
            pending,
            g.media.len(),
            g.media.values().filter(|m| m.blob.is_some()).count()
        );
        if let (Some(id), Some(out)) = (std::env::var_os("MEDIA"), std::env::var_os("OUT")) {
            let id = id.to_string_lossy().to_string();
            let m = &g.media[&id];
            let bytes = story.read_blob(m.blob.as_ref().expect("not made")).unwrap();
            std::fs::write(&out, &bytes[..]).unwrap();
            println!("wrote {id} ({:?}) to {}", m.mime, out.to_string_lossy());
            return;
        }
        for m in g.media.values().filter(|m| m.beat.is_none()) {
            println!("setup media {} {:?} {:?}", m.id, m.status, m.caption);
        }
        for p in plays.plays.values() {
            println!("playthrough {} at depth {}", p.name, p.path.len());
            for (i, h) in p.path.iter().enumerate() {
                let b = &g.beats[&h.beat];
                if let Some(l) = &b.label {
                    println!("\n## {l}");
                }
                println!("[{i}] media {:?}", b.show);
                if let yarner_engine::model::Show::New { media } = &b.show
                    && let Some(m) = g.media.get(media)
                {
                    println!(
                        "    {:?} {:?} caption {:?} objects {:?} refs {:?} | prompt: {}",
                        m.kind,
                        m.status,
                        m.caption,
                        m.objects,
                        m.refs,
                        m.prompt.replace(char::from(10), " / ")
                    );
                }
                for l in &b.lines {
                    println!("  {}: {}", l.speaker.as_deref().unwrap_or("-"), l.text);
                }
                if let Next::Choices { choices } = &b.next {
                    for (j, c) in choices.iter().enumerate() {
                        let m = if h.exit == Some(j) { ">" } else { " " };
                        println!("  {m} {}", c.label);
                    }
                }
                if !b.state.set.is_empty() {
                    println!("  state: {:?}", b.state.set);
                }
                if std::env::var_os("RAW").is_some()
                    && let Some(r) = &b.reply
                {
                    println!("  raw ({} chars): {r}", r.len());
                }
            }
        }
    }
}
