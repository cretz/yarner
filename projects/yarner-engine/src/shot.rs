//! The shot step: once a beat asks for a new picture or video, one more short turn
//! (continuing the beat's own session, so only this turn is read) writes it out in full
//! for the picture/video model, which has never read the story and sees nothing but the
//! words it's given.
//!
//! Inside the beat's reply the shot was one field among the lines, choices and state, and
//! small models wrote it in a hurry ("Mira at the gate"); the picture model then copied a
//! reference picture or made things up. Here it's the only job, with the story's
//! descriptions and notes of what may be in view, the last pictures for continuity, an
//! example of the density wanted, and fields to fill. All of it generic: a shot has a
//! camera, a place, what's in view and (for a video) motion and sound. What's in view is
//! the storyteller's call; the checks only name gaps in what it said is there.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Graph, Hop, MediaKind, MediaStatus, Resolver, STYLE_PROMPT, Show, state_at};

/// One subject in view: a person, creature or thing that matters to the shot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Subject {
    #[serde(default)]
    pub who: String,
    /// How much of them is in view: "whole", "most", "face" or "part" (a hand, a back:
    /// no reference picture of them is needed, and a face one would be painted in).
    #[serde(default)]
    pub in_view: String,
    #[serde(default)]
    pub look: String,
    #[serde(default)]
    pub doing: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Shot {
    #[serde(default)]
    pub camera: String,
    #[serde(default)]
    pub subjects: Vec<Subject>,
    #[serde(default)]
    pub place: String,
    #[serde(default)]
    pub details: String,
    #[serde(default)]
    pub mood: String,
    #[serde(default)]
    pub motion: String,
    /// A video's moments in order: what happens when (who says what, when).
    #[serde(default)]
    pub timeline: Vec<Moment>,
    #[serde(default)]
    pub sound: String,
}

/// One stretch of a video: its time ("0-2 s") and what happens in it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Moment {
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub what: String,
}

/// Moments a video's timeline needs at least.
const MOMENTS: usize = 3;

/// The reply's schema (motion and sound only for a video).
pub fn schema(video: bool) -> Value {
    let s = json!({"type": "string"});
    let mut props = json!({
        "camera": s,
        "subjects": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "who": s,
                    "in_view": {"type": "string", "enum": ["whole", "most", "face", "part"]},
                    "look": s,
                    "doing": s,
                },
                "required": ["who", "in_view", "look", "doing"],
            },
        },
        "place": s,
        "details": s,
        "mood": s,
    });
    let mut required = vec!["camera", "subjects", "place", "details", "mood"];
    if video {
        props["motion"] = s.clone();
        props["timeline"] = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {"at": s, "what": s},
                "required": ["at", "what"],
            },
        });
        props["sound"] = s.clone();
        required.extend(["motion", "timeline", "sound"]);
    }
    json!({"type": "object", "properties": props, "required": required})
}

pub fn parse(reply: &str) -> Result<Shot, String> {
    let t = reply.trim();
    let start = t.find('{').ok_or("no JSON object in reply")?;
    let end = t.rfind('}').ok_or("no JSON object in reply")?;
    serde_json::from_str(&t[start..=end]).map_err(|e| format!("bad shot JSON: {e}"))
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

impl Shot {
    /// What's missing or too thin, said so it can be fixed ("the place is too thin").
    /// `named`: who and what the storyteller said is in it (the beat's media objects).
    pub fn gaps(&self, video: bool, named: &[String]) -> Vec<String> {
        let mut gaps = Vec::new();
        if words(&self.camera) < 4 {
            gaps.push("\"camera\" is missing or too thin: say the framing, angle and where things sit in the frame".to_string());
        }
        if words(&self.place) < 8 {
            gaps.push("\"place\" is too thin: say where it is, the light, the time of day and the background".to_string());
        }
        for s in &self.subjects {
            let who = if s.who.trim().is_empty() {
                "a subject"
            } else {
                s.who.trim()
            };
            if words(&s.look) < 8 {
                gaps.push(format!(
                    "\"{who}\" has too little \"look\": say exactly how they look right now"
                ));
            }
            if words(&s.doing) < 4 {
                gaps.push(format!(
                    "\"{who}\" has too little \"doing\": say their pose, expression and action"
                ));
            }
        }
        // Someone or something the storyteller put in the shot, and nobody described:
        // the picture model would invent them. (Matching names was too strict: a station
        // named "the Spindle" described as "the station" cost a retry; 2026-11-23.)
        if self.subjects.is_empty() && named.iter().any(|n| !n.trim().is_empty()) {
            gaps.push(format!(
                "\"subjects\" is empty, but the shot shows {}: describe each one in view",
                named.join(", ")
            ));
        }
        if video {
            if words(&self.motion) < 8 {
                gaps.push("\"motion\" is too thin: say what moves and how, from start to end, and the camera".to_string());
            }
            if self.timeline.len() < MOMENTS {
                gaps.push(format!("\"timeline\" needs at least {MOMENTS} moments covering the whole clip, in order: what happens in each second or two, and who says what when"));
            } else if self.timeline.iter().any(|m| words(&m.what) < 5) {
                gaps.push(
                    "\"timeline\" has a thin moment: say exactly what is seen and heard then"
                        .to_string(),
                );
            }
            if words(&self.sound) < 2 {
                gaps.push("\"sound\" is missing: the ambient sound, effects and music".to_string());
            }
        }
        gaps
    }

    /// The description sent to the picture/video model: the shot, then what's in it,
    /// the place and the rest (motion and sound last, for a video).
    pub fn compose(&self, video: bool) -> String {
        fn part(s: &str) -> Option<String> {
            let s = s.trim().trim_end_matches(['.', ';', ',']).trim();
            (!s.is_empty()).then(|| format!("{s}."))
        }
        let mut out: Vec<String> = Vec::new();
        out.extend(part(&self.camera));
        for s in &self.subjects {
            let body = [s.look.trim(), s.doing.trim()]
                .into_iter()
                .filter(|x| !x.is_empty())
                .map(|x| x.trim_end_matches(['.', ';', ',']))
                .collect::<Vec<_>>()
                .join("; ");
            let line = match (s.who.trim(), body.is_empty()) {
                (w, false) if !w.is_empty() => format!("{w}: {body}"),
                (_, false) => body,
                (w, true) => w.to_string(),
            };
            out.extend(part(&line));
        }
        out.extend(part(&self.place));
        out.extend(part(&self.details));
        out.extend(part(&self.mood));
        if video {
            if let Some(m) = part(&self.motion) {
                out.push(format!("Motion: {m}"));
            }
            let moments: Vec<String> = self
                .timeline
                .iter()
                .filter(|m| !m.what.trim().is_empty())
                .map(|m| {
                    let what = m.what.trim().trim_end_matches(['.', ';', ',']);
                    match m.at.trim() {
                        "" => what.to_string(),
                        at => format!("{at}: {what}"),
                    }
                })
                .collect();
            if !moments.is_empty() {
                out.push(format!("Timeline: {}.", moments.join("; ")));
            }
            let sound = self.sound.trim();
            if !sound.is_empty() && !sound.eq_ignore_ascii_case("none") {
                out.extend(part(&format!("Sound: {sound}")));
            }
        }
        out.join(" ")
    }
}

/// Whether a subject (`who`, as the shot step wrote it) is the object `name`: any
/// distinctive word of the name will do ("Ruth" for "Ruth Calder").
pub fn names_match(who: &str, name: &str) -> bool {
    let who: Vec<String> = who
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let name = name.to_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !["the", "and", "of"].contains(w))
        .collect();
    if words.is_empty() {
        return who.join(" ") == name.trim();
    }
    words.iter().any(|w| who.iter().any(|x| x == w))
}

/// Which fields the gaps are about ("look,place"), for the content-free log.
pub fn gap_kinds(gaps: &[String]) -> String {
    let mut kinds: Vec<&str> = gaps
        .iter()
        .map(|g| {
            for k in [
                "camera", "place", "look", "doing", "motion", "timeline", "sound",
            ] {
                if g.contains(&format!("\"{k}\"")) {
                    return k;
                }
            }
            if g.contains("\"subjects\" is empty") {
                "unnamed"
            } else {
                "invalid"
            }
        })
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds.join(",")
}

/// A heading the mock (and any reader of a session) knows the shot turn by.
pub const HEADING: &str = "# THE SHOT";

/// The shot turn: what to do, what may be in view, the last pictures, the style.
#[allow(clippy::too_many_arguments)]
pub fn turn(
    graph: &Graph,
    path: &[Hop],
    kind: MediaKind,
    idea: &str,
    objects: &[String],
    refs: &[String],
    lines: &[String],
    seconds: f32,
) -> String {
    let video = kind == MediaKind::Video;
    let (what, model) = if video {
        ("video", "video model")
    } else {
        ("picture", "picture model")
    };
    let mut t = format!("{HEADING}\n");
    // What to do and how is in the system prompt ([`guide`], read once per session); this
    // turn carries only what's particular to this shot.
    t.push_str(&format!(
        "The beat you just wrote shows a new {what}: write it out in full for the {model}, as WRITING THE SHOT says.{length}\n",
        length = if video {
            format!(" The clip lasts {} seconds.", seconds.round() as u32)
        } else {
            String::new()
        },
    ));
    t.push_str(&format!("Your idea for it: {}\n", idea.trim()));
    if !objects.is_empty() {
        t.push_str(&format!("You said it shows: {}\n", objects.join(", ")));
    }
    if !refs.is_empty() {
        t.push_str("Reference pictures (name them exactly so, and say what each gives: a face, a look, a place, or for a video \"starts exactly as <Picture 1>\"):\n");
        for (i, r) in refs.iter().enumerate() {
            let caption = graph
                .media
                .get(r)
                .map(|m| m.caption.trim().to_string())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| "a reference picture".into());
            t.push_str(&format!("- <Picture {}>: {caption}\n", i + 1));
        }
    }
    let res = Resolver::new(graph, path, None);
    let said = format!("{} {} {}", idea, objects.join(" "), lines.join(" ")).to_lowercase();
    let in_view: Vec<_> = res
        .objects()
        .into_iter()
        .filter(|o| {
            let n = o.name.to_lowercase();
            !n.is_empty() && said.contains(&n)
        })
        .collect();
    let notes = state_at(graph, path);
    if !in_view.is_empty() {
        t.push_str("\n# WHO AND WHAT MAY BE IN VIEW (from the story; describe only what is actually in this shot)\n");
        for o in &in_view {
            t.push_str(&format!("- {}: {}\n", o.name, o.description.trim()));
            let n = o.name.to_lowercase();
            let now: Vec<String> = notes
                .iter()
                .filter(|(k, v)| k.to_lowercase().contains(&n) || v.to_lowercase().contains(&n))
                .take(6)
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            if !now.is_empty() {
                t.push_str(&format!("  Now: {}\n", now.join("; ")));
            }
        }
    }
    // The last pictures on this path, for continuity (the place, the looks).
    let mut last: Vec<&str> = Vec::new();
    for h in path.iter().rev() {
        let Some(b) = graph.beats.get(&h.beat) else {
            continue;
        };
        let shown = match &b.show {
            Show::New { media } | Show::Reuse { media } => Some(media),
            Show::Keep => None,
        };
        if let Some(m) = shown
            .and_then(|id| graph.media.get(id))
            .filter(|m| m.status != MediaStatus::Failed && !m.prompt.trim().is_empty())
            && !last.contains(&m.prompt.as_str())
        {
            last.push(&m.prompt);
        }
        if last.len() >= 2 {
            break;
        }
    }
    if !last.is_empty() {
        t.push_str("\n# THE LAST PICTURES ON SCREEN (most recent first; keep the place and looks consistent unless the story changed them, but frame this one differently: another angle, distance or composition, another expression or action, so it never looks like a copy of them)\n");
        for p in &last {
            let p: String = p.chars().take(700).collect();
            t.push_str(&format!("- {}\n", p.trim()));
        }
    }
    if let Some(s) = res
        .prompt(STYLE_PROMPT)
        .map(|p| p.text.trim())
        .filter(|s| !s.is_empty())
    {
        t.push_str(&format!("\n# STYLE\n{s}\n"));
    }
    t.push_str(if video {
        "\nReply with only the shot JSON object, with \"motion\", \"timeline\" and \"sound\".\n"
    } else {
        "\nReply with only the shot JSON object.\n"
    });
    t
}

/// How to write a shot, once in the system prompt (so a shot turn is short and its
/// instructions are read once per session, not with every picture: ~800 tokens, ~5 s).
pub fn guide() -> String {
    format!(
        "# WRITING THE SHOT
After some beats you are asked (a turn starting \"{HEADING}\") to write out the new picture or video that beat shows, for the picture or video model. That model has never read the story and sees nothing but your words (and the reference pictures named): whatever you leave out is missing or made up, and a name alone means nothing to it. Describe exactly what is on screen at that moment, concretely and specifically, as a director briefs a painter or a film crew: plain visual words, specific colours, materials, shapes and positions, never vague ones (\"nice clothes\", \"a room\"). Show that exact moment, not a portrait from the setup: start from what you know of each one, then change whatever the moment changes (what is worn, held or carried and how, posture, state), and describe only the state it is in now: never both the old and the new (not a hat \"on the head\" and \"in the hand\"). Check it against your lines: whatever they say is visible now (someone sitting, something in a hand, a light on) must be in the shot.

Reply to that turn with only a JSON object:
- \"camera\": the framing (close-up, medium or wide shot), the angle, and where things sit in the frame.
- \"subjects\": each person, creature or thing that matters and is visible in this shot (only those; an empty list if no one is in it). The player (\"you\") is the camera, never a subject: if their hands or body show, say so in \"details\": {{\"who\": name, \"in_view\": how much of them is in view (\"whole\", \"most\", \"face\", or \"part\": only a hand, a back, a corner), \"look\": exactly how they look right now (build, age, face, hair, clothing and its colours, what they wear or carry, any visible state; for a thing its shape, size, material, colour and condition), \"doing\": first the body position (sitting, standing, kneeling, lying, walking, leaning on...), then the pose, expression, where they look, what they are doing with their hands, and where they are in the frame}}.
- \"place\": where it is, the time of day, the light (its source, colour and direction), the weather, and the background in detail.
- \"details\": anything else in view that matters to this moment.
- \"mood\": the feeling and the colour palette.
For a video, also:
- \"motion\": the overall movement and what the camera does (still, pan, push in).
- \"timeline\": the clip second by second, in order, covering all of it (the turn says how long it lasts): [{{\"at\": \"0-1 s\", \"what\": exactly what is seen and heard then}}, ...], at least three moments. Anyone speaking on screen: NAME says: \"their exact words\" in the moment they say them (from your lines).
- \"sound\": everything heard besides the words spoken (those are in the timeline, once): the ambient sound, sound effects and any music, and how loud. \"none\" only for a silent shot.

Examples, for a made-up scene (match their detail, not their content).
A picture: {EXAMPLE_IMAGE}
A video: {EXAMPLE_VIDEO}
"
    )
}

const EXAMPLE_IMAGE: &str = r#"{"camera":"Medium shot at eye level, the baker on the left third, the doorway and street behind her on the right.","subjects":[{"who":"Old baker","in_view":"most","look":"A broad woman in her sixties, short grey hair under a flour-dusted blue headscarf, round red cheeks, a white apron over a rust-orange wool dress, sleeves rolled to the elbow, forearms white with flour","doing":"Leaning on the counter with both hands, smiling warmly, looking straight at the viewer as if greeting a customer"}],"place":"A small stone bakery at dawn: warm yellow light from the oven glow on the left, cool blue morning light through the open door, wooden shelves of round loaves, a chalkboard menu, a cobbled street outside still wet from rain.","details":"A tray of steaming bread on the counter, a tabby cat asleep on a flour sack in the corner.","mood":"Cosy and welcoming; warm ambers against cool blues."}"#;

const EXAMPLE_VIDEO: &str = r#"{"camera":"Medium shot at eye level, the baker on the left third, the doorway behind her on the right.","subjects":[{"who":"Old baker","in_view":"most","look":"A broad woman in her sixties, short grey hair under a flour-dusted blue headscarf, round red cheeks, a white apron over a rust-orange wool dress, sleeves rolled to the elbow","doing":"Behind the counter, lifting a steaming loaf from a tray, then looking up at the viewer with a warm smile"}],"place":"A small stone bakery at dawn: warm oven glow from the left, cool blue light through the open door, shelves of round loaves, a wet cobbled street outside.","details":"Steam curling from the tray, a tabby cat on a flour sack.","mood":"Cosy and welcoming; warm ambers against cool blues.","motion":"She lifts the loaf, turns it in her hands and leans toward the viewer; the camera pushes in slowly.","timeline":[{"at":"0-1 s","what":"She lifts a steaming loaf from the tray with both hands, steam curling up past her face"},{"at":"1-3 s","what":"She turns the loaf to show its golden crust and looks up at the viewer, smiling; Old baker says: \"First batch of the day, still warm.\""},{"at":"3-5 s","what":"She sets the loaf on the counter and leans forward on her floury hands, still smiling, as the cat lifts its head"}],"sound":"A crackling oven, rain dripping softly outside, a distant church bell, no music."}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thin_shot_names_its_gaps_and_a_full_one_has_none() {
        let thin = Shot {
            camera: "close".into(),
            subjects: vec![Subject {
                who: "Mira".into(),
                look: "tall".into(),
                doing: "stands".into(),
                ..Default::default()
            }],
            place: "a gate".into(),
            ..Default::default()
        };
        let g = thin.gaps(true, &["Mira".into(), "The Lighthouse".into()]);
        // Someone named, nobody described: asked again for them.
        let nobody = Shot::default().gaps(false, &["Ruth Calder".into()]);
        assert!(
            nobody.iter().any(|x| x.contains("\"subjects\" is empty")),
            "{nobody:?}"
        );
        assert!(g.iter().any(|x| x.contains("camera")), "{g:?}");
        assert!(g.iter().any(|x| x.contains("place")), "{g:?}");
        assert!(
            g.iter()
                .any(|x| x.contains("\"Mira\" has too little \"look\"")),
            "{g:?}"
        );
        // Subjects given: names aren't matched word for word.
        assert!(
            !g.iter().any(|x| x.contains("\"subjects\" is empty")),
            "{g:?}"
        );
        assert!(g.iter().any(|x| x.contains("motion")), "{g:?}");
        assert!(g.iter().any(|x| x.contains("timeline")), "{g:?}");
        assert!(g.iter().any(|x| x.contains("sound")), "{g:?}");
        let full = parse(EXAMPLE_VIDEO).unwrap();
        assert!(full.gaps(true, &["old baker".into()]).is_empty());
        // Who's in it is the storyteller's call: nobody named, nobody required.
        let empty = Shot {
            subjects: vec![],
            ..full.clone()
        };
        assert!(empty.gaps(false, &[]).is_empty());
    }

    #[test]
    fn the_description_leads_with_the_shot_and_ends_with_sound() {
        let s = parse(EXAMPLE_VIDEO).unwrap();
        let p = s.compose(true);
        assert!(p.starts_with("Medium shot"), "{p}");
        assert!(p.contains("Old baker: A broad woman"), "{p}");
        assert!(p.contains("Motion: She lifts"), "{p}");
        // What happens when, the words said once, in their moment.
        assert!(p.contains("Timeline: 0-1 s: She lifts"), "{p}");
        assert_eq!(p.matches("Old baker says:").count(), 1, "{p}");
        assert!(p.trim_end().ends_with("no music."), "{p}");
        let img = parse(EXAMPLE_IMAGE).unwrap().compose(false);
        assert!(!img.contains("Motion:") && !img.contains("Sound:"), "{img}");
        assert!(img.split_whitespace().count() > 80, "{img}");
    }
}
