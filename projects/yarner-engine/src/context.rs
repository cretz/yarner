//! Building what the LLM sees for one generation, and answering its tool calls.
//!
//! Ordered for KV-cache prefix reuse: the system message (engine prompt, story prompts,
//! objects) changes rarely; the user message (summaries, state, pictures, recent beats)
//! changes every beat.

use std::collections::HashSet;
use std::fmt::Write;

use serde_json::{Value, json};

use crate::backend::Message;
use crate::contract::ENGINE_PROMPT;
use crate::model::{
    DEFAULT_MEDIA_GUIDE, Graph, Hop, MEDIA_PROMPT, MediaStatus, Next, PICTURE_PROMPT, Resolver,
    STEER_PROMPT, STORY_PROMPT, STYLE_PROMPT, Scope,
};
use crate::model::{Show, state_at};

pub struct Limits {
    pub recent_beats: usize,
    pub summaries: usize,
    pub pictures: usize,
    pub rejoin_targets: usize,
    /// Pictures and earlier lines the engine looks up for each beat.
    pub lookups: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            recent_beats: 12,
            summaries: 6,
            pictures: 24,
            rejoin_targets: 8,
            lookups: 4,
        }
    }
}

fn line_text(speaker: &Option<String>, text: &str) -> String {
    match speaker {
        Some(s) => format!("{s}: {text}"),
        None => text.to_string(),
    }
}

fn choice_label(graph: &Graph, hop: &Hop) -> Option<String> {
    let b = graph.beats.get(&hop.beat)?;
    match (&b.next, hop.exit) {
        // A choice the player wrote themselves: said so, so the story takes it as a free
        // action (and what else they wrote) rather than one of its own options.
        (Next::Choices { choices }, Some(e)) => choices.get(e).map(|c| {
            if c.own {
                format!(
                    "{} [the player wrote this themselves, not one of your options: act on it as best fits the story]",
                    c.label
                )
            } else {
                c.label.clone()
            }
        }),
        _ => None,
    }
}

/// Messages for generating the beat after `parent` (or, with `target`, regenerating that
/// existing beat, which sits after `parent`).
pub fn messages(graph: &Graph, parent: &[Hop], target: Option<&str>, lim: &Limits) -> Vec<Message> {
    let (sys, user) = parts(graph, parent, target, lim);
    vec![Message::new("system", sys), Message::new("user", user)]
}

/// A beat's generation as a chat, for backends that keep a conversation's processed
/// state (KV cache sessions).
///
/// The story is cut into segments. A segment's first turn carries the full context
/// (summaries, notes, pictures, recent beats); each later beat adds a short turn (the
/// choice taken, new pictures, rejoin targets) and its reply. A segment ends where the
/// system prompt changes (a revision anchored on the path) or after `segment` beats, so
/// the conversation stays short. Every turn and reply is stored on its beat, so the chat
/// is always rebuildable without the backend's cache.
pub struct Chat {
    pub system: String,
    /// Hash of `system` (stored on the new beat).
    pub sys: u64,
    /// Earlier beats of this segment: (beat, user turn, reply).
    pub history: Vec<(String, String, String)>,
    /// The turn that asks for this beat.
    pub user: String,
}

impl Chat {
    /// Starts a new segment (the turn carries the full context).
    pub fn seg_start(&self) -> bool {
        self.history.is_empty()
    }

    /// The whole conversation.
    pub fn messages(&self) -> Vec<Message> {
        let mut m = vec![Message::new("system", self.system.clone())];
        for (_, u, a) in &self.history {
            m.push(Message::new("user", u.clone()));
            m.push(Message::new("assistant", a.clone()));
        }
        m.push(Message::new("user", self.user.clone()));
        m
    }
}

pub fn hash_text(s: &str) -> u64 {
    // FNV-1a: stable across runs and builds (stored on beats).
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

pub fn chat(
    graph: &Graph,
    parent: &[Hop],
    target: Option<&str>,
    lim: &Limits,
    segment: usize,
) -> Chat {
    let (system, full) = parts(graph, parent, target, lim);
    let sys = hash_text(&system);
    // Walk back through this segment: same system prompt, turns recorded, until its start.
    let mut history = Vec::new();
    let mut complete = false;
    for h in parent.iter().rev() {
        let Some(b) = graph.beats.get(&h.beat) else {
            break;
        };
        let (Some(turn), Some(reply)) = (&b.turn, &b.reply) else {
            break;
        };
        if b.sys != sys {
            break;
        }
        history.push((b.id.clone(), turn.clone(), reply.clone()));
        if b.seg_start {
            complete = true;
            break;
        }
    }
    history.reverse();
    if !complete || history.len() >= segment {
        return Chat {
            system,
            sys,
            history: vec![],
            user: full,
        };
    }
    let user = turn(graph, parent, target, lim, &history);
    Chat {
        system,
        sys,
        history,
        user,
    }
}

/// Who and what is in the scene right now (named or speaking in the last beat, or in the
/// picture on screen), as the setup describes them, near where the next beat is written:
/// a small model forgets a description far up the system prompt (a one-eyed robot got
/// "a pair of glowing blue eyes"; 2026-11-23).
fn here_now(u: &mut String, graph: &Graph, parent: &[Hop]) {
    let Some(last) = parent.last().and_then(|h| graph.beats.get(&h.beat)) else {
        return;
    };
    let mut said = last
        .lines
        .iter()
        .map(|l| format!("{} {}", l.speaker.as_deref().unwrap_or(""), l.text))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if let Some(on) = crate::model::showing_at(graph, parent) {
        said.push(' ');
        said.push_str(&on.objects.join(" ").to_lowercase());
    }
    let res = Resolver::new(graph, parent, None);
    let here: Vec<_> = res
        .objects()
        .into_iter()
        .filter(|o| {
            let n = o.name.trim().to_lowercase();
            !n.is_empty() && !o.description.trim().is_empty() && said.contains(&n)
        })
        .take(4)
        .collect();
    if here.is_empty() {
        return;
    }
    u.push_str("# HERE NOW (as the story set them up; stay true to these)\n");
    for o in here {
        let d: String = o.description.trim().chars().take(300).collect();
        let _ = writeln!(u, "- {}: {d}", o.name);
    }
}

/// The short turn for a beat inside a segment.
fn turn(
    graph: &Graph,
    parent: &[Hop],
    target: Option<&str>,
    lim: &Limits,
    history: &[(String, String, String)],
) -> String {
    let mut u = String::new();
    if let Some(h) = parent.last() {
        if let Some(c) = choice_label(graph, h) {
            let _ = writeln!(u, "(The player chose: {c})");
        }
        let new: Vec<&crate::model::Media> = graph
            .media
            .values()
            .filter(|m| {
                m.beat.as_deref() == Some(h.beat.as_str()) && m.status != MediaStatus::Failed
            })
            .collect();
        if !new.is_empty() {
            u.push_str(
                "# NEW PICTURES (ids you can reuse or pass as refs)
",
            );
            for m in new {
                let _ = writeln!(
                    u,
                    "- {} \"{}\" objects: {}",
                    m.id,
                    m.caption,
                    m.objects.join(", ")
                );
            }
        }
    }
    if let Some(on) = crate::model::showing_at(graph, parent) {
        let _ = writeln!(u, "On screen now: {}", on.id);
    }
    here_now(&mut u, graph, parent);
    let targets = rejoin_targets(graph, parent, lim.rejoin_targets.min(5));
    if !targets.is_empty() {
        u.push_str(
            "# REJOIN TARGETS (existing beats you may flow into)
",
        );
        for (id, desc) in &targets {
            let _ = writeln!(u, "- {id}: {desc}");
        }
    }
    // What the engine looked up for this moment, minus what this segment already saw.
    let said = |x: &str| {
        history
            .iter()
            .any(|(_, t, a)| t.contains(x) || a.contains(x))
    };
    let cue = cue(graph, parent);
    let pics: Vec<String> = relevant_pictures(graph, &cue, &HashSet::new(), lim.lookups)
        .into_iter()
        .filter(|id| !said(id))
        .collect();
    if !pics.is_empty() {
        u.push_str("# PICTURES THAT MAY FIT NOW\n");
        for id in &pics {
            picture_line(&mut u, &graph.media[id]);
        }
    }
    // Lines from before this segment's chat (its first turn showed the recent beats).
    let seg_from = parent.len().saturating_sub(history.len());
    let before = seg_from.saturating_sub(lim.recent_beats);
    let mem: Vec<(usize, String)> = memories(graph, parent, before, &cue, lim.lookups)
        .into_iter()
        .filter(|(_, l)| !said(l))
        .collect();
    if !mem.is_empty() {
        u.push_str("# EARLIER MOMENTS THAT MAY MATTER NOW\n");
        for (i, l) in &mem {
            let _ = writeln!(u, "- (beat {}) {l}", i + 1);
        }
    }
    u.push_str(&pacing(graph, parent));
    if let Some(t) = target {
        u.push_str(
            "Write a fresh take on the next beat (the previous take was rejected).
",
        );
        if let Some(p) = here_direction(graph, t) {
            let _ = writeln!(u, "Direction for this beat: {p}");
        }
        if let Some(p) = next_direction(graph, parent) {
            let _ = writeln!(u, "Direction for this beat: {p}");
        }
    } else {
        if let Some(p) = next_direction(graph, parent) {
            let _ = writeln!(u, "Direction for this beat (from the author): {p}");
        }
        u.push_str(
            "Write the next beat.
",
        );
    }
    u
}

/// A direction given for just the beat after `parent` ("take another direction next").
fn next_direction<'a>(graph: &'a Graph, parent: &[Hop]) -> Option<&'a str> {
    let after = parent.last()?.beat.as_str();
    graph
        .prompts
        .iter()
        .filter(|p| p.scope == Scope::Next && p.anchor.as_deref() == Some(after))
        .filter(|p| !graph.retired.contains(&p.rev))
        .max_by_key(|p| p.at)
        .map(|p| p.text.as_str())
        .filter(|t| !t.trim().is_empty())
}

fn here_direction<'a>(graph: &'a Graph, beat: &str) -> Option<&'a str> {
    graph
        .prompts
        .iter()
        .filter(|p| p.scope == Scope::Here && p.anchor.as_deref() == Some(beat))
        .filter(|p| !graph.retired.contains(&p.rev))
        .max_by_key(|p| p.at)
        .map(|p| p.text.as_str())
}

/// The system prompt and the full-context user turn.
fn parts(graph: &Graph, parent: &[Hop], target: Option<&str>, lim: &Limits) -> (String, String) {
    let res = Resolver::new(graph, parent, target);
    let mut sys = String::from(ENGINE_PROMPT);
    sys.push_str("\n\n");
    sys.push_str(&crate::shot::guide());

    sys.push_str("\n\n# STORY SETUP\n");
    let prompts = res.prompts();
    for p in prompts.iter().filter(|p| p.id == STORY_PROMPT) {
        let _ = writeln!(sys, "\n## Story\n{}", p.text.trim());
    }
    for p in prompts.iter().filter(|p| p.id == STYLE_PROMPT) {
        let _ = writeln!(
            sys,
            "\n## Visual style (applies to every picture)\n{}",
            p.text.trim()
        );
    }
    let guide = prompts
        .iter()
        .find(|p| p.id == MEDIA_PROMPT)
        .map(|p| p.text.trim())
        .filter(|t| !t.is_empty())
        .unwrap_or(DEFAULT_MEDIA_GUIDE);
    let video = res.settings().video;
    let _ = writeln!(
        sys,
        "\n## Pictures and video (the author's guidance: follow it)\n{guide}{}",
        if video {
            ""
        } else {
            "\nThis story has no video: use pictures only."
        }
    );
    for p in prompts.iter().filter(|p| {
        p.id != STORY_PROMPT
            && p.id != STYLE_PROMPT
            && p.id != MEDIA_PROMPT
            && p.id != PICTURE_PROMPT
            && p.scope == Scope::FromHere
    }) {
        let _ = writeln!(
            sys,
            "\n## {} (from the author: it overrides the defaults and the guidance above where they differ)\n{}",
            if p.id == STEER_PROMPT {
                "Latest direction"
            } else {
                "Direction"
            },
            p.text.trim()
        );
    }
    let settings = res.settings();
    // The narrator's object is only its voice: not a character in the story.
    let objects: Vec<_> = res
        .objects()
        .into_iter()
        .filter(|o| settings.narrator.as_deref() != Some(o.id.as_str()))
        .collect();
    let deliveries = voiced(graph, &settings, &objects);
    if !objects.is_empty() {
        sys.push_str("\n## Objects (characters, places, things; use their names)\n");
        for o in &objects {
            let _ = write!(sys, "- {}: {}", o.name, o.description.trim());
            if let Some(v) = o.voice.as_ref().filter(|_| settings.voices && o.talks)
                && v.deliveries.len() > 1
            {
                let names: Vec<&str> = v.deliveries.iter().map(|d| d.name.as_str()).collect();
                let _ = write!(
                    sys,
                    " Speaks aloud; deliveries (\"tone\"): {}.",
                    names.join(", ")
                );
            }
            if !o.refs.is_empty() {
                let refs: Vec<String> = o
                    .refs
                    .iter()
                    .map(|r| {
                        let mut s = format!("{} \"{}\"", r.media, r.caption);
                        if !r.use_for.trim().is_empty() {
                            s.push_str(&format!(" (the author: {})", r.use_for.trim()));
                        }
                        s
                    })
                    .collect();
                let _ = write!(sys, " Reference pictures: {}", refs.join(", "));
            }
            sys.push('\n');
        }
    }
    sys.push_str(&crate::contract::audio_guide(
        deliveries.len() > 1,
        settings.sounds,
    ));

    let mut user = String::new();
    let summaries: Vec<&str> = parent
        .iter()
        .filter_map(|h| graph.beats.get(&h.beat)?.summary.as_deref())
        .collect();
    if !summaries.is_empty() {
        user.push_str("# STORY SO FAR\n");
        for s in &summaries[summaries.len().saturating_sub(lim.summaries)..] {
            let _ = writeln!(user, "- {s}");
        }
        user.push('\n');
    }

    let state = state_at(graph, parent);
    if !state.is_empty() {
        user.push_str("# YOUR NOTES (state)\n");
        for (k, v) in &state {
            let _ = writeln!(user, "- {k}: {v}");
        }
        user.push('\n');
    }

    let cue = cue(graph, parent);
    let mut pics = pictures(
        graph,
        parent,
        &objects.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
        lim.pictures,
    );
    let listed: HashSet<String> = pics.iter().cloned().collect();
    pics.extend(relevant_pictures(graph, &cue, &listed, lim.lookups));
    if !pics.is_empty() {
        user.push_str("# PICTURES YOU CAN REUSE OR USE AS REFS\n");
        for id in &pics {
            let m = &graph.media[id];
            if m.status != MediaStatus::Ready {
                continue;
            }
            let status = "";
            let kind = match m.kind {
                crate::model::MediaKind::Video => "video",
                _ => "image",
            };
            let _ = writeln!(
                user,
                "- {} [{kind}] \"{}\" objects: {}{status}",
                m.id,
                m.caption,
                m.objects.join(", ")
            );
        }
        if let Some(on) = crate::model::showing_at(graph, parent) {
            let _ = writeln!(user, "On screen now: {}", on.id);
        }
        user.push('\n');
    }

    let targets = rejoin_targets(graph, parent, lim.rejoin_targets);
    if !targets.is_empty() {
        user.push_str("# REJOIN TARGETS (existing beats you may flow into)\n");
        for (id, desc) in &targets {
            let _ = writeln!(user, "- {id}: {desc}");
        }
        user.push('\n');
    }

    let before = parent.len().saturating_sub(lim.recent_beats);
    let mem = memories(graph, parent, before, &cue, lim.lookups);
    if !mem.is_empty() {
        user.push_str("# EARLIER MOMENTS THAT MAY MATTER NOW\n");
        for (i, l) in &mem {
            let _ = writeln!(user, "- (beat {}) {l}", i + 1);
        }
        user.push('\n');
    }

    if parent.is_empty() {
        user.push_str(&pacing(graph, parent));
        user.push_str("Begin the story with its opening beat.\n");
    } else {
        user.push_str("# RECENT BEATS\n");
        let from = parent.len().saturating_sub(lim.recent_beats);
        for h in &parent[from..] {
            let Some(b) = graph.beats.get(&h.beat) else {
                continue;
            };
            if let Some(l) = &b.label {
                let _ = writeln!(user, "[scene: {l}]");
            }
            for l in &b.lines {
                let _ = writeln!(user, "{}", line_text(&l.speaker, &l.text));
            }
            if let Some(c) = choice_label(graph, h) {
                let _ = writeln!(user, "(The player chose: {c})");
            }
        }
        user.push('\n');
        user.push_str(&pacing(graph, parent));
        if let Some(t) = target {
            let here: Vec<&str> = graph
                .prompts
                .iter()
                .filter(|p| p.scope == Scope::Here && p.anchor.as_deref() == Some(t))
                .filter(|p| !graph.retired.contains(&p.rev))
                .map(|p| p.text.as_str())
                .collect();
            user.push_str(
                "Write a fresh take on the next beat (the previous take was rejected).\n",
            );
            if let Some(p) = here.last() {
                let _ = writeln!(user, "Direction for this beat: {p}");
            }
            if let Some(p) = next_direction(graph, parent) {
                let _ = writeln!(user, "Direction for this beat: {p}");
            }
        } else {
            if let Some(p) = next_direction(graph, parent) {
                let _ = writeln!(user, "Direction for this beat (from the author): {p}");
            }
            user.push_str("Write the next beat.\n");
        }
    }

    (sys, user)
}

fn picture_line(out: &mut String, m: &crate::model::Media) {
    if m.status != MediaStatus::Ready {
        return;
    }
    let status = "";
    let kind = match m.kind {
        crate::model::MediaKind::Video => "video",
        _ => "image",
    };
    // What it shows, briefly: the storyteller can't see pictures, so this is what it
    // compares a new shot against before asking for one (2026-11-21 alpha).
    let shot: String = m.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let shot: String = shot.chars().take(160).collect();
    let _ = writeln!(
        out,
        "- {} [{kind}] \"{}\" objects: {}{status}{}",
        m.id,
        m.caption,
        m.objects.join(", "),
        if shot.is_empty() {
            String::new()
        } else {
            format!(" shows: {shot}")
        }
    );
}

/// Words too common to say what a moment is about.
const STOP: &[&str] = &[
    "about", "after", "again", "against", "also", "back", "been", "before", "being", "between",
    "both", "could", "does", "down", "each", "even", "every", "from", "have", "having", "here",
    "into", "just", "like", "little", "long", "made", "make", "more", "most", "much", "must",
    "never", "only", "other", "over", "same", "says", "should", "some", "still", "such", "than",
    "that", "their", "them", "then", "there", "these", "they", "thing", "this", "those", "through",
    "under", "very", "want", "were", "what", "when", "where", "which", "while", "will", "with",
    "without", "would", "your", "yours", "she's", "he's", "it's", "don't", "isn't", "can't",
    "won't",
];

/// The distinctive words of a text (lowercase, 4+ letters, not stop words).
pub fn keywords(text: &str) -> HashSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| w.chars().count() >= 4 && !STOP.contains(&w.as_str()))
        .collect()
}

/// What the next beat is about: the last two beats, the choice taken, the scene.
fn cue(graph: &Graph, parent: &[Hop]) -> HashSet<String> {
    let mut text = String::new();
    for h in parent.iter().rev().take(2) {
        let Some(b) = graph.beats.get(&h.beat) else {
            continue;
        };
        for l in &b.lines {
            text.push_str(&line_text(&l.speaker, &l.text));
            text.push(' ');
        }
        if let Some(c) = choice_label(graph, h) {
            text.push_str(&c);
            text.push(' ');
        }
    }
    if let Some(l) = parent
        .iter()
        .rev()
        .find_map(|h| graph.beats.get(&h.beat)?.label.as_deref())
    {
        text.push_str(l);
    }
    keywords(&text)
}

/// Pictures from anywhere in the story (any branch) whose caption, objects or prompt share
/// at least two distinctive words with the moment, best first.
fn relevant_pictures(
    graph: &Graph,
    cue: &HashSet<String>,
    skip: &HashSet<String>,
    cap: usize,
) -> Vec<String> {
    if cue.is_empty() || cap == 0 {
        return vec![];
    }
    let mut hits: Vec<(usize, &crate::model::Media)> = graph
        .media
        .values()
        .filter(|m| m.status == MediaStatus::Ready && !skip.contains(&m.id))
        .map(|m| {
            let words = keywords(&format!(
                "{} {} {}",
                m.caption,
                m.objects.join(" "),
                m.prompt
            ));
            (words.intersection(cue).count(), m)
        })
        .filter(|(n, _)| *n >= 2)
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.at.cmp(&a.1.at)));
    hits.into_iter()
        .take(cap)
        .map(|(_, m)| m.id.clone())
        .collect()
}

/// Lines from this path before index `before` that share at least two distinctive words
/// with the moment: the best `cap`, in story order, with their beat index.
fn memories(
    graph: &Graph,
    path: &[Hop],
    before: usize,
    cue: &HashSet<String>,
    cap: usize,
) -> Vec<(usize, String)> {
    if cue.is_empty() || cap == 0 {
        return vec![];
    }
    let mut hits: Vec<(usize, usize, String)> = Vec::new();
    for (i, h) in path.iter().enumerate().take(before) {
        let Some(b) = graph.beats.get(&h.beat) else {
            continue;
        };
        for l in &b.lines {
            let t = line_text(&l.speaker, &l.text);
            let n = keywords(&t).intersection(cue).count();
            if n >= 2 {
                hits.push((n, i, t));
            }
        }
    }
    // Best first (later beats win ties), then back into story order.
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    hits.truncate(cap);
    hits.sort_by_key(|h| h.1);
    hits.dedup_by(|a, b| a.2 == b.2);
    hits.into_iter().map(|(_, i, t)| (i, t)).collect()
}

/// Beats that must pass after a choice before the next one may be offered.
pub const CHOICE_GAP: usize = 2;

/// Beats in a row at the end of `parent` that offered no choice.
pub fn since_choice(graph: &Graph, parent: &[Hop]) -> usize {
    parent
        .iter()
        .rev()
        .take_while(|h| {
            !graph
                .beats
                .get(&h.beat)
                .is_some_and(|b| matches!(b.next, Next::Choices { .. }))
        })
        .count()
}

/// May the beat after `parent` offer choices? Not within [`CHOICE_GAP`] beats of the
/// last one.
pub fn choices_allowed(graph: &Graph, parent: &[Hop]) -> bool {
    parent.iter().rev().take(CHOICE_GAP).all(|h| {
        !graph
            .beats
            .get(&h.beat)
            .is_some_and(|b| matches!(b.next, Next::Choices { .. }))
    })
}

/// What the beat after `parent` may do (see [`crate::contract::Allow`]).
pub fn allow(graph: &Graph, parent: &[Hop], lim: &Limits) -> crate::contract::Allow {
    let on = crate::model::showing_at(graph, parent);
    let blank = on.is_none();
    let settings = Resolver::new(graph, parent, None).settings();
    let gap = settings.picture_gap as usize;
    let new_media = blank
        || parent.iter().rev().take(gap).all(|h| {
            !graph
                .beats
                .get(&h.beat)
                .is_some_and(|b| matches!(b.show, Show::New { .. }))
        });
    // Only what exists (painted): the grammar can't produce any other id, so nothing the
    // model names is invented or still to be made.
    let mut reuse: Vec<&crate::model::Media> = graph
        .media
        .values()
        .filter(|m| {
            m.status == MediaStatus::Ready
                && matches!(
                    m.kind,
                    crate::model::MediaKind::Image | crate::model::MediaKind::Video
                )
        })
        .collect();
    reuse.sort_by_key(|m| std::cmp::Reverse(m.at));
    // References are images (a video can be shown again, not drawn from).
    let refs: Vec<String> = reuse
        .iter()
        .filter(|m| m.kind == crate::model::MediaKind::Image)
        .take(200)
        .map(|m| m.id.clone())
        .collect();
    // The same picture too long: this beat changes it, a new one or another one again
    // (2026-11-21 alpha: told to, the model still kept one picture for 10+ beats).
    let stale = settings.picture_max > 0
        && !blank
        && crate::model::screen_age(graph, parent) >= settings.picture_max as usize;
    let reuse: Vec<String> = reuse
        .into_iter()
        .filter(|m| !stale || on.is_none_or(|o| o.id != m.id))
        .take(200)
        .map(|m| m.id.clone())
        .collect();
    let must_change = stale && (new_media || !reuse.is_empty());
    let res = Resolver::new(graph, parent, None);
    let objects: Vec<_> = res
        .objects()
        .into_iter()
        .filter(|o| settings.narrator.as_deref() != Some(o.id.as_str()))
        .collect();
    let deliveries = voiced(graph, &settings, &objects);
    crate::contract::Allow {
        deliveries: (deliveries.len() > 1).then_some(deliveries),
        sounds: settings.sounds,

        choices: choices_allowed(graph, parent),
        // Too long without one: this beat offers a choice (2026-11-24 alpha: told "one
        // beat in three or four", the model offered 4 in 63 beats).
        must_choose: settings.choice_max > 0
            && parent.len() >= settings.choice_max as usize
            && since_choice(graph, parent) >= settings.choice_max as usize,
        new_media,
        rejoin: Some(
            rejoin_targets(graph, parent, lim.rejoin_targets)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        ),
        reuse: Some(reuse),
        refs: Some(refs),
        // Not shortened after choices: every beat costs about the same to generate, so
        // a short beat is just less reading to cover the next one (2026-11-21 v7: stall
        // 35% -> 50% with 3-line reactions).
        max_lines: 6,
        // Never a black screen: the first beat paints (or reuses) a picture.
        must_show: blank || must_change,
        // Not before the story has had room to be a story (2026-11-21 alpha: "the end"
        // came far too often).
        end: parent.len() + 1 >= settings.min_beats as usize
            // Not again soon after the reader kept going past an ending.
            && !parent.iter().rev().take(KEPT_GOING_FOR).any(|h| {
                graph.beats.get(&h.beat).is_some_and(|b| b.kept_going)
            }),
    }
}

/// The deliveries the story's voices offer (every talking object's, the narrator's
/// aside: the narration is read in its usual way), names deduplicated in order. Empty
/// when voices are off.
pub fn voiced(
    _graph: &Graph,
    settings: &crate::model::Settings,
    objects: &[&crate::model::ObjectRev],
) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    if !settings.voices {
        return out;
    }
    for o in objects.iter().filter(|o| o.talks) {
        for d in o.voice.iter().flat_map(|v| &v.deliveries) {
            let n = d.name.trim().to_lowercase();
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// Beats after "Keep going" that the story keeps going (no ending, a chapter's nudge).
const KEPT_GOING_FOR: usize = 20;

/// Short reminders from where the path stands, sent with every beat: rules given once at
/// the top of a long chat fade, and the model can't count beats itself.
pub fn pacing(graph: &Graph, parent: &[Hop]) -> String {
    let mut out = String::new();
    let n = parent.len();
    let res = Resolver::new(graph, parent, None);
    let settings = res.settings();
    let since = |f: &dyn Fn(&crate::model::Beat) -> bool| {
        parent
            .iter()
            .rev()
            .position(|h| graph.beats.get(&h.beat).is_some_and(f))
            .unwrap_or(n)
    };
    let _ = writeln!(out, "(Beat {} of the story.)", n + 1);
    if crate::model::showing_at(graph, parent).is_none() {
        out.push_str(
            "Nothing is on screen yet: this beat must make a picture (media type image).\n",
        );
    } else {
        let age = crate::model::screen_age(graph, parent);
        let max = settings.picture_max as usize;
        if max > 0 && age >= max {
            let _ = writeln!(
                out,
                "The same picture has been up for {age} beats: this beat must change it (a new picture, or reuse a different one that fits)."
            );
        } else if age >= 2 {
            let _ = writeln!(
                out,
                "The picture hasn't changed for {age} beats: change it now if the place, the moment, the framing or who is in view has changed at all."
            );
        }
    }
    let choice = since(&|b| matches!(b.next, Next::Choices { .. }));
    if n > 0 && choice == 0 {
        out.push_str("The player just chose: let the choice play out (continue).\n");
    } else if settings.choice_max > 0
        && n >= settings.choice_max as usize
        && choice >= settings.choice_max as usize
    {
        // Held to it in the grammar too (see `allow`).
        let _ = writeln!(
            out,
            "No choice for {choice} beats: this beat must end at a decision for the player (choices), so bring the moment to one."
        );
    } else if choice >= 4 {
        let _ = writeln!(
            out,
            "No choice for {choice} beats: give the player a decision soon, where one arises naturally."
        );
    }
    // Past an ending the reader wanted more of: a new chapter, not an epilogue (2026-11-21
    // alpha: after "Keep going" the story stopped offering decisions).
    let kept = since(&|b| b.kept_going);
    if kept < n && kept < 3 {
        out.push_str(
            "The story had ended here, but the reader wants it to go on: this is a new chapter, not an epilogue. Open new threads and give the player real decisions again.\n",
        );
    }
    if n >= 6 && since(&|b| b.summary.is_some()) >= 8 {
        out.push_str("Include a summary in this beat.\n");
    }
    // The author's latest steer, every beat: given once at the top it lost to the default
    // guidance (2026-11-21 alpha: "more pictures" changed nothing).
    if let Some(p) = res
        .prompts()
        .into_iter()
        .filter(|p| p.id == STEER_PROMPT && p.scope == Scope::FromHere)
        .max_by_key(|p| p.at)
        .map(|p| p.text.trim())
        .filter(|t| !t.is_empty())
    {
        let _ = writeln!(
            out,
            "The author's latest direction (it overrides the defaults and the guidance above): {p}"
        );
    }
    if let Some(p) = res
        .prompts()
        .into_iter()
        .filter(|p| p.id == PICTURE_PROMPT)
        .max_by_key(|p| p.at)
        .map(|p| p.text.trim())
        .filter(|t| !t.is_empty())
    {
        let _ = writeln!(
            out,
            "The author's direction for every picture and video (added to each prompt as is; write prompts that fit it): {p}"
        );
    }
    out
}

/// Media worth listing: recently shown on this path, then anything featuring objects
/// mentioned recently.
fn pictures(graph: &Graph, path: &[Hop], object_names: &[&str], cap: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for h in path.iter().rev() {
        if let Some(b) = graph.beats.get(&h.beat)
            && let Show::Reuse { media } | Show::New { media } = &b.show
            && graph
                .media
                .get(media)
                .is_some_and(|m| m.status == MediaStatus::Ready)
            && seen.insert(media.clone())
        {
            out.push(media.clone());
        }
        if out.len() >= cap / 2 {
            break;
        }
    }
    let recent: String = path
        .iter()
        .rev()
        .take(6)
        .filter_map(|h| graph.beats.get(&h.beat))
        .flat_map(|b| {
            b.lines
                .iter()
                .map(|l| format!("{:?} {}", l.speaker, l.text))
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mentioned: Vec<&str> = object_names
        .iter()
        .copied()
        .filter(|n| recent.contains(n))
        .collect();
    let mut rest: Vec<&crate::model::Media> = graph
        .media
        .values()
        .filter(|m| m.status == MediaStatus::Ready && !seen.contains(&m.id))
        .filter(|m| m.objects.iter().any(|o| mentioned.contains(&o.as_str())))
        .collect();
    rest.sort_by_key(|m| std::cmp::Reverse(m.at));
    for m in rest {
        if out.len() >= cap {
            break;
        }
        out.push(m.id.clone());
    }
    out
}

/// Beats off this path the story could flow back into: labeled scenes and the first beats
/// of sibling branches.
/// Beats a new branch must have of its own before it may flow back into another line.
pub const BRANCH_OWN: usize = 3;
/// A sibling line's first beats are its own consequences: never rejoined.
const SIBLING_SKIP: usize = 2;

fn rejoin_targets(graph: &Graph, path: &[Hop], cap: usize) -> Vec<(String, String)> {
    // A branch plays out its own consequences first (2026-11-21 alpha: every untaken
    // choice flowed straight back into the taken path, leaving nothing to go back to).
    let since_fork = path
        .iter()
        .rev()
        .position(|h| {
            graph
                .beats
                .get(&h.beat)
                .is_some_and(|b| matches!(b.next, Next::Choices { .. }))
        })
        .unwrap_or(path.len());
    if since_fork + 1 < BRANCH_OWN {
        return vec![];
    }
    let on_path: HashSet<&str> = path.iter().map(|h| h.beat.as_str()).collect();
    let mut cands: Vec<&crate::model::Beat> = Vec::new();
    // Siblings of branch points on the path, nearest first, past their first beats:
    // paths meet again later, not right away.
    for h in path.iter().rev() {
        let Some(b) = graph.beats.get(&h.beat) else {
            continue;
        };
        for exit in 0..b.next.exits() {
            if Some(exit) == h.exit {
                continue;
            }
            let mut cur = graph.follow(&b.id, exit, None);
            for i in 0..SIBLING_SKIP + 4 {
                let Some(c) = cur else { break };
                if i >= SIBLING_SKIP && !on_path.contains(c.id.as_str()) {
                    cands.push(c);
                }
                cur = graph.follow(&c.id, 0, None);
            }
        }
    }
    let mut labeled: Vec<&crate::model::Beat> = graph
        .beats
        .values()
        .filter(|b| b.label.is_some() && !b.pending && !on_path.contains(b.id.as_str()))
        .collect();
    labeled.sort_by_key(|b| std::cmp::Reverse(b.at));
    cands.extend(labeled);
    let reachable = graph.reachable();
    let mut seen = HashSet::new();
    cands
        .into_iter()
        .filter(|b| reachable.contains(&b.id) && seen.insert(b.id.clone()))
        .filter(|b| !graph.reaches_any(&b.id, &on_path))
        .take(cap)
        .map(|b| {
            let first = b
                .lines
                .first()
                .map(|l| line_text(&l.speaker, &l.text))
                .unwrap_or_default();
            let desc = match &b.label {
                Some(l) => format!("[scene: {l}] {first}"),
                None => first,
            };
            (b.id.clone(), desc)
        })
        .collect()
}

/// Answer one tool call.
pub fn run_tool(graph: &Graph, path: &[Hop], name: &str, args: &Value) -> Value {
    let q = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let words: Vec<&str> = q.split_whitespace().collect();
    let score = |hay: &str| {
        let hay = hay.to_lowercase();
        words.iter().filter(|w| hay.contains(*w)).count()
    };
    match name {
        "search_images" => {
            let mut hits: Vec<(usize, &crate::model::Media)> = graph
                .media
                .values()
                .filter(|m| m.status != MediaStatus::Failed)
                .map(|m| {
                    let hay = format!("{} {} {}", m.caption, m.objects.join(" "), m.prompt);
                    (score(&hay), m)
                })
                .filter(|(s, _)| *s > 0)
                .collect();
            hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.at.cmp(&a.1.at)));
            json!(
                hits.into_iter()
                    .take(10)
                    .map(|(_, m)| json!({
                        "id": m.id, "caption": m.caption, "objects": m.objects,
                        "ready": m.status == MediaStatus::Ready,
                    }))
                    .collect::<Vec<_>>()
            )
        }
        "recall" => {
            let mut hits = Vec::new();
            for (i, h) in path.iter().enumerate() {
                let Some(b) = graph.beats.get(&h.beat) else {
                    continue;
                };
                for l in &b.lines {
                    let t = line_text(&l.speaker, &l.text);
                    if score(&t) > 0 {
                        hits.push(json!({"beat_index": i, "line": t}));
                    }
                }
            }
            let n = hits.len();
            json!(
                hits.into_iter()
                    .skip(n.saturating_sub(10))
                    .collect::<Vec<_>>()
            )
        }
        other => json!({"error": format!("unknown tool {other}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Beat, Line, Media, MediaKind, StateOps};

    fn beat(id: &str, lines: &[&str], show: Show) -> Beat {
        Beat {
            id: id.into(),
            origin: None,
            epoch: 0,
            lines: lines
                .iter()
                .map(|t| Line {
                    speaker: None,
                    text: (*t).into(),
                    tone: None,
                })
                .collect(),
            show,
            state: StateOps::default(),
            summary: None,
            label: None,
            next: Next::Continue,
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

    fn media(id: &str, caption: &str, objects: &[&str], at: u64) -> Media {
        Media {
            id: id.into(),
            kind: MediaKind::Image,
            status: MediaStatus::Ready,
            blob: None,
            mime: None,
            caption: caption.into(),
            objects: objects.iter().map(|o| (*o).into()).collect(),
            prompt: String::new(),
            refs: vec![],
            width: 640,
            height: 352,
            beat: None,
            error: None,
            at,
            detailed: false,
            seconds: None,
            audio: None,
        }
    }

    /// 30 beats: an early promise about a brass key, filler, then the key comes back.
    fn story() -> (Graph, Vec<Hop>) {
        let mut g = Graph::default();
        let mut path = vec![];
        let mut add = |g: &mut Graph, id: String, lines: &[&str], show: Show| {
            g.beats.insert(id.clone(), beat(&id, lines, show));
            path.push(Hop {
                beat: id,
                exit: None,
            });
        };
        add(
            &mut g,
            "b0".into(),
            &["Tomas hands Mira a brass key from the drowned chapel."],
            Show::New { media: "m0".into() },
        );
        for i in 1..28 {
            add(
                &mut g,
                format!("b{i}"),
                &["Waves fold over the rocks."],
                Show::Keep,
            );
        }
        add(
            &mut g,
            "b28".into(),
            &["Mira finds a lock shaped for a brass key."],
            Show::Keep,
        );
        add(
            &mut g,
            "b29".into(),
            &["The lock sits under the drowned chapel's altar."],
            Show::Keep,
        );
        g.media
            .insert("m0".into(), media("m0", "Gallery at dusk", &["Mira"], 1));
        g.media.insert(
            "m1".into(),
            media("m1", "The drowned chapel altar", &["Chapel"], 2),
        );
        g.media.insert(
            "m2".into(),
            media("m2", "Tomas mending nets", &["Tomas"], 3),
        );
        (g, path)
    }

    #[test]
    fn keywords_skip_short_and_common_words() {
        let k = keywords("She said that the Brass key's lock was here, in the chapel.");
        assert!(k.contains("brass") && k.contains("chapel") && k.contains("key's"));
        assert!(!k.contains("that") && !k.contains("here") && !k.contains("the"));
    }

    #[test]
    fn the_moment_recalls_old_lines_and_pictures() {
        let (g, path) = story();
        let cue = cue(&g, &path);
        let mem = memories(&g, &path, path.len() - 12, &cue, 4);
        assert_eq!(mem.len(), 1, "{mem:?}");
        assert_eq!(mem[0].0, 0);
        assert!(mem[0].1.contains("brass key"));
        let pics = relevant_pictures(&g, &cue, &HashSet::new(), 4);
        assert_eq!(pics, vec!["m1".to_string()], "the chapel, not the nets");

        // Both reach the prompts: the full one, and a session turn that hasn't seen them.
        let (_, full) = parts(&g, &path, None, &Limits::default());
        assert!(full.contains("EARLIER MOMENTS") && full.contains("brass key from the drowned"));
        assert!(full.contains("- m1 "));
        let t = turn(&g, &path, None, &Limits::default(), &[]);
        assert!(t.contains("- m1 ") && t.contains("(beat 1) Tomas hands"));
        // Already said in this segment: not repeated.
        let seen = vec![(
            "b1".into(),
            "- m1 ...".into(),
            "Tomas hands Mira a brass key from the drowned chapel.".into(),
        )];
        let t = turn(&g, &path, None, &Limits::default(), &seen);
        assert!(
            !t.contains("- m1 ") && !t.contains("EARLIER MOMENTS"),
            "{t}"
        );
    }

    #[test]
    fn who_is_here_now_is_described_in_each_turn() {
        let (mut g, path) = story();
        // The last beat names Mira; Tomas isn't in it.
        for (id, name, d) in [
            ("o1", "Mira", "A one-eyed keeper in a green oilskin."),
            ("o2", "Tomas", "A fisherman."),
        ] {
            g.objects.push(crate::model::ObjectRev {
                id: id.into(),
                rev: format!("r{id}"),
                anchor: None,
                name: name.into(),
                description: d.into(),
                refs: vec![],
                talks: false,
                voice: None,
                removed: false,
                at: 0,
            });
        }
        let t = turn(&g, &path, None, &Limits::default(), &[]);
        assert!(t.contains("# HERE NOW"), "{t}");
        assert!(t.contains("- Mira: A one-eyed keeper"), "{t}");
        assert!(!t.contains("- Tomas:"), "{t}");
    }

    #[test]
    fn pacing_asks_for_pictures_choices_and_the_authors_steer() {
        let g = Graph::default();
        let p = pacing(&g, &[]);
        assert!(p.contains("must make a picture"));
        let (mut g, path) = story();
        let p = pacing(&g, &path);
        assert!(p.contains("Beat 31") && p.contains("up for 29 beats: this beat must change it"));
        assert!(
            p.contains("No choice for 30 beats: this beat must end at a decision")
                && p.contains("summary")
        );
        // The author's steer rides along with every beat from where it was given.
        g.apply(crate::model::Record::Prompt(crate::model::PromptRev {
            rev: "st1".into(),
            id: STEER_PROMPT.into(),
            text: "More pictures, please.".into(),
            anchor: Some(path[3].beat.clone()),
            scope: Scope::FromHere,
            at: 5,
        }));
        let p = pacing(&g, &path);
        assert!(
            p.contains("latest direction") && p.contains("More pictures, please."),
            "{p}"
        );
        assert!(!pacing(&g, &path[..2]).contains("More pictures"));
        let (sys, _) = parts(&g, &path, None, &Limits::default());
        let guide = sys.find("## Pictures and video").unwrap();
        let steer = sys.find("## Latest direction").unwrap();
        assert!(steer > guide && sys[steer..].contains("overrides"), "{sys}");
    }

    #[test]
    fn a_picture_left_up_too_long_must_change_and_failures_dont_count() {
        let (mut g, mut path) = story();
        let lim = Limits::default();
        // m0 (a painted picture) has been up for 29 beats: no "keep", and not m0 again.
        let a = allow(&g, &path, &lim);
        assert!(a.must_show && a.new_media);
        assert!(!a.reuse.as_ref().unwrap().contains(&"m0".to_string()));
        let schema = crate::contract::schema_with(&a).to_string();
        assert!(!schema.contains("\"const\":\"keep\""), "{schema}");
        // A new picture that failed never showed: the old one is still up, as long.
        g.media
            .insert("mf".into(), media("mf", "Gone wrong", &[], 9));
        g.media.get_mut("mf").unwrap().status = MediaStatus::Failed;
        g.beats.insert(
            "bf".into(),
            beat("bf", &["Smoke."], Show::New { media: "mf".into() }),
        );
        path.push(Hop {
            beat: "bf".into(),
            exit: None,
        });
        assert_eq!(crate::model::screen_age(&g, &path), 30);
        assert_eq!(crate::model::showing_at(&g, &path).unwrap().id, "m0");
        assert!(allow(&g, &path, &lim).must_show);
        // A fresh one: free again for a while.
        g.media
            .insert("mn".into(), media("mn", "The lamp room", &[], 10));
        g.beats.insert(
            "bn".into(),
            beat("bn", &["Up the stairs."], Show::New { media: "mn".into() }),
        );
        path.push(Hop {
            beat: "bn".into(),
            exit: None,
        });
        assert_eq!(crate::model::screen_age(&g, &path), 0);
        assert!(!allow(&g, &path, &lim).must_show);
        // The author can turn it off.
        g.apply(crate::model::Record::Settings(crate::model::SettingsRev {
            rev: "s0".into(),
            anchor: None,
            settings: crate::model::Settings {
                picture_max: 0,
                ..Default::default()
            },
            at: 0,
        }));
        path.truncate(path.len() - 2);
        assert!(!allow(&g, &path, &lim).must_show);
    }

    #[test]
    fn too_long_without_a_choice_the_beat_must_offer_one() {
        let (mut g, path) = story();
        let lim = Limits::default();
        // 30 beats and not one choice: no carrying on, no rejoining; choices (or the end).
        let a = allow(&g, &path, &lim);
        assert!(a.choices && a.must_choose);
        let schema = crate::contract::schema_with(&a).to_string();
        assert!(!schema.contains(r#""const":"continue""#), "{schema}");
        assert!(!schema.contains(r#""rejoin""#) && schema.contains(r#""choices""#));
        // Not at the start of a story, and the author can turn it off.
        assert!(!allow(&g, &path[..3], &lim).must_choose);
        g.apply(crate::model::Record::Settings(crate::model::SettingsRev {
            rev: "s0".into(),
            anchor: None,
            settings: crate::model::Settings {
                choice_max: 0,
                ..Default::default()
            },
            at: 0,
        }));
        let a = allow(&g, &path, &lim);
        assert!(!a.must_choose);
        assert!(
            crate::contract::schema_with(&a)
                .to_string()
                .contains(r#""const":"continue""#)
        );
    }

    #[test]
    fn a_young_story_cant_end() {
        let (mut g, path) = story();
        let lim = Limits::default();
        let a = allow(&g, &path, &lim);
        assert!(!a.end, "beat 31 of a story that can end from beat 40");
        assert!(
            !crate::contract::schema_with(&a)
                .to_string()
                .contains("\"const\":\"end\"")
        );
        g.apply(crate::model::Record::Settings(crate::model::SettingsRev {
            rev: "s0".into(),
            anchor: None,
            settings: crate::model::Settings {
                min_beats: 20,
                ..Default::default()
            },
            at: 0,
        }));
        let a = allow(&g, &path, &lim);
        assert!(a.end);
        assert!(
            crate::contract::schema_with(&a)
                .to_string()
                .contains("\"const\":\"end\"")
        );
    }

    #[test]
    fn pictures_are_spaced_and_a_blank_screen_must_paint() {
        let (mut g, mut path) = story();
        let lim = Limits::default();
        assert!(allow(&g, &path, &lim).new_media);
        // The author asks for at least 2 beats between new pictures (the default is no
        // limit: the guidance handles spacing), and no most.
        g.apply(crate::model::Record::Settings(crate::model::SettingsRev {
            rev: "s1".into(),
            anchor: None,
            settings: crate::model::Settings {
                picture_gap: 2,
                picture_max: 0,
                ..Default::default()
            },
            at: 0,
        }));
        // A new picture one beat back: not another yet.
        g.media
            .insert("mn".into(), media("mn", "The lamp room", &[], 9));
        g.beats.insert(
            "bn".into(),
            beat("bn", &["Up the stairs."], Show::New { media: "mn".into() }),
        );
        path.push(Hop {
            beat: "bn".into(),
            exit: None,
        });
        assert!(!allow(&g, &path, &lim).new_media);
        for i in 0..2 {
            let id = format!("bk{i}");
            g.beats
                .insert(id.clone(), beat(&id, &["The lamp turns."], Show::Keep));
            path.push(Hop {
                beat: id,
                exit: None,
            });
        }
        assert!(allow(&g, &path, &lim).new_media);
        // References: painted images only.
        let a = allow(&g, &path, &lim);
        assert!(a.refs.as_ref().unwrap().contains(&"mn".to_string()));
        g.media.get_mut("mn").unwrap().status = MediaStatus::Requested;
        let a = allow(&g, &path, &lim);
        assert!(!a.refs.as_ref().unwrap().contains(&"mn".to_string()));
        assert!(!a.reuse.as_ref().unwrap().contains(&"mn".to_string()));
        let blank = allow(&Graph::default(), &[], &lim);
        assert!(blank.new_media && blank.must_show);
    }
}
