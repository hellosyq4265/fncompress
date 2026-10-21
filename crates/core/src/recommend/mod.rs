//! 样片推荐 (ADR 0010): encode a few samples with one profile at several
//! quality values, score every trial with VMAF, and propose the qualities
//! whose lowest sample score (最低样片分) reaches the target. Nothing is
//! published; the result is a set of parameter candidates, and it only
//! speaks for the samples tested.
//!
//! The encoder's presets are compared rather than taken from the profile
//! (backlog 18): each preset gets its own quality search on the same
//! samples, and the result shows what each one costs in time and saves in
//! size, so the user picks the preset together with the quality.
//!
//! Samples are files: auto samples are cut from the source by stream copy
//! (from a keyframe, so they decode on their own), ready-made samples are
//! used as they are. Trial encodes use the system FFmpeg with exactly the
//! video options of a real transcode; scoring uses the scoring runtime's
//! FFmpeg, which has libvmaf (ADR 0006). HDR and Dolby Vision sources are
//! scored through a display proxy (ADR 0011); for P5 and P8 each trial gets
//! its sample's RPU first, as the transcode would.

mod samples;
mod score;
mod search;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use serde::{Deserialize, Serialize};

pub use score::{probe_display_proxy, probe_scorer, ScorerInfo};
pub use search::{quality_range, MAX_TRIALS};

use crate::{
    command,
    dovi::{self, mkv::FrameMetadata, DoviTool},
    encoder::Encoder,
    hdr10plus,
    probe::{DoviConfig, DynamicRange, ProbeDepth, ProbeError, Prober},
    route::{self, RangeRoute, RouteOptions, SkipReason},
    settings::{Profile, SettingsSnapshot},
    tool::{CancelToken, ToolError},
};

pub const DEFAULT_TARGET_VMAF: f64 = 95.0;
pub const DEFAULT_SAMPLE_COUNT: u32 = 4;
pub const DEFAULT_SAMPLE_SECS: f64 = 10.0;
pub const MAX_SAMPLES: u32 = 8;

/// How auto samples are picked from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum AutoMethod {
    /// By the source's video bitrate: reads packet headers only, no decoding.
    #[default]
    Bitrate,
    /// Decodes the whole video and picks demanding, varied scenes (detail,
    /// motion, contrast, brightness). Much slower on long films.
    Scene,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SampleSpec {
    /// Cut from the source.
    Auto {
        #[serde(default)]
        method: AutoMethod,
        #[serde(default = "default_count")]
        count: u32,
        /// Length of each sample, seconds.
        #[serde(default = "default_secs")]
        secs: f64,
    },
    /// Ready-made sample files with the source's picture format.
    Files {
        #[cfg_attr(feature = "openapi", schema(value_type = Vec<String>))]
        paths: Vec<PathBuf>,
    },
}

fn default_count() -> u32 {
    DEFAULT_SAMPLE_COUNT
}
fn default_secs() -> f64 {
    DEFAULT_SAMPLE_SECS
}
fn default_target() -> f64 {
    DEFAULT_TARGET_VMAF
}

impl Default for SampleSpec {
    fn default() -> Self {
        SampleSpec::Auto {
            method: AutoMethod::default(),
            count: DEFAULT_SAMPLE_COUNT,
            secs: DEFAULT_SAMPLE_SECS,
        }
    }
}

/// What the user asks for; the profile comes with the settings snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct RecommendRequest {
    /// Every sample's mean VMAF must reach this (default 95).
    #[serde(default = "default_target")]
    pub target_vmaf: f64,
    #[serde(default)]
    pub samples: SampleSpec,
    /// Presets to compare, each with its own quality search. `None`: the
    /// encoder's usual spread ([`Encoder::compared_presets`]) and the base
    /// profile's own preset. Encoders without presets (VAAPI) take none.
    #[serde(default)]
    pub presets: Option<Vec<String>>,
}

impl Default for RecommendRequest {
    fn default() -> Self {
        Self {
            target_vmaf: DEFAULT_TARGET_VMAF,
            samples: SampleSpec::default(),
            presets: None,
        }
    }
}

impl RecommendRequest {
    /// Checks the numbers; files are checked when the task runs.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.target_vmaf > 0.0 && self.target_vmaf <= 100.0) {
            return Err("VMAF 目标应在 0 到 100 之间".into());
        }
        match &self.samples {
            SampleSpec::Auto { count, secs, .. } => {
                if !(1..=MAX_SAMPLES).contains(count) {
                    return Err(format!("样片数量应为 1 到 {MAX_SAMPLES} 个"));
                }
                if !(2.0..=60.0).contains(secs) {
                    return Err("样片长度应为 2 到 60 秒".into());
                }
            }
            SampleSpec::Files { paths } => {
                if paths.is_empty() || paths.len() > MAX_SAMPLES as usize {
                    return Err(format!("现成样片应为 1 到 {MAX_SAMPLES} 个"));
                }
            }
        }
        Ok(())
    }

    /// The presets compared with `profile`, fastest first. An encoder
    /// without presets (VAAPI) is searched once with the profile as it is.
    pub fn presets_for(&self, profile: &Profile) -> Result<Vec<Option<String>>, String> {
        let offered = profile.encoder.presets();
        let own = profile.preset_or_default();
        if offered.is_empty() {
            if self.presets.as_ref().is_some_and(|p| !p.is_empty()) {
                return Err(format!(
                    "{} 没有可以对比的预设",
                    profile.encoder.ffmpeg_name()
                ));
            }
            return Ok(vec![own]);
        }
        let mut list: Vec<String> = match &self.presets {
            None => profile
                .encoder
                .compared_presets()
                .iter()
                .map(|p| p.to_string())
                .chain(own.clone())
                .collect(),
            Some(list) if list.is_empty() => return Err("至少选择一个预设".into()),
            Some(list) => {
                // The profile's own preset may be one the list does not know.
                if let Some(p) = list
                    .iter()
                    .find(|p| !offered.contains(&p.as_str()) && Some(*p) != own.as_ref())
                {
                    return Err(format!("{} 没有预设 {p}", profile.encoder.ffmpeg_name()));
                }
                list.clone()
            }
        };
        list.sort_by_key(|p| offered.iter().position(|o| o == p).unwrap_or(usize::MAX));
        list.dedup();
        Ok(list.into_iter().map(Some).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SampleKind {
    /// Cut from the source.
    Auto,
    /// A ready-made file.
    File,
}

/// 样片.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Sample {
    pub kind: SampleKind,
    /// The source for auto samples, the file otherwise.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub path: PathBuf,
    /// Where an auto sample starts in the source, seconds.
    pub start_s: Option<f64>,
    pub duration_s: f64,
    pub frames: u64,
    /// Video bytes of the sample in the source.
    pub video_bytes: u64,
    pub bitrate_bps: f64,
    /// Why this part was picked.
    pub reason: String,
}

/// One quality value tried on the samples with one preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Trial {
    /// `None` for an encoder without presets (VAAPI).
    #[serde(default)]
    pub preset: Option<String>,
    pub quality: u8,
    /// Mean VMAF per sample, in sample order. `None`: not scored, either
    /// still to come or skipped because another sample already missed the
    /// target (the hardest samples go first).
    pub scores: Vec<Option<f64>>,
    /// Lowest score so far (最低样片分 once every sample is scored).
    pub min_score: Option<f64>,
    /// Every sample has been scored.
    pub done: bool,
    /// Done, and every sample reached the target.
    pub passed: bool,
    /// Encoded video bytes of all samples, once every sample was encoded.
    pub video_bytes: Option<u64>,
    /// `video_bytes` relative to the samples' bytes in the source.
    pub size_ratio: Option<f64>,
    pub bitrate_bps: Option<f64>,
    pub encode_secs: f64,
    /// Seconds of sample video encoded in `encode_secs`.
    #[serde(default)]
    pub encoded_secs: f64,
    pub score_secs: f64,
}

/// A trial that reached the target and is not beaten by a smaller one with
/// a better score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecommendCandidate {
    /// `None` for an encoder without presets (VAAPI).
    #[serde(default)]
    pub preset: Option<String>,
    pub quality: u8,
    /// 最低样片分.
    pub min_score: f64,
    pub scores: Vec<f64>,
    pub video_bytes: u64,
    pub size_ratio: f64,
    pub bitrate_bps: f64,
}

/// How one compared preset did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PresetResult {
    /// `None` for an encoder without presets (VAAPI).
    pub preset: Option<String>,
    /// Its quality search has ended.
    pub done: bool,
    pub trials: u32,
    /// Sample video seconds encoded per second (times real time), over all
    /// its trials. Other work on the machine at the time slows it down too.
    pub speed: Option<f64>,
    /// Encode time of the whole source at that speed, seconds.
    pub full_secs: Option<f64>,
    /// `full_secs` is beyond the transcode's encode timeout.
    pub over_timeout: bool,
    /// Its cheapest candidate.
    pub best: Option<RecommendCandidate>,
    /// No other preset's cheapest candidate is at least as fast and at
    /// least as small, and better in one (the Pareto front).
    pub front: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Recommendation {
    pub target_vmaf: f64,
    pub encoder: Encoder,
    /// Only on recommendations from before presets were compared: the one
    /// preset searched. Each trial and candidate names its preset now.
    pub preset: Option<String>,
    /// The compared presets in the order they run, fastest first.
    #[serde(default)]
    pub presets: Vec<PresetResult>,
    /// Output picture size, where VMAF is measured.
    pub width: u32,
    pub height: u32,
    /// VMAF model, e.g. `vmaf_v0.6.1`.
    pub model: String,
    /// Quality values searched, inclusive; larger is smaller output.
    pub range: [u8; 2],
    pub samples: Vec<Sample>,
    /// Preset by preset, in the order they ran.
    pub trials: Vec<Trial>,
    /// Per preset, in the order of `presets`: cheapest first, at most three.
    pub candidates: Vec<RecommendCandidate>,
    /// Recommendations stored before M5 scored SDR directly.
    #[serde(default)]
    pub scoring: Scoring,
}

impl Recommendation {
    /// Brings a recommendation stored before presets were compared to the
    /// current shape: its one preset goes on every trial and candidate and
    /// into `presets`.
    pub fn upgrade(&mut self) {
        if !self.presets.is_empty() || self.trials.is_empty() {
            return;
        }
        for t in &mut self.trials {
            t.preset = self.preset.clone();
            // Samples are scored right after they are encoded.
            t.encoded_secs = t
                .scores
                .iter()
                .zip(&self.samples)
                .filter(|(s, _)| s.is_some())
                .map(|(_, sample)| sample.duration_s)
                .sum();
        }
        for c in &mut self.candidates {
            c.preset = self.preset.clone();
        }
        let done = self.trials.iter().all(|t| t.done);
        self.presets = vec![search::summarize(
            self.preset.clone(),
            &self.trials,
            &self.candidates,
            done,
            None,
            None,
        )];
        search::mark_front(&mut self.presets);
    }
}

/// How samples and trials are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Scoring {
    /// SDR: VMAF on the pictures themselves.
    #[default]
    Direct,
    /// HDR10, HDR10+, HLG and Dolby Vision P7 (its HDR10 base layer): both
    /// sides mapped to SDR BT.709 (libplacebo, BT.2390) before VMAF. HDR10+
    /// trials carry their sample's HDR10+ messages.
    HdrProxy,
    /// Dolby Vision P5/P8: as the HDR proxy, with the RPU applied on both
    /// sides; trials carry their sample's RPU (and HDR10+ messages).
    DolbyVisionProxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum RecommendStage {
    /// Reading the source (and decoding it for scene analysis).
    Analyzing,
    /// Cutting or checking the samples.
    Sampling,
    Encoding,
    Scoring,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecommendProgress {
    pub stage: RecommendStage,
    /// 1-based index into the recommendation's `presets`; 0 before the
    /// first trial.
    #[serde(default)]
    pub preset: u32,
    #[serde(default)]
    pub presets: u32,
    /// 1-based trial number over all presets; 0 before the first trial.
    pub trial: u32,
    pub quality: Option<u8>,
    /// 1-based sample being worked on; 0 when not per sample.
    pub sample: u32,
    pub samples: u32,
    /// Rough overall progress: the number of trials is not known up front.
    pub percent: f64,
}

pub struct RecommendJob<'a> {
    pub source: &'a Path,
    /// Base profile and global settings.
    pub snapshot: &'a SettingsSnapshot,
    pub request: &'a RecommendRequest,
    /// Scratch directory for this job only; removed when the job ends.
    pub workdir: &'a Path,
    /// Encoders the capability probe found working.
    pub available: Option<&'a [Encoder]>,
    pub cancel: Option<&'a CancelToken>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RecommendOutcome {
    /// The source cannot be transcoded with this profile, so no
    /// recommendation would apply to it.
    Skipped { reason: SkipReason },
    /// The search ran; `candidates` is empty when no trial reached the target.
    Finished { recommendation: Recommendation },
}

#[derive(Debug, thiserror::Error)]
pub enum RecommendError {
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Invalid(String),
    #[error("{what}失败：{source}")]
    Tool {
        what: &'static str,
        source: ToolError,
    },
    #[error("评分结果异常：{0}")]
    Score(String),
    #[error("工作目录不可用：{0}")]
    Io(#[from] std::io::Error),
}

impl RecommendError {
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            RecommendError::Tool {
                source: ToolError::Cancelled { .. },
                ..
            } | RecommendError::Probe(ProbeError::Tool(ToolError::Cancelled { .. }))
        )
    }

    fn tool(what: &'static str) -> impl FnOnce(ToolError) -> Self {
        move |source| RecommendError::Tool { what, source }
    }
}

/// Metadata of one sample that its trials get before scoring.
#[derive(Default)]
struct SampleMetadata {
    rpus: Option<(Vec<Vec<u8>>, DoviConfig)>,
    hdr10_plus: Option<Vec<Vec<u8>>>,
}

impl SampleMetadata {
    fn present(&self) -> bool {
        self.rpus.is_some() || self.hdr10_plus.is_some()
    }

    /// The metadata of the last `frames` frames shown. A sample cut at a CRA
    /// keyframe starts with RASL frames, which come before it in display
    /// order and which the decoder drops, so the trial has fewer frames.
    fn frames(&self, frames: usize) -> Option<FrameMetadata<'_>> {
        fn last(list: &[Vec<u8>], frames: usize) -> &[Vec<u8>] {
            &list[list.len().saturating_sub(frames)..]
        }
        self.present().then(|| FrameMetadata {
            rpus: self.rpus.as_ref().map(|(rpus, c)| (last(rpus, frames), c)),
            hdr10_plus: self.hdr10_plus.as_deref().map(|m| last(m, frames)),
        })
    }
}

/// Removes the scratch directory however the job ends.
struct Workdir<'a>(&'a Path);

impl Drop for Workdir<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0);
    }
}

/// Share of the progress bar before the first trial.
const PREPARE_SHARE: f64 = 10.0;

pub fn run(
    prober: &Prober,
    job: &RecommendJob,
    mut on_progress: impl FnMut(RecommendProgress),
    mut on_update: impl FnMut(&Recommendation),
) -> Result<RecommendOutcome, RecommendError> {
    job.request.validate().map_err(RecommendError::Invalid)?;
    let snapshot = job.snapshot;
    let presets = job
        .request
        .presets_for(&snapshot.profile)
        .map_err(RecommendError::Invalid)?;
    let info = prober.probe(job.source, ProbeDepth::Quick, job.cancel)?;
    let dovi_tool = DoviTool::new(snapshot.global.dovi_tool_path());
    let dovi_unavailable = (info.dynamic_range() == DynamicRange::DolbyVision)
        .then(|| dovi_tool.check().err())
        .flatten();
    // An already processed source may still be studied; the user asked.
    let opts = RouteOptions {
        force: true,
        available: job.available,
        dovi_unavailable: dovi_unavailable.as_deref(),
    };
    let plan = match route::plan(&info, job.source, &snapshot.profile, &opts) {
        Ok(plan) => plan,
        Err(reason) => return Ok(RecommendOutcome::Skipped { reason }),
    };
    let scoring = match plan.dynamic_range {
        RangeRoute::Sdr => Scoring::Direct,
        RangeRoute::DolbyVision { source, .. } if source.profile != 7 => Scoring::DolbyVisionProxy,
        _ => Scoring::HdrProxy,
    };
    let scorer = score::Scorer::new(
        snapshot
            .global
            .scoring_ffmpeg
            .clone()
            .unwrap_or_else(|| snapshot.global.ffmpeg.clone()),
    );
    if scoring != Scoring::Direct {
        score::probe_display_proxy(&scorer.ffmpeg).map_err(|e| {
            RecommendError::Invalid(format!(
                "HDR 片源要用 libplacebo 显示代理评分，但它不可用：{e}"
            ))
        })?;
    }

    fs::create_dir_all(job.workdir)?;
    let _cleanup = Workdir(job.workdir);

    let mut progress = RecommendProgress {
        stage: RecommendStage::Analyzing,
        preset: 0,
        presets: presets.len() as u32,
        trial: 0,
        quality: None,
        sample: 0,
        samples: 0,
        percent: 0.0,
    };
    on_progress(progress);
    let prepared = samples::prepare(prober, job, &info, &plan, |stage, done, total| {
        progress.stage = stage;
        progress.samples = total;
        progress.sample = if stage == RecommendStage::Sampling {
            (done + 1.0).min(f64::from(total)) as u32
        } else {
            0
        };
        progress.percent = match stage {
            RecommendStage::Analyzing => done * PREPARE_SHARE * 0.8,
            _ => PREPARE_SHARE * (0.8 + 0.2 * done / f64::from(total.max(1))),
        };
        on_progress(progress);
    })?;
    let n = prepared.len();
    progress.samples = n as u32;

    let encoder = plan.encoder;
    let (lo, hi) = quality_range(encoder);
    let first = snapshot.profile.quality.clamp(lo, hi);
    let expected = search::expected_trials(lo, hi, presets.len());
    let model = if plan.width >= 3840 || plan.height >= 2160 {
        "vmaf_4k_v0.6.1"
    } else {
        "vmaf_v0.6.1"
    };
    let spec = score::VmafSpec {
        width: plan.width,
        height: plan.height,
        format: score_format(&plan.pix_fmt),
        full_range: plan.full_range,
        model,
        threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
        proxy: (scoring != Scoring::Direct).then_some(score::DisplayProxy {
            dolby_vision: scoring == Scoring::DolbyVisionProxy,
        }),
    };
    // Each sample's metadata, injected into its trials before scoring so
    // the display proxy sees the same on both sides: the RPUs of P5/P8 and
    // HDR10+ messages.
    let mut sample_metadata: Vec<SampleMetadata> = (0..n).map(|_| Default::default()).collect();
    for (i, p) in prepared.iter().enumerate() {
        let timeout = command::encode_timeout(snapshot, Some(p.sample.duration_s));
        if scoring == Scoring::DolbyVisionProxy {
            let (_, output) = plan
                .dynamic_range
                .dolby_vision()
                .expect("a Dolby Vision plan");
            let rpu = job.workdir.join(format!("sample-{i}.rpu"));
            dovi_tool
                .extract(
                    &snapshot.global.ffmpeg,
                    &p.file,
                    p.stream,
                    false,
                    &rpu,
                    timeout,
                    job.cancel,
                )
                .map_err(RecommendError::tool("提取样片 RPU"))?;
            sample_metadata[i].rpus = Some((dovi::read_rpus(&rpu)?, *output));
        }
        if plan.dynamic_range.hdr10_plus() {
            let found = hdr10plus::extract(
                &snapshot.global.ffmpeg,
                &p.file,
                p.stream,
                &job.workdir.join(format!("sample-{i}-sei.mkv")),
                timeout,
                job.cancel,
            )
            .map_err(RecommendError::tool("提取样片 HDR10+ 元数据"))?;
            sample_metadata[i].hdr10_plus = Some(found.into_iter().flatten().collect());
        }
    }
    let target = job.request.target_vmaf;
    let source_bytes: u64 = prepared.iter().map(|p| p.sample.video_bytes).sum();
    let total_secs: f64 = prepared.iter().map(|p| p.sample.duration_s).sum();
    let duration_s = info.format.duration_s;
    let timeout_s = Some(command::encode_timeout(snapshot, duration_s).as_secs_f64());
    let summarize = |rec: &Recommendation, i: usize, done: bool| {
        search::summarize(
            presets[i].clone(),
            &rec.trials,
            &rec.candidates,
            done,
            duration_s,
            timeout_s,
        )
    };
    // Hardest first: a trial that fails stops at the first sample below target.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        prepared[b]
            .sample
            .bitrate_bps
            .total_cmp(&prepared[a].sample.bitrate_bps)
    });

    let mut rec = Recommendation {
        target_vmaf: target,
        encoder,
        preset: None,
        presets: Vec::new(),
        width: plan.width,
        height: plan.height,
        model: model.into(),
        range: [lo, hi],
        samples: prepared.iter().map(|p| p.sample.clone()).collect(),
        trials: Vec::new(),
        candidates: Vec::new(),
        scoring,
    };
    rec.presets = (0..presets.len())
        .map(|i| summarize(&rec, i, false))
        .collect();
    on_update(&rec);

    let mut trial_snapshot = snapshot.clone();
    // Where the previous preset's boundary was: the next one starts there.
    let mut hint: Option<u8> = None;
    for (pi, preset) in presets.iter().enumerate() {
        trial_snapshot.profile.preset = preset.clone();
        progress.preset = pi as u32 + 1;
        let start = rec.trials.len();
        loop {
            let tried: Vec<_> = rec.trials[start..]
                .iter()
                .map(|t| (t.quality, t.passed))
                .collect();
            if tried.len() >= MAX_TRIALS {
                break;
            }
            let next = match hint {
                None => search::next_quality(lo, hi, first, &tried),
                Some(hint) => search::next_near(lo, hi, hint, &tried),
            };
            let Some(q) = next else {
                break;
            };
            trial_snapshot.profile.quality = q;
            rec.trials.push(Trial {
                preset: preset.clone(),
                quality: q,
                scores: vec![None; n],
                min_score: None,
                done: false,
                passed: false,
                video_bytes: None,
                size_ratio: None,
                bitrate_bps: None,
                encode_secs: 0.0,
                encoded_secs: 0.0,
                score_secs: 0.0,
            });
            let index = rec.trials.len() - 1;
            progress.trial = rec.trials.len() as u32;
            progress.quality = Some(q);
            let mut bytes = 0u64;
            let mut encoded_all = true;
            for (k, &i) in order.iter().enumerate() {
                let p = &prepared[i];
                let pct = |part: f64| {
                    let done = index as f64 + (k as f64 + part) / n as f64;
                    (PREPARE_SHARE + (100.0 - PREPARE_SHARE) * done / expected as f64).min(99.0)
                };
                progress.sample = i as u32 + 1;
                progress.stage = RecommendStage::Encoding;
                progress.percent = pct(0.0);
                on_progress(progress);

                let out = job.workdir.join(format!("trial-p{pi}-q{q}-s{i}.mkv"));
                let _ = fs::remove_file(&out);
                let cmd =
                    command::build_trial(&info, &plan, &trial_snapshot, &p.file, p.stream, &out);
                tracing::debug!(command = %cmd.display(), "trial encode");
                let started = Instant::now();
                command::run(
                    &cmd,
                    command::encode_timeout(snapshot, Some(p.sample.duration_s)),
                    job.cancel,
                    |_| {},
                )
                .map_err(RecommendError::tool("试编码"))?;
                let metadata = match sample_metadata[i].present() {
                    true => {
                        let (frames, _) =
                            samples::video_packets(&snapshot.global.ffprobe, &out, 0, job.cancel)?;
                        sample_metadata[i].frames(frames as usize)
                    }
                    false => None,
                };
                let out = match metadata {
                    Some(metadata) => {
                        let with = job.workdir.join(format!("trial-p{pi}-q{q}-s{i}-meta.mkv"));
                        dovi::mkv::inject_frames(&out, &metadata, &with).map_err(|e| {
                            RecommendError::Score(format!(
                                "给样片 {} 的试编码注入 HDR 元数据失败：{e}",
                                i + 1
                            ))
                        })?;
                        let _ = fs::remove_file(&out);
                        with
                    }
                    None => out,
                };
                rec.trials[index].encode_secs += started.elapsed().as_secs_f64();
                rec.trials[index].encoded_secs += p.sample.duration_s;
                let (frames, trial_bytes) =
                    samples::video_packets(&snapshot.global.ffprobe, &out, 0, job.cancel)?;
                bytes += trial_bytes;

                progress.stage = RecommendStage::Scoring;
                progress.percent = pct(0.5);
                on_progress(progress);
                let started = Instant::now();
                let score = score::vmaf(
                    &scorer,
                    &p.file,
                    p.stream,
                    &out,
                    &spec,
                    job.workdir,
                    command::encode_timeout(snapshot, Some(p.sample.duration_s)),
                    job.cancel,
                )?;
                let _ = fs::remove_file(&out);
                rec.trials[index].score_secs += started.elapsed().as_secs_f64();
                if score.frames != frames {
                    return Err(RecommendError::Score(format!(
                        "样片 {} 评分了 {} 帧，试编码有 {frames} 帧",
                        i + 1,
                        score.frames
                    )));
                }
                let t = &mut rec.trials[index];
                t.scores[i] = Some(score.mean);
                t.min_score = Some(t.min_score.map_or(score.mean, |m| m.min(score.mean)));
                on_update(&rec);
                if score.mean < target && k + 1 < n {
                    encoded_all = false;
                    break;
                }
            }
            let t = &mut rec.trials[index];
            t.done = true;
            t.passed = t.scores.iter().all(|s| s.is_some_and(|s| s >= target));
            if encoded_all {
                t.video_bytes = Some(bytes);
                t.size_ratio = (source_bytes > 0).then(|| bytes as f64 / source_bytes as f64);
                t.bitrate_bps = (total_secs > 0.0).then(|| bytes as f64 * 8.0 / total_secs);
            }
            rec.presets[pi] = summarize(&rec, pi, false);
            on_update(&rec);
        }
        let mine = &rec.trials[start..];
        hint = mine
            .iter()
            .filter(|t| t.passed)
            .map(|t| t.quality)
            .max()
            .or_else(|| mine.iter().map(|t| t.quality).min());
        let found = search::candidates(mine);
        rec.candidates.extend(found);
        rec.presets[pi] = summarize(&rec, pi, true);
        search::mark_front(&mut rec.presets);
        on_update(&rec);
    }
    Ok(RecommendOutcome::Finished {
        recommendation: rec,
    })
}

/// Pixel format VMAF compares in: the planar form of what the encoder is fed.
fn score_format(encoder_pix_fmt: &str) -> &str {
    match encoder_pix_fmt {
        "nv12" => "yuv420p",
        "p010le" => "yuv420p10le",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_default_and_validate() {
        let r: RecommendRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(r, RecommendRequest::default());
        assert_eq!(r.target_vmaf, 95.0);
        assert!(r.validate().is_ok());
        let r: RecommendRequest =
            serde_json::from_str(r#"{"samples":{"mode":"auto","method":"scene"}}"#).unwrap();
        assert_eq!(
            r.samples,
            SampleSpec::Auto {
                method: AutoMethod::Scene,
                count: 4,
                secs: 10.0
            }
        );
        let bad = |json: &str| {
            serde_json::from_str::<RecommendRequest>(json)
                .unwrap()
                .validate()
                .unwrap_err()
        };
        assert!(bad(r#"{"target_vmaf":0}"#).contains("VMAF"));
        assert!(bad(r#"{"samples":{"mode":"auto","count":9}}"#).contains("数量"));
        assert!(bad(r#"{"samples":{"mode":"auto","secs":1}}"#).contains("长度"));
        assert!(bad(r#"{"samples":{"mode":"files","paths":[]}}"#).contains("现成样片"));
        assert!(serde_json::from_str::<RecommendRequest>(r#"{"x":1}"#).is_err());
    }

    #[test]
    fn presets_compared_by_default_and_on_request() {
        let profile = |encoder, preset: Option<&str>| Profile {
            encoder,
            preset: preset.map(Into::into),
            ..Default::default()
        };
        let req = |presets: Option<&[&str]>| RecommendRequest {
            presets: presets.map(|p| p.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let names = |r: Result<Vec<Option<String>>, String>| {
            r.unwrap()
                .into_iter()
                .map(|p| p.unwrap())
                .collect::<Vec<_>>()
        };
        // The usual spread, with the profile's own preset in its place.
        let x265 = profile(Encoder::Libx265, Some("veryfast"));
        assert_eq!(
            names(req(None).presets_for(&x265)),
            ["veryfast", "fast", "medium", "slow"]
        );
        let svt = profile(Encoder::Libsvtav1, None);
        assert_eq!(names(req(None).presets_for(&svt)), ["10", "8", "6"]);
        // Asked for: sorted fastest first, duplicates dropped.
        assert_eq!(
            names(req(Some(&["slow", "ultrafast", "slow"])).presets_for(&x265)),
            ["ultrafast", "slow"]
        );
        assert!(req(Some(&[])).presets_for(&x265).is_err());
        assert!(req(Some(&["placebo"]))
            .presets_for(&x265)
            .unwrap_err()
            .contains("placebo"));
        // A preset the list does not know is accepted when it is the profile's.
        let odd = profile(Encoder::Libx265, Some("placebo"));
        assert_eq!(
            names(req(Some(&["placebo", "medium"])).presets_for(&odd)),
            ["medium", "placebo"]
        );
        // VAAPI: one search with the profile as it is.
        let vaapi = profile(Encoder::HevcVaapi, None);
        assert_eq!(req(None).presets_for(&vaapi), Ok(vec![None]));
        assert_eq!(req(Some(&[])).presets_for(&vaapi), Ok(vec![None]));
        assert!(req(Some(&["medium"])).presets_for(&vaapi).is_err());
    }

    #[test]
    fn recommendations_from_before_presets_are_upgraded() {
        let old = serde_json::json!({
            "target_vmaf": 95.0,
            "encoder": "libx265",
            "preset": "medium",
            "width": 1920, "height": 1080, "model": "vmaf_v0.6.1", "range": [14, 38],
            "samples": [
                {"kind": "auto", "path": "/a.mkv", "start_s": 10.0, "duration_s": 10.0,
                 "frames": 240, "video_bytes": 1000, "bitrate_bps": 800.0, "reason": "码率最高"},
                {"kind": "auto", "path": "/a.mkv", "start_s": 50.0, "duration_s": 8.0,
                 "frames": 192, "video_bytes": 500, "bitrate_bps": 500.0, "reason": "码率偏低"}
            ],
            "trials": [
                {"quality": 24, "scores": [96.0, 97.0], "min_score": 96.0, "done": true,
                 "passed": true, "video_bytes": 600, "size_ratio": 0.4, "bitrate_bps": 266.0,
                 "encode_secs": 9.0, "score_secs": 3.0},
                {"quality": 31, "scores": [94.0, null], "min_score": 94.0, "done": true,
                 "passed": false, "video_bytes": null, "size_ratio": null, "bitrate_bps": null,
                 "encode_secs": 3.0, "score_secs": 1.0}
            ],
            "candidates": [
                {"quality": 24, "min_score": 96.0, "scores": [96.0, 97.0], "video_bytes": 600,
                 "size_ratio": 0.4, "bitrate_bps": 266.0}
            ]
        });
        let mut rec: Recommendation = serde_json::from_value(old).unwrap();
        assert!(rec.presets.is_empty());
        rec.upgrade();
        let medium = Some("medium".to_string());
        assert!(rec.trials.iter().all(|t| t.preset == medium));
        assert_eq!(rec.trials[0].encoded_secs, 18.0);
        assert_eq!(rec.trials[1].encoded_secs, 10.0);
        assert_eq!(rec.candidates[0].preset, medium);
        assert_eq!(rec.presets.len(), 1);
        let p = &rec.presets[0];
        assert_eq!(p.preset, medium);
        assert!(p.done);
        assert_eq!(p.trials, 2);
        assert_eq!(p.speed, Some(28.0 / 12.0));
        assert_eq!(p.best.as_ref().map(|b| b.quality), Some(24));
        assert!(p.front);
        // Once upgraded, nothing changes.
        let again = rec.clone();
        rec.upgrade();
        assert_eq!(rec, again);
    }

    #[test]
    fn scores_in_planar_formats() {
        assert_eq!(score_format("nv12"), "yuv420p");
        assert_eq!(score_format("p010le"), "yuv420p10le");
        assert_eq!(score_format("yuv444p10le"), "yuv444p10le");
    }
}
