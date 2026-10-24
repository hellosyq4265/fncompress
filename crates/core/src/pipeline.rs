//! One transcode from source to result: probe, route, predict the savings
//! (when the profile asks, [`crate::predict`]), encode, probe the candidate,
//! verify, check the savings, publish. The CLI and the M2 scheduler share
//! this. Dolby Vision and HDR10+ routes encode in steps
//! (see [`crate::dovi`], [`crate::hdr10plus`]) and verify the RPU and the
//! HDR10+ messages as well.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    command::{self, Progress},
    dovi::{self, mkv::FrameMetadata, DoviTool, RpuSummary, WorkFiles},
    encoder::Encoder,
    hdr10plus,
    identity::FileIdentity,
    inspect,
    predict::{self, Estimate, PredictProgress},
    probe::{DynamicRange, MediaInfo, ProbeDepth, ProbeError, Prober},
    publish::{self, PublishError, PublishRequest, Published, Savings},
    recycle,
    route::{self, RouteOptions, RoutePlan, SkipReason},
    settings::SettingsSnapshot,
    tool::{CancelToken, ToolError},
    verify::{self, Expectation, Finding, Inspection, Report, Severity, Verdict},
};

pub struct Job<'a> {
    pub source: &'a Path,
    pub snapshot: &'a SettingsSnapshot,
    /// Manual transcode of an already-marked file.
    pub force: bool,
    /// Encoders the capability probe found working.
    pub available: Option<&'a [Encoder]>,
    /// Publish a passing candidate; otherwise stop at [`Outcome::Verified`].
    pub publish: bool,
    pub cancel: Option<&'a CancelToken>,
}

/// What a transcode reports while it runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobProgress {
    /// Encoding the samples of the savings prediction.
    Predicting(PredictProgress),
    Encoding(Progress),
}

/// A verified (or rejected) candidate next to its source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Candidate {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub source: PathBuf,
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub path: PathBuf,
    /// Where publication puts it.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub target: PathBuf,
    /// Identities when verification finished; publication refuses if either
    /// file changed since.
    pub source_identity: FileIdentity,
    pub identity: FileIdentity,
    pub savings: Savings,
    pub report: Report,
    pub plan: RoutePlan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Protective skip (保护性跳过): nothing was encoded.
    Skipped { reason: SkipReason },
    /// A hard check failed: the candidate awaits manual release (待确认成品).
    Rejected { candidate: Candidate },
    /// 预判无收益: the savings prediction put the saving well below the
    /// threshold, so nothing but its samples was encoded; the source stays.
    PredictedNoGain {
        estimate: Estimate,
        min_savings_pct: f64,
    },
    /// Saved less than the threshold (收益不足), whatever the verdict in
    /// `report`; the candidate is gone and the source stays.
    InsufficientSavings {
        savings: Savings,
        min_savings_pct: f64,
        report: Report,
    },
    /// Passed, not published (publication was not requested).
    Verified { candidate: Candidate },
    Published {
        published: Published,
        savings: Savings,
        report: Report,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Probe(#[from] ProbeError),
    #[error("编码失败：{0}")]
    Encode(#[from] ToolError),
    #[error("校验失败：{0}")]
    Inspect(ToolError),
    #[error("杜比视界处理失败：{0}")]
    DolbyVision(String),
    #[error("HDR10+ 处理失败：{0}")]
    Hdr10Plus(String),
    #[error(transparent)]
    Publish(#[from] PublishError),
}

pub fn transcode_file(
    prober: &Prober,
    job: &Job,
    mut on_progress: impl FnMut(JobProgress),
) -> Result<Outcome, PipelineError> {
    let source = job.source;
    let snapshot = job.snapshot;
    let info = prober.probe(source, ProbeDepth::Full, job.cancel)?;
    // Dolby Vision routes need dovi_tool; without it they are skipped.
    let dovi_unavailable = (info.dynamic_range() == DynamicRange::DolbyVision)
        .then(|| {
            DoviTool::new(snapshot.global.dovi_tool_path())
                .check()
                .err()
        })
        .flatten();
    let opts = RouteOptions {
        force: job.force,
        available: job.available,
        dovi_unavailable: dovi_unavailable.as_deref(),
    };
    let plan = match route::plan(&info, source, &snapshot.profile, &opts) {
        Ok(plan) => plan,
        Err(reason) => return Ok(Outcome::Skipped { reason }),
    };

    let min = snapshot.global.min_savings_pct;
    // The estimate of a transcode that runs goes into its report.
    let mut prediction = None;
    if snapshot.profile.predict_savings {
        match predict::estimate(&info, &plan, snapshot, source, job.cancel, |p| {
            on_progress(JobProgress::Predicting(p))
        }) {
            Ok(Some(estimate)) if estimate.no_gain(min) => {
                return Ok(Outcome::PredictedNoGain {
                    estimate,
                    min_savings_pct: min,
                })
            }
            Ok(estimate) => prediction = Some(Ok(estimate)),
            Err(e @ ToolError::Cancelled { .. }) => return Err(e.into()),
            // A failed estimate costs the transcode nothing.
            Err(e) => {
                tracing::warn!(source = %source.display(), "savings prediction failed: {e}");
                prediction = Some(Err(e));
            }
        }
    }

    let path = command::candidate_path(source, plan.container);
    // Intermediate files of a Dolby Vision or HDR10+ route; the RPU file
    // lives until verification has compared the candidate's with it.
    let mut work = WorkFiles::default();
    let injected = if plan.dynamic_range.injects() {
        encode_with_metadata(
            &info,
            &plan,
            snapshot,
            source,
            &path,
            &mut work,
            job.cancel,
            |p| on_progress(JobProgress::Encoding(p)),
        )?
    } else {
        let cmd = command::build(&info, &plan, snapshot, source, &path);
        tracing::info!(source = %source.display(), command = %cmd.display(), "encoding");
        command::run(
            &cmd,
            command::encode_timeout(snapshot, info.format.duration_s),
            job.cancel,
            |p| on_progress(JobProgress::Encoding(p)),
        )?;
        Injected::default()
    };

    let out = match prober.probe(&path, ProbeDepth::Full, job.cancel) {
        Ok(out) => out,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e.into());
        }
    };
    let inspection = match inspect::inspect(
        source,
        &info,
        &path,
        &out,
        &plan,
        snapshot,
        &injected.expected(),
        job.cancel,
    ) {
        Ok(i) => i,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(PipelineError::Inspect(e));
        }
    };
    let mut report = verify_against_plan(&info, &out, &plan, snapshot, &inspection);
    if let Some((_, summary)) = &injected.rpu {
        report.findings.push(dolby_vision_note(&plan, summary));
    }
    if let Some(prediction) = &prediction {
        report.findings.push(prediction_note(prediction));
    }
    drop(work);
    let candidate = Candidate {
        source: source.to_owned(),
        path,
        target: plan.target.clone(),
        source_identity: info.identity,
        identity: out.identity,
        savings: Savings::new(info.identity.size, out.identity.size),
        report,
        plan,
    };

    // Savings first: a candidate that saves too little is never published,
    // so it is not worth a review whatever the checks said. The report
    // keeps the verdict.
    if !candidate.savings.sufficient(min) {
        let _ = std::fs::remove_file(&candidate.path);
        return Ok(insufficient(candidate, min));
    }
    if candidate.report.verdict == Verdict::Rejected {
        return Ok(Outcome::Rejected { candidate });
    }
    if !job.publish {
        return Ok(Outcome::Verified { candidate });
    }
    publish_candidate(candidate, snapshot)
}

/// Savings check and publication of a candidate: the last step of
/// [`transcode_file`], and the manual release of a pending output. An
/// insufficient candidate is left on disk for the caller to decide.
pub fn publish_candidate(
    candidate: Candidate,
    snapshot: &SettingsSnapshot,
) -> Result<Outcome, PipelineError> {
    let min = snapshot.global.min_savings_pct;
    if !candidate.savings.sufficient(min) {
        return Ok(insufficient(candidate, min));
    }
    let recycle_dir = recycle::dir_for(&candidate.source, &snapshot.global);
    let published = publish::publish(&PublishRequest {
        source: &candidate.source,
        candidate: &candidate.path,
        target: &candidate.target,
        source_identity: candidate.source_identity,
        candidate_identity: candidate.identity,
        recycle_dir: &recycle_dir,
    })?;
    Ok(Outcome::Published {
        published,
        savings: candidate.savings,
        report: candidate.report,
    })
}

fn insufficient(candidate: Candidate, min_savings_pct: f64) -> Outcome {
    Outcome::InsufficientSavings {
        savings: candidate.savings,
        min_savings_pct,
        report: candidate.report,
    }
}

/// Re-checks an existing candidate against its source (for the `verify` and
/// `publish` commands, where no pipeline state exists).
pub fn assess(
    prober: &Prober,
    source: &Path,
    candidate: &Path,
    snapshot: &SettingsSnapshot,
) -> Result<Result<Candidate, SkipReason>, PipelineError> {
    let src = prober.probe(source, ProbeDepth::Full, None)?;
    let out = prober.probe(candidate, ProbeDepth::Full, None)?;
    // The source may carry an old marker; the plan only sets the expectation.
    let opts = RouteOptions {
        force: true,
        ..Default::default()
    };
    let plan = match route::plan(&src, source, &snapshot.profile, &opts) {
        Ok(plan) => plan,
        Err(reason) => return Ok(Err(reason)),
    };
    let mut work = WorkFiles::default();
    let mut injected = Injected::default();
    if plan.dynamic_range.dolby_vision().is_some() {
        let rpu = work.add(command::work_path(source, "rpu"));
        let summary = source_rpu(snapshot, &src, source, &plan, &rpu, None)?;
        injected.rpu = Some((rpu, summary));
    }
    if plan.dynamic_range.hdr10_plus() {
        injected.hdr10_plus = Some(source_hdr10_plus(snapshot, &src, source, &plan, None)?);
    }
    let inspection = inspect::inspect(
        source,
        &src,
        candidate,
        &out,
        &plan,
        snapshot,
        &injected.expected(),
        None,
    )
    .map_err(PipelineError::Inspect)?;
    Ok(Ok(Candidate {
        source: source.to_owned(),
        path: candidate.to_owned(),
        target: plan.target.clone(),
        source_identity: src.identity,
        identity: out.identity,
        savings: Savings::new(src.identity.size, out.identity.size),
        report: verify_against_plan(&src, &out, &plan, snapshot, &inspection),
        plan,
    }))
}

/// What a Dolby Vision or HDR10+ route put into the candidate.
#[derive(Debug, Default)]
struct Injected {
    /// The RPU file (kept until verification) and its summary.
    rpu: Option<(PathBuf, RpuSummary)>,
    /// Each frame's HDR10+ message, in display order.
    hdr10_plus: Option<Vec<Vec<u8>>>,
}

impl Injected {
    fn expected(&self) -> inspect::Expected<'_> {
        inspect::Expected {
            rpu: self.rpu.as_ref().map(|(path, _)| path.as_path()),
            hdr10_plus: self.hdr10_plus.as_deref(),
        }
    }
}

/// A Dolby Vision or HDR10+ route, step by step: the source's RPUs (P7
/// converted to P8.1) and HDR10+ messages, the video encoded alone into
/// Matroska, the metadata injected, then the final mux with the source's
/// other streams.
#[allow(clippy::too_many_arguments)]
fn encode_with_metadata(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    source: &Path,
    candidate: &Path,
    work: &mut WorkFiles,
    cancel: Option<&CancelToken>,
    on_progress: impl FnMut(Progress),
) -> Result<Injected, PipelineError> {
    let timeout = command::encode_timeout(snapshot, info.format.duration_s);
    let dolby_vision = match plan.dynamic_range.dolby_vision() {
        Some((_, output)) => {
            let rpu = work.add(command::work_path(source, "rpu"));
            let summary = source_rpu(snapshot, info, source, plan, &rpu, cancel)?;
            let rpus =
                dovi::read_rpus(&rpu).map_err(|e| PipelineError::DolbyVision(e.to_string()))?;
            Some((rpu, summary, rpus, *output))
        }
        None => None,
    };
    let hdr10_plus = if plan.dynamic_range.hdr10_plus() {
        Some(source_hdr10_plus(snapshot, info, source, plan, cancel)?)
    } else {
        None
    };

    let video = work.add(command::work_path(source, "video.mkv"));
    let cmd = command::build_trial(info, plan, snapshot, source, plan.video_index, &video);
    tracing::info!(source = %source.display(), command = %cmd.display(), "encoding (metadata injected afterwards)");
    command::run(&cmd, timeout, cancel, on_progress)?;

    let with_metadata = work.add(command::work_path(source, "inject.mkv"));
    let metadata = FrameMetadata {
        rpus: dolby_vision
            .as_ref()
            .map(|(_, _, rpus, output)| (rpus.as_slice(), output)),
        hdr10_plus: hdr10_plus.as_deref(),
    };
    dovi::mkv::inject_frames(&video, &metadata, &with_metadata).map_err(|e| {
        if dolby_vision.is_some() {
            PipelineError::DolbyVision(e.to_string())
        } else {
            PipelineError::Hdr10Plus(e.to_string())
        }
    })?;
    let _ = std::fs::remove_file(&video);
    let cmd = command::build_injected_mux(info, plan, snapshot, source, &with_metadata, candidate);
    tracing::info!(command = %cmd.display(), "muxing");
    command::run(&cmd, timeout, cancel, |_| {})?;
    let _ = std::fs::remove_file(&with_metadata);
    Ok(Injected {
        rpu: dolby_vision.map(|(rpu, summary, ..)| (rpu, summary)),
        hdr10_plus,
    })
}

/// The source's HDR10+ messages, one per frame in display order. Every
/// frame must carry one; with more than one processing window (picture
/// coordinates) the picture must keep its size.
pub fn source_hdr10_plus(
    snapshot: &SettingsSnapshot,
    info: &MediaInfo,
    source: &Path,
    plan: &RoutePlan,
    cancel: Option<&CancelToken>,
) -> Result<Vec<Vec<u8>>, PipelineError> {
    let scratch = command::work_path(source, "sei.mkv");
    let found = hdr10plus::extract(
        &snapshot.global.ffmpeg,
        source,
        plan.video_index,
        &scratch,
        command::encode_timeout(snapshot, info.format.duration_s),
        cancel,
    )
    .map_err(|e| match e {
        e @ ToolError::Cancelled { .. } => PipelineError::Encode(e),
        e => PipelineError::Hdr10Plus(format!("提取 HDR10+ 元数据失败：{e}")),
    })?;
    let frames = info
        .stream(plan.video_index)
        .and_then(|s| s.packets.as_ref())
        .map_or(found.len() as u64, |p| p.count);
    let messages: Vec<_> = found.into_iter().flatten().collect();
    if messages.len() as u64 != frames {
        return Err(PipelineError::Hdr10Plus(format!(
            "源片有 {frames} 帧，只有 {} 帧带 HDR10+",
            messages.len()
        )));
    }
    if plan.scales(info)
        && messages
            .iter()
            .any(|m| hdr10plus::windows(m).unwrap_or(1) > 1)
    {
        return Err(PipelineError::Hdr10Plus(
            "HDR10+ 用了多个处理窗口（带画面坐标），不能缩小分辨率".into(),
        ));
    }
    Ok(messages)
}

/// Extracts the source's RPUs to `out`, converting P7 to P8.1, and checks
/// there is one per frame.
pub fn source_rpu(
    snapshot: &SettingsSnapshot,
    info: &MediaInfo,
    source: &Path,
    plan: &RoutePlan,
    out: &Path,
    cancel: Option<&CancelToken>,
) -> Result<RpuSummary, PipelineError> {
    let (dv, output) = plan
        .dynamic_range
        .dolby_vision()
        .expect("a Dolby Vision plan");
    let tool = DoviTool::new(snapshot.global.dovi_tool_path());
    let failed = |e: ToolError| match e {
        e @ ToolError::Cancelled { .. } => PipelineError::Encode(e),
        e => PipelineError::DolbyVision(format!("提取 RPU 失败：{e}")),
    };
    let timeout = command::encode_timeout(snapshot, info.format.duration_s);
    let ffmpeg = &snapshot.global.ffmpeg;
    let v = plan.video_index;
    let summary = if dv.profile == 7 {
        // The enhancement layer type is only in the unconverted RPU.
        let mut raw = WorkFiles::default();
        let p7 = raw.add(out.with_extension("p7.rpu"));
        tool.extract(ffmpeg, source, v, false, &p7, timeout, cancel)
            .map_err(failed)?;
        let el = tool.summary(&p7, cancel).map_err(failed)?.el;
        tool.to_p81(&p7, out, cancel).map_err(failed)?;
        RpuSummary {
            el,
            ..tool.summary(out, cancel).map_err(failed)?
        }
    } else {
        tool.extract(ffmpeg, source, v, false, out, timeout, cancel)
            .map_err(failed)?;
        tool.summary(out, cancel).map_err(failed)?
    };
    if summary.profile != output.profile {
        return Err(PipelineError::DolbyVision(format!(
            "RPU 是 P{}，与配置记录的 {} 不符",
            summary.profile,
            output.label()
        )));
    }
    let frames = info
        .stream(plan.video_index)
        .and_then(|s| s.packets.as_ref())
        .map(|p| p.count);
    if frames.is_some_and(|f| f != summary.frames) {
        return Err(PipelineError::DolbyVision(format!(
            "源片有 {} 帧，RPU 只有 {} 个",
            frames.unwrap_or_default(),
            summary.frames
        )));
    }
    Ok(summary)
}

/// The savings prediction of a transcode that ran, for the report: the
/// estimate next to the measured saving shows how well it guessed.
fn prediction_note(prediction: &Result<Option<Estimate>, ToolError>) -> Finding {
    let message = match prediction {
        Ok(Some(e)) => format!(
            "转码前预判节省约 {:.1}%（{} 段样片，共 {:.0} 秒，用时 {:.0} 秒）",
            e.saved_pct, e.samples, e.sample_secs, e.secs
        ),
        Ok(None) => "片子太短或视频大小未知，没有预判收益".into(),
        Err(e) => format!("预判收益失败，直接转码：{e}"),
    };
    Finding {
        check: "predict_savings".into(),
        severity: Severity::Info,
        message,
    }
}

/// What a Dolby Vision route did, for the report.
fn dolby_vision_note(plan: &RoutePlan, summary: &RpuSummary) -> Finding {
    let (source, output) = plan
        .dynamic_range
        .dolby_vision()
        .expect("a Dolby Vision plan");
    let message = match summary.el {
        Some(el) if source.profile == 7 => format!(
            "杜比视界 P7（{}）转为 {}，舍弃增强层；保留 {} 帧 RPU",
            match el {
                dovi::EnhancementLayer::Mel => "MEL",
                dovi::EnhancementLayer::Fel => "FEL，增强层的细节会丢失",
            },
            output.label(),
            summary.frames
        ),
        _ => format!(
            "保留了杜比视界 {} 的 {} 帧 RPU",
            output.label(),
            summary.frames
        ),
    };
    Finding {
        check: "dolby_vision".into(),
        severity: Severity::Info,
        message,
    }
}

pub fn verify_against_plan(
    src: &MediaInfo,
    out: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    inspection: &Inspection,
) -> Report {
    let exp = Expectation::new(plan, command::marker_for(snapshot));
    verify::verify(src, out, &exp, &snapshot.global.verify, inspection)
}
