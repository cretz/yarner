//! Export a story from the engine, then play it read-only through the browser player.

use std::time::Duration;

use serde_json::Value;
use yarner_engine::model::{STORY_PROMPT, Scope};
use yarner_engine::{Advance, Config, Engine};
use yarner_format::{Install, KdfParams};
use yarner_testkit::mock::{MockThinfer, Timing};
use yarner_wasm::Player;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exported_story_plays_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let inst = Install::open_with(dir.path(), KdfParams::insecure_fast()).unwrap();
    let story = inst.create_story("pw", br#"{"title":"Lamp"}"#).unwrap();
    let e = Engine::open(
        story,
        MockThinfer::new(Timing::instant()),
        Config::default(),
    )
    .unwrap();
    e.set_prompt(STORY_PROMPT, "A lighthouse mystery.", None, Scope::FromHere)
        .unwrap();
    e.put_object(None, "Mira", "The keeper.", vec![], false, None, None)
        .unwrap();
    let play = e.new_play("p").unwrap();
    let mut moved = 0;
    while moved < 12 {
        match e.advance(&play, Some(0)).unwrap_or(Advance::Waiting) {
            Advance::Moved => moved += 1,
            Advance::Ended => break,
            Advance::Waiting => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
    while !e.idle() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (cur, report) = e
        .export(&inst, "share it", std::io::Cursor::new(Vec::new()))
        .unwrap();
    assert!(report.beats >= 12);

    let mut p = Player::open(cur.into_inner(), "share it").unwrap();
    assert_eq!(p.title(), "Lamp");
    let mut path = p.start();
    let mut seen = 0;
    let mut images = 0;
    loop {
        let v: Value = serde_json::from_str(&p.view(&path).unwrap()).unwrap();
        let beat = &v["beat"];
        if let Some(id) = beat["shows"]["id"].as_str()
            && beat["shows"]["status"] == "ready"
        {
            assert!(!p.media(id).unwrap().is_empty());
            assert_eq!(p.media_mime(id), "image/png");
            images += 1;
        }
        seen += 1;
        // Always the first exit: the one the player took, so it must be available.
        let r: Value = serde_json::from_str(&p.advance(&path, Some(0)).unwrap()).unwrap();
        match r["result"].as_str().unwrap() {
            "moved" => path = r["path"].to_string(),
            "unavailable" => {
                assert!(seen >= 12, "taken exits must be in the export ({seen})");
                break;
            }
            _ => break,
        }
        if seen > 200 {
            break;
        }
    }
    assert!(seen >= 12, "{seen}");
    assert!(images > 0);
    // Untaken choices are unavailable in a read-only export.
    let map: Value = serde_json::from_str(&p.map(&path).unwrap()).unwrap();
    assert!(
        map["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["pending"] == false)
    );
}
