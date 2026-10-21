//! Quality search: bisection for the largest quality value (smallest output)
//! whose lowest sample score still reaches the target, the candidates
//! offered from the trials, and how the compared presets stand against each
//! other.

use super::{PresetResult, RecommendCandidate, Trial};
use crate::encoder::Encoder;

/// Trials per preset, each covering every sample.
pub const MAX_TRIALS: usize = 8;

/// Trials a preset after the first usually takes: its boundary is near the
/// one found before.
const NEAR_TRIALS: usize = 3;

/// Quality values worth searching per encoder (CRF, ICQ `global_quality` or
/// CQP), inclusive. Outside these the result is either far larger than
/// needed or plainly degraded.
pub fn quality_range(encoder: Encoder) -> (u8, u8) {
    match encoder {
        Encoder::Libx264 => (14, 36),
        Encoder::Libx265 => (14, 38),
        Encoder::Libsvtav1 => (18, 55),
        Encoder::H264Qsv | Encoder::HevcQsv | Encoder::Av1Qsv => (14, 40),
        Encoder::H264Vaapi | Encoder::HevcVaapi | Encoder::Av1Vaapi => (14, 40),
    }
}

/// The next quality to try, or `None` when the boundary is found. Starts at
/// `first` (the base profile's value), then bisects between the largest
/// passing value and the smallest failing one above it. VMAF is not
/// strictly monotonic in the quality value; a failure below a pass is
/// ignored.
pub fn next_quality(lo: u8, hi: u8, first: u8, tried: &[(u8, bool)]) -> Option<u8> {
    if tried.is_empty() {
        return Some(first.clamp(lo, hi));
    }
    let base = tried
        .iter()
        .filter(|t| t.1)
        .map(|t| i32::from(t.0))
        .max()
        .unwrap_or(i32::from(lo) - 1);
    let fail = tried
        .iter()
        .filter(|t| !t.1 && i32::from(t.0) > base)
        .map(|t| i32::from(t.0))
        .min()
        .unwrap_or(i32::from(hi) + 1);
    if fail - base <= 1 {
        return None;
    }
    let mid = ((base + fail) / 2) as u8;
    (!tried.iter().any(|t| t.0 == mid)).then_some(mid)
}

/// The next quality to try for a preset after the first, starting at `hint`
/// (the boundary the previous preset found): steps away from it by 1, 1, 2,
/// 4... until the other side of the boundary is reached, then bisects. A
/// boundary next to the hint takes two or three trials instead of a full
/// bisection of the range.
pub fn next_near(lo: u8, hi: u8, hint: u8, tried: &[(u8, bool)]) -> Option<u8> {
    let hint = i32::from(hint.clamp(lo, hi));
    if tried.is_empty() {
        return Some(hint as u8);
    }
    let (lo, hi) = (i32::from(lo), i32::from(hi));
    let base = tried.iter().filter(|t| t.1).map(|t| i32::from(t.0)).max();
    let fail = tried
        .iter()
        .filter(|t| !t.1 && base.is_none_or(|b| i32::from(t.0) > b))
        .map(|t| i32::from(t.0))
        .min();
    let next = match (base, fail) {
        (Some(b), Some(f)) if f - b <= 1 => return None,
        (Some(b), Some(f)) => (b + f) / 2,
        (Some(b), None) if b >= hi => return None,
        (Some(b), None) => (b + (b - hint).max(1)).min(hi),
        (None, Some(f)) if f <= lo => return None,
        (None, Some(f)) => (f - (hint - f).max(1)).max(lo),
        (None, None) => unreachable!("tried is not empty"),
    };
    let next = next as u8;
    (!tried.iter().any(|t| t.0 == next)).then_some(next)
}

/// Trials a search over `lo..=hi` usually takes, for the progress estimate:
/// a full bisection for the first preset, a few near its boundary for the
/// others.
pub fn expected_trials(lo: u8, hi: u8, presets: usize) -> usize {
    let span = f64::from(hi - lo) + 2.0;
    let first = (1 + span.log2().ceil() as usize).min(MAX_TRIALS);
    first + NEAR_TRIALS * presets.saturating_sub(1)
}

/// Passing trials of one preset not beaten by one that is smaller and
/// scores at least as well, cheapest first. More than three are thinned to
/// the cheapest, the middle and the safest.
pub fn candidates<'a>(trials: impl IntoIterator<Item = &'a Trial>) -> Vec<RecommendCandidate> {
    let eligible: Vec<RecommendCandidate> = trials
        .into_iter()
        .filter(|t| t.passed)
        .filter_map(|t| {
            Some(RecommendCandidate {
                preset: t.preset.clone(),
                quality: t.quality,
                min_score: t.min_score?,
                scores: t.scores.iter().copied().collect::<Option<_>>()?,
                video_bytes: t.video_bytes?,
                size_ratio: t.size_ratio.unwrap_or(0.0),
                bitrate_bps: t.bitrate_bps.unwrap_or(0.0),
            })
        })
        .collect();
    let mut front: Vec<RecommendCandidate> = eligible
        .iter()
        .filter(|c| {
            !eligible.iter().any(|o| {
                o.quality != c.quality
                    && o.min_score >= c.min_score
                    && o.video_bytes <= c.video_bytes
                    && (o.min_score > c.min_score || o.video_bytes < c.video_bytes)
            })
        })
        .cloned()
        .collect();
    front.sort_by(|a, b| {
        a.video_bytes
            .cmp(&b.video_bytes)
            .then(a.min_score.total_cmp(&b.min_score))
    });
    if front.len() > 3 {
        let last = front.len() - 1;
        front = vec![
            front[0].clone(),
            front[last / 2].clone(),
            front[last].clone(),
        ];
    }
    front
}

/// Where one preset stands: its speed over all its trials, its cheapest
/// candidate, and the encode time that speed means for the whole source.
/// `front` is filled in by [`mark_front`].
pub fn summarize(
    preset: Option<String>,
    trials: &[Trial],
    candidates: &[RecommendCandidate],
    done: bool,
    duration_s: Option<f64>,
    timeout_s: Option<f64>,
) -> PresetResult {
    let mine = || trials.iter().filter(|t| t.preset == preset);
    let encode: f64 = mine().map(|t| t.encode_secs).sum();
    let encoded: f64 = mine().map(|t| t.encoded_secs).sum();
    let speed = (encode > 0.0 && encoded > 0.0).then(|| encoded / encode);
    let best = candidates
        .iter()
        .filter(|c| c.preset == preset)
        .min_by_key(|c| c.video_bytes)
        .cloned();
    let full_secs = speed.zip(duration_s).map(|(s, d)| d / s);
    PresetResult {
        over_timeout: full_secs.zip(timeout_s).is_some_and(|(f, t)| f > t),
        trials: mine().count() as u32,
        preset,
        done,
        speed,
        full_secs,
        best,
        front: false,
    }
}

/// Marks the presets whose cheapest candidate no other preset beats on both
/// counts: at least as fast and at least as small, and better in one.
pub fn mark_front(presets: &mut [PresetResult]) {
    let points: Vec<Option<(f64, u64)>> = presets
        .iter()
        .map(|p| p.speed.zip(p.best.as_ref().map(|b| b.video_bytes)))
        .collect();
    for (i, p) in presets.iter_mut().enumerate() {
        p.front = points[i].is_some_and(|(speed, bytes)| {
            !points.iter().enumerate().any(|(j, o)| {
                j != i
                    && o.is_some_and(|(s, b)| s >= speed && b <= bytes && (s > speed || b < bytes))
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the search against a score function of the quality value.
    fn search(lo: u8, hi: u8, first: u8, pass: impl Fn(u8) -> bool) -> Vec<u8> {
        let mut tried = Vec::new();
        while tried.len() < MAX_TRIALS {
            let Some(q) = next_quality(lo, hi, first, &tried) else {
                break;
            };
            tried.push((q, pass(q)));
        }
        tried.into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn bisects_to_the_largest_passing_quality() {
        let tried = search(14, 38, 24, |q| q <= 29);
        assert_eq!(tried, [24, 31, 27, 29, 30]);
        // The base value fails: search below it.
        let tried = search(14, 38, 24, |q| q <= 19);
        assert_eq!(tried, [24, 18, 21, 19, 20]);
        // Everything passes: stops at the top of the range.
        assert_eq!(*search(14, 38, 24, |_| true).last().unwrap(), 38);
        // Nothing passes: ends at the bottom.
        assert_eq!(*search(14, 38, 24, |_| false).last().unwrap(), 14);
        // The base value outside the range is clamped.
        assert_eq!(search(14, 38, 51, |_| true)[0], 38);
    }

    #[test]
    fn non_monotonic_failures_below_a_pass_are_ignored() {
        let tried = [(24, true), (31, false), (27, true), (22, false)];
        assert_eq!(next_quality(14, 38, 24, &tried), Some(29));
    }

    /// [`search`] for a later preset.
    fn search_near(lo: u8, hi: u8, hint: u8, pass: impl Fn(u8) -> bool) -> Vec<u8> {
        let mut tried = Vec::new();
        while tried.len() < MAX_TRIALS {
            let Some(q) = next_near(lo, hi, hint, &tried) else {
                break;
            };
            tried.push((q, pass(q)));
        }
        tried.into_iter().map(|t| t.0).collect()
    }

    #[test]
    fn later_presets_step_out_from_the_previous_boundary() {
        // Same boundary: two trials.
        assert_eq!(search_near(14, 38, 27, |q| q <= 27), [27, 28]);
        // A little above.
        assert_eq!(search_near(14, 38, 27, |q| q <= 28), [27, 28, 29]);
        assert_eq!(search_near(14, 38, 27, |q| q <= 30), [27, 28, 29, 31, 30]);
        // Below: the hint fails.
        assert_eq!(search_near(14, 38, 27, |q| q <= 26), [27, 26]);
        assert_eq!(search_near(14, 38, 27, |q| q <= 24), [27, 26, 25, 23, 24]);
        // The ends of the range.
        assert_eq!(search_near(14, 38, 37, |_| true), [37, 38]);
        assert_eq!(search_near(14, 38, 15, |_| false), [15, 14]);
        assert_eq!(search_near(14, 38, 51, |_| true), [38]);
        // Up to 8 away, the boundary is still found within the trial limit;
        // further away the search may stop short of it.
        for edge in 22..=38 {
            let tried = search_near(14, 38, 30, |q| q <= edge);
            let passed = tried.iter().filter(|&&q| q <= edge).max().copied();
            assert_eq!(passed, Some(edge), "{edge}: {tried:?}");
        }
    }

    #[test]
    fn expected_trials_cover_the_range() {
        assert_eq!(expected_trials(14, 38, 1), 6);
        assert_eq!(expected_trials(14, 38, 3), 12);
        for (lo, hi) in [(14, 36), (14, 38), (18, 55), (14, 40)] {
            let worst = (lo..=hi)
                .map(|edge| search(lo, hi, 24, |q| q <= edge).len())
                .max()
                .unwrap();
            assert!(worst <= MAX_TRIALS, "{lo}..{hi}: {worst}");
        }
    }

    fn trial(q: u8, scores: &[f64], bytes: u64, target: f64) -> Trial {
        let min = scores.iter().copied().fold(f64::INFINITY, f64::min);
        Trial {
            preset: None,
            quality: q,
            scores: scores.iter().map(|s| Some(*s)).collect(),
            min_score: Some(min),
            done: true,
            passed: min >= target,
            video_bytes: Some(bytes),
            size_ratio: Some(bytes as f64 / 1000.0),
            bitrate_bps: Some(bytes as f64 * 8.0),
            encode_secs: 1.0,
            encoded_secs: 20.0,
            score_secs: 1.0,
        }
    }

    #[test]
    fn candidates_are_the_passing_front_cheapest_first() {
        let t = |q, s: f64, b| trial(q, &[s + 1.0, s], b, 95.0);
        let trials = [
            t(24, 97.5, 900),
            t(31, 94.0, 400),
            t(27, 96.0, 600),
            t(29, 95.2, 500),
            // Passing but larger and worse than q27: dominated.
            t(26, 95.8, 650),
            t(30, 94.9, 450),
        ];
        let c = candidates(&trials);
        let q: Vec<_> = c.iter().map(|c| c.quality).collect();
        assert_eq!(q, [29, 27, 24]);
        assert_eq!(c[0].min_score, 95.2);
        assert_eq!(c[0].scores, [96.2, 95.2]);

        let more = [
            t(20, 99.0, 1500),
            t(22, 98.0, 1200),
            t(24, 97.0, 900),
            t(26, 96.0, 700),
            t(28, 95.0, 550),
        ];
        let q: Vec<_> = candidates(&more).iter().map(|c| c.quality).collect();
        assert_eq!(q, [28, 24, 20]);
    }

    #[test]
    fn unfinished_or_partial_trials_are_not_candidates() {
        let mut partial = trial(30, &[96.0], 100, 95.0);
        partial.scores.push(None);
        partial.passed = false;
        assert!(candidates(&[partial]).is_empty());
    }

    #[test]
    fn presets_are_summarized_and_the_front_marked() {
        let at = |preset: &str, q, bytes, encode_secs| Trial {
            preset: Some(preset.into()),
            encode_secs,
            ..trial(q, &[96.0], bytes, 95.0)
        };
        let trials = [
            at("fast", 28, 600, 1.0),
            at("fast", 30, 500, 1.0),
            at("medium", 30, 450, 2.0),
            at("slow", 30, 460, 4.0),
            Trial {
                passed: false,
                ..at("veryslow", 30, 400, 8.0)
            },
        ];
        let names = ["fast", "medium", "slow", "veryslow"];
        let cands: Vec<_> = names
            .into_iter()
            .flat_map(|p| candidates(trials.iter().filter(|t| t.preset.as_deref() == Some(p))))
            .collect();
        assert!(cands.iter().all(|c| c.preset.is_some()));
        let summary = |p: &str, timeout| {
            summarize(
                Some(p.into()),
                &trials,
                &cands,
                true,
                Some(3600.0),
                Some(timeout),
            )
        };
        let mut presets: Vec<_> = names.into_iter().map(|p| summary(p, 5400.0)).collect();
        mark_front(&mut presets);
        let fast = &presets[0];
        assert_eq!(fast.trials, 2);
        assert_eq!(fast.speed, Some(20.0));
        assert_eq!(fast.full_secs, Some(180.0));
        assert_eq!(fast.best.as_ref().map(|b| b.quality), Some(30));
        assert_eq!(presets[2].full_secs, Some(720.0));
        assert_eq!(presets[3].speed, Some(2.5));
        assert!(presets[3].best.is_none());
        assert!(!presets.iter().any(|p| p.over_timeout));
        // slow is slower and larger than medium; veryslow has no candidate.
        let front: Vec<_> = presets.iter().map(|p| p.front).collect();
        assert_eq!(front, [true, true, false, false]);

        assert!(summary("slow", 600.0).over_timeout);
    }
}
