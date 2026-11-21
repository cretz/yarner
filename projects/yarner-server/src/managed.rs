//! A private thinfer-serve that yarner starts and stops itself: loopback only, a random
//! token, its own config and directories, and its own (by default empty) weight sources
//! file so it never picks up overrides meant for another thinfer on the machine.
//!
//! Kept (`Spec::keep`): it outlives yarner, and the next start of yarner takes it over
//! when it was started the same way, so restarting yarner doesn't reload models.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tokio::process::{Child, Command};

pub struct Managed {
    pub url: String,
    pub token: String,
    /// The process we started (none: one taken over from an earlier yarner).
    child: Option<Child>,
    /// Ties the child's life to ours even if we're killed outright (not when kept).
    _reaper: Option<reaper::Reaper>,
    /// How to start it again (same config: same port and token).
    launch: Launch,
    /// Taken over from an earlier yarner rather than started now.
    pub adopted: bool,
}

#[derive(Clone)]
struct Launch {
    bin: PathBuf,
    dir: PathBuf,
    config: PathBuf,
    sources: PathBuf,
    /// Outlives yarner (no reaper; its own process group, so Ctrl-C to yarner spares it).
    keep: bool,
}

impl Launch {
    fn spawn(&self) -> anyhow::Result<(Child, Option<reaper::Reaper>)> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("serve.log"))?;
        let mut cmd = Command::new(&self.bin);
        if self.keep {
            #[cfg(windows)]
            {
                // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
                cmd.creation_flags(0x0000_0200 | 0x0800_0000);
            }
            #[cfg(unix)]
            {
                cmd.process_group(0);
            }
        } else {
            reaper::prepare(&mut cmd);
        }
        let child = cmd
            .arg("--config")
            .arg(&self.config)
            .env("THINFER_SOURCES", &self.sources)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(!self.keep)
            .spawn()
            .with_context(|| format!("starting {}", self.bin.display()))?;
        let reaper = (!self.keep).then(|| reaper::attach(&child));
        Ok((child, reaper))
    }
}

/// What a kept thinfer-serve was started as (`kept.json` beside its config): the next
/// yarner takes it over only if it would start it the same way.
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Kept {
    port: u16,
    token: String,
    pid: Option<u32>,
    /// The binary, its weight sources and options.
    fingerprint: String,
}

/// What a start of thinfer-serve depends on (a change starts a fresh one).
fn fingerprint(spec: &Spec) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    spec.bin.display().to_string().hash(&mut h);
    std::fs::metadata(&spec.bin)
        .and_then(|m| m.modified())
        .ok()
        .hash(&mut h);
    spec.sources
        .as_ref()
        .and_then(|s| std::fs::read(s).ok())
        .hash(&mut h);
    spec.download.hash(&mut h);
    spec.vault_dir.hash(&mut h);
    CONFIG_VERSION.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Bump when the config yarner writes for it changes.
const CONFIG_VERSION: u32 = 4;

async fn healthy(url: &str, token: &str, within: Duration) -> bool {
    let Ok(client) = thinfer_client::Client::builder(url)
        .token(token.to_string())
        .build()
    else {
        return false;
    };
    tokio::time::timeout(within, client.capabilities())
        .await
        .is_ok_and(|r| r.is_ok())
}

/// End a kept thinfer-serve we no longer want (it answered with our token: it's ours).
/// The pid a kept thinfer-serve was recorded with.
fn kept_pid(dir: &std::path::Path) -> Option<u32> {
    std::fs::read(dir.join("kept.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Kept>(&b).ok())
        .and_then(|k| k.pid)
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
}

/// A managed thinfer-serve kept running: restarted (same port and token, so clients
/// keep working) whenever it exits, until stopped.
pub struct Supervised {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    /// Up and answering (false while held, and until it's back after one).
    pub up: tokio::sync::watch::Receiver<bool>,
}

impl Supervised {
    /// Stop it now (and don't restart it).
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

/// Make sure the private thinfer-serve dies with yarner, however yarner ends.
mod reaper {
    #[cfg(windows)]
    pub struct Reaper(windows_sys::Win32::Foundation::HANDLE);

    // SAFETY: a job object handle may be used and closed from any thread.
    #[cfg(windows)]
    unsafe impl Send for Reaper {}
    #[cfg(windows)]
    unsafe impl Sync for Reaper {}

    /// Put the child in a job object that kills its processes when the last handle
    /// closes, which the OS does when this process exits for any reason.
    #[cfg(windows)]
    pub fn attach(child: &tokio::process::Child) -> Reaper {
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: plain FFI with a zeroed, fully owned info struct and our child's handle.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if !job.is_null() {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if let Some(h) = child.raw_handle() {
                    AssignProcessToJobObject(job, h as _);
                }
            }
            Reaper(job)
        }
    }

    #[cfg(windows)]
    impl Drop for Reaper {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: we own the handle; closing it kills the job's processes.
                unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
            }
        }
    }

    #[cfg(not(windows))]
    pub struct Reaper;

    #[cfg(not(windows))]
    pub fn attach(_child: &tokio::process::Child) -> Reaper {
        Reaper
    }

    /// Linux: the child gets SIGKILL when we die (set before exec).
    #[cfg(target_os = "linux")]
    pub fn prepare(cmd: &mut tokio::process::Command) {
        // SAFETY: prctl is async-signal-safe and only affects the forked child.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn prepare(_cmd: &mut tokio::process::Command) {}
}

pub struct Spec {
    /// The thinfer-serve binary.
    pub bin: PathBuf,
    /// Where its config, artifacts, vault and log go.
    pub dir: PathBuf,
    /// Weight sources to use (copied in). None = an empty file: stock weights only.
    pub sources: Option<PathBuf>,
    /// Let it download missing weights.
    pub download: bool,
    /// Leave it running when yarner exits, and take over one left running before.
    pub keep: bool,
    /// The LoRA vault folder to use (another thinfer's, to share its adapters). None: its
    /// own, under `dir`.
    pub vault_dir: Option<PathBuf>,
}

fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// A TOML literal string (no escapes, so Windows paths survive).
fn lit(p: &Path) -> String {
    format!("'{}'", p.display())
}

impl Managed {
    pub async fn start(mut spec: Spec) -> anyhow::Result<Self> {
        std::fs::create_dir_all(spec.dir.join("artifacts"))?;
        // Absolute: thinfer-serve runs from its own directory, so a relative data path
        // (`--data .\data`) would point it at a config that isn't there.
        spec.dir = std::fs::canonicalize(&spec.dir)?;
        let vault = match &spec.vault_dir {
            Some(v) => v.clone(),
            None => spec.dir.join("vault"),
        };
        std::fs::create_dir_all(&vault)?;
        let sources = spec.dir.join("sources.toml");
        match &spec.sources {
            Some(src) => {
                std::fs::copy(src, &sources)
                    .with_context(|| format!("copying {}", src.display()))?;
            }
            None => std::fs::write(&sources, "")?,
        }
        let config = spec.dir.join("serve.toml");
        let kept_file = spec.dir.join("kept.json");
        let print = fingerprint(&spec);
        let launch = Launch {
            bin: spec.bin.clone(),
            dir: spec.dir.clone(),
            config: config.clone(),
            sources: sources.clone(),
            keep: spec.keep,
        };
        // One left running by an earlier yarner: take it over if it was started the same
        // way, else end it (it holds the GPU).
        let earlier: Option<Kept> = std::fs::read(&kept_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if let Some(k) = earlier {
            let url = format!("http://127.0.0.1:{}", k.port);
            let up = healthy(&url, &k.token, Duration::from_secs(3)).await;
            if up && spec.keep && k.fingerprint == print {
                return Ok(Self {
                    url,
                    token: k.token,
                    child: None,
                    _reaper: None,
                    launch,
                    adopted: true,
                });
            }
            if up && let Some(pid) = k.pid {
                tracing::info!("ending the thinfer-serve left running (started differently)");
                kill_pid(pid);
            }
            let _ = std::fs::remove_file(&kept_file);
        }
        let port = free_port()?;
        let token = hex::encode(yarner_format::crypto::random_array::<24>());
        std::fs::write(
            &config,
            format!(
                "# Written by yarner for its private thinfer-serve. Rewritten on every start.\n\
                 bind = \"127.0.0.1:{port}\"\n\
                 auth_token = \"{token}\"\n\
                 tls_self_signed = false\n\
                 workers = 1\n\
                 artifact_dir = {}\n\
                 vault_dir = {}\n\
                 download_as_needed = {}\n\
                 # A story writes text all the time between pictures: keep the text\n\
                 # model warm through them, host side included (tokenizer, pipelines,\n\
                 # page-locked expert cache), for as long as its sessions live.\n\
                 session_ttl_secs = 1800\n\
                 # Each kept session state is ~100-400 MB of host RAM; thinfer's 4G\n\
                 # default kept too few for a story filled out wide (2026-11-22 alpha:\n\
                 # most background beats re-read 10-12k tokens).\n\
                 session_ram_budget = \"8G\"\n\
                 warm_text_secs = 1800\n\
                 keep_text_host = true\n\
                 # The speech model is loaded per job (~3 s): kept warm, the text job\n\
                 # after it failed to load (thinfer 7857f42 sized the text model's\n\
                 # VRAM before freeing it; 2026-11-23 alpha, every voiced screen).\n\
                 warm_audio_secs = 0\n",
                lit(&spec.dir.join("artifacts")),
                lit(&vault),
                spec.download,
            ),
        )?;
        // A fresh log each start of yarner (restarts append to it).
        std::fs::File::create(spec.dir.join("serve.log"))?;
        let (child, reaper) = launch.spawn()?;
        if spec.keep {
            let k = Kept {
                port,
                token: token.clone(),
                pid: child.id(),
                fingerprint: print,
            };
            std::fs::write(&kept_file, serde_json::to_vec(&k)?)?;
        }
        let mut m = Self {
            url: format!("http://127.0.0.1:{port}"),
            token,
            child: Some(child),
            _reaper: reaper,
            launch,
            adopted: false,
        };
        m.wait_ready().await?;
        Ok(m)
    }

    /// Start it again on the same port and token (it died or stopped answering).
    async fn restart(&mut self) -> anyhow::Result<()> {
        let (child, reaper) = self.launch.spawn()?;
        if self.launch.keep {
            let file = self.launch.dir.join("kept.json");
            if let Some(mut k) = std::fs::read(&file)
                .ok()
                .and_then(|b| serde_json::from_slice::<Kept>(&b).ok())
            {
                k.pid = child.id();
                std::fs::write(&file, serde_json::to_vec(&k)?)?;
            }
        }
        self.child = Some(child);
        self._reaper = reaper;
        self.wait_ready().await
    }

    /// Stop it while `hold` is true (the GPU given back: no process, no GPU memory) and
    /// start it again, same port and token, once it's false.
    async fn held(
        &mut self,
        hold: &mut tokio::sync::watch::Receiver<bool>,
        stopped: &mut tokio::sync::oneshot::Receiver<()>,
        up: &tokio::sync::watch::Sender<bool>,
    ) -> bool {
        if !*hold.borrow() {
            return true;
        }
        up.send_replace(false);
        tracing::info!("GPU given back: stopping the private thinfer-serve");
        if let Some(mut c) = self.child.take() {
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
        self._reaper = None;
        tokio::select! {
            _ = &mut *stopped => return false,
            r = hold.wait_for(|h| !*h) => if r.is_err() { return false },
        }
        tracing::info!("GPU taken back: starting the private thinfer-serve");
        loop {
            match self.restart().await {
                Ok(()) => {
                    up.send_replace(true);
                    return true;
                }
                Err(e) => tracing::warn!("thinfer-serve restart: {e:#}"),
            }
            tokio::select! {
                _ = &mut *stopped => return false,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }

    /// Keep it running: whenever it exits (crash, out of memory, killed), start it again
    /// after a short wait that grows if it keeps dying. Stopped while `hold` is true.
    pub fn supervise(mut self, mut hold: tokio::sync::watch::Receiver<bool>) -> Supervised {
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let (up_tx, up) = tokio::sync::watch::channel(true);
        if self.launch.keep {
            return Supervised {
                stop,
                task: tokio::spawn(async move { self.watch(stopped, hold, up_tx).await }),
                up,
            };
        }
        let task = tokio::spawn(async move {
            let mut fails: u32 = 0;
            // Nobody left to change it (its sender gone): never held again.
            let mut hold_gone = false;
            loop {
                if !self.held(&mut hold, &mut stopped, &up_tx).await {
                    return;
                }
                let Some(child) = self.child.as_mut() else {
                    return;
                };
                tokio::select! {
                    _ = &mut stopped => {
                        let _ = child.kill().await;
                        return;
                    }
                    r = hold.changed(), if !hold_gone => hold_gone = r.is_err(),
                    status = child.wait() => {
                        fails += 1;
                        let wait = Duration::from_secs((1u64 << fails.min(5)).min(30));
                        tracing::warn!(
                            "private thinfer-serve exited ({status:?}); restarting in {wait:?}"
                        );
                        tokio::select! {
                            _ = &mut stopped => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
                        match self.restart().await {
                            Ok(()) => {
                                tracing::info!("private thinfer-serve is back");
                                fails = 0;
                            }
                            Err(e) => tracing::warn!("thinfer-serve restart: {e:#}"),
                        }
                    }
                }
            }
        });
        Supervised { stop, task, up }
    }

    /// A kept one: ask it every few seconds; three missed answers in a row (or its
    /// process gone, when it's ours) and it's started again. Never stopped by us.
    async fn watch(
        mut self,
        mut stopped: tokio::sync::oneshot::Receiver<()>,
        mut hold: tokio::sync::watch::Receiver<bool>,
        up: tokio::sync::watch::Sender<bool>,
    ) {
        let mut missed = 0;
        let mut hold_gone = false;
        loop {
            // A kept one we didn't start (adopted) has no child handle: by its pid.
            if *hold.borrow()
                && self.child.is_none()
                && let Some(pid) = kept_pid(&self.launch.dir)
            {
                kill_pid(pid);
            }
            if !self.held(&mut hold, &mut stopped, &up).await {
                return;
            }
            tokio::select! {
                _ = &mut stopped => return,
                r = hold.changed(), if !hold_gone => {
                    hold_gone = r.is_err();
                    continue;
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            let exited = self
                .child
                .as_mut()
                .is_some_and(|c| c.try_wait().ok().flatten().is_some());
            if !exited && healthy(&self.url, &self.token, Duration::from_secs(10)).await {
                missed = 0;
                continue;
            }
            missed += 1;
            if exited || missed >= 3 {
                tracing::warn!("kept thinfer-serve isn't answering; starting it again");
                if let Some(pid) = self.child.as_ref().and_then(|c| c.id()) {
                    kill_pid(pid);
                }
                match self.restart().await {
                    Ok(()) => {
                        tracing::info!("private thinfer-serve is back");
                        missed = 0;
                    }
                    Err(e) => tracing::warn!("thinfer-serve restart: {e:#}"),
                }
            }
        }
    }

    async fn wait_ready(&mut self) -> anyhow::Result<()> {
        let client = thinfer_client::Client::builder(&self.url)
            .token(self.token.clone())
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let t0 = Instant::now();
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(status) = child.try_wait()?
            {
                bail!("thinfer-serve exited during startup ({status}); see its serve.log");
            }
            if client.capabilities().await.is_ok() {
                return Ok(());
            }
            if t0.elapsed() > Duration::from_secs(120) {
                bail!("thinfer-serve didn't come up within 2 minutes; see its serve.log");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Stop it now (a kept one is left running).
    pub async fn stop(mut self) {
        if self.launch.keep {
            return;
        }
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a thinfer-serve left running: answers `/capabilities` for `token`.
    async fn fake_serve(token: &'static str) -> u16 {
        use axum::http::{HeaderMap, StatusCode};
        let app = axum::Router::new().route(
            "/capabilities",
            axum::routing::get(move |h: HeaderMap| async move {
                let ok = h
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v == format!("Bearer {token}"));
                if ok {
                    (StatusCode::OK, r#"{"coopmat":false}"#)
                } else {
                    (StatusCode::UNAUTHORIZED, "{}")
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await });
        port
    }

    fn spec(dir: &Path, keep: bool) -> Spec {
        Spec {
            // Never started in these tests: taking over must not need it.
            bin: dir.join("no-such-thinfer-serve"),
            dir: dir.to_path_buf(),
            sources: None,
            download: false,
            keep,
            vault_dir: None,
        }
    }

    fn leave(dir: &Path, port: u16, token: &str, print: String) {
        let k = Kept {
            port,
            token: token.into(),
            pid: None,
            fingerprint: print,
        };
        std::fs::write(dir.join("kept.json"), serde_json::to_vec(&k).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_kept_thinfer_is_taken_over_when_started_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let d = std::fs::canonicalize(dir.path()).unwrap();
        let port = fake_serve("sekrit").await;
        leave(&d, port, "sekrit", fingerprint(&spec(&d, true)));
        let m = Managed::start(spec(&d, true)).await.unwrap();
        assert!(m.adopted);
        assert_eq!(m.url, format!("http://127.0.0.1:{port}"));
        assert_eq!(m.token, "sekrit");
        // Stopping yarner leaves it running.
        m.supervise(tokio::sync::watch::channel(false).1)
            .stop()
            .await;
        assert!(
            healthy(
                &format!("http://127.0.0.1:{port}"),
                "sekrit",
                Duration::from_secs(3)
            )
            .await
        );
    }

    #[tokio::test]
    async fn a_kept_thinfer_started_differently_isnt_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let d = std::fs::canonicalize(dir.path()).unwrap();
        let port = fake_serve("sekrit").await;
        // Other options; then: not kept this time; then: a token that isn't its.
        for (print, keep, token) in [
            ("something else".to_string(), true, "sekrit"),
            (fingerprint(&spec(&d, false)), false, "sekrit"),
            (fingerprint(&spec(&d, true)), true, "wrong"),
        ] {
            leave(&d, port, token, print);
            // It would have to start a new one, and that binary doesn't exist.
            let e = Managed::start(spec(&d, keep))
                .await
                .err()
                .expect("not taken over");
            assert!(format!("{e:#}").contains("starting"), "{e:#}");
            assert!(!d.join("kept.json").exists());
        }
    }
}
