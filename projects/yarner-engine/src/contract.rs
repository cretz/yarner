//! The LLM contract: what one generation returns, the schema enforcing it, the default
//! engine prompt, and the tools the LLM may call.
//!
//! The engine owns a small fixed set of mechanics (lines, media, choices, state, labels,
//! rejoin). Everything inside them is loose English.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::backend::ToolDef;
use crate::model::{Choice, Line, Next, StateOps};

// Each option is named by its own key ("keep", {"image":{...}}, {"choices":[...]}), never
// by a "type" field: servers may sort a schema's keys (serde_json without
// `preserve_order` does), and a grammar that writes keys in that order would put an
// option's other fields ahead of its "type", so a model that opens with "type" could
// only ever pick the options with no other fields (2026-11-21: no picture or choice was
// ever made). One key per option is unambiguous in any order, and shorter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaOut {
    Keep,
    Reuse(String),
    Image {
        prompt: String,
        #[serde(default)]
        caption: String,
        #[serde(default)]
        objects: Vec<String>,
        #[serde(default)]
        refs: Vec<String>,
    },
    Video {
        prompt: String,
        #[serde(default)]
        caption: String,
        #[serde(default)]
        objects: Vec<String>,
        #[serde(default)]
        refs: Vec<String>,
        #[serde(default)]
        seconds: Option<f32>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextOut {
    Continue,
    Choices(Vec<Choice>),
    Rejoin(String),
    End,
}

/// One beat as the LLM writes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeatOut {
    pub lines: Vec<Line>,
    #[serde(default = "keep")]
    pub media: MediaOut,
    #[serde(default)]
    pub state: StateOps,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub next: NextOut,
    /// Sounds to play (sound effects, background sounds).
    #[serde(default)]
    pub sounds: Vec<SoundOut>,
}

/// A sound the storyteller asks for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoundOut {
    /// The sound, in words ("a heavy door creaks open").
    pub sound: String,
    /// The line it plays at (from 0).
    #[serde(default)]
    pub line: u32,
    /// A steady background sound that loops until the scene changes.
    #[serde(default, rename = "loop")]
    pub looped: bool,
}

fn keep() -> MediaOut {
    MediaOut::Keep
}

impl NextOut {
    pub fn into_next(self) -> Next {
        match self {
            NextOut::Continue => Next::Continue,
            NextOut::Choices(choices) => Next::Choices { choices },
            NextOut::Rejoin(beat) => Next::Rejoin { beat },
            NextOut::End => Next::End,
        }
    }
}

/// Parse a reply, tolerating code fences and leading prose (models without enforced
/// schemas wrap JSON).
pub fn parse(reply: &str) -> Result<BeatOut, String> {
    let t = reply.trim();
    let start = t.find('{').ok_or("no JSON object in reply")?;
    let end = t.rfind('}').ok_or("no JSON object in reply")?;
    let mut out: BeatOut =
        serde_json::from_str(&t[start..=end]).map_err(|e| format!("bad beat JSON: {e}"))?;
    out.lines.iter_mut().for_each(narration_is_nobody);
    if out.lines.is_empty() {
        return Err("beat has no lines".into());
    }
    if let NextOut::Choices(choices) = &out.next
        && (choices.is_empty() || choices.iter().any(|c| c.label.trim().is_empty()))
    {
        return Err("choices must be non-empty labels".into());
    }
    Ok(out)
}

/// A reply's shape, for the content-free log: whether its JSON closed, the last field
/// it was writing (a schema key, never content), how many lines it had and the most
/// times any one stretch of text repeated (a model going round in circles).
pub struct Shape {
    pub closed: bool,
    pub last_key: String,
    pub lines: usize,
    pub repeated: usize,
}

pub fn shape(reply: &str) -> Shape {
    const KEYS: [&str; 12] = [
        "lines", "speaker", "text", "tone", "media", "state", "summary", "label", "next",
        "choices", "prompt", "caption",
    ];
    let t = reply.trim();
    let closed = serde_json::from_str::<Value>(t).is_ok();
    let last_key = KEYS
        .iter()
        .filter_map(|k| t.rfind(&format!("\"{k}\":")).map(|at| (at, *k)))
        .max()
        .map_or("none", |(_, k)| k)
        .to_string();
    let lines = t.matches("\"text\":").count();
    // Chunks of 40 chars: the most times one recurs.
    let chars: Vec<char> = t.chars().collect();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for c in chars.chunks(40).filter(|c| c.len() == 40) {
        *seen.entry(c.iter().collect()).or_default() += 1;
    }
    let repeated = seen.values().copied().max().unwrap_or(0);
    Shape {
        closed,
        last_key,
        lines,
        repeated,
    }
}

/// A line written as narration that starts with a known name and a colon
/// ("Ruth Calder: 'Go on.'") is that one speaking: the name moves to `speaker` (a first
/// name will do), the quotes around the words go.
pub fn lift_speakers(lines: &mut [Line], known: &[String]) {
    const OPEN: [char; 4] = ['\'', '"', '\u{2018}', '\u{201c}'];
    const CLOSE: [char; 4] = ['\'', '"', '\u{2019}', '\u{201d}'];
    for l in lines.iter_mut().filter(|l| l.speaker.is_none()) {
        let Some((head, rest)) = l.text.split_once(':') else {
            continue;
        };
        let head = head.trim();
        let rest = rest.trim();
        if head.is_empty() || rest.is_empty() || head.split_whitespace().count() > 4 {
            continue;
        }
        let h = head.to_lowercase();
        let Some(name) = known.iter().find(|k| {
            let k = k.to_lowercase();
            k == h || k.split_whitespace().next() == Some(h.as_str())
        }) else {
            continue;
        };
        let said = rest
            .strip_prefix(OPEN)
            .and_then(|r| r.strip_suffix(CLOSE))
            .unwrap_or(rest)
            .to_string();
        l.speaker = Some(if h == name.to_lowercase() {
            name.clone()
        } else {
            head.to_string()
        });
        l.text = said;
    }
}

/// Spoken words and stage directions in the wrong place (2026-11-23 real run, once lines
/// were voiced): a narration line that is only a quotation, right after a narration line
/// naming exactly one known one, is that one speaking ("Jonah shifted the box." then
/// "\"Six.\""); one naming nobody stays narration (the player: "I nodded." "\"We do.\"").
/// A spoken "line" that is only a direction ("dryly", "(softly)", "*sighs*") isn't
/// said: it goes (unless it's all there is).
pub fn tidy_lines(lines: &mut Vec<Line>, known: &[String]) {
    const OPEN: [char; 3] = ['"', '\u{201c}', '\u{2018}'];
    const CLOSE: [char; 3] = ['"', '\u{201d}', '\u{2019}'];
    let words = |t: &str| -> Vec<String> {
        t.split(|c: char| !c.is_alphanumeric() && c != '\'')
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    for i in 1..lines.len() {
        let t = lines[i].text.trim();
        let quoted = lines[i].speaker.is_none()
            && t.chars().count() > 2
            && t.starts_with(OPEN)
            && t.ends_with(CLOSE)
            // One quotation, not two with narration between them.
            && t.chars().filter(|c| OPEN.contains(c) || CLOSE.contains(c)).count() <= 2;
        if !quoted || lines[i - 1].speaker.is_some() {
            continue;
        }
        let before = words(&lines[i - 1].text);
        let named: Vec<&String> = known
            .iter()
            .filter(|k| {
                let k = words(k);
                !k.is_empty()
                    && (before.windows(k.len()).any(|w| w == k.as_slice())
                        || before.contains(&k[0]))
            })
            .collect();
        let mut names: Vec<&String> = named.clone();
        names.dedup();
        if names.len() == 1 {
            let said = t
                .trim_start_matches(OPEN)
                .trim_end_matches(CLOSE)
                .trim()
                .to_string();
            lines[i].speaker = Some(names[0].clone());
            lines[i].text = said;
        }
    }
    let direction = |l: &Line| {
        let t = l.text.trim();
        l.speaker.is_some()
            && ((t.starts_with('(') && t.ends_with(')'))
                || (t.starts_with('*') && t.ends_with('*') && t.len() > 1)
                || (!t.contains(' ')
                    && t.to_lowercase().ends_with("ly")
                    && t.chars().all(|c| c.is_alphabetic())))
    };
    if lines.iter().any(|l| !direction(l)) {
        lines.retain(|l| !direction(l));
    }
}

/// What the model writes as the speaker of a line nobody says.
const NARRATION: [&str; 5] = ["narration", "narrator", "none", "null", ""];

/// A line the model says is narration ("speaker":"narration") has no speaker.
fn narration_is_nobody(l: &mut Line) {
    if l.speaker
        .as_deref()
        .is_some_and(|s| NARRATION.contains(&s.trim().to_lowercase().as_str()))
    {
        l.speaker = None;
    }
}

/// The lines completed so far in a reply that is still being generated (compact JSON as
/// the schema produces it): every whole `{...}` object inside `"lines":[`.
pub fn partial_lines(reply: &str) -> Vec<Line> {
    let Some(start) = reply.find("\"lines\"") else {
        return vec![];
    };
    let Some(open) = reply[start..].find('[') else {
        return vec![];
    };
    let body = &reply[start + open + 1..];
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc, mut obj_start) = (0usize, false, false, None);
    for (i, c) in body.char_indices() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => {
                if depth == 0 {
                    obj_start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(st) = obj_start.take()
                    && let Ok(mut l) = serde_json::from_str::<Line>(&body[st..=i])
                {
                    narration_is_nobody(&mut l);
                    out.push(l);
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// JSON Schema for [`BeatOut`]: every object closed (no extra keys) and compact JSON with
/// no free whitespace, so a model can't wander off into keys or blank space it invents.
pub fn schema() -> Value {
    schema_with(&Allow::default())
}

/// What one beat may do, enforced in the grammar rather than asked for in the prompt:
/// models given a free hand offer choices on every beat (2026-11-21: 30 of 30), make
/// pictures back to back, and invent ids to rejoin or reuse.
#[derive(Clone, Debug)]
pub struct Allow {
    pub choices: bool,
    /// The beat must offer choices (or end): too many have gone by without one.
    pub must_choose: bool,
    /// A new picture or video.
    pub new_media: bool,
    /// Beats it may rejoin (`None`: any id).
    pub rejoin: Option<Vec<String>>,
    /// Pictures it may show again (`None`: any id).
    pub reuse: Option<Vec<String>>,
    /// Images it may use as references (`None`: any id).
    pub refs: Option<Vec<String>>,
    /// Most lines in the beat.
    pub max_lines: u32,
    /// The beat must put a picture up (no "keep"): nothing is on screen yet, or the same
    /// one has been up too long.
    pub must_show: bool,
    /// The story may end here.
    pub end: bool,
    /// How a line may be said (the deliveries of the voices; `None`: lines aren't
    /// voiced, no field).
    pub deliveries: Option<Vec<String>>,
    /// Sounds may be asked for.
    pub sounds: bool,
}

impl Default for Allow {
    fn default() -> Self {
        Self {
            choices: true,
            must_choose: false,
            new_media: true,
            rejoin: None,
            reuse: None,
            refs: None,
            max_lines: 6,
            must_show: false,
            end: true,
            deliveries: None,
            sounds: false,
        }
    }
}

/// [`schema`] limited to what `allow` permits.
pub fn schema_with(allow: &Allow) -> Value {
    let mut v = schema_open(allow);
    close_objects(&mut v);
    v["x-guidance"] = json!({
        "whitespace_flexible": false,
        "item_separator": ",",
        "key_separator": ":"
    });
    v
}

/// Set `additionalProperties: false` on every object schema that lists properties.
fn close_objects(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if m.contains_key("properties") && !m.contains_key("additionalProperties") {
                m.insert("additionalProperties".into(), Value::Bool(false));
            }
            for (_, x) in m.iter_mut() {
                close_objects(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(close_objects),
        _ => {}
    }
}

fn schema_open(allow: &Allow) -> Value {
    // Bounded everywhere: an unbounded string or list lets a model ramble until it hits
    // the token cap and the beat comes back truncated.
    let s = |max: u32| json!({"type": "string", "maxLength": max});
    let strs =
        json!({"type": "array", "maxItems": 6, "items": {"type": "string", "maxLength": 80}});
    // One option: a bare name, or an object with that name as its only key.
    let named = |name: &str| json!({"const": name});
    let one = |name: &str, v: Value| json!({"type": "object", "properties": {name: v}, "required": [name]});
    // References: only images that exist; at most what the model takes.
    let refs = |max: u32| match &allow.refs {
        None => json!({"type": "array", "maxItems": max, "items": s(40)}),
        Some(v) if v.is_empty() => json!({"type": "array", "maxItems": 0}),
        Some(v) => json!({"type": "array", "maxItems": max, "items": {"enum": v}}),
    };
    let media_new = |kind: &str, max_refs: u32, extra: Value| {
        let mut props = json!({
            // Room for a detailed shot (2026-11-21 alpha: prompts were too thin).
            "prompt": s(900),
            "caption": s(80),
            "objects": strs,
            "refs": refs(max_refs),
        });
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        one(
            kind,
            json!({"type": "object", "properties": props, "required": ["prompt", "caption"]}),
        )
    };
    // An id from a known list (an empty list leaves the option out).
    let ids = |list: &Option<Vec<String>>| match list {
        None => Some(s(40)),
        Some(v) if v.is_empty() => None,
        Some(v) => Some(json!({"enum": v})),
    };
    let mut media_options = vec![];
    let can_reuse = ids(&allow.reuse).is_some();
    if !allow.must_show || (!allow.new_media && !can_reuse) {
        media_options.push(named("keep"));
    }
    if let Some(id) = ids(&allow.reuse) {
        media_options.push(one("reuse", id));
    }
    if allow.new_media {
        media_options.push(media_new("image", 4, json!({})));
        media_options.push(media_new(
            "video",
            9,
            json!({"seconds": {"type": "number"}}),
        ));
    }
    // Forced to choose: no carrying on (or flowing back into another line) instead.
    let choose = allow.must_choose && allow.choices;
    let mut next_options = vec![];
    if !choose {
        next_options.push(named("continue"));
    }
    if allow.end {
        next_options.push(named("end"));
    }
    if let Some(id) = ids(&allow.rejoin).filter(|_| !choose) {
        next_options.push(one("rejoin", id));
    }
    if allow.choices {
        next_options.push(one(
            "choices",
            json!({"type": "array", "minItems": 2, "maxItems": 4, "items": {
                "type": "object",
                // No JSON punctuation inside a label: a model that loses track of
                // where it is writes the rest of the object into the string.
                "properties": {
                    "label": {"type": "string", "maxLength": 80, "pattern": r"^[^{}\[\]]*$"},
                    "likely": {"type": "number", "minimum": 0, "maximum": 1}
                },
                "required": ["label"]
            }}),
        ));
    }
    // Said by someone on every line, named first ("narration" when nobody speaks): left
    // optional, models left it out of dialogue too, and the line wasn't spoken in its
    // speaker's voice (2026-11-24 alpha, again and again). Choosing it first makes the
    // model decide who says a line before writing it.
    // No quotation marks in a line: what someone says can only be a line of theirs (with
    // "speaker"), never quoted inside narration, where it isn't spoken in their voice
    // (2026-11-23: about half of all speech came as quoted narration, silent). Apostrophes
    // ("don't", "Elena's", the closing single quote) stay.
    let text =
        json!({"type": "string", "maxLength": 240, "pattern": "^[^\"\u{201c}\u{201d}\u{2018}]*$"});
    let mut line = json!({"speaker": s(40), "text": text});
    // How it's said: only when there are voices with a choice of deliveries (it costs
    // tokens on every line: ~8, 2026-11-21).
    if let Some(d) = allow.deliveries.as_ref().filter(|d| d.len() > 1) {
        line["tone"] = json!({"enum": d});
    }
    let mut v = json!({
        "type": "object",
        "properties": {
            "lines": {
                "type": "array",
                "minItems": 1,
                "maxItems": allow.max_lines.max(1),
                "items": {
                    "type": "object",
                    "properties": line,
                    "required": ["speaker", "text"]
                }
            },
            "media": {"anyOf": media_options},
            "state": {
                "type": "object",
                "properties": {
                    "set": {"type": "object", "maxProperties": 6, "additionalProperties": s(120)},
                    "clear": strs
                }
            },
            "summary": {"type": ["string", "null"], "maxLength": 600},
            "label": {"type": ["string", "null"], "maxLength": 60},
            "next": {"anyOf": next_options}
        },
        "required": ["lines", "media", "next"]
    });
    if allow.sounds {
        v["properties"]["sounds"] = json!({
            "type": "array",
            "maxItems": 2,
            "items": {
                "type": "object",
                "properties": {
                    "sound": s(160),
                    "line": {"type": "integer", "minimum": 0, "maximum": allow.max_lines.max(1) - 1},
                    "loop": {"type": "boolean"}
                },
                "required": ["sound"]
            }
        });
    }
    v
}

/// How the storyteller uses voices and sounds, when the story has them.
pub fn audio_guide(deliveries: bool, sounds: bool) -> String {
    let mut out = String::new();
    if deliveries {
        out.push_str("- \"tone\" (on a spoken line): how it's said, one of the speaker's deliveries listed with the objects. Leave it out for their usual way of speaking. Lines are spoken aloud: write them to be heard. Sighs, laughs and gasps belong in the words (\"Ha! You came.\"). Whatever someone says aloud is a line with their \"speaker\", even one word in the middle of a scene, never quoted inside narration (narration isn't spoken in their voice). How they say it goes in \"tone\" or in narration, never as their words (no line that is just \"dryly\").\n");
    }
    if sounds {
        out.push_str("- \"sounds\" (optional): [{\"sound\":\"...\",\"line\":0,\"loop\":false}]. A sound effect at a line (from 0), or with \"loop\":true a steady background sound (rain, wind, a crowd, a humming engine) that plays until another one or a new scene. Describe it concretely (\"a heavy wooden door creaks open slowly\"). Sounds are rare and slow to make: only when a sound truly matters to the moment, usually none; at most one background sound per scene. Never speech or music. Voices are made separately: never a sound for what someone says.\n");
    }
    if out.is_empty() {
        return out;
    }
    format!("\n# VOICES AND SOUNDS\n{out}")
}

pub fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "search_images".into(),
            description: "Search every image and video made so far in this story (any branch) by caption, objects or prompt. Returns ids you can reuse or pass as refs.".into(),
            parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
        },
        ToolDef {
            name: "recall".into(),
            description: "Search earlier beats on the current path for exact wording or details that are no longer in view.".into(),
            parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
        },
    ]
}

/// The engine's own prompt. Authors override any of this in plain English in their prompts.
pub const ENGINE_PROMPT: &str = r#"You are the storyteller of an interactive adventure story. The player reads one beat at a time and sometimes picks from choices you offer. You decide everything: who speaks, what they say, what the player sees, when to offer choices and how the world reacts.

Write ONE beat per reply as a JSON object:
- "lines": 1 to 6 short lines shown one at a time. "speaker" on every line, first: the name of whoever says it aloud, as the reader knows them now (once the story gives them a name, use it, not your first description of them), or "narration" when nobody does. A line someone says aloud is theirs: put the words alone in "text" (no quote marks, no "she says") and their name in "speaker"; narration describes, it never carries anyone's spoken words. Several people talking is several lines, one per speaker.

- "media": what is on screen.
  - "keep" keeps the current picture. Several beats usually play over one picture.
  - {"reuse":"<id>"} shows an earlier picture again. Great for conversations: switch between existing shots of each speaker instead of making new ones.
  - {"image":{"prompt":"...","caption":"...","objects":[names],"refs":[ids]}} makes a new picture. When to make one is up to the author's guidance on pictures and video below. "prompt" describes the full shot in rich, concrete detail (the picture model knows nothing of the story or the earlier pictures): who or what is in it and exactly how they look, their pose, expression, gaze and what they are doing in this moment, the place, light and time of day, mood, and the framing and camera angle. "caption" is a short label so you can find and reuse it later. "objects" names who and what is in it.
  - {"video":{...}} same fields plus optional "seconds": a moving scene, when the author's guidance calls for one. Its "prompt" describes the whole shot as fully as a picture's (who, how they look and are placed, the place, light, framing) and also what moves and how, from start to end.
  - "refs" (both): ids of existing images the new one draws on, from the pictures listed to you and the objects' reference pictures. List every image you use in "refs" (by id); in "prompt", call them <Picture 1>, <Picture 2>, ... in the order of "refs" (<Picture 1> is the first id in "refs"; a <Picture N> with no Nth ref means nothing) and say how each is used: keep a face or a look ("Mira's face from <Picture 1>, now in a storm coat"), include something, or, for a video, start exactly from it ("starts exactly as <Picture 1>, then ..."). Which images, and how, is your call (follow the author's guidance if it says). Every ref makes it slower to make: use only the ones that matter, usually none to two.
- "state": short notes you want to remember: {"set":{"key":"value"},"clear":["key"]}. Track what matters later (relationships, items, injuries, promises, where people are) and how each one in view is right now (where they are, their pose and what they're doing, clothes, hair, what they carry): pictures are drawn from these notes, so keep them current.
- "summary": every 5 to 10 beats, or when a chapter of events wraps up, a few sentences summarizing what happened since your last summary. Otherwise null.
- "label": a short scene name when a new scene starts, else null.
- "next": how the story continues.
  - "continue" for the next beat.
  - {"choices":[{"label":"...","likely":0.5},...]} to let the player decide (2 to 4 options, short labels in the player's voice). "likely" is your guess how probable each pick is. Most beats just continue. Offer choices only at real decision points, roughly one beat in three or four; never on two beats in a row unless the moment truly demands it.
  - {"rejoin":"<id>"} to continue into an existing beat listed under REJOIN TARGETS. Rejoin rarely: only once this branch's own consequences have played out and the story can truly flow back into that line. Only rejoin if that beat makes sense coming from here.
  - "end" only when the story is truly over (when it's offered at all).

Keep the story moving. Stay consistent with the objects, prompts, state and summaries you are given. Lines are read slowly, so keep each one short and vivid."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_and_colon_in_narration_becomes_the_speaker() {
        let line = |t: &str| Line {
            speaker: None,
            text: t.into(),
            tone: None,
        };
        let mut ls = vec![
            line("Ruth Calder: 'What is it you're looking for?'"),
            line("Ruth: Go on then."),
            line("Note: the tide is out."),
            line("She says it twice: once loud, once soft."),
        ];
        lift_speakers(&mut ls, &["Ruth Calder".into()]);
        assert_eq!(ls[0].speaker.as_deref(), Some("Ruth Calder"));
        assert_eq!(ls[0].text, "What is it you're looking for?");
        assert_eq!(ls[1].speaker.as_deref(), Some("Ruth"));
        assert_eq!(ls[1].text, "Go on then.");
        assert_eq!(ls[2].speaker, None);
        assert_eq!(ls[3].speaker, None);
    }

    #[test]
    fn quoted_narration_after_a_name_is_theirs_and_directions_go() {
        let n = |t: &str| Line {
            speaker: None,
            text: t.into(),
            tone: None,
        };
        let s = |who: &str, t: &str| Line {
            speaker: Some(who.into()),
            text: t.into(),
            tone: None,
        };
        let known = vec!["Jonah Reyes".to_string(), "Elena".to_string()];
        let mut ls = vec![
            n("I nodded, stepping closer to the counter."),
            n("\"We do. She's how old?\""),
            n("Jonah shifted the wooden box slightly."),
            n("\u{201c}Six. She thinks the moon is cheese.\u{201d}"),
            s("Jonah", "dryly"),
            n("Jonah and Elena both laughed."),
            n("\"Well then.\""),
            n("\"First,\" he said, \"tea.\""),
        ];
        tidy_lines(&mut ls, &known);
        assert_eq!(ls.len(), 7, "the direction went: {ls:?}");
        // The player's words stay narration.
        assert_eq!(ls[1].speaker, None);
        assert_eq!(ls[3].speaker.as_deref(), Some("Jonah Reyes"));
        assert_eq!(ls[3].text, "Six. She thinks the moon is cheese.");
        // Two named: nobody can tell who.
        assert_eq!(ls[5].speaker, None);
        // Quotes with narration between them stay as written.
        assert_eq!(ls[6].speaker, None);
        // A beat that is only a direction keeps it.
        let mut only = vec![s("Jonah", "(softly)")];
        tidy_lines(&mut only, &known);
        assert_eq!(only.len(), 1);
    }

    #[test]
    fn partial_lines_from_a_reply_in_progress() {
        let full = r#"{"lines":[{"speaker":null,"text":"Rain {on} \"glass\"."},{"speaker":"Mira","text":"Wait."},{"speaker":null,"text":"Half"#;
        let l = partial_lines(full);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].text, "Rain {on} \"glass\".");
        assert_eq!(l[1].speaker.as_deref(), Some("Mira"));
        assert!(partial_lines(r#"{"lin"#).is_empty());
        let done = r#"{"lines":[{"speaker":null,"text":"A"}],"media":"keep","next":"end"}"#;
        assert_eq!(partial_lines(done).len(), 1);
    }

    #[test]
    fn deliveries_and_sounds_are_offered_only_when_there_are_some() {
        let plain = schema().to_string();
        assert!(!plain.contains("\"tone\"") && !plain.contains("\"sounds\""));
        // Speech can't hide in narration as a quotation.
        let pat = schema()["properties"]["lines"]["items"]["properties"]["text"]["pattern"]
            .as_str()
            .unwrap()
            .to_string();
        let re = |t: &str| {
            // The pattern is one negated class: no character of it may appear.
            let banned: Vec<char> = pat[2..pat.len() - 3].chars().collect();
            !t.chars().any(|c| banned.contains(&c))
        };
        // Someone says every line (narration by name), decided first.
        assert_eq!(
            schema()["properties"]["lines"]["items"]["required"],
            json!(["speaker", "text"])
        );
        let b = parse(r#"{"lines":[{"speaker":"narration","text":"Rain."},{"speaker":"Mira","text":"Wait."}],"media":"keep","next":"end"}"#).unwrap();
        assert_eq!(b.lines[0].speaker, None);
        assert_eq!(b.lines[1].speaker.as_deref(), Some("Mira"));
        let p = partial_lines(r#"{"lines":[{"speaker":"Narration","text":"Rain."},{"speak"#);
        assert_eq!(p[0].speaker, None);
        assert!(re("Don't you dare, Elena's waiting."));
        assert!(!re("\"Six,\" he says."));
        assert!(!re("\u{201c}Six.\u{201d}"));
        let v = schema_with(&Allow {
            deliveries: Some(vec!["natural".into(), "angry".into()]),
            sounds: true,
            ..Allow::default()
        });
        assert_eq!(
            v["properties"]["lines"]["items"]["properties"]["tone"]["enum"],
            json!(["natural", "angry"])
        );
        let snd = &v["properties"]["sounds"];
        assert_eq!(snd["maxItems"], json!(2));
        assert_eq!(snd["items"]["additionalProperties"], json!(false));
        // One delivery is no choice: no field.
        let one = schema_with(&Allow {
            deliveries: Some(vec!["natural".into()]),
            ..Allow::default()
        });
        assert!(
            one["properties"]["lines"]["items"]["properties"]
                .get("tone")
                .is_none()
        );
        let b = parse(r#"{"lines":[{"speaker":"Mira","text":"Out!","tone":"angry"}],"media":"keep","next":"continue","sounds":[{"sound":"rain on glass","line":0,"loop":true}]}"#).unwrap();
        assert_eq!(b.lines[0].tone.as_deref(), Some("angry"));
        assert_eq!(b.sounds.len(), 1);
        assert!(b.sounds[0].looped);
    }

    #[test]
    fn parse_tolerates_wrapping() {
        let r = r#"Sure! ```json
        {"lines":[{"speaker":null,"text":"Rain."}],"media":"keep","next":"continue"}
        ```"#;
        let b = parse(r).unwrap();
        assert_eq!(b.lines[0].text, "Rain.");
        assert_eq!(b.media, MediaOut::Keep);
        assert!(parse(r#"{"lines":[],"next":"end"}"#).is_err());
        assert!(parse(r#"{"lines":[{"text":"x"}],"next":{"choices":[]}}"#).is_err());
        let b = parse(
            r#"{"lines":[{"speaker":"Mira","text":"Look."}],"media":{"image":{"caption":"c","prompt":"p"}},"next":{"choices":[{"label":"Go"},{"label":"Stay","likely":0.2}]}}"#,
        )
        .unwrap();
        assert!(matches!(b.media, MediaOut::Image { .. }));
        assert!(matches!(&b.next, NextOut::Choices(c) if c.len() == 2));
        let b = parse(r#"{"lines":[{"text":"x"}],"media":{"reuse":"m1"},"next":{"rejoin":"b2"}}"#)
            .unwrap();
        assert_eq!(b.media, MediaOut::Reuse("m1".into()));
        assert_eq!(b.next, NextOut::Rejoin("b2".into()));
    }

    /// Every option of every choice point opens with its own key, whatever order a
    /// server puts an object's keys in, so a grammar can never lock options out.
    #[test]
    fn options_are_distinguishable_in_any_key_order() {
        fn check(v: &Value, at: &str) {
            if let Some(opts) = v.get("anyOf").and_then(Value::as_array) {
                let mut seen = std::collections::HashSet::new();
                for o in opts {
                    let first = if let Some(c) = o.get("const") {
                        c.to_string()
                    } else {
                        let props = o["properties"].as_object().expect("object option");
                        assert_eq!(props.len(), 1, "{at}: option with several keys: {o}");
                        props.keys().next().unwrap().clone()
                    };
                    assert!(
                        seen.insert(first.clone()),
                        "{at}: two options open with {first}"
                    );
                }
            }
            match v {
                Value::Object(m) => m.iter().for_each(|(k, x)| check(x, &format!("{at}/{k}"))),
                Value::Array(a) => a.iter().for_each(|x| check(x, at)),
                _ => {}
            }
        }
        check(&schema(), "");
        let tight = Allow {
            choices: false,
            must_choose: false,
            new_media: false,
            rejoin: Some(vec![]),
            reuse: Some(vec!["m1".into()]),
            refs: Some(vec![]),
            max_lines: 3,
            must_show: false,
            end: true,
            deliveries: None,
            sounds: false,
        };
        check(&schema_with(&tight), "");
        let t = schema_with(&tight).to_string();
        for gone in ["\"choices\"", "\"rejoin\"", "\"image\"", "\"video\""] {
            assert!(!t.contains(gone), "{gone} should be left out");
        }
        assert!(t.contains("\"enum\":[\"m1\"]"));
        let first = schema_with(&Allow {
            must_show: true,
            ..Allow::default()
        })
        .to_string();
        assert!(
            !first.contains("\"const\":\"keep\""),
            "a blank screen gets a picture"
        );
        // References: only the listed images, never free text.
        let r = schema_with(&Allow {
            refs: Some(vec!["m7".into()]),
            ..Allow::default()
        });
        // Found by structure, never by text (key order isn't guaranteed).
        fn find_refs(v: &Value) -> Option<&Value> {
            match v {
                Value::Object(m) => m
                    .get("image")
                    .and_then(|i| i["properties"].get("refs"))
                    .or_else(|| m.values().find_map(find_refs)),
                Value::Array(a) => a.iter().find_map(find_refs),
                _ => None,
            }
        }
        let refs = find_refs(&r).expect("image refs in the schema");
        assert_eq!(refs["items"]["enum"], json!(["m7"]), "{refs}");
    }
}
