import { describe, expect, it } from 'vitest'
import type { PresetResult, RecommendCandidate, Recommendation, Trial } from '../api/types'
import { task } from '../test/fixtures'
import { outcomeSummary, progressView } from './format'
import {
  candidateViews,
  defaultChoice,
  defaultPresets,
  emptyForm,
  formProblem,
  formatBitrate,
  formatEstimate,
  formatScore,
  formatSpeed,
  presetRows,
  presetsText,
  recommendSummary,
  sampleWhere,
  samplesText,
  scoringNote,
  slowPresetsNote,
  sortPresets,
  stageText,
  suggestedPreset,
  toRequest,
  trialRows,
} from './recommend'

function trial(quality: number, scores: (number | null)[], over: Partial<Trial> = {}): Trial {
  const done = scores.filter((s): s is number => s !== null)
  const min = done.length ? Math.min(...done) : null
  return {
    preset: 'medium',
    quality,
    scores,
    min_score: min,
    done: true,
    passed: scores.every((s) => s !== null && s >= 95),
    video_bytes: 1000,
    size_ratio: 0.42,
    bitrate_bps: 4_200_000,
    encode_secs: 1,
    encoded_secs: 15,
    score_secs: 1,
    ...over,
  }
}

const cand = (preset: string, quality: number, min: number, bytes: number, ratio: number, bps: number): RecommendCandidate => ({
  preset,
  quality,
  min_score: min,
  scores: [min, min + 1.5],
  video_bytes: bytes,
  size_ratio: ratio,
  bitrate_bps: bps,
})

function result(preset: string, over: Partial<PresetResult> = {}): PresetResult {
  return {
    preset,
    done: true,
    trials: 2,
    speed: 2,
    full_secs: 3600,
    over_timeout: false,
    best: null,
    front: false,
    ...over,
  }
}

const rec: Recommendation = {
  target_vmaf: 95,
  encoder: 'hevc_qsv',
  preset: null,
  presets: [
    result('veryfast', { speed: 12.5, full_secs: 576, best: cand('veryfast', 26, 95.2, 1100, 0.5, 4_000_000), front: true }),
    result('medium', { speed: 4, full_secs: 1800, best: cand('medium', 27, 95.4, 900, 0.42, 3_400_000), front: true }),
    result('slow', { speed: 3.9, full_secs: 1846, best: cand('slow', 27, 95.3, 950, 0.44, 3_500_000) }),
    result('veryslow', { done: false, trials: 1, speed: 0.4, full_secs: 18_000, over_timeout: true }),
  ],
  width: 1920,
  height: 1080,
  model: 'vmaf_v0.6.1',
  range: [14, 40],
  samples: [
    {
      kind: 'auto',
      path: '/vol1/a.mkv',
      start_s: 750,
      duration_s: 10.01,
      frames: 240,
      video_bytes: 10_000_000,
      bitrate_bps: 8_000_000,
      reason: '码率最高的片段（最难压缩）',
    },
    {
      kind: 'file',
      path: '/vol1/clips/dark.mkv',
      start_s: null,
      duration_s: 5,
      frames: 120,
      video_bytes: 2_000_000,
      bitrate_bps: 3_200_000,
      reason: '现成样片',
    },
  ],
  trials: [
    trial(24, [97.1, 98.0]),
    trial(31, [93.2, null], { passed: false, video_bytes: null, size_ratio: null }),
    trial(27, [95.4, 96.9]),
    trial(29, [94.8, null], { done: false, passed: false }),
  ],
  candidates: [
    cand('veryfast', 26, 95.2, 1100, 0.5, 4_000_000),
    cand('medium', 27, 95.4, 900, 0.42, 3_400_000),
    cand('medium', 24, 97.1, 1500, 0.61, 5_000_000),
    cand('slow', 27, 95.3, 950, 0.44, 3_500_000),
  ],
}

describe('wizard form', () => {
  it('explains what is missing', () => {
    const f = emptyForm()
    expect(formProblem(f)).toBe('先选择原片')
    f.path = '/vol1/a.mkv'
    expect(formProblem(f)).toBeNull()
    f.count = 9
    expect(formProblem(f)).toBe('样片数量应为 1 到 8 个')
    f.count = 4
    f.secs = 1
    expect(formProblem(f)).toBe('样片长度应为 2 到 60 秒')
    f.sampleMode = 'files'
    expect(formProblem(f)).toBe('至少选择一个现成样片')
    f.files = ['/vol1/s.mkv']
    expect(formProblem(f)).toBeNull()
    f.target = 0
    expect(formProblem(f)).toBe('VMAF 目标应在 0 到 100 之间')
  })

  it('builds the create request for either sample mode', () => {
    const f = { ...emptyForm(), path: ' /vol1/a.mkv ', profileId: 'movies', encoder: 'libx265' as const, presets: ['fast', 'slow'] }
    expect(toRequest(f)).toEqual({
      type: 'recommend',
      source: 'manual',
      path: '/vol1/a.mkv',
      profile_id: 'movies',
      recommend: {
        target_vmaf: 95,
        presets: ['fast', 'slow'],
        samples: { mode: 'auto', method: 'bitrate', count: 4, secs: 10 },
      },
    })
    expect(samplesText(f)).toBe('自动挑 4 个 × 10 秒（按码率）')
    const files = { ...f, sampleMode: 'files' as const, files: ['/vol1/s.mkv'], target: 93 }
    expect(toRequest(files).recommend).toEqual({
      target_vmaf: 93,
      presets: ['fast', 'slow'],
      samples: { mode: 'files', paths: ['/vol1/s.mkv'] },
    })
    expect(samplesText({ ...f, method: 'scene' })).toBe('自动挑 4 个 × 10 秒（按场景分析）')
    expect(samplesText(files)).toBe('1 个现成样片')
  })

  it('compares the usual presets and the profile\'s own, fastest first', () => {
    expect(defaultPresets('libx265', null)).toEqual(['fast', 'medium', 'slow'])
    expect(defaultPresets('libx265', 'veryfast')).toEqual(['veryfast', 'fast', 'medium', 'slow'])
    expect(defaultPresets('hevc_qsv', 'medium')).toEqual(['veryfast', 'medium', 'slow', 'veryslow'])
    expect(defaultPresets('libsvtav1', '4')).toEqual(['10', '8', '6', '4'])
    expect(defaultPresets('libsvtav1', null)).toEqual(['10', '8', '6'])
    expect(defaultPresets('hevc_vaapi', null)).toEqual([])
    expect(sortPresets('libx265', ['slow', 'ultrafast', 'slow'])).toEqual(['ultrafast', 'slow'])

    const f = { ...emptyForm(), path: '/vol1/a.mkv', encoder: 'libx265' as const, presets: [] as string[] }
    expect(formProblem(f)).toBe('至少选择一个预设')
    expect(formProblem({ ...f, encoder: 'hevc_vaapi' })).toBeNull()
    expect(presetsText({ ...f, presets: ['fast', 'medium'] })).toBe('对比 2 个预设：fast / medium')
    expect(presetsText({ ...f, presets: ['medium'] })).toBe('只用预设 medium')
    expect(presetsText({ ...f, encoder: 'hevc_vaapi' })).toBe('这个编码器没有预设，只搜索质量值')
    expect(slowPresetsNote({ ...f, presets: ['medium', 'veryslow'] })).toContain('veryslow 很慢')
    expect(slowPresetsNote({ ...f, presets: ['medium'] })).toBeNull()
    expect(slowPresetsNote({ encoder: 'libsvtav1', presets: ['8', '4', '2'] })).toContain('4、2')
  })
})

describe('progress', () => {
  it('names the stage, trial and sample', () => {
    const p = { stage: 'scoring' as const, trial: 3, quality: 27, sample: 2, samples: 4, percent: 55 }
    expect(stageText(p)).toBe('试编码 3（质量 27）· 样片 2/4 评分中')
    expect(stageText({ ...p, stage: 'encoding' })).toBe('试编码 3（质量 27）· 样片 2/4 编码中')
    expect(stageText({ ...p, stage: 'sampling', trial: 0, quality: null })).toBe('准备样片 2/4')
    expect(stageText({ ...p, stage: 'analyzing', percent: 4 })).toBe('分析原片（50%）')
    expect(progressView({ id: 1, out_time_s: 0, percent: 55, recommend: p })).toEqual({
      percent: 55,
      text: '试编码 3（质量 27）· 样片 2/4 评分中',
    })
    // Several presets: which one, by name when the recommendation is at hand.
    const many = { ...p, preset: 2, presets: 4 }
    expect(stageText(many)).toBe('预设 2/4 · 试编码 3（质量 27）· 样片 2/4 评分中')
    expect(stageText(many, rec)).toBe('预设 medium（2/4）· 试编码 3（质量 27）· 样片 2/4 评分中')
    expect(progressView({ id: 1, out_time_s: 0, percent: 55, recommend: many }, rec).text).toContain('预设 medium')
  })
})

describe('results', () => {
  it('lays out samples and trials', () => {
    expect(sampleWhere(rec.samples[0]!)).toBe('12:30 起 10 秒')
    expect(sampleWhere(rec.samples[1]!)).toBe('dark.mkv')
    const rows = trialRows(rec)
    expect(rows.map((r) => r.status)).toEqual(['达标', '未达标', '达标', '进行中'])
    expect(rows[0]!.preset).toBe('medium')
    expect(rows[1]!.scores).toEqual([
      { text: '93.20', low: true },
      { text: '—', low: false },
    ])
    expect(rows[1]!.size).toBe('—')
    expect(rows[0]!.size).toBe('42%')
    expect(formatBitrate(3_400_000)).toBe('3.4 Mbps')
    expect(formatBitrate(800_000)).toBe('800 kbps')
  })

  it('never rounds a score up to the target', () => {
    expect(formatScore(94.99961664344262)).toBe('94.99')
    expect(formatScore(95)).toBe('95.00')
    expect(formatScore(96.62)).toBe('96.62')
    expect(formatScore(null)).toBe('—')
  })

  it('compares the presets', () => {
    const rows = presetRows(rec)
    expect(rows.map((r) => [r.name, r.quality, r.size, r.speed, r.full, r.front, r.status])).toEqual([
      ['veryfast', 26, '50%', '13× 实时', '约 10 分钟', true, null],
      ['medium', 27, '42%', '4.0× 实时', '约 30 分钟', true, null],
      ['slow', 27, '44%', '3.9× 实时', '约 31 分钟', false, null],
      ['veryslow', null, '—', '0.40× 实时', '约 5.0 小时', false, '进行中'],
    ])
    expect(rows[3]!.overTimeout).toBe(true)
    expect(rows[1]!.min).toBe('95.40')
    expect(rows[1]!.bitrate).toBe('3.4 Mbps')
    // The smallest on the front.
    expect(suggestedPreset(rec)).toBe('medium')
    // Over the timeout is passed over while something else fits.
    const tight = {
      ...rec,
      presets: rec.presets!.map((p) => (p.preset === 'medium' ? { ...p, over_timeout: true } : p)),
    }
    expect(suggestedPreset(tight)).toBe('veryfast')
    expect(suggestedPreset({ ...rec, presets: [] })).toBeUndefined()
    expect(formatSpeed(null)).toBe('—')
    expect(formatEstimate(40)).toBe('约 1 分钟')
    expect(formatEstimate(12 * 3600)).toBe('约 12 小时')
  })

  it('names the candidates and the defaults for saving one', () => {
    expect(candidateViews(rec, 'medium')).toEqual([
      { quality: 27, title: '质量 27 · 最省空间', detail: '最低样片分 95.40 · 视频码率 3.4 Mbps，约为原样片的 42%' },
      { quality: 24, title: '质量 24 · 最稳妥', detail: '最低样片分 97.10 · 视频码率 5.0 Mbps，约为原样片的 61%' },
    ])
    expect(candidateViews(rec, 'slow').map((c) => c.title)).toEqual(['质量 27 · 唯一达标'])
    expect(candidateViews(rec, 'veryslow')).toEqual([])
    const t = task(7, { type: 'recommend', path: '/vol1/影视/电影.2020.mkv' })
    expect(defaultChoice(t, 'slow', 27)).toEqual({ id: 'rec-7-slow-q27', name: '推荐：电影.2020 预设 slow 质量 27' })
    expect(defaultChoice(t, null, 27)).toEqual({ id: 'rec-7-q27', name: '推荐：电影.2020 质量 27' })
  })

  it('summarises a recommendation task', () => {
    const base = { type: 'recommend' as const, quota: 'cpu' as const, recommendation: rec }
    expect(recommendSummary(task(1, { ...base, state: 'awaiting_choice' }))).toBe(
      '4 个预设中 3 个达到 VMAF 95，等待选择',
    )
    const one = { ...rec, presets: rec.presets!.slice(1, 2), candidates: rec.candidates.slice(1, 3) }
    expect(recommendSummary(task(1, { ...base, recommendation: one, state: 'awaiting_choice' }))).toBe(
      '2 个候选达到 VMAF 95，等待选择',
    )
    expect(
      outcomeSummary(
        task(1, { ...base, state: 'chosen', choice: { quality: 27, profile_id: 'rec-1-q27', transcode_task: 9 } }),
      ),
    ).toBe('已保存为方案 rec-1-q27（质量 27），转码任务 #9')
    expect(
      outcomeSummary(
        task(1, {
          ...base,
          state: 'chosen',
          choice: { preset: 'slow', quality: 27, profile_id: 'rec-1-slow-q27', transcode_task: null },
        }),
      ),
    ).toBe('已保存为方案 rec-1-slow-q27（预设 slow，质量 27）')
    expect(outcomeSummary(task(1, { ...base, state: 'queued' }))).toBe('等待空闲的 CPU 配额（推荐一次只运行一个）')
    expect(outcomeSummary(task(1, { ...base, state: 'failed', error: '没有质量值……' }))).toBe('没有质量值……')
    expect(outcomeSummary(task(1, { ...base, state: 'discarded' }))).toBe('没有选择候选')
  })
})

describe('scoringNote', () => {
  it('explains the display proxy', () => {
    const dv = (profile: number) => ({
      version_major: 1,
      version_minor: 0,
      profile,
      level: 6,
      rpu_present: true,
      el_present: profile === 7,
      bl_present: true,
      bl_compat_id: profile === 5 ? 0 : profile === 7 ? 6 : 1,
      md_compression: 0,
    })
    expect(scoringNote({ kind: 'sdr' })).toBeNull()
    expect(scoringNote({ kind: 'hdr10' })).toContain('BT.2390')
    expect(scoringNote({ kind: 'dolby_vision', source: dv(8), output: dv(8) })).toContain('注入样片的 RPU')
    expect(scoringNote({ kind: 'dolby_vision', source: dv(7), output: dv(8) })).toContain('HDR10 基础层')
    expect(scoringNote({ kind: 'hdr10_plus' })).toContain('注入样片的 HDR10+')
    expect(scoringNote({ kind: 'dolby_vision', source: dv(8), output: dv(8), hdr10_plus: true })).toContain(
      'HDR10+ 元数据同样注入',
    )
  })
})
