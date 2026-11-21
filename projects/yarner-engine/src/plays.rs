//! Playthroughs, saves and seen flags: game state over a story's content. Stored in the
//! story's plays log, never exported.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{BeatId, MediaId, Path};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Play {
    pub id: String,
    pub name: String,
    pub path: Path,
    pub at: u64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Save {
    pub id: String,
    pub name: String,
    pub play: String,
    pub path: Path,
    #[serde(default)]
    pub thumb: Option<MediaId>,
    /// The line on screen when saved (the beat's first in older saves), for the save screen.
    #[serde(default)]
    pub excerpt: String,
    /// Which line of the beat was on screen (loading comes back to it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Saved at a decision: the choices that were on screen, for the save screen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Its place on the save screen (saving to a taken slot replaces what was there).
    #[serde(default)]
    pub slot: Option<u32>,
    pub at: u64,
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PlayRecord {
    Play(Play),
    Save(Save),
    Seen { beats: Vec<BeatId> },
    Prefs { prefs: Value },
}

#[derive(Default, Debug)]
pub struct Plays {
    pub plays: HashMap<String, Play>,
    pub saves: HashMap<String, Save>,
    pub seen: HashSet<BeatId>,
    pub prefs: Value,
}

impl Plays {
    pub fn apply(&mut self, r: PlayRecord) {
        match r {
            PlayRecord::Play(p) => {
                if p.deleted {
                    self.plays.remove(&p.id);
                } else {
                    self.plays.insert(p.id.clone(), p);
                }
            }
            PlayRecord::Save(s) => {
                if s.deleted {
                    self.saves.remove(&s.id);
                } else {
                    self.saves.insert(s.id.clone(), s);
                }
            }
            PlayRecord::Seen { beats } => self.seen.extend(beats),
            PlayRecord::Prefs { prefs } => self.prefs = prefs,
        }
    }
}
