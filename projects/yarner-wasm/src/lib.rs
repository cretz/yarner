//! The read-only player in the browser. An export bundle is opened in memory with its
//! passphrase; decrypted content never leaves this module except as the views and media
//! bytes the page asks for. Nothing is stored.
//!
//! Paths cross the boundary as JSON (`[{"beat": "...", "exit": 0}, ...]`), views as the
//! same JSON the server sends, so the web client renders both the same way.

use std::io::Cursor;

use serde_json::json;
use wasm_bindgen::prelude::*;
use yarner_engine::model::{Graph, Hop, Path, Record};
use yarner_engine::plays::{Play, Plays};
use yarner_engine::view;
use yarner_format::bundle::Bundle;

#[wasm_bindgen]
pub struct Player {
    bundle: Bundle<Cursor<Vec<u8>>>,
    graph: Graph,
    plays: Plays,
    title: String,
}

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn parse_path(json: &str) -> Result<Path, JsError> {
    if json.trim().is_empty() {
        return Ok(vec![]);
    }
    serde_json::from_str(json).map_err(err)
}

#[wasm_bindgen]
impl Player {
    /// Open a bundle. Fails with "wrong passphrase" on a bad passphrase.
    #[wasm_bindgen(constructor)]
    pub fn open(bytes: Vec<u8>, passphrase: &str) -> Result<Player, JsError> {
        let mut bundle = Bundle::open(Cursor::new(bytes), passphrase).map_err(|e| match e {
            yarner_format::Error::Decrypt => JsError::new("wrong passphrase"),
            e => err(e),
        })?;
        let meta = bundle.meta().map_err(err)?;
        let title = serde_json::from_slice::<serde_json::Value>(&meta)
            .ok()
            .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
            .unwrap_or_default();
        let mut graph = Graph::default();
        for r in bundle.records().map_err(err)? {
            if let Ok(rec) = serde_json::from_slice::<Record>(&r) {
                graph.apply(rec);
            }
        }
        Ok(Player {
            bundle,
            graph,
            plays: Plays::default(),
            title,
        })
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }

    /// The opening path, or empty if the story has no beats.
    pub fn start(&self) -> String {
        let path: Path = self
            .graph
            .root
            .iter()
            .map(|b| Hop {
                beat: b.clone(),
                exit: None,
            })
            .collect();
        serde_json::to_string(&path).expect("json")
    }

    /// The play view at `path` (same shape as the server's). Marks the beat seen.
    pub fn view(&mut self, path: &str) -> Result<String, JsError> {
        let path = parse_path(path)?;
        if let Some(h) = path.last() {
            self.plays.seen.insert(h.beat.clone());
        }
        let play = Play {
            id: "local".into(),
            name: self.title.clone(),
            path,
            at: 0,
            deleted: false,
        };
        let v = view::play_view(
            &self.graph,
            &self.plays,
            &play,
            None,
            &|_, _, _| false,
            &|_| false,
        );
        Ok(serde_json::to_string(&v).expect("json"))
    }

    /// Step through `exit`: `{"result": "moved" | "ended" | "unavailable", "path": [...]}`.
    /// A choice nobody took before export is unavailable.
    pub fn advance(&self, path: &str, exit: Option<u32>) -> Result<String, JsError> {
        let mut path = parse_path(path)?;
        let result = match path.last().and_then(|h| self.graph.beats.get(&h.beat)) {
            None => match &self.graph.root {
                Some(r) => {
                    path = vec![Hop {
                        beat: r.clone(),
                        exit: None,
                    }];
                    "moved"
                }
                None => "ended",
            },
            Some(b) if b.next.exits() == 0 => "ended",
            Some(b) => {
                let exit = exit.unwrap_or(0) as usize;
                match self.graph.follow(&b.id, exit, None) {
                    Some(c) if exit < b.next.exits() => {
                        let id = c.id.clone();
                        path.last_mut().expect("non-empty").exit = Some(exit);
                        path.push(Hop {
                            beat: id,
                            exit: None,
                        });
                        "moved"
                    }
                    _ => "unavailable",
                }
            }
        };
        Ok(json!({ "result": result, "path": path }).to_string())
    }

    /// The story map with `path` as the current one.
    pub fn map(&self, path: &str) -> Result<String, JsError> {
        let path = parse_path(path)?;
        let v = view::map_view(&self.graph, &self.plays, &path, &Default::default());
        Ok(serde_json::to_string(&v).expect("json"))
    }

    /// MIME type of a media item, or empty if unknown / not in the bundle.
    pub fn media_mime(&self, id: &str) -> String {
        self.graph
            .media
            .get(id)
            .and_then(|m| m.mime.clone())
            .unwrap_or_default()
    }

    /// Decrypted media bytes.
    pub fn media(&mut self, id: &str) -> Result<Vec<u8>, JsError> {
        let blob = self
            .graph
            .media
            .get(id)
            .and_then(|m| m.blob.clone())
            .ok_or_else(|| JsError::new("media not in bundle"))?;
        Ok(self.bundle.read_blob(&blob).map_err(err)?.to_vec())
    }
}
