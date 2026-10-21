//! Shared helpers for integration tests that need FFmpeg. Tools come from
//! `FNCOMPRESS_FFMPEG` / `FNCOMPRESS_FFPROBE` or `PATH`; without them the tests
//! print a note and pass, so `cargo test` still works on a bare machine.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

use fncompress::{
    app::App,
    db::Db,
    events::Events,
    model::{SavedProfile, ServerSettings, Task, TaskState},
    scheduler::Scheduler,
};
use fncompress_core::{
    encoder::Encoder,
    settings::{GlobalSettings, Profile},
};

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

pub fn tools() -> Option<Tools> {
    let find = |var: &str, name: &str| -> Option<PathBuf> {
        let path = std::env::var_os(var).map_or_else(|| PathBuf::from(name), PathBuf::from);
        let ok = Command::new(&path)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success());
        ok.then_some(path)
    };
    let tools = Tools {
        ffmpeg: find("FNCOMPRESS_FFMPEG", "ffmpeg")?,
        ffprobe: find("FNCOMPRESS_FFPROBE", "ffprobe")?,
    };
    Some(tools)
}

/// Returns early from a test when FFmpeg is not available.
#[macro_export]
macro_rules! require_tools {
    () => {
        match common::tools() {
            Some(t) => t,
            None => {
                eprintln!(
                    "skipped: ffmpeg/ffprobe not found (set FNCOMPRESS_FFMPEG/FNCOMPRESS_FFPROBE)"
                );
                return;
            }
        }
    };
}

/// Runs ffmpeg with `args` after the usual quiet, non-interactive options.
pub fn ffmpeg(tools: &Tools, args: &[&str]) {
    let out = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-v", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An H.264 + AAC MKV of `secs` seconds, encoded at high quality so a
/// re-encode saves plenty.
pub fn sample(tools: &Tools, path: &Path, secs: u32) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    let video = format!("testsrc2=s=320x240:r=25:d={secs}");
    let audio = format!("sine=d={secs}:sample_rate=48000");
    ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            &video,
            "-f",
            "lavfi",
            "-i",
            &audio,
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "8",
            "-c:a",
            "aac",
            path.to_str().unwrap(),
        ],
    );
}

pub struct Harness {
    pub app: Arc<App>,
    pub dir: tempfile::TempDir,
    pub runner: tokio::task::JoinHandle<()>,
}

/// A server core (database, scheduler loop) in a temp directory, with a
/// fast libx265 default profile and the test tools.
pub fn harness(
    tools: &Tools,
    server: ServerSettings,
    tweak: impl FnOnce(&mut GlobalSettings),
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&dir.path().join("fncompress.db")).unwrap());
    let mut global = GlobalSettings {
        ffmpeg: tools.ffmpeg.clone(),
        ffprobe: tools.ffprobe.clone(),
        ..Default::default()
    };
    tweak(&mut global);
    db.set_global_settings(&global).unwrap();
    db.set_server_settings(&server).unwrap();
    db.put_profile(&SavedProfile {
        id: "default".into(),
        name: "软件 HEVC（x265）".into(),
        profile: Profile {
            encoder: Encoder::Libx265,
            quality: 32,
            preset: Some("ultrafast".into()),
            ..Default::default()
        },
    })
    .unwrap();
    let events = Events::default();
    let scheduler = Scheduler::new(
        db.clone(),
        events.clone(),
        None,
        dir.path().join("recommend"),
    );
    let app = Arc::new(App::new(db, events, scheduler.clone(), vec![]));
    let runner = tokio::spawn(async move { scheduler.run().await });
    Harness { app, dir, runner }
}

/// No observation mode, one slot each, no space reserve (the NAS's /tmp
/// may be a small tmpfs).
pub fn plain() -> ServerSettings {
    ServerSettings {
        observe_first_n: 0,
        min_free_gib: 0,
        ..Default::default()
    }
}

/// Polls until the task satisfies `done`.
pub async fn wait_for(app: &App, id: i64, done: impl Fn(&Task) -> bool) -> Task {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let t = app.task(id).unwrap();
        if done(&t) {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "task #{id} stuck in {:?}",
            t.state
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits until the task left the queue and the scheduler.
pub async fn settled(app: &App, id: i64) -> Task {
    wait_for(app, id, |t| {
        t.state != TaskState::Queued && t.state != TaskState::Running
    })
    .await
}
