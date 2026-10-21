use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use fncompress_core::{
    caps, command,
    dovi::DoviTool,
    encoder::Encoder,
    pipeline::{self, Candidate, Job, Outcome},
    probe::{DynamicRange, JsonDirCache, ProbeDepth, Prober},
    recommend::{
        self, AutoMethod, RecommendJob, RecommendOutcome, RecommendRequest, RecommendStage,
        SampleSpec,
    },
    recycle,
    route::{self, RouteOptions},
    settings::{ContainerChoice, Resize, SettingsSnapshot},
    tool::CancelToken,
    verify::Verdict,
};
use serde_json::json;

#[derive(Parser)]
#[command(name = "fncompress", version, about = "NAS video compressor")]
struct Cli {
    /// Settings file (TOML); see `fncompress settings`.
    #[arg(long, global = true, env = "FNCOMPRESS_SETTINGS")]
    settings: Option<PathBuf>,
    #[command(flatten)]
    tools: Tools,
    #[command(subcommand)]
    command: Command,
}

/// Tool locations; these override the settings file.
#[derive(Args, Clone)]
struct Tools {
    /// ffmpeg binary.
    #[arg(long, global = true, env = "FNCOMPRESS_FFMPEG")]
    ffmpeg: Option<PathBuf>,
    /// ffprobe binary.
    #[arg(long, global = true, env = "FNCOMPRESS_FFPROBE")]
    ffprobe: Option<PathBuf>,
    /// Render node used by QSV and VAAPI.
    #[arg(long, global = true)]
    render_device: Option<String>,
    /// FFmpeg with libvmaf for 样片推荐 (the scoring runtime's ffmpeg).
    #[arg(long, global = true, env = "FNCOMPRESS_SCORING_FFMPEG")]
    scoring_ffmpeg: Option<PathBuf>,
    /// dovi_tool for Dolby Vision sources (default: next to the scoring
    /// ffmpeg, else on PATH).
    #[arg(long, global = true, env = "FNCOMPRESS_DOVI_TOOL")]
    dovi_tool: Option<PathBuf>,
}

/// Profile fields that override the settings file for one run.
#[derive(Args)]
struct ProfileOverrides {
    /// Profile id written into the processed marker.
    #[arg(long)]
    profile_id: Option<String>,
    /// Encoder by FFmpeg name, e.g. libx265, hevc_qsv, hevc_vaapi.
    #[arg(long)]
    encoder: Option<Encoder>,
    /// CRF / global_quality / QP, depending on the encoder.
    #[arg(long)]
    quality: Option<u8>,
    #[arg(long)]
    preset: Option<String>,
    /// keep, mkv or mp4.
    #[arg(long, value_parser = parse_container)]
    container: Option<ContainerChoice>,
    /// Scale down to at most this height.
    #[arg(long)]
    max_height: Option<u32>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the default settings as TOML, as a starting point for --settings.
    Settings,
    /// List the encoders this build knows about and the quota each one uses.
    Encoders {
        /// Run a one-second trial encode with each encoder to see which work here.
        #[arg(long)]
        probe: bool,
    },
    /// Probe a media file and print the structured result as JSON.
    Probe {
        file: PathBuf,
        /// Also read every packet header: counts, start and end times per stream.
        #[arg(long)]
        full: bool,
        /// Cache results here, keyed by file identity.
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Decide the route for a file and encode a candidate next to it.
    Transcode {
        file: PathBuf,
        #[command(flatten)]
        overrides: ProfileOverrides,
        /// Process a file that already carries the processed marker.
        #[arg(long)]
        force: bool,
        /// Print the route decision and the FFmpeg command without encoding.
        #[arg(long)]
        dry_run: bool,
        /// Publish a passing candidate over the source (the old source goes to the recycle).
        #[arg(long)]
        publish: bool,
        #[arg(long)]
        cache_dir: Option<PathBuf>,
    },
    /// Run the tiered checks on a candidate against its source. Pass the same
    /// settings as the transcode: they determine the expected output.
    Verify {
        source: PathBuf,
        candidate: PathBuf,
        #[command(flatten)]
        overrides: ProfileOverrides,
    },
    /// Publish a candidate: re-verify it, check the savings, replace the source
    /// and recycle it. Pass the same settings as the transcode.
    Publish {
        source: PathBuf,
        candidate: PathBuf,
        /// Release a candidate that failed hard checks (人工放行).
        #[arg(long)]
        release: bool,
        #[command(flatten)]
        overrides: ProfileOverrides,
    },
    /// 样片推荐: encode samples of the file with the profile's encoder at
    /// several presets and quality values, score them with VMAF and print the
    /// candidates whose lowest sample score reaches the target. Nothing is
    /// published.
    Recommend {
        file: PathBuf,
        #[command(flatten)]
        overrides: ProfileOverrides,
        /// Every sample's mean VMAF must reach this.
        #[arg(long, default_value_t = recommend::DEFAULT_TARGET_VMAF)]
        target: f64,
        /// Number of auto samples.
        #[arg(long, default_value_t = recommend::DEFAULT_SAMPLE_COUNT)]
        samples: u32,
        /// Length of each auto sample, seconds.
        #[arg(long, default_value_t = recommend::DEFAULT_SAMPLE_SECS)]
        sample_secs: f64,
        /// Pick auto samples by decoding and measuring scenes instead of by
        /// bitrate (slow on long films).
        #[arg(long)]
        scene: bool,
        /// A ready-made sample to use instead of auto samples (repeatable).
        #[arg(long = "sample")]
        sample_files: Vec<PathBuf>,
        /// Presets to compare, comma-separated, e.g. `fast,medium,slow`; the
        /// encoder's usual spread and the profile's preset by default.
        #[arg(long, value_delimiter = ',')]
        compare_presets: Option<Vec<String>>,
        /// Scratch directory; a new one under the system temp directory by default.
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Manage a recycle directory, e.g. /vol1/Movies/.fncompress-recycle.
    Recycle {
        #[command(subcommand)]
        action: RecycleAction,
    },
    /// Print the HTTP API's OpenAPI document (for openapi-typescript in web/).
    Openapi,
    /// Run the server: task scheduler, directory monitoring and the HTTP API
    /// on one port. Settings live in the database under --data-dir.
    Serve {
        #[arg(long, default_value = "0.0.0.0:5180", env = "FNCOMPRESS_LISTEN")]
        listen: std::net::SocketAddr,
        /// Database and server state.
        #[arg(long, default_value = "fncompress-data", env = "FNCOMPRESS_DATA")]
        data_dir: PathBuf,
        /// Skip the startup trial encode (every encoder counts as available).
        #[arg(long)]
        no_encoder_probe: bool,
        /// Also serve on this Unix socket (the fnOS unified gateway at
        /// /app/fncompress/).
        #[arg(long, env = "FNCOMPRESS_SOCKET")]
        socket: Option<PathBuf>,
        /// File with the directories authorized in fnOS (`:`-separated),
        /// listed first when browsing.
        #[arg(long, env = "FNCOMPRESS_ROOTS_FILE")]
        roots_file: Option<PathBuf>,
        /// The scoring FFmpeg shipped with the package; used when the
        /// scoring FFmpeg setting is empty.
        #[arg(long, env = "FNCOMPRESS_BUNDLED_SCORING_FFMPEG")]
        bundled_scoring_ffmpeg: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum RecycleAction {
    /// List entries, oldest first.
    List { dir: PathBuf },
    /// Delete entries older than the retention period.
    Purge {
        dir: PathBuf,
        /// Retention in days; defaults to the settings value.
        #[arg(long)]
        days: Option<u32>,
    },
    /// Delete every entry.
    Empty { dir: PathBuf },
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    DryRun,
    Encode,
    Publish,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The server logs at info unless RUST_LOG says otherwise; CLI commands
    // stay quiet.
    let default = if matches!(cli.command, Command::Serve { .. }) {
        "info"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    if let Command::Serve {
        listen,
        data_dir,
        no_encoder_probe,
        socket,
        roots_file,
        bundled_scoring_ffmpeg,
    } = cli.command
    {
        let opts = fncompress::server::ServeOptions {
            listen,
            data_dir,
            ffmpeg: cli.tools.ffmpeg,
            ffprobe: cli.tools.ffprobe,
            render_device: cli.tools.render_device,
            scoring_ffmpeg: cli.tools.scoring_ffmpeg,
            dovi_tool: cli.tools.dovi_tool,
            bundled_scoring_ffmpeg,
            socket,
            roots_file,
            probe_encoders: !no_encoder_probe,
        };
        return tokio::runtime::Runtime::new()?.block_on(fncompress::server::serve(opts));
    }
    let mut snapshot = load_settings(cli.settings.as_deref())?;
    apply_tools(&mut snapshot, cli.tools);
    let global = &snapshot.global;

    match cli.command {
        Command::Settings => print!("{}", toml::to_string_pretty(&snapshot)?),
        Command::Encoders { probe: false } => {
            let rows: Vec<_> = Encoder::ALL
                .iter()
                .map(|e| json!({ "encoder": e, "quota": e.quota() }))
                .collect();
            print_json(&rows)?;
        }
        Command::Encoders { probe: true } => {
            print_json(&caps::probe_all(&global.ffmpeg, &global.render_device))?;
        }
        Command::Probe {
            file,
            full,
            cache_dir,
        } => {
            let depth = if full {
                ProbeDepth::Full
            } else {
                ProbeDepth::Quick
            };
            let info = prober(&snapshot, cache_dir).probe(&file, depth, None)?;
            print_json(&json!({ "dynamic_range": info.dynamic_range(), "info": info }))?;
        }
        Command::Transcode {
            file,
            overrides,
            force,
            dry_run,
            publish,
            cache_dir,
        } => {
            apply_overrides(&mut snapshot, overrides);
            let mode = match (dry_run, publish) {
                (true, _) => Mode::DryRun,
                (false, true) => Mode::Publish,
                (false, false) => Mode::Encode,
            };
            transcode(&snapshot, &file, force, mode, cache_dir)?;
        }
        Command::Verify {
            source,
            candidate,
            overrides,
        } => {
            apply_overrides(&mut snapshot, overrides);
            verify(&snapshot, &source, &candidate)?;
        }
        Command::Publish {
            source,
            candidate,
            release,
            overrides,
        } => {
            apply_overrides(&mut snapshot, overrides);
            publish(&snapshot, &source, &candidate, release)?;
        }
        Command::Recommend {
            file,
            overrides,
            target,
            samples,
            sample_secs,
            scene,
            sample_files,
            compare_presets,
            workdir,
        } => {
            apply_overrides(&mut snapshot, overrides);
            let samples = if sample_files.is_empty() {
                SampleSpec::Auto {
                    method: if scene {
                        AutoMethod::Scene
                    } else {
                        AutoMethod::Bitrate
                    },
                    count: samples,
                    secs: sample_secs,
                }
            } else {
                SampleSpec::Files {
                    paths: sample_files,
                }
            };
            let request = RecommendRequest {
                target_vmaf: target,
                samples,
                presets: compare_presets,
            };
            recommend_cli(&snapshot, &file, &request, workdir)?;
        }
        Command::Recycle { action } => match action {
            RecycleAction::List { dir } => print_json(&recycle::list(&dir)?)?,
            RecycleAction::Purge { dir, days } => {
                let days = days.unwrap_or(snapshot.global.recycle_retention_days);
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                print_json(&recycle::purge_expired(&dir, days, now)?)?;
            }
            RecycleAction::Empty { dir } => print_json(&recycle::empty(&dir)?)?,
        },
        Command::Openapi => println!("{}", fncompress::api::openapi().to_pretty_json()?),
        Command::Serve { .. } => unreachable!("handled above"),
    }
    Ok(())
}

fn transcode(
    snapshot: &SettingsSnapshot,
    file: &Path,
    force: bool,
    mode: Mode,
    cache_dir: Option<PathBuf>,
) -> Result<()> {
    let prober = prober(snapshot, cache_dir);
    if mode == Mode::DryRun {
        let info = prober.probe(file, ProbeDepth::Full, None)?;
        let dovi_unavailable = (info.dynamic_range() == DynamicRange::DolbyVision)
            .then(|| {
                DoviTool::new(snapshot.global.dovi_tool_path())
                    .check()
                    .err()
            })
            .flatten();
        let opts = RouteOptions {
            force,
            available: None,
            dovi_unavailable: dovi_unavailable.as_deref(),
        };
        let plan = match route::plan(&info, file, &snapshot.profile, &opts) {
            Ok(plan) => plan,
            Err(reason) => return print_json(&Outcome::Skipped { reason }),
        };
        let candidate = command::candidate_path(file, plan.container);
        if plan.dynamic_range.dolby_vision().is_some() {
            let video = command::work_path(file, "video.mkv");
            let cmd = command::build_trial(&info, &plan, snapshot, file, plan.video_index, &video);
            return print_json(&json!({
                "outcome": "planned",
                "fingerprint": snapshot.profile.fingerprint(),
                "plan": plan,
                "steps": "dovi_tool 提取 RPU → 只编码视频（command）→ 注入 RPU → 与其他流封装",
                "command": cmd.display(),
            }));
        }
        let cmd = command::build(&info, &plan, snapshot, file, &candidate);
        return print_json(&json!({
            "outcome": "planned",
            "fingerprint": snapshot.profile.fingerprint(),
            "plan": plan,
            "command": cmd.display(),
        }));
    }

    // FFmpeg runs in its own process group, so a terminal Ctrl-C never reaches
    // it; turn SIGINT/SIGTERM into a cancel that kills the group and removes
    // the partial candidate.
    let cancel = CancelToken::default();
    let token = cancel.clone();
    ctrlc::set_handler(move || {
        eprintln!("正在取消……");
        token.cancel();
    })?;

    let started = Instant::now();
    let mut last = Instant::now();
    let job = Job {
        source: file,
        snapshot,
        force,
        available: None,
        publish: mode == Mode::Publish,
        cancel: Some(&cancel),
    };
    let outcome = pipeline::transcode_file(&prober, &job, |p| {
        if last.elapsed() >= Duration::from_secs(5) {
            last = Instant::now();
            let speed = p.speed.map_or(String::new(), |s| format!(" ({s:.2}x)"));
            eprintln!("进度 {:.1}s{speed}", p.out_time_s);
        }
    })?;
    eprintln!("用时 {:.1}s", started.elapsed().as_secs_f64());
    print_json(&outcome)
}

fn recommend_cli(
    snapshot: &SettingsSnapshot,
    file: &Path,
    request: &RecommendRequest,
    workdir: Option<PathBuf>,
) -> Result<()> {
    let workdir = workdir.unwrap_or_else(|| {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::env::temp_dir().join(format!("fncompress-recommend-{nonce:x}"))
    });
    let cancel = CancelToken::default();
    let token = cancel.clone();
    ctrlc::set_handler(move || {
        eprintln!("正在取消……");
        token.cancel();
    })?;
    let job = RecommendJob {
        source: file,
        snapshot,
        request,
        workdir: &workdir,
        available: None,
        cancel: Some(&cancel),
    };
    let prober = Prober::new(&snapshot.global.ffprobe);
    let started = Instant::now();
    let mut last = None;
    let outcome = recommend::run(
        &prober,
        &job,
        |p| {
            let key = (p.stage, p.trial, p.sample);
            if last == Some(key) {
                return;
            }
            last = Some(key);
            let stage = match p.stage {
                RecommendStage::Analyzing => "分析源片".to_owned(),
                RecommendStage::Sampling => format!("准备样片 {}/{}", p.sample, p.samples),
                RecommendStage::Encoding | RecommendStage::Scoring => format!(
                    "预设 {}/{} 试编码 {}（质量 {}）样片 {}/{} {}",
                    p.preset,
                    p.presets,
                    p.trial,
                    p.quality.unwrap_or_default(),
                    p.sample,
                    p.samples,
                    if p.stage == RecommendStage::Encoding {
                        "编码"
                    } else {
                        "评分"
                    }
                ),
            };
            eprintln!("[{:>3.0}%] {stage}", p.percent);
        },
        |_| {},
    )?;
    eprintln!("用时 {:.1}s", started.elapsed().as_secs_f64());
    if let RecommendOutcome::Finished {
        recommendation: rec,
    } = &outcome
    {
        for t in &rec.trials {
            let scores: Vec<String> = t
                .scores
                .iter()
                .map(|s| s.map_or("—".into(), |s| format!("{s:.2}")))
                .collect();
            eprintln!(
                "预设 {} 质量 {:>2}：{} {}",
                t.preset.as_deref().unwrap_or("—"),
                t.quality,
                scores.join(" / "),
                if t.passed { "达标" } else { "未达标" }
            );
        }
        for p in &rec.presets {
            let best = p.best.as_ref().map_or("无达标".into(), |b| {
                format!("质量 {} 体积 {:.0}%", b.quality, b.size_ratio * 100.0)
            });
            eprintln!(
                "预设 {}：{best}，速度 {}{}",
                p.preset.as_deref().unwrap_or("—"),
                p.speed.map_or("—".into(), |s| format!("{s:.2}x")),
                if p.front { "（前沿）" } else { "" }
            );
        }
        if rec.candidates.is_empty() {
            eprintln!("没有达到 VMAF {} 的质量值", rec.target_vmaf);
        }
    }
    print_json(&outcome)
}

fn assess(snapshot: &SettingsSnapshot, source: &Path, candidate: &Path) -> Result<Candidate> {
    let prober = Prober::new(&snapshot.global.ffprobe);
    pipeline::assess(&prober, source, candidate, snapshot)?
        .map_err(|skip| anyhow!("按当前设置无法为源片规划路线：{skip}"))
}

fn verify(snapshot: &SettingsSnapshot, source: &Path, candidate: &Path) -> Result<()> {
    print_json(&assess(snapshot, source, candidate)?.report)
}

fn publish(
    snapshot: &SettingsSnapshot,
    source: &Path,
    candidate: &Path,
    release: bool,
) -> Result<()> {
    let c = assess(snapshot, source, candidate)?;
    if c.report.verdict == Verdict::Rejected && !release {
        print_json(&c.report)?;
        bail!("成品有硬性项未通过；确认无误后可加 --release 人工放行");
    }
    print_json(&pipeline::publish_candidate(c, snapshot)?)
}

fn load_settings(path: Option<&Path>) -> Result<SettingsSnapshot> {
    let Some(path) = path else {
        return Ok(SettingsSnapshot::default());
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取设置文件 {}", path.display()))?;
    SettingsSnapshot::from_toml(&text).with_context(|| format!("解析设置文件 {}", path.display()))
}

fn apply_tools(snapshot: &mut SettingsSnapshot, tools: Tools) {
    let g = &mut snapshot.global;
    if let Some(p) = tools.ffmpeg {
        g.ffmpeg = p;
    }
    if let Some(p) = tools.ffprobe {
        g.ffprobe = p;
    }
    if let Some(d) = tools.render_device {
        g.render_device = d;
    }
    if let Some(p) = tools.scoring_ffmpeg {
        g.scoring_ffmpeg = Some(p);
    }
    if let Some(p) = tools.dovi_tool {
        g.dovi_tool = Some(p);
    }
}

fn apply_overrides(snapshot: &mut SettingsSnapshot, o: ProfileOverrides) {
    let p = &mut snapshot.profile;
    if let Some(v) = o.profile_id {
        p.id = v;
    }
    if let Some(v) = o.encoder {
        p.encoder = v;
    }
    if let Some(v) = o.quality {
        p.quality = v;
    }
    if let Some(v) = o.preset {
        p.preset = Some(v);
    }
    if let Some(v) = o.container {
        p.container = v;
    }
    if let Some(v) = o.max_height {
        p.resize = Resize::MaxHeight(v);
    }
}

fn prober(snapshot: &SettingsSnapshot, cache_dir: Option<PathBuf>) -> Prober {
    let prober = Prober::new(&snapshot.global.ffprobe);
    match cache_dir {
        Some(dir) => prober.with_cache(JsonDirCache::new(dir)),
        None => prober,
    }
}

fn parse_container(s: &str) -> Result<ContainerChoice, String> {
    match s {
        "keep" => Ok(ContainerChoice::Keep),
        "mkv" => Ok(ContainerChoice::Mkv),
        "mp4" => Ok(ContainerChoice::Mp4),
        _ => Err(format!("{s}：应为 keep、mkv 或 mp4")),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
