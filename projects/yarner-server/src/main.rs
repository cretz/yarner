//! `yarner`: the server. Holds unlocked stories, generates through thinfer-serve (or a
//! mock), and serves the web client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use yarner_engine::Config;
use yarner_engine::backend::Backend;
use yarner_format::Install;
use yarner_server::app::{self, AppState};
use yarner_testkit::mock::{MockThinfer, Timing};

#[derive(Parser)]
#[command(version, about = "Build and play interactive adventure stories.")]
struct Args {
    /// Data directory (stories, plays). Everything in it but `config.json` is encrypted.
    #[arg(long, env = "YARNER_HOME", default_value = "yarner-data")]
    data: PathBuf,
    #[arg(long, default_value = "127.0.0.1:7878")]
    bind: SocketAddr,
    /// Built web client to serve (the `dist` of yarner-client).
    #[arg(long)]
    client: Option<PathBuf>,
    /// thinfer-serve URL.
    #[arg(long, conflicts_with = "mock")]
    thinfer: Option<String>,
    /// Bearer token for thinfer-serve.
    #[arg(long, env = "YARNER_THINFER_TOKEN")]
    thinfer_token: Option<String>,
    /// CA certificate (PEM) to trust for thinfer-serve, e.g. its self-signed cert.
    #[arg(long)]
    thinfer_ca: Option<PathBuf>,
    /// Start a private thinfer-serve (loopback, own token, config and dirs under the data
    /// directory) and stop it on exit.
    #[arg(long, conflicts_with_all = ["thinfer", "mock"])]
    thinfer_managed: bool,
    /// The thinfer-serve binary for --thinfer-managed (default: `thinfer-serve` on PATH).
    #[arg(long, default_value = "thinfer-serve")]
    thinfer_bin: PathBuf,
    /// Weight sources file for the managed thinfer-serve (default: none, stock weights).
    #[arg(long)]
    thinfer_sources: Option<PathBuf>,
    /// Let the managed thinfer-serve download missing weights.
    #[arg(long)]
    thinfer_download: bool,
    /// Leave the managed thinfer-serve running when yarner exits, and take it over on the
    /// next start (if started the same way), so restarting yarner doesn't reload models.
    /// It keeps holding the GPU until ended (Task Manager, or a start without this flag).
    #[arg(long, requires = "thinfer_managed")]
    thinfer_keep: bool,
    /// The LoRA vault folder for the managed thinfer-serve: point it at your own
    /// thinfer's (it logs `adapter vault vault_dir=...` at startup) to use the adapters
    /// you added there. Default: its own, under the data directory.
    #[arg(long, requires = "thinfer_managed")]
    thinfer_vault_dir: Option<PathBuf>,
    /// Use the built-in mock generator (placeholder pictures, scripted story).
    #[arg(long)]
    mock: bool,
    /// Mock job durations as a fraction of real-world ones (0 = instant).
    #[arg(long, default_value_t = 0.05)]
    mock_scale: f64,
    /// Lock a story this long after its last client leaves.
    #[arg(long, default_value_t = 30)]
    idle_minutes: u64,
    /// TLS certificate and key (PEM). Required off loopback unless --self-signed.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Serve HTTPS with a generated self-signed certificate.
    #[arg(long)]
    self_signed: bool,
    /// Beats to generate ahead of each player.
    #[arg(long, default_value_t = 2)]
    depth: usize,
    /// Append content-free telemetry to this file: ids, counts, kinds, timings and error
    /// classes; never story text, prompts, captions, names or error messages.
    #[arg(long)]
    diag: Option<PathBuf>,
    /// A TOML file of these same options (`data = "data"`, `thinfer-managed = true`, ...);
    /// options given on the command line win. Relative paths in it are relative to it.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Options that name files: relative ones in a config file are taken from its folder.
const PATH_OPTIONS: &[&str] = &[
    "data",
    "client",
    "thinfer-ca",
    "thinfer-bin",
    "thinfer-sources",
    "thinfer-vault-dir",
    "tls-cert",
    "tls-key",
    "diag",
];

/// The command line with a config file's options put first (so the command line wins).
fn with_config(cli: Vec<std::ffi::OsString>) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let path = cli.iter().enumerate().find_map(|(i, a)| {
        let a = a.to_str()?;
        a.strip_prefix("--config=")
            .map(PathBuf::from)
            .or_else(|| (a == "--config").then(|| cli.get(i + 1).map(PathBuf::from))?)
    });
    let Some(path) = path else { return Ok(cli) };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    let mut out = vec![cli[0].clone()];
    for (k, v) in table {
        let key = k.replace('_', "-");
        if key == "config" {
            bail!("a config file can't name another config file");
        }
        let flag = format!("--{key}");
        // Given on the command line too: that one wins.
        if cli.iter().filter_map(|a| a.to_str()).any(|a| {
            a == flag
                || a.strip_prefix(flag.as_str())
                    .is_some_and(|r| r.starts_with('='))
        }) {
            continue;
        }
        match v {
            toml::Value::Boolean(true) => out.push(flag.into()),
            toml::Value::Boolean(false) => {}
            toml::Value::String(s) => {
                let p = PathBuf::from(&s);
                // A bare program name (thinfer-bin = "thinfer-serve") is looked up on PATH.
                let is_path = PATH_OPTIONS.contains(&key.as_str())
                    && p.is_relative()
                    && (key != "thinfer-bin"
                        || p.components().count() > 1
                        || dir.join(&p).exists());
                out.push(flag.into());
                out.push(if is_path {
                    dir.join(p).into()
                } else {
                    s.into()
                });
            }
            toml::Value::Integer(i) => {
                out.push(flag.into());
                out.push(i.to_string().into());
            }
            toml::Value::Float(f) => {
                out.push(flag.into());
                out.push(f.to_string().into());
            }
            other => bail!("config option {k}: unsupported value {other}"),
        }
    }
    out.extend(cli.into_iter().skip(1));
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse_from(with_config(std::env::args_os().collect())?);
    {
        use tracing_subscriber::prelude::*;
        // The console gets the usual log; the diag file only the content-free telemetry.
        let console = tracing_subscriber::fmt::layer().with_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,yarner_diag=off".into()),
        );
        let diag = match &args.diag {
            Some(p) => {
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .with_context(|| format!("opening {}", p.display()))?;
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_writer(Mutex::new(f))
                        .with_ansi(false)
                        .with_target(false)
                        .with_filter(
                            tracing_subscriber::filter::Targets::new()
                                .with_target("yarner_diag", tracing::Level::INFO),
                        ),
                )
            }
            None => None,
        };
        tracing_subscriber::registry()
            .with(console)
            .with(diag)
            .init();
    }

    let mut managed = None;
    // "Pause" (the GPU given back) survives a restart: the private thinfer-serve is
    // stopped again as soon as it's up, and the stories start nothing.
    let (mut gpu, hold) = yarner_server::app::Gpu::new(args.data.join("gpu-paused"));
    let backend: Arc<dyn Backend> = if args.thinfer_managed {
        let m = yarner_server::managed::Managed::start(yarner_server::managed::Spec {
            bin: args.thinfer_bin.clone(),
            dir: args.data.join("thinfer"),
            sources: args.thinfer_sources.clone(),
            download: args.thinfer_download,
            keep: args.thinfer_keep,
            vault_dir: args.thinfer_vault_dir.clone(),
        })
        .await?;
        if m.adopted {
            tracing::info!("took over the thinfer-serve left running at {}", m.url);
        } else {
            tracing::info!("private thinfer-serve up at {}", m.url);
        }
        let b = yarner_server::thinfer::ThinferBackend::new(
            &m.url,
            yarner_server::thinfer::Options {
                token: Some(m.token.clone()),
                ca_pem: None,
            },
        )?;
        let sup = m.supervise(hold);
        gpu.up = Some(sup.up.clone());
        managed = Some(sup);
        Arc::new(b)
    } else if args.mock {
        let t = if args.mock_scale <= 0.0 {
            Timing::instant()
        } else {
            Timing::realistic(args.mock_scale)
        };
        MockThinfer::new(t)
    } else if let Some(url) = &args.thinfer {
        let ca_pem = match &args.thinfer_ca {
            Some(p) => Some(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?),
            None => None,
        };
        Arc::new(yarner_server::thinfer::ThinferBackend::new(
            url,
            yarner_server::thinfer::Options {
                token: args.thinfer_token.clone(),
                ca_pem,
            },
        )?)
    } else {
        // No generator: stories play as far as they're written (no GPU needed). A backend
        // that refuses all work; the engines are paused anyway.
        tracing::info!(
            "no generator: playing what's written only (add --thinfer-managed to write)"
        );
        let m = MockThinfer::new(Timing::instant());
        m.set_down(true);
        m
    };

    let install =
        Install::open(&args.data).with_context(|| format!("opening {}", args.data.display()))?;
    let state = Arc::new(AppState {
        install: Arc::new(install),
        backend,
        engine_cfg: Config {
            speculate_depth: args.depth,
            ..Config::default()
        },
        idle_timeout: Duration::from_secs(args.idle_minutes * 60),
        open: Mutex::new(Default::default()),
        generator: args.thinfer_managed || args.mock || args.thinfer.is_some(),
        gpu,
    });
    tokio::spawn(app::reaper(state.clone()));

    let mut router = app::router(state);
    if let Some(dir) = &args.client {
        router = router.fallback_service(
            tower_http::services::ServeDir::new(dir)
                .fallback(tower_http::services::ServeFile::new(dir.join("index.html"))),
        );
    }
    let router = router.layer(axum::middleware::from_fn(app::headers));

    let tls = match (&args.tls_cert, &args.tls_key, args.self_signed) {
        (Some(c), Some(k), _) => {
            Some(axum_server::tls_rustls::RustlsConfig::from_pem_file(c, k).await?)
        }
        (_, _, true) => {
            let cert = rcgen::generate_simple_self_signed(vec![
                "localhost".into(),
                args.bind.ip().to_string(),
            ])?;
            Some(
                axum_server::tls_rustls::RustlsConfig::from_pem(
                    cert.cert.pem().into_bytes(),
                    cert.signing_key.serialize_pem().into_bytes(),
                )
                .await?,
            )
        }
        _ => None,
    };
    if tls.is_none() && !args.bind.ip().is_loopback() {
        bail!(
            "refusing to serve without TLS off loopback: pass --self-signed or --tls-cert/--tls-key"
        );
    }
    let scheme = if tls.is_some() { "https" } else { "http" };
    tracing::info!("yarner listening on {scheme}://{}", args.bind);
    let serve = async {
        match tls {
            Some(cfg) => {
                axum_server::bind_rustls(args.bind, cfg)
                    .serve(router.into_make_service())
                    .await
            }
            None => {
                let l = tokio::net::TcpListener::bind(args.bind).await?;
                axum::serve(l, router).await
            }
        }
    };
    // Ctrl-C (or the server failing) ends the process; a private thinfer goes with it.
    let result = tokio::select! {
        r = serve => r.map_err(anyhow::Error::from),
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    if let Some(m) = managed {
        m.stop().await;
    }
    result?;
    Ok(())
}
