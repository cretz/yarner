//! Real-generation probe against a thinfer-serve: alternate story beats (Gemma, our beat
//! contract) and pictures (z-image), timing each job, to measure generation and
//! model-switch costs.
//!
//! `cargo run --release -p yarner-server --example probe -- <url> <token-file> <out-dir>`

use std::time::Instant;

use yarner_engine::backend::*;
use yarner_engine::contract;
use yarner_server::thinfer::{Options, ThinferBackend};

const STORY: &str = "A lighthouse keeper named Mira on a remote basalt island notices the lamp \
    has started signalling something under the sea. Quiet, eerie, warm. Other objects: \
    Tomas, a fisherman who brings supplies; The Lighthouse, a whitewashed tower.";

async fn run(b: &ThinferBackend, job: Job) -> (JobOutput, f64) {
    let t = Instant::now();
    let sub = b.submit(job, 100, false).await.expect("submit");
    let out = sub.done.await.expect("job");
    (out, t.elapsed().as_secs_f64())
}

fn text_job(history: &[String]) -> Job {
    let mut user = String::new();
    if history.is_empty() {
        user.push_str("Begin the story with its opening beat.\n");
    } else {
        user.push_str("# RECENT BEATS\n");
        for h in history {
            user.push_str(h);
            user.push('\n');
        }
        user.push_str("\nWrite the next beat.\n");
    }
    Job::Text(TextRequest {
        model: "gemma-4-26b-a4b".into(),
        messages: vec![
            Message::new(
                "system",
                format!(
                    "{}\n\n# STORY SETUP\n\n## Story\n{STORY}",
                    contract::ENGINE_PROMPT
                ),
            ),
            Message::new("user", user),
        ],
        tools: vec![],
        schema: Some(contract::schema()),
        max_tokens: Some(1200),
        temperature: Some(0.9),
        seed: None,
        think: false,
        session: None,
    })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = std::path::PathBuf::from(&args[3]);
    // `managed:<thinfer-serve path>` starts a private one.
    let mut _managed = None;
    let (url, token) = if let Some(bin) = args[1].strip_prefix("managed:") {
        let m = yarner_server::managed::Managed::start(yarner_server::managed::Spec {
            bin: bin.into(),
            dir: out_dir.join("thinfer"),
            sources: None,
            download: false,
            keep: false,
            vault_dir: None,
        })
        .await
        .expect("managed thinfer-serve");
        let r = (m.url.clone(), m.token.clone());
        _managed = Some(m);
        r
    } else {
        let token = std::fs::read_to_string(&args[2])
            .expect("token")
            .trim()
            .to_string();
        (args[1].clone(), token)
    };
    let url = &url;
    // `text` = beats only (warm model, no switching).
    let text_only = args.get(4).is_some_and(|a| a == "text");
    std::fs::create_dir_all(&out_dir).unwrap();
    let b = ThinferBackend::new(
        url,
        Options {
            token: Some(token),
            ca_pem: None,
        },
    )
    .unwrap();

    if args.get(4).is_some_and(|a| a == "st") {
        let Job::Text(base) = text_job(&[]) else {
            unreachable!()
        };
        let req = TextRequest {
            tools: contract::tools(),
            ..base
        };
        let (out, secs) = run(&b, Job::Text(req)).await;
        let JobOutput::Text(t) = out else { panic!() };
        std::fs::write(out_dir.join("schema_tools_reply.txt"), &t.content).unwrap();
        println!("{secs:.1}s gen {} tok", t.generated_tokens);
        return;
    }
    if args.get(4).is_some_and(|a| a == "ab") {
        ab(&b).await;
        return;
    }
    if args.get(4).is_some_and(|a| a == "schema") {
        let (out, secs) = run(
            &b,
            Job::Text(TextRequest {
                model: "gemma-4-26b-a4b".into(),
                messages: vec![Message::new("user", "Write a short poem about rain.")],
                schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"a": {"const": "x"}},
                    "required": ["a"],
                    "additionalProperties": false,
                    "x-guidance": {"whitespace_flexible": false, "item_separator": ",", "key_separator": ":"}
                })),
                max_tokens: Some(200),
                ..Default::default()
            }),
        )
        .await;
        let JobOutput::Text(t) = out else { panic!() };
        println!(
            "schema probe ({secs:.1}s, {} tok): {:?}",
            t.generated_tokens, t.content
        );
        return;
    }
    let mut history: Vec<String> = Vec::new();
    let mut picture_prompt =
        "a whitewashed lighthouse on a basalt island at dusk, moody".to_string();
    for round in 0..3 {
        let (out, secs) = run(&b, text_job(&history)).await;
        let JobOutput::Text(t) = out else {
            panic!("text job returned media")
        };
        println!(
            "text  round {round}: {secs:6.1}s  prompt {} tok (prefill {:.1}s), generated {} tok (decode {:.1}s, {:.1} tok/s), other {:.1}s",
            t.prompt_tokens,
            t.prefill_secs,
            t.generated_tokens,
            t.decode_secs,
            t.generated_tokens as f64 / t.decode_secs.max(0.001),
            secs - t.prefill_secs - t.decode_secs
        );
        match contract::parse(&t.content) {
            Ok(beat) => {
                for l in &beat.lines {
                    let who = l.speaker.clone().unwrap_or_else(|| "(narration)".into());
                    println!("        {who}: {}", l.text);
                    history.push(format!("{}: {}", who, l.text));
                }
                println!("        media: {:?}", beat.media);
                println!("        next:  {:?}", beat.next);
                if let contract::MediaOut::Image { prompt, .. }
                | contract::MediaOut::Video { prompt, .. } = &beat.media
                {
                    picture_prompt = prompt.clone();
                }
            }
            Err(e) => {
                println!("        INVALID beat: {e}");
                std::fs::write(out_dir.join(format!("raw{round}.txt")), &t.content).unwrap();
            }
        }
        if text_only {
            continue;
        }
        let (out, secs) = run(
            &b,
            Job::Image(ImageRequest {
                model: "zimage-turbo-q4".into(),
                prompt: format!("{picture_prompt}, moody watercolor"),
                width: 640,
                height: 360,
                refs: vec![],
                seed: None,
                loras: vec![],
            }),
        )
        .await;
        let JobOutput::Media { bytes, mime } = out else {
            panic!("image job returned text")
        };
        let path = out_dir.join(format!("round{round}.png"));
        std::fs::write(&path, &bytes).unwrap();
        println!(
            "image round {round}: {secs:6.1}s  {mime} {} KB -> {}",
            bytes.len() / 1024,
            path.display()
        );
    }
}

/// The same beat prompt with and without the schema and tools, twice each, to see
/// what each costs.
async fn ab(b: &ThinferBackend) {
    let Job::Text(base) = text_job(&[]) else {
        unreachable!()
    };
    let variants: Vec<(&str, TextRequest)> = vec![
        (
            "plain",
            TextRequest {
                schema: None,
                ..base.clone()
            },
        ),
        ("schema", base.clone()),
        (
            "tools",
            TextRequest {
                schema: None,
                tools: contract::tools(),
                ..base.clone()
            },
        ),
        (
            "schema+tools",
            TextRequest {
                tools: contract::tools(),
                ..base.clone()
            },
        ),
        (
            "schema max200",
            TextRequest {
                max_tokens: Some(200),
                ..base.clone()
            },
        ),
    ];
    for round in 0..2 {
        for (name, req) in &variants {
            let (out, secs) = run(b, Job::Text(req.clone())).await;
            let JobOutput::Text(t) = out else { panic!() };
            println!(
                "{round} {name:14} {secs:6.1}s prompt {} tok prefill {:.1}s, gen {} tok decode {:.1}s ({:.1} tok/s), other {:.1}s, tool calls {}",
                t.prompt_tokens,
                t.prefill_secs,
                t.generated_tokens,
                t.decode_secs,
                t.generated_tokens as f64 / t.decode_secs.max(0.001),
                secs - t.prefill_secs - t.decode_secs,
                t.tool_calls.len(),
            );
        }
    }
}
