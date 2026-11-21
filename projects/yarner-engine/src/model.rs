//! The story graph: records as stored, and the in-memory graph they fold into.
//!
//! Everything is a record appended to the story's graph log, and the graph is the fold of
//! those records. Later records with the same id replace earlier ones, so replaying a
//! record twice changes nothing.
//!
//! - Beats form a DAG. A beat's exits (one per choice, or a single "continue") each lead to
//!   one or more versions of the next beat. Forking adds a version; rejoining points an exit
//!   at an existing beat.
//! - Overwriting a beat bumps its epoch: exits from older epochs are ignored.
//! - Prompts, objects and settings are revisions anchored at a beat (or the root) and apply
//!   to paths through that beat.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub type BeatId = String;
pub type MediaId = String;
pub type RevId = String;

pub fn new_id(prefix: &str) -> String {
    let b: [u8; 8] = yarner_format::crypto::random_array();
    format!("{prefix}{}", hex::encode(b))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where a revision applies from. `None` is the root: everywhere.
pub type Anchor = Option<BeatId>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Only when (re)generating the anchor beat itself.
    Here,
    /// The anchor and everything after it.
    FromHere,
    /// Only the beat right after the anchor (whichever way the story goes from it).
    Next,
}

/// Well-known prompt ids.
pub const STORY_PROMPT: &str = "story";
pub const STYLE_PROMPT: &str = "style";
/// The author's guidance on when the story changes pictures and uses video.
pub const MEDIA_PROMPT: &str = "media";

/// The author's steer from play ("where the story goes from here on").
pub const STEER_PROMPT: &str = "steer";
/// The author's direction for pictures and video from play ("she looks at the viewer"),
/// added word for word to every picture and video prompt from its beat on.
pub const PICTURE_PROMPT: &str = "pictures";

/// Used when the author hasn't written their own [`MEDIA_PROMPT`].
pub const DEFAULT_MEDIA_GUIDE: &str = "Pictures carry this story: change the picture whenever the place, the moment, the framing or who is in view changes, usually every 2 or 3 beats. Show an earlier picture again whenever the place, who is in it and the framing are unchanged; make a new one only when something visible changes. In conversations, cut between shots of each speaker: reuse an earlier shot that fits, or make a new one when none does, or when an expression changes. Show the moment, not a posed portrait: anyone in the shot is caught in the middle of this beat, with a clear expression, where they look, their body language, and what they are doing and with whom. The viewer sees through the player's eyes: whoever speaks to the player looks straight at the viewer. Write every picture and video prompt in rich, concrete detail, since the picture model knows nothing of the story: who or what is in it and exactly how they look, their pose, expression and action, the place and its details, time of day and light, the mood, and the framing (close-up, medium or wide shot, angle). Every new picture must look clearly different from the recent ones, never a near copy: change the camera angle and distance (close-up, over the shoulder, low or high angle, wide), the facial expression, the pose and gesture, who is in focus, or the light. Keep anyone's look the same from picture to picture (clothes, hair, what they carry, any visible state) until the story changes it, and describe it every time. Use a video only for the biggest moments, whose motion a still can't carry: something breaking, arriving, transforming, a chase, a big reveal; describe what moves and how, from start to end. Videos are very rare: at most one every 15 or 20 beats, many scenes have none, never for people just talking; when in doubt, use a picture.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefImage {
    pub media: MediaId,
    #[serde(default)]
    pub caption: String,
    /// What a picture drawn from it takes from it, in the author's words ("face only");
    /// empty: the model's call.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub use_for: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjectRev {
    pub id: String,
    pub rev: RevId,
    pub anchor: Anchor,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub refs: Vec<RefImage>,
    /// It speaks in the story (the author marks it so): it gets a voice section in setup.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub talks: bool,
    /// How it sounds and the clips it speaks from (`talks` only).
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub removed: bool,
    pub at: u64,
}

/// A voice: how it sounds, in words, and a clip for each way it speaks. A line is spoken
/// in the voice of the clip of its delivery (the speech model copies a clip's voice and
/// its delivery, not just its timbre: one clip per mood keeps the voice and changes the
/// mood). With no clip made yet, lines are spoken from the description alone (close,
/// but not quite the same voice line to line).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Voice {
    /// How it sounds: "a warm, low woman's voice, a slight rasp, unhurried".
    #[serde(default)]
    pub description: String,
    /// What every made clip says (the same words in each delivery, so the clips differ
    /// only in how they're said).
    #[serde(default)]
    pub words: String,
    /// The ways it speaks, its usual one first.
    #[serde(default)]
    pub deliveries: Vec<Delivery>,
}

/// One way a voice speaks, and its clip.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// A short name the storyteller picks it by ("angry").
    pub name: String,
    /// How it's said, in words ("angry, sharp and raised").
    #[serde(default)]
    pub how: String,
    /// The clip (audio media: made from the description, or uploaded).
    #[serde(default)]
    pub clip: Option<MediaId>,
    /// What the clip says (its transcript: with it the speech model carries the clip's
    /// delivery over too).
    #[serde(default)]
    pub words: String,
}

impl Voice {
    /// The delivery a line asks for by name, else the usual one.
    pub fn delivery(&self, name: Option<&str>) -> Option<&Delivery> {
        let want = name.map(|n| n.trim().to_lowercase());
        want.and_then(|w| {
            self.deliveries
                .iter()
                .find(|d| d.name.trim().to_lowercase() == w)
        })
        .or_else(|| self.deliveries.first())
    }
}

/// The deliveries a new voice starts with (the author changes, adds or removes any).
pub const DEFAULT_DELIVERIES: [(&str, &str); 6] = [
    ("natural", "speaking naturally, calm and even"),
    ("happy", "happy and warm, smiling as they speak"),
    ("sad", "sad and subdued, close to tears"),
    ("angry", "angry, sharp and raised"),
    ("scared", "frightened, tense and breathless"),
    ("whispering", "whispering softly, close and secret"),
];

/// What a new voice's clips say: about six seconds, a little of everything.
pub const DEFAULT_VOICE_WORDS: &str = "I wasn't sure you'd come. Well, you're here now, so listen closely, because I'll only say this once.";

/// A field an older build wrote in another shape (an object's `voice` was once a
/// string): read as unset rather than dropping the whole record.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

impl ObjectRev {
    /// Its voice's clips.
    pub fn clips(&self) -> impl Iterator<Item = &MediaId> {
        self.voice
            .iter()
            .flat_map(|v| v.deliveries.iter().filter_map(|d| d.clip.as_ref()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptRev {
    pub id: String,
    pub rev: RevId,
    pub anchor: Anchor,
    pub scope: Scope,
    pub text: String,
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub text_model: String,
    pub image_model: String,
    pub image_edit_model: String,
    pub video_model: String,
    pub video_ref_model: String,
    pub width: u32,
    pub height: u32,
    /// Allow the LLM to ask for video.
    pub video: bool,
    /// Let the text model reason before each beat (thinking mode). Slower; off by default.
    #[serde(default)]
    pub think: bool,
    /// Beats that must pass after a new picture before the next one (0 = no limit).
    #[serde(default = "default_picture_gap")]
    pub picture_gap: u32,
    /// Most beats one picture stays on screen before it must change (0 = no limit).
    #[serde(default = "default_picture_max")]
    pub picture_max: u32,
    /// Most beats in a row without a choice before one must be offered (0 = never
    /// forced).
    #[serde(default = "default_choice_max")]
    pub choice_max: u32,
    /// The story can't end before this many beats (0 = any time).
    #[serde(default = "default_min_beats")]
    pub min_beats: u32,
    /// How the story is framed: its pictures' shape, and how it's played on a phone.
    #[serde(default)]
    pub orientation: Orientation,
    /// At a decision, the player may write their own choice instead of picking one.
    #[serde(default = "yes")]
    pub own_choices: bool,
    /// LoRA adapters switched on, by picture/video model id (vault entries; only these
    /// are used, each at its weight). Needs the vault password set to take effect.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub loras: std::collections::BTreeMap<String, Vec<Lora>>,
    /// Videos' own size; unset = the pictures' size (see `size_for`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_size: Option<(u32, u32)>,
    /// Speak the lines of talking characters aloud.
    #[serde(default = "yes")]
    pub voices: bool,
    /// Sound effects and background sounds the storyteller asks for.
    #[serde(default = "yes")]
    pub sounds: bool,
    /// The talking object whose voice reads the narration (`None`: narration is silent).
    /// That object is only a voice: the storyteller isn't told about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrator: Option<String>,
    /// Speech in a clip's voice.
    #[serde(default = "default_voice_model")]
    pub voice_model: String,
    /// Speech from a description (voice design): makes the clips, and speaks for a voice
    /// with none made.
    #[serde(default = "default_voice_design_model")]
    pub voice_design_model: String,
    #[serde(default = "default_sound_model")]
    pub sound_model: String,
}

fn default_voice_model() -> String {
    "qwen3-tts".into()
}

fn default_voice_design_model() -> String {
    "qwen3-tts-design".into()
}

fn default_sound_model() -> String {
    "moss-sfx".into()
}

impl Settings {
    /// The size a new picture or video is made at: the story's, unless videos have
    /// their own.
    pub fn size_for(&self, kind: MediaKind) -> (u32, u32) {
        match (kind, self.video_size) {
            (MediaKind::Video, Some(s)) => s,
            _ => (self.width, self.height),
        }
    }
}

/// A LoRA adapter to blend in: a vault entry, at a weight.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lora {
    pub id: String,
    pub weight: f64,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    Landscape,
    Portrait,
}

fn default_picture_gap() -> u32 {
    0
}

/// A safety net, not a pace (that's the author's guidance): 2026-11-21 alpha, left to
/// it, the model kept one picture for 10+ beats. The author can set it, 0 = never forced.
fn default_picture_max() -> u32 {
    8
}

/// A safety net, not a pace (that's the prompt's "one beat in three or four"): 2026-11-24
/// alpha, left to it, the model offered choices on 4 of 63 beats, once after 24 in a row.
fn default_choice_max() -> u32 {
    6
}

/// 2026-11-21 alpha: stories ended far too soon.
fn default_min_beats() -> u32 {
    40
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            text_model: "gemma-4-26b-a4b".into(),
            // Both take reference images (characters stay themselves picture to picture).
            image_model: "qwen-image-2.1-turbo".into(),
            image_edit_model: "qwen-image-2.1-turbo".into(),
            video_model: "fasth3-trim".into(),
            video_ref_model: "fasth3-omni".into(),
            // 480p: the floor for the video models (thinfer's presets); multiples of 16.
            width: 832,
            height: 480,
            video: true,
            think: false,
            picture_gap: default_picture_gap(),
            picture_max: default_picture_max(),
            choice_max: default_choice_max(),
            min_beats: default_min_beats(),
            orientation: Orientation::Landscape,
            own_choices: true,
            loras: Default::default(),
            video_size: None,
            voices: true,
            sounds: true,
            narrator: None,
            voice_model: default_voice_model(),
            voice_design_model: default_voice_design_model(),
            sound_model: default_sound_model(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsRev {
    pub rev: RevId,
    pub anchor: Anchor,
    pub settings: Settings,
    pub at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// `None` is narration.
    #[serde(default)]
    pub speaker: Option<String>,
    pub text: String,
    /// How it's said: one of the speaker's deliveries by name ("angry"), for voices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
}

/// What a beat shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Show {
    /// Keep whatever is on screen.
    Keep,
    /// Show existing media again.
    Reuse { media: MediaId },
    /// Show media this beat asked for.
    New { media: MediaId },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    /// The LLM's guess of how likely the player picks it (speculation order).
    #[serde(default)]
    pub likely: Option<f32>,
    /// Written by the player in their own words (not offered by the storyteller).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Next {
    Continue,
    Choices {
        choices: Vec<Choice>,
    },
    /// Continue into an existing beat (exit 0 leads there).
    Rejoin {
        beat: BeatId,
    },
    End,
}

impl Next {
    pub fn exits(&self) -> usize {
        match self {
            Next::Continue | Next::Rejoin { .. } => 1,
            Next::Choices { choices } => choices.len(),
            Next::End => 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StateOps {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub clear: Vec<String>,
}

/// One edge out of a beat: `from` at `epoch`, exit `exit`, to `to`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub from: BeatId,
    pub epoch: u32,
    pub exit: usize,
    pub to: BeatId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Beat {
    pub id: BeatId,
    /// The edge this beat was generated for. `None` for the opening beat.
    pub origin: Option<(BeatId, u32, usize)>,
    /// Bumped by overwrite. Exits recorded under older epochs no longer count.
    pub epoch: u32,
    pub lines: Vec<Line>,
    pub show: Show,
    #[serde(default)]
    pub state: StateOps,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub next: Next,
    /// Revisions in effect when this content was generated.
    #[serde(default)]
    pub used: Vec<RevId>,
    /// Speculative: generated for an exit no player has taken yet.
    pub pending: bool,
    pub at: u64,
    /// The chat this beat came from: the user turn it answered and the raw reply, so the
    /// conversation can be rebuilt exactly when the backend's cached state is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    /// Hash of the system prompt it was written under; a chat segment ends where it
    /// changes.
    #[serde(default)]
    pub sys: u64,
    /// First beat of a chat segment (its turn carries the full context).
    #[serde(default)]
    pub seg_start: bool,
    /// It was an ending, and the reader chose to keep going from it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept_going: bool,
    /// Each line's spoken clip (audio media), by line; `None`: not spoken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<Option<MediaId>>,
    /// Sounds it plays (audio media), each from one of its lines on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds: Vec<SoundCue>,
}

impl Beat {
    /// Its audio: every line's clip and every sound.
    pub fn audio(&self) -> impl Iterator<Item = &MediaId> {
        self.voices
            .iter()
            .flatten()
            .chain(self.sounds.iter().map(|s| &s.media))
    }
}

/// A sound a beat plays: once when its line shows, or a background loop from that line
/// until another loop takes over or the scene changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoundCue {
    pub media: MediaId,
    #[serde(default)]
    pub line: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub looped: bool,
}

/// What an audio clip is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRole {
    /// A voice's clip (one delivery of it), made in setup or uploaded.
    Voice,
    /// A beat's line, spoken.
    Line,
    /// A sound effect or background sound.
    Sound,
}

/// An audio clip's particulars.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioMeta {
    pub role: AudioRole,
    /// What's said (a voice clip, a line), or the sound in words.
    #[serde(default)]
    pub words: String,
    /// How it's said, in words: the voice and delivery (speech from a description).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub how: String,
    /// What the reference clip says (its transcript), for speech in its voice.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ref_words: String,
    /// A sound that loops seamlessly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub looped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaStatus {
    Requested,
    Ready,
    Failed,
}

/// A catalog entry: generated or uploaded media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub id: MediaId,
    pub kind: MediaKind,
    pub status: MediaStatus,
    #[serde(default)]
    pub blob: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub caption: String,
    #[serde(default)]
    pub objects: Vec<String>,
    /// The generation prompt (empty for uploads).
    #[serde(default)]
    pub prompt: String,
    /// Media used as references.
    #[serde(default)]
    pub refs: Vec<MediaId>,
    pub width: u32,
    pub height: u32,
    /// The beat that asked for it, if any.
    #[serde(default)]
    pub beat: Option<BeatId>,
    #[serde(default)]
    pub error: Option<String>,
    pub at: u64,
    /// Its prompt was written out in full by the shot step (`crate::shot`): it already
    /// says how everyone looks, so the descriptions and notes aren't added again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detailed: bool,
    /// A video's length, as the storyteller asked (clamped; unset = the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f32>,
    /// Audio: what it is and how it's made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioMeta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Object(ObjectRev),
    Prompt(PromptRev),
    Settings(SettingsRev),
    /// Drop revisions ("make this the version everywhere" retires the others).
    Retire {
        revs: Vec<RevId>,
    },
    Beat(Beat),
    Edge(Edge),
    /// A pending beat was reached by a player.
    Taken {
        beat: BeatId,
    },
    Media(Media),
}

/// The folded graph.
#[derive(Default, Debug)]
pub struct Graph {
    pub beats: HashMap<BeatId, Beat>,
    /// (from, epoch, exit) -> versions, oldest first.
    pub edges: HashMap<(BeatId, u32, usize), Vec<BeatId>>,
    pub objects: Vec<ObjectRev>,
    pub prompts: Vec<PromptRev>,
    pub settings: Vec<SettingsRev>,
    pub retired: HashSet<RevId>,
    pub media: HashMap<MediaId, Media>,
    pub root: Option<BeatId>,
    /// Versions of the opening, oldest first (the root, then forks of it: edges from "").
    pub openings: Vec<BeatId>,
}

impl Graph {
    pub fn apply(&mut self, r: Record) {
        match r {
            Record::Object(o) => upsert(&mut self.objects, o, |a, b| a.rev == b.rev),
            Record::Prompt(p) => upsert(&mut self.prompts, p, |a, b| a.rev == b.rev),
            Record::Settings(s) => upsert(&mut self.settings, s, |a, b| a.rev == b.rev),
            Record::Retire { revs } => self.retired.extend(revs),
            Record::Beat(mut b) => {
                if b.origin.is_none() && self.root.is_none() {
                    self.root = Some(b.id.clone());
                    self.openings.insert(0, b.id.clone());
                }
                if let Some(old) = self.beats.get(&b.id) {
                    // A taken beat never goes back to pending.
                    b.pending &= old.pending;
                }
                self.beats.insert(b.id.clone(), b);
            }
            Record::Edge(e) => {
                // A fork of the opening: another version of it.
                if e.from.is_empty() && !self.openings.contains(&e.to) {
                    self.openings.push(e.to.clone());
                }
                let v = self.edges.entry((e.from, e.epoch, e.exit)).or_default();
                if !v.contains(&e.to) {
                    v.push(e.to);
                }
            }
            Record::Taken { beat } => {
                if let Some(b) = self.beats.get_mut(&beat) {
                    b.pending = false;
                }
            }
            Record::Media(m) => {
                self.media.insert(m.id.clone(), m);
            }
        }
    }

    /// Versions of the beat behind `exit` of `from` (current epoch), oldest first.
    pub fn versions(&self, from: &str, exit: usize) -> &[BeatId] {
        // The opening's versions ("before the opening" is the empty id).
        if from.is_empty() {
            return &self.openings;
        }
        let Some(b) = self.beats.get(from) else {
            return &[];
        };
        self.edges
            .get(&(from.to_string(), b.epoch, exit))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The beat a path step leads to: the chosen version, else the newest.
    pub fn follow(&self, from: &str, exit: usize, version: Option<&str>) -> Option<&Beat> {
        let vs = self.versions(from, exit);
        let id = version
            .filter(|v| vs.iter().any(|x| x == v))
            .or_else(|| vs.last().map(String::as_str))?;
        self.beats.get(id)
    }

    /// Can `from` reach any beat in `targets` (including itself) through current exits?
    pub fn reaches_any(&self, from: &str, targets: &HashSet<&str>) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from.to_string()];
        while let Some(id) = stack.pop() {
            if targets.contains(id.as_str()) {
                return true;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(b) = self.beats.get(&id) {
                for exit in 0..b.next.exits() {
                    stack.extend(self.versions(&id, exit).iter().cloned());
                }
            }
        }
        false
    }

    /// Every beat reachable from the root through current-epoch exits.
    pub fn reachable(&self) -> HashSet<BeatId> {
        let mut seen = HashSet::new();
        let mut stack: Vec<BeatId> = self.openings.clone();
        stack.extend(self.root.iter().cloned());
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(b) = self.beats.get(&id) {
                for exit in 0..b.next.exits() {
                    stack.extend(self.versions(&id, exit).iter().cloned());
                }
            }
        }
        seen
    }

    /// Records that rebuild this graph (for compaction and export). With `drop_pending`,
    /// pending beats, edges into them and media only they use are left out.
    pub fn records(&self, drop_pending: bool) -> Vec<Record> {
        let keep: HashSet<BeatId> = self
            .reachable()
            .into_iter()
            .filter(|id| !drop_pending || self.beats.get(id).is_some_and(|b| !b.pending))
            .collect();
        let mut out: Vec<Record> = Vec::new();
        out.extend(self.settings.iter().cloned().map(Record::Settings));
        out.extend(self.prompts.iter().cloned().map(Record::Prompt));
        out.extend(self.objects.iter().cloned().map(Record::Object));
        if !self.retired.is_empty() {
            let mut revs: Vec<RevId> = self.retired.iter().cloned().collect();
            revs.sort();
            out.push(Record::Retire { revs });
        }
        let used = self.used_media(&keep);
        let mut media: Vec<&Media> = self
            .media
            .values()
            .filter(|m| used.contains(&m.id) || !drop_pending)
            .collect();
        media.sort_by_key(|m| m.at);
        out.extend(media.into_iter().cloned().map(Record::Media));
        let mut beats: Vec<&Beat> = keep.iter().filter_map(|id| self.beats.get(id)).collect();
        beats.sort_by_key(|b| b.at);
        out.extend(beats.into_iter().cloned().map(Record::Beat));
        let mut edges: Vec<Edge> = Vec::new();
        for ((from, epoch, exit), tos) in &self.edges {
            let opening = from.is_empty();
            if !opening
                && (!keep.contains(from) || self.beats.get(from).map(|b| b.epoch) != Some(*epoch))
            {
                continue;
            }
            for to in tos.iter().filter(|t| keep.contains(*t)) {
                edges.push(Edge {
                    from: from.clone(),
                    epoch: *epoch,
                    exit: *exit,
                    to: to.clone(),
                });
            }
        }
        edges.sort_by(|a, b| (&a.from, a.exit, &a.to).cmp(&(&b.from, b.exit, &b.to)));
        out.extend(edges.into_iter().map(Record::Edge));
        out
    }

    /// Media referenced by setup (object refs) or by the given beats, plus their refs.
    pub fn used_media(&self, beats: &HashSet<BeatId>) -> HashSet<MediaId> {
        let mut used: HashSet<MediaId> = HashSet::new();
        for o in &self.objects {
            used.extend(o.refs.iter().map(|r| r.media.clone()));
            used.extend(o.clips().cloned());
        }
        for id in beats {
            if let Some(b) = self.beats.get(id) {
                match &b.show {
                    Show::Reuse { media } | Show::New { media } => {
                        used.insert(media.clone());
                    }
                    Show::Keep => {}
                }
                used.extend(b.audio().cloned());
            }
        }
        // Media requested by kept beats even if not shown (e.g. a video still rendering).
        for m in self.media.values() {
            if m.beat.as_ref().is_some_and(|b| beats.contains(b)) {
                used.insert(m.id.clone());
            }
        }
        used
    }
}

fn upsert<T>(v: &mut Vec<T>, item: T, same: impl Fn(&T, &T) -> bool) {
    if let Some(slot) = v.iter_mut().find(|x| same(x, &item)) {
        *slot = item;
    } else {
        v.push(item);
    }
}

/// One step of a path: a beat, and (except at the end) the exit taken out of it and the
/// version it led to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hop {
    pub beat: BeatId,
    #[serde(default)]
    pub exit: Option<usize>,
}

/// A position: the path from the opening beat to the current one.
pub type Path = Vec<Hop>;

/// Depth of each beat on a path.
fn depths(path: &[Hop]) -> HashMap<&str, usize> {
    path.iter()
        .enumerate()
        .map(|(i, h)| (h.beat.as_str(), i))
        .collect()
}

/// Revision resolution for one path. `target` is the beat being generated, if it already
/// exists (a re-run): `Here` prompts anchored at it apply.
pub struct Resolver<'a> {
    graph: &'a Graph,
    depth: HashMap<&'a str, usize>,
    target: Option<&'a str>,
    /// The last beat on the path (the one the beat being resolved for comes after).
    last: Option<&'a str>,
}

impl<'a> Resolver<'a> {
    pub fn new(graph: &'a Graph, path: &'a [Hop], target: Option<&'a str>) -> Self {
        Self {
            graph,
            depth: depths(path),
            target,
            last: path.last().map(|h| h.beat.as_str()),
        }
    }

    /// Rank of an anchor for this path, or `None` if it doesn't apply.
    fn rank(&self, anchor: &Anchor, scope: Scope) -> Option<i64> {
        match (anchor, scope) {
            (None, _) => Some(-1),
            (Some(a), Scope::Here) => (self.target == Some(a.as_str())).then_some(i64::MAX),
            (Some(a), Scope::Next) => (self.last == Some(a.as_str())).then_some(i64::MAX - 2),
            (Some(a), Scope::FromHere) => {
                if self.target == Some(a.as_str()) {
                    return Some(i64::MAX - 1);
                }
                self.depth.get(a.as_str()).map(|d| *d as i64)
            }
        }
    }

    fn pick<T>(
        &self,
        items: impl Iterator<Item = &'a T>,
        anchor: impl Fn(&T) -> (&Anchor, Scope, &RevId, u64),
    ) -> Option<&'a T>
    where
        T: 'a,
    {
        items
            .filter(|x| !self.graph.retired.contains(anchor(x).2))
            .filter_map(|x| {
                let (a, s, _, at) = anchor(x);
                self.rank(a, s).map(|r| (r, at, x))
            })
            .max_by_key(|(r, at, _)| (*r, *at))
            .map(|(_, _, x)| x)
    }

    pub fn prompt(&self, id: &str) -> Option<&'a PromptRev> {
        self.pick(self.graph.prompts.iter().filter(|p| p.id == id), |p| {
            (&p.anchor, p.scope, &p.rev, p.at)
        })
    }

    /// Every prompt id in effect, resolved.
    pub fn prompts(&self) -> Vec<&'a PromptRev> {
        let mut ids: Vec<&str> = self.graph.prompts.iter().map(|p| p.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        ids.into_iter()
            .filter_map(|id| self.prompt(id))
            .filter(|p| !p.text.trim().is_empty())
            .collect()
    }

    pub fn objects(&self) -> Vec<&'a ObjectRev> {
        let mut ids: Vec<&str> = self.graph.objects.iter().map(|o| o.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        ids.into_iter()
            .filter_map(|id| {
                self.pick(self.graph.objects.iter().filter(|o| o.id == id), |o| {
                    (&o.anchor, Scope::FromHere, &o.rev, o.at)
                })
            })
            .filter(|o| !o.removed)
            .collect()
    }

    pub fn settings(&self) -> Settings {
        self.pick(self.graph.settings.iter(), |s| {
            (&s.anchor, Scope::FromHere, &s.rev, s.at)
        })
        .map(|s| s.settings.clone())
        .unwrap_or_default()
    }

    /// Revision ids in effect (recorded on generated beats, compared for staleness).
    pub fn used(&self) -> Vec<RevId> {
        // A picture direction doesn't change what's written (it's added to pictures as
        // they're made), so beats written ahead aren't rewritten for it.
        let mut out: Vec<RevId> = self
            .prompts()
            .iter()
            .filter(|p| p.id != PICTURE_PROMPT)
            .map(|p| p.rev.clone())
            .collect();
        out.extend(self.objects().iter().map(|o| o.rev.clone()));
        if let Some(s) = self.pick(self.graph.settings.iter(), |s| {
            (&s.anchor, Scope::FromHere, &s.rev, s.at)
        }) {
            out.push(s.rev.clone());
        }
        out.sort();
        out
    }
}

/// LLM state at the end of a path.
pub fn state_at(graph: &Graph, path: &[Hop]) -> BTreeMap<String, String> {
    let mut state = BTreeMap::new();
    for h in path {
        if let Some(b) = graph.beats.get(&h.beat) {
            for k in &b.state.clear {
                state.remove(k);
            }
            for (k, v) in &b.state.set {
                state.insert(k.clone(), v.clone());
            }
        }
    }
    state
}

/// The media on screen at the end of a path: the last beat that showed something.
/// Media is settled once it's done, or failed (the previous picture then stays up).
pub fn media_settled(graph: &Graph, id: &str) -> bool {
    graph
        .media
        .get(id)
        .is_none_or(|m| m.status != MediaStatus::Requested)
}

/// Can this beat go on screen: is what it shows painted, and what it says and plays made?
pub fn screen_ready(graph: &Graph, b: &Beat) -> bool {
    let shown = match &b.show {
        Show::Keep => true,
        Show::Reuse { media } | Show::New { media } => media_settled(graph, media),
    };
    shown && b.audio().all(|m| media_settled(graph, m))
}

/// Is this beat's picture made, but some of its voices or sounds not yet?
pub fn sound_pending(graph: &Graph, b: &Beat) -> bool {
    b.audio().any(|m| !media_settled(graph, m))
}

/// The background sound playing at the end of `path`: the last loop started in this
/// scene (a beat with a scene label starts a new scene, and silence unless it starts a
/// loop itself). One that failed to be made never played.
pub fn ambience_at<'a>(graph: &'a Graph, path: &[Hop]) -> Option<&'a Media> {
    for h in path.iter().rev() {
        let b = graph.beats.get(&h.beat)?;
        if let Some(c) = b.sounds.iter().rev().find(|c| c.looped) {
            return graph
                .media
                .get(&c.media)
                .filter(|m| m.status == MediaStatus::Ready);
        }
        if b.label.is_some() {
            return None;
        }
    }
    None
}

/// What's on screen at the end of `path`: the last picture or video a beat put up. One
/// that failed to be made never showed, so the one before it is still up.
pub fn showing_at<'a>(graph: &'a Graph, path: &[Hop]) -> Option<&'a Media> {
    path.iter().rev().find_map(|h| {
        let b = graph.beats.get(&h.beat)?;
        match &b.show {
            Show::Keep => None,
            Show::Reuse { media } | Show::New { media } => graph
                .media
                .get(media)
                .filter(|m| m.status != MediaStatus::Failed),
        }
    })
}

/// Beats the screen has stayed the same at the end of `path`: since a beat last put up a
/// different picture or video (a failed one doesn't count: it never showed). The whole
/// path if nothing ever showed.
pub fn screen_age(graph: &Graph, path: &[Hop]) -> usize {
    let mut prev: Option<&str> = None;
    let mut age = 0;
    // Walk forward: a beat that shows what was already up doesn't change the screen.
    for h in path {
        let shown = graph.beats.get(&h.beat).and_then(|b| match &b.show {
            Show::Keep => None,
            Show::Reuse { media } | Show::New { media } => graph
                .media
                .get(media)
                .filter(|m| m.status != MediaStatus::Failed)
                .map(|m| m.id.as_str()),
        });
        match shown {
            Some(m) if prev != Some(m) => {
                prev = Some(m);
                age = 0;
            }
            _ => age += 1,
        }
    }
    if prev.is_none() { path.len() } else { age }
}

/// Is a beat stale for this path (generated under revisions no longer in effect)?
pub fn is_stale(graph: &Graph, path: &[Hop]) -> bool {
    let Some(last) = path.last() else {
        return false;
    };
    let Some(beat) = graph.beats.get(&last.beat) else {
        return false;
    };
    // Judged by the way it was first written (its origin), whichever way it's reached: a
    // beat two ways lead to, each with its own "from here" setup, was otherwise out of
    // date for one way after every rewrite for the other, and was rewritten forever
    // (2026-11-22 e2e: ~30 rewrites a second, a reader's own choice never written).
    let own = origin_path(graph, &beat.id);
    let path = own.as_deref().unwrap_or(path);
    let parent = &path[..path.len() - 1];
    text_revs(graph, &Resolver::new(graph, parent, Some(&beat.id)).used())
        != text_revs(graph, &beat.used)
}

/// The way a beat was first written: back through the beats it was written after, to an
/// opening. None: a beat on the way back is missing.
pub fn origin_path(graph: &Graph, beat: &str) -> Option<Vec<Hop>> {
    let mut path = vec![Hop {
        beat: beat.to_string(),
        exit: None,
    }];
    let mut cur = graph.beats.get(beat)?;
    while let Some((from, _, exit)) = &cur.origin {
        if path.len() > 100_000 || path.iter().any(|h| &h.beat == from) {
            return None;
        }
        path.push(Hop {
            beat: from.clone(),
            exit: Some(*exit),
        });
        cur = graph.beats.get(from)?;
    }
    path.reverse();
    Some(path)
}

/// The revisions in `used` that can change what a beat says: pictures-only ones (the
/// style, the picture steer, a settings change that only touches pictures: shape,
/// size, picture and video models) are left out, so they repaint pictures but don't
/// send written text round again (2026-11-21 alpha: switching to portrait just before
/// Start rewrote the opening, 26 s).
fn text_revs(graph: &Graph, used: &[RevId]) -> Vec<String> {
    // What the storyteller is told or held to by the settings.
    let key = |t: &Settings| {
        // The choice limit only when changed: added later, and a story's written text
        // shouldn't all go stale because the key got longer.
        let choice_max = if t.choice_max == default_choice_max() {
            String::new()
        } else {
            format!(":choices{}", t.choice_max)
        };
        format!(
            "settings:{}:{}:{}:{}:{}:{}:{}{}",
            t.text_model,
            t.think,
            t.video,
            t.picture_gap,
            t.picture_max,
            t.min_beats,
            t.own_choices,
            choice_max
        )
    };
    let mut settings = None;
    let mut out: Vec<String> = used
        .iter()
        .filter_map(|r| {
            if let Some(p) = graph.prompts.iter().find(|p| &p.rev == r) {
                return (p.id != STYLE_PROMPT && p.id != PICTURE_PROMPT).then(|| r.clone());
            }
            if let Some(s) = graph.settings.iter().find(|s| &s.rev == r) {
                settings = Some(key(&s.settings));
                return None;
            }
            // An object by what the storyteller is told of it: a change to its voice
            // alone (clips made, a delivery added) doesn't send written text round again.
            if let Some(o) = graph.objects.iter().find(|o| &o.rev == r) {
                let refs: Vec<String> = o
                    .refs
                    .iter()
                    .map(|x| format!("{}|{}|{}", x.media, x.caption, x.use_for))
                    .collect();
                return Some(format!(
                    "object:{}:{}:{}:{}:{}:{}",
                    o.id,
                    o.name,
                    o.description,
                    o.removed,
                    refs.join(","),
                    o.talks
                ));
            }
            Some(r.clone())
        })
        .collect();
    // No settings saved yet means the defaults: the same as saving them unchanged.
    out.push(settings.unwrap_or_else(|| key(&Settings::default())));
    out.sort();
    out
}

/// Check a path against the graph: every step follows a current exit.
pub fn validate_path(graph: &Graph, path: &[Hop]) -> bool {
    let Some(first) = path.first() else {
        return true;
    };
    if graph.root.as_deref() != Some(first.beat.as_str()) && !graph.openings.contains(&first.beat) {
        return false;
    }
    path.windows(2).all(|w| {
        let Some(exit) = w[0].exit else {
            return false;
        };
        graph.versions(&w[0].beat, exit).contains(&w[1].beat)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn videos_are_the_pictures_size_unless_set() {
        let s = |w, h| Settings {
            width: w,
            height: h,
            ..Default::default()
        };
        assert_eq!(s(576, 1024).size_for(MediaKind::Video), (576, 1024));
        assert_eq!(s(576, 1024).size_for(MediaKind::Image), (576, 1024));
        let set = Settings {
            video_size: Some((480, 832)),
            ..s(576, 1024)
        };
        assert_eq!(set.size_for(MediaKind::Video), (480, 832));
        assert_eq!(set.size_for(MediaKind::Image), (576, 1024));
    }

    use super::*;

    fn beat(id: &str, origin: Option<(&str, usize)>, next: Next) -> Beat {
        Beat {
            id: id.into(),
            origin: origin.map(|(b, e)| (b.into(), 0, e)),
            epoch: 0,
            lines: vec![Line {
                speaker: None,
                text: id.into(),
                tone: None,
            }],
            show: Show::Keep,
            state: StateOps::default(),
            summary: None,
            label: None,
            next,
            used: vec![],
            pending: false,
            at: 0,
            turn: None,
            reply: None,
            sys: 0,
            seg_start: false,
            kept_going: false,
            voices: vec![],
            sounds: vec![],
        }
    }

    fn edge(from: &str, exit: usize, to: &str) -> Record {
        Record::Edge(Edge {
            from: from.into(),
            epoch: 0,
            exit,
            to: to.into(),
        })
    }

    fn prompt(rev: &str, anchor: Option<&str>, scope: Scope, at: u64) -> Record {
        Record::Prompt(PromptRev {
            id: STORY_PROMPT.into(),
            rev: rev.into(),
            anchor: anchor.map(Into::into),
            scope,
            text: rev.into(),
            at,
        })
    }

    #[test]
    fn a_beat_two_ways_lead_to_is_judged_by_the_way_it_was_written() {
        // r -> a -> s (written there), and r -> b -> s (b rejoins s); "from here" at a.
        let mut g = Graph::default();
        let two = Next::Choices {
            choices: vec![
                Choice {
                    own: false,
                    label: "a".into(),
                    likely: None,
                },
                Choice {
                    own: false,
                    label: "b".into(),
                    likely: None,
                },
            ],
        };
        g.apply(Record::Beat(beat("r", None, two)));
        g.apply(Record::Beat(beat("a", Some(("r", 0)), Next::Continue)));
        g.apply(Record::Beat(beat("b", Some(("r", 1)), Next::Continue)));
        g.apply(edge("r", 0, "a"));
        g.apply(edge("r", 1, "b"));
        g.apply(prompt("root", None, Scope::FromHere, 1));
        g.apply(prompt("at-a", Some("a"), Scope::FromHere, 2));
        let hop = |b: &str, e: Option<usize>| Hop {
            beat: b.into(),
            exit: e,
        };
        let via_a = vec![hop("r", Some(0)), hop("a", Some(0)), hop("s", None)];
        let via_b = vec![hop("r", Some(1)), hop("b", Some(0)), hop("s", None)];
        let mut s = beat("s", Some(("a", 0)), Next::End);
        s.used = Resolver::new(&g, &via_a[..2], Some("s")).used();
        g.apply(Record::Beat(s));
        g.apply(edge("a", 0, "s"));
        g.apply(edge("b", 0, "s"));
        assert_eq!(origin_path(&g, "s").unwrap(), via_a);
        // Up to date the way it was written, whichever way it's reached: before, it was
        // out of date by way of b, rewritten for b, then out of date by way of a, forever.
        assert!(!is_stale(&g, &via_a));
        assert!(!is_stale(&g, &via_b));
        // A change on the way it was written still counts, whichever way it's reached.
        g.apply(prompt("at-a-2", Some("a"), Scope::FromHere, 4));
        assert!(is_stale(&g, &via_b));
    }

    #[test]
    fn revisions_resolve_by_path_depth() {
        let mut g = Graph::default();
        let two = Next::Choices {
            choices: vec![
                Choice {
                    own: false,
                    label: "a".into(),
                    likely: None,
                },
                Choice {
                    own: false,
                    label: "b".into(),
                    likely: None,
                },
            ],
        };
        g.apply(Record::Beat(beat("r", None, two)));
        g.apply(Record::Beat(beat("a", Some(("r", 0)), Next::Continue)));
        g.apply(Record::Beat(beat("b", Some(("r", 1)), Next::Continue)));
        g.apply(edge("r", 0, "a"));
        g.apply(edge("r", 1, "b"));
        g.apply(prompt("root", None, Scope::FromHere, 1));
        g.apply(prompt("at-a", Some("a"), Scope::FromHere, 2));
        g.apply(prompt("here-b", Some("b"), Scope::Here, 3));

        let pa = vec![
            Hop {
                beat: "r".into(),
                exit: Some(0),
            },
            Hop {
                beat: "a".into(),
                exit: None,
            },
        ];
        let pb = vec![
            Hop {
                beat: "r".into(),
                exit: Some(1),
            },
            Hop {
                beat: "b".into(),
                exit: None,
            },
        ];
        assert!(validate_path(&g, &pa));
        assert_eq!(
            Resolver::new(&g, &pa, None)
                .prompt(STORY_PROMPT)
                .unwrap()
                .rev,
            "at-a"
        );
        // `Here` only applies when re-running b itself.
        assert_eq!(
            Resolver::new(&g, &pb, None)
                .prompt(STORY_PROMPT)
                .unwrap()
                .rev,
            "root"
        );
        assert_eq!(
            Resolver::new(&g, &pb[..1], Some("b"))
                .prompt(STORY_PROMPT)
                .unwrap()
                .rev,
            "here-b"
        );
        // Retiring the deeper revision makes the root one win again.
        g.apply(Record::Retire {
            revs: vec!["at-a".into()],
        });
        assert_eq!(
            Resolver::new(&g, &pa, None)
                .prompt(STORY_PROMPT)
                .unwrap()
                .rev,
            "root"
        );
    }

    #[test]
    fn epochs_versions_and_pending_records() {
        let mut g = Graph::default();
        g.apply(Record::Beat(beat("r", None, Next::Continue)));
        let mut a = beat("a", Some(("r", 0)), Next::End);
        a.pending = true;
        g.apply(Record::Beat(a));
        g.apply(edge("r", 0, "a"));
        g.apply(Record::Beat(beat("a2", Some(("r", 0)), Next::End)));
        g.apply(edge("r", 0, "a2"));
        assert_eq!(g.versions("r", 0), ["a", "a2"]);
        assert_eq!(g.follow("r", 0, None).unwrap().id, "a2");
        assert_eq!(g.follow("r", 0, Some("a")).unwrap().id, "a");

        let recs = g.records(true);
        let mut g2 = Graph::default();
        recs.into_iter().for_each(|r| g2.apply(r));
        assert!(!g2.beats.contains_key("a"));
        assert_eq!(g2.versions("r", 0), ["a2"]);

        // Overwrite r: new epoch, old exits gone.
        let mut r = beat("r", None, Next::Continue);
        r.epoch = 1;
        g.apply(Record::Beat(r));
        assert!(g.versions("r", 0).is_empty());
        assert_eq!(g.reachable().len(), 1);
    }
}
