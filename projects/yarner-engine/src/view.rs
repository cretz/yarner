//! What clients see: serializable views of beats, playthroughs, the map and setup.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::model::*;
use crate::plays::{Play, Plays};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaView {
    pub id: MediaId,
    pub kind: MediaKind,
    pub status: MediaStatus,
    pub mime: Option<String>,
    pub caption: String,
    pub width: u32,
    pub height: u32,
}

impl MediaView {
    pub fn of(m: &Media) -> Self {
        Self {
            id: m.id.clone(),
            kind: m.kind,
            status: m.status,
            mime: m.mime.clone(),
            caption: m.caption.clone(),
            width: m.width,
            height: m.height,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChoiceView {
    pub label: String,
    /// One the player wrote themselves.
    #[serde(default)]
    pub own: bool,
    /// The screen behind it is ready: its beat is written and its picture painted.
    pub ready: bool,
    /// Its beat is being written.
    pub generating: bool,
    /// Its beat is written; its picture is being painted (or waits its turn).
    #[serde(default)]
    pub painting: bool,
    /// What's being made for it is a video (with `painting`).
    #[serde(default)]
    pub filming: bool,
    /// Its picture is made; its voices or sounds are being made.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub voicing: bool,
    /// How far its picture or video is, 0..1, while it's being made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f32>,
    /// What's made for it failed and is tried again shortly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retrying: bool,
    /// Waiting its turn: where it stands in line (1 = next), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Started, but the backend is still loading the model that makes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NextView {
    Continue {
        ready: bool,
        generating: bool,
        #[serde(default)]
        painting: bool,
        #[serde(default)]
        filming: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        voicing: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<f32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        retrying: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        loading: bool,
    },
    Choices {
        choices: Vec<ChoiceView>,
    },
    End,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeatView {
    pub id: BeatId,
    pub lines: Vec<Line>,
    /// Media this beat itself shows (`None`: keep what's on screen).
    pub shows: Option<MediaView>,
    /// What's on screen at this beat (its own media or held from earlier).
    pub screen: Option<MediaView>,
    pub label: Option<String>,
    pub next: NextView,
    pub pending: bool,
    pub stale: bool,
    pub seen: bool,
    pub epoch: u32,
    /// Index among the versions behind the exit that led here, and how many there are.
    pub version: usize,
    pub versions: Vec<BeatId>,
    /// At this decision the player may write their own choice.
    #[serde(default)]
    pub own_choices: bool,
    /// Each line's spoken clip, when made (by line; `None`: not spoken).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub voices: Vec<Option<MediaId>>,
    /// Sounds it plays (made ones only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sounds: Vec<SoundCue>,
    /// The background sound playing on this screen (started here or earlier in the scene).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ambience: Option<MediaId>,
}

/// Share of a beat's voices and sounds made (or given up on), 0..1.
pub fn audio_done(g: &Graph, b: &Beat) -> Option<f32> {
    let all: Vec<&MediaId> = b.audio().collect();
    if all.is_empty() {
        return None;
    }
    let done = all.iter().filter(|m| media_settled(g, m)).count();
    Some(done as f32 / all.len() as f32)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayView {
    pub id: String,
    pub name: String,
    pub beat: Option<BeatView>,
    pub depth: usize,
    pub favor: Option<usize>,
    /// Being regenerated in place right now.
    pub regenerating: bool,
    /// Nothing to show yet (story not started and the opening beat isn't ready).
    pub waiting_for_start: bool,
    /// Before the opening is on screen: what's being made for it ("writing" or
    /// "painting"/"filming") and how far it is (0..1, when known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opening: Option<(String, Option<f32>)>,
    pub state: Vec<(String, String)>,
    /// Recent dialog on this path, oldest first (history screen).
    pub history: Vec<HistoryEntry>,
    /// While waiting for the next beat: its lines written so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<Vec<Line>>,
    /// The story's picture size (its shape: the play screen keeps it).
    #[serde(default)]
    pub frame: (u32, u32),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub index: usize,
    pub beat: BeatId,
    pub lines: Vec<Line>,
    pub chose: Option<String>,
}

pub type GenBusy<'a> = &'a dyn Fn(&str, u32, usize) -> bool;

pub fn beat_view(g: &Graph, plays: &Plays, b: &Beat, path: &[Hop], busy: GenBusy) -> BeatView {
    // (written, picture made, it's a video) for the screen behind an exit.
    // Written ahead but from before a setup change (it gets rewritten before it's shown):
    // as good as not written yet.
    let unseen_stale = |exit: usize, c: &Beat| {
        if !c.pending || path.is_empty() {
            return false;
        }
        let mut with = path.to_vec();
        with.last_mut().expect("non-empty").exit = Some(exit);
        with.push(Hop {
            beat: c.id.clone(),
            exit: None,
        });
        is_stale(g, &with)
    };
    // (written, picture made, it's a video, voices and sounds made).
    let behind = |exit: usize| match g.follow(&b.id, exit, None) {
        Some(c) if unseen_stale(exit, c) => (false, false, false, false),
        Some(c) => {
            let video = match &c.show {
                Show::New { media } | Show::Reuse { media } => g
                    .media
                    .get(media)
                    .is_some_and(|m| m.kind == MediaKind::Video),
                Show::Keep => false,
            };
            let painted = match &c.show {
                Show::Keep => true,
                Show::New { media } | Show::Reuse { media } => media_settled(g, media),
            };
            (true, painted, video, !sound_pending(g, c))
        }
        None => (false, false, false, false),
    };
    let next = match &b.next {
        Next::End => NextView::End,
        Next::Choices { choices } => NextView::Choices {
            choices: choices
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let (written, painted, video, voiced) = behind(i);
                    ChoiceView {
                        label: c.label.clone(),
                        own: c.own,
                        ready: written && painted && voiced,
                        generating: busy(&b.id, b.epoch, i),
                        painting: written && !painted,
                        filming: written && !painted && video,
                        voicing: written && painted && !voiced,
                        progress: None,
                        retrying: false,
                        line: None,
                        loading: false,
                    }
                })
                .collect(),
        },
        Next::Continue | Next::Rejoin { .. } => {
            let (written, painted, video, voiced) = behind(0);
            NextView::Continue {
                ready: written && painted && voiced,
                generating: busy(&b.id, b.epoch, 0),
                painting: written && !painted,
                filming: written && !painted && video,
                voicing: written && painted && !voiced,
                progress: None,
                retrying: false,
                line: None,
                loading: false,
            }
        }
    };
    let shows = match &b.show {
        Show::Keep => None,
        Show::Reuse { media } | Show::New { media } => g.media.get(media).map(MediaView::of),
    };
    let screen = if path.is_empty() {
        shows.clone()
    } else {
        showing_at(g, path).map(MediaView::of)
    };
    let (version, versions) = match &b.origin {
        Some((from, _, exit)) => {
            let vs = g.versions(from, *exit).to_vec();
            (vs.iter().position(|v| v == &b.id).unwrap_or(0), vs)
        }
        // The opening: its versions too (forks of it).
        None => {
            let vs = if g.openings.is_empty() {
                vec![b.id.clone()]
            } else {
                g.openings.clone()
            };
            (vs.iter().position(|v| v == &b.id).unwrap_or(0), vs)
        }
    };
    BeatView {
        own_choices: matches!(b.next, Next::Choices { .. })
            && Resolver::new(g, path, None).settings().own_choices,
        id: b.id.clone(),
        lines: b.lines.clone(),
        shows,
        screen,
        label: b.label.clone(),
        next,
        pending: b.pending,
        stale: !path.is_empty() && is_stale(g, path),
        seen: plays.seen.contains(&b.id),
        epoch: b.epoch,
        version,
        versions,
        voices: b
            .voices
            .iter()
            .map(|v| {
                v.clone().filter(|m| {
                    g.media
                        .get(m)
                        .is_some_and(|x| x.status == MediaStatus::Ready)
                })
            })
            .collect(),
        sounds: b
            .sounds
            .iter()
            .filter(|c| {
                g.media
                    .get(&c.media)
                    .is_some_and(|x| x.status == MediaStatus::Ready)
            })
            .cloned()
            .collect(),
        ambience: if path.is_empty() {
            ambience_at(
                g,
                &[Hop {
                    beat: b.id.clone(),
                    exit: None,
                }],
            )
        } else {
            ambience_at(g, path)
        }
        .map(|m| m.id.clone()),
    }
}

pub fn play_view(
    g: &Graph,
    plays: &Plays,
    p: &Play,
    favor: Option<usize>,
    busy: GenBusy,
    rerunning: &dyn Fn(&str) -> bool,
) -> PlayView {
    let beat = p
        .path
        .last()
        .and_then(|h| g.beats.get(&h.beat))
        .map(|b| beat_view(g, plays, b, &p.path, busy));
    let from = p.path.len().saturating_sub(60);
    let history = p.path[from..]
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let b = g.beats.get(&h.beat)?;
            let chose = match (&b.next, h.exit) {
                (Next::Choices { choices }, Some(e)) => choices.get(e).map(|c| c.label.clone()),
                _ => None,
            };
            Some(HistoryEntry {
                index: from + i,
                beat: b.id.clone(),
                lines: b.lines.clone(),
                chose,
            })
        })
        .collect();
    PlayView {
        id: p.id.clone(),
        name: p.name.clone(),
        regenerating: p.path.last().is_some_and(|h| rerunning(&h.beat)),
        beat,
        depth: p.path.len(),
        favor,
        waiting_for_start: p.path.is_empty(),
        opening: None,
        state: state_at(g, &p.path).into_iter().collect(),
        history,
        draft: None,
        frame: {
            let s = Resolver::new(g, &p.path, None).settings();
            (s.width, s.height)
        },
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapNode {
    pub id: BeatId,
    pub label: Option<String>,
    pub excerpt: String,
    pub thumb: Option<MediaId>,
    /// The thumb is a video (the map marks it; its tile is a frame of it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub video: bool,
    pub pending: bool,
    pub seen: bool,
    pub on_path: bool,
    pub ending: bool,
    /// Index on the current path, if on it.
    pub path_index: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapEdge {
    pub from: BeatId,
    pub exit: usize,
    pub to: BeatId,
    pub label: Option<String>,
    /// Rejoins an existing line.
    pub rejoin: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapView {
    pub root: Option<BeatId>,
    /// Every version of the opening (the root first).
    #[serde(default)]
    pub openings: Vec<BeatId>,
    pub nodes: Vec<MapNode>,
    pub edges: Vec<MapEdge>,
    /// Exits being generated right now.
    pub generating: Vec<(BeatId, usize)>,
}

pub fn map_view(
    g: &Graph,
    plays: &Plays,
    path: &[Hop],
    generating: &HashSet<(BeatId, usize)>,
) -> MapView {
    let reach = g.reachable();
    let idx: std::collections::HashMap<&str, usize> = path
        .iter()
        .enumerate()
        .map(|(i, h)| (h.beat.as_str(), i))
        .collect();
    let mut nodes: Vec<MapNode> = reach
        .iter()
        .filter_map(|id| g.beats.get(id))
        .map(|b| {
            let mut screen: Vec<Hop> = vec![];
            if let Some(i) = idx.get(b.id.as_str()) {
                screen = path[..=*i].to_vec();
            }
            let thumb = match &b.show {
                Show::Reuse { media } | Show::New { media } => Some(media.clone()),
                // Off the current path too: the picture still up from the beats it came
                // from (2026-11-21 alpha: such beats were gray blocks on the map).
                Show::Keep => showing_at(g, &screen).map(|m| m.id.clone()).or_else(|| {
                    let mut cur = b;
                    for _ in 0..64 {
                        let (from, _, _) = cur.origin.as_ref()?;
                        cur = g.beats.get(from)?;
                        if let Show::Reuse { media } | Show::New { media } = &cur.show
                            && g.media
                                .get(media)
                                .is_some_and(|m| m.status != crate::model::MediaStatus::Failed)
                        {
                            return Some(media.clone());
                        }
                    }
                    None
                }),
            };
            // Plays a video itself (a beat held on one's last frame isn't one).
            let video = match &b.show {
                Show::Reuse { media } | Show::New { media } => g
                    .media
                    .get(media)
                    .is_some_and(|m| m.kind == MediaKind::Video),
                Show::Keep => false,
            };
            MapNode {
                id: b.id.clone(),
                label: b.label.clone(),
                excerpt: b
                    .lines
                    .first()
                    .map(|l| l.text.chars().take(80).collect())
                    .unwrap_or_default(),
                thumb,
                video,
                pending: b.pending,
                seen: plays.seen.contains(&b.id),
                on_path: idx.contains_key(b.id.as_str()),
                ending: matches!(b.next, Next::End),
                path_index: idx.get(b.id.as_str()).copied(),
            }
        })
        .collect();
    nodes.sort_by_key(|n| g.beats[&n.id].at);
    let mut edges = Vec::new();
    for id in &reach {
        let b = &g.beats[id];
        for exit in 0..b.next.exits() {
            for to in g.versions(id, exit) {
                edges.push(MapEdge {
                    from: id.clone(),
                    exit,
                    to: to.clone(),
                    label: match &b.next {
                        Next::Choices { choices } => choices.get(exit).map(|c| c.label.clone()),
                        _ => None,
                    },
                    rejoin: matches!(b.next, Next::Rejoin { .. }),
                });
            }
        }
    }
    edges.sort_by(|a, b| (&a.from, a.exit, &a.to).cmp(&(&b.from, b.exit, &b.to)));
    let mut generating: Vec<(BeatId, usize)> = generating.iter().cloned().collect();
    generating.sort();
    MapView {
        root: g.root.clone(),
        openings: g.openings.clone(),
        nodes,
        edges,
        generating,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptView {
    pub id: String,
    pub rev: RevId,
    pub text: String,
    pub anchor: Anchor,
    pub scope: Scope,
    /// Revisions of this prompt on the current path, oldest first.
    pub history: Vec<PromptRev>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObjectView {
    pub id: String,
    pub rev: RevId,
    pub name: String,
    pub description: String,
    pub refs: Vec<(RefImage, Option<MediaView>)>,
    pub talks: bool,
    pub voice: Option<Voice>,
    /// The voice's clips as they stand (made, being made, failed), by delivery.
    pub clips: Vec<Option<MediaView>>,
    pub anchor: Anchor,
    pub history: Vec<ObjectRev>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetupView {
    pub prompts: Vec<PromptView>,
    pub objects: Vec<ObjectView>,
    pub settings: Settings,
    /// Setup pictures (not made for a beat), newest first.
    pub library: Vec<MediaView>,
    /// The pictures-and-video guidance used when the author hasn't written one.
    pub media_guide_default: String,
    /// What a new voice starts with: its deliveries (name, how) and the words its clips
    /// say.
    pub voice_default: Voice,
}

/// A new voice: the default deliveries and words, no clips.
pub fn default_voice(description: &str) -> Voice {
    Voice {
        description: description.into(),
        words: DEFAULT_VOICE_WORDS.into(),
        deliveries: DEFAULT_DELIVERIES
            .iter()
            .map(|(n, how)| Delivery {
                name: (*n).into(),
                how: (*how).into(),
                clip: None,
                words: String::new(),
            })
            .collect(),
    }
}

pub fn setup_view(g: &Graph, path: &[Hop]) -> SetupView {
    let target = None;
    let res = Resolver::new(g, path, target);
    let on_path: HashSet<&str> = path.iter().map(|h| h.beat.as_str()).collect();
    let applies = |a: &Anchor| a.as_ref().is_none_or(|b| on_path.contains(b.as_str()));
    let prompts = res
        .prompts()
        .into_iter()
        .map(|p| PromptView {
            id: p.id.clone(),
            rev: p.rev.clone(),
            text: p.text.clone(),
            anchor: p.anchor.clone(),
            scope: p.scope,
            history: {
                let mut h: Vec<PromptRev> = g
                    .prompts
                    .iter()
                    .filter(|x| x.id == p.id && applies(&x.anchor))
                    .cloned()
                    .collect();
                h.sort_by_key(|x| x.at);
                h
            },
        })
        .collect();
    let objects = res
        .objects()
        .into_iter()
        .map(|o| ObjectView {
            id: o.id.clone(),
            rev: o.rev.clone(),
            name: o.name.clone(),
            description: o.description.clone(),
            refs: o
                .refs
                .iter()
                .map(|r| (r.clone(), g.media.get(&r.media).map(MediaView::of)))
                .collect(),
            talks: o.talks,
            voice: o.voice.clone(),
            clips: o
                .voice
                .iter()
                .flat_map(|v| &v.deliveries)
                .map(|d| {
                    d.clip
                        .as_ref()
                        .and_then(|c| g.media.get(c))
                        .map(MediaView::of)
                })
                .collect(),
            anchor: o.anchor.clone(),
            history: {
                let mut h: Vec<ObjectRev> = g
                    .objects
                    .iter()
                    .filter(|x| x.id == o.id && applies(&x.anchor))
                    .cloned()
                    .collect();
                h.sort_by_key(|x| x.at);
                h
            },
        })
        .collect();
    // Setup pictures (voice clips are shown with their voice).
    let mut library: Vec<&Media> = g
        .media
        .values()
        .filter(|m| m.beat.is_none() && m.kind != MediaKind::Audio)
        .collect();
    library.sort_by_key(|m| std::cmp::Reverse(m.at));
    SetupView {
        prompts,
        objects,
        settings: res.settings(),
        library: library.into_iter().map(MediaView::of).collect(),
        media_guide_default: DEFAULT_MEDIA_GUIDE.into(),
        voice_default: default_voice(""),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExportReport {
    pub beats: usize,
    pub pending_dropped: usize,
    /// Choices nobody took: disabled for players of a read-only export.
    pub untaken: Vec<(BeatId, String)>,
    /// Media not finished: left out.
    pub media_unfinished: usize,
}

pub fn export_report(g: &Graph) -> ExportReport {
    let reach = g.reachable();
    let mut r = ExportReport::default();
    for id in &reach {
        let b = &g.beats[id];
        if b.pending {
            r.pending_dropped += 1;
            continue;
        }
        r.beats += 1;
        if let Next::Choices { choices } = &b.next {
            for (i, c) in choices.iter().enumerate() {
                let taken = g
                    .versions(id, i)
                    .iter()
                    .any(|v| g.beats.get(v).is_some_and(|x| !x.pending));
                if !taken {
                    r.untaken.push((id.clone(), c.label.clone()));
                }
            }
        }
    }
    r.untaken.sort();
    r.media_unfinished = g
        .media
        .values()
        .filter(|m| m.status != MediaStatus::Ready)
        .count();
    r
}
