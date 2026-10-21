// 样片推荐 as the page shows it: the wizard form and its request, progress
// text, the trial table, the candidates and the defaults for saving one.
import type {
  CreateTask,
  Encoder,
  PresetResult,
  RangeRoute,
  RecommendCandidate,
  RecommendProgress,
  Recommendation,
  Sample,
  Scoring,
  Task,
} from '../api/types'
import { baseName, formatDuration } from './format'
import { comparedPresets, defaultPreset, isSlowPreset, presetsFastFirst } from './profile'

export type SampleMode = 'auto' | 'files'
export type AutoMethod = 'bitrate' | 'scene'

export interface WizardForm {
  path: string
  profileId: string | null
  /** The encoder of the chosen profile, once the source's route is known. */
  encoder: Encoder | null
  /** Presets to compare; empty for an encoder without presets (VAAPI). */
  presets: string[]
  target: number
  sampleMode: SampleMode
  method: AutoMethod
  count: number
  secs: number
  files: string[]
}

export function emptyForm(): WizardForm {
  return {
    path: '',
    profileId: null,
    encoder: null,
    presets: [],
    target: 95,
    sampleMode: 'auto',
    method: 'bitrate',
    count: 4,
    secs: 10,
    files: [],
  }
}

/**
 * The presets compared unless the user changes them: the usual spread and
 * the profile's own preset, fastest first (as the server does with none
 * given).
 */
export function defaultPresets(e: Encoder, own: string | null | undefined): string[] {
  const mine = own ?? defaultPreset(e)
  const list = comparedPresets(e)
  return sortPresets(e, mine && !list.includes(mine) ? [...list, mine] : list)
}

/** Fastest first; a preset the list does not know goes last. */
export function sortPresets(e: Encoder, list: string[]): string[] {
  const order = presetsFastFirst(e)
  const at = (p: string) => (order.includes(p) ? order.indexOf(p) : order.length)
  return [...new Set(list)].sort((a, b) => at(a) - at(b))
}

/** "对比 fast / medium / slow", or what an encoder without presets does. */
export function presetsText(f: Pick<WizardForm, 'encoder' | 'presets'>): string {
  if (!f.encoder || !presetsFastFirst(f.encoder).length) return '这个编码器没有预设，只搜索质量值'
  if (f.presets.length === 1) return `只用预设 ${f.presets[0]}`
  return `对比 ${f.presets.length} 个预设：${f.presets.join(' / ')}`
}

/** A warning for the slow presets in the list, or null. */
export function slowPresetsNote(f: Pick<WizardForm, 'encoder' | 'presets'>): string | null {
  const slow = f.encoder ? f.presets.filter((p) => isSlowPreset(f.encoder!, p)) : []
  if (!slow.length) return null
  return `${slow.join('、')} 很慢：试编码要多花几倍时间，长片按这个预设转码可能要很多小时`
}

/** What is still missing before the form can be sent; null when complete. */
export function formProblem(f: WizardForm): string | null {
  if (!f.path.trim()) return '先选择原片'
  if (f.encoder && presetsFastFirst(f.encoder).length && !f.presets.length) return '至少选择一个预设'
  if (!(f.target > 0 && f.target <= 100)) return 'VMAF 目标应在 0 到 100 之间'
  if (f.sampleMode === 'files') {
    if (f.files.length === 0) return '至少选择一个现成样片'
    if (f.files.length > 8) return '最多 8 个样片'
  } else {
    if (!(f.count >= 1 && f.count <= 8)) return '样片数量应为 1 到 8 个'
    if (!(f.secs >= 2 && f.secs <= 60)) return '样片长度应为 2 到 60 秒'
  }
  return null
}

export function toRequest(f: WizardForm): CreateTask {
  return {
    type: 'recommend',
    source: 'manual',
    path: f.path.trim(),
    profile_id: f.profileId,
    recommend: {
      target_vmaf: f.target,
      presets: f.presets,
      samples:
        f.sampleMode === 'files'
          ? { mode: 'files', paths: f.files }
          : { mode: 'auto', method: f.method, count: f.count, secs: f.secs },
    },
  }
}

/** One line for the request, e.g. "自动挑 4 个 × 10 秒（按码率）". */
export function samplesText(f: Pick<WizardForm, 'sampleMode' | 'method' | 'count' | 'secs' | 'files'>): string {
  if (f.sampleMode === 'files') return `${f.files.length} 个现成样片`
  return `自动挑 ${f.count} 个 × ${f.secs} 秒（${f.method === 'scene' ? '按场景分析' : '按码率'}）`
}

/** "试编码 3（质量 27）· 样片 2/4 评分中". */
export const SCORING_LABEL: Record<Scoring, string> = {
  direct: '直接测 VMAF',
  hdr_proxy: 'HDR 显示代理',
  dolby_vision_proxy: '杜比视界显示代理',
}

/** How a source with this route will be scored, or null for SDR. */
export function scoringNote(r: RangeRoute | null | undefined): string | null {
  if (!r || r.kind === 'sdr') return null
  const proxy = '原样片和试编码都先用 libplacebo 映射到 SDR（BT.2390 → BT.709），再测 VMAF；分数是显示代理下的结果'
  // libplacebo reads HDR10+ too, so the trials carry the sample's messages.
  const plus = r.kind === 'hdr10_plus' || (r.kind === 'dolby_vision' && r.hdr10_plus) ? '；HDR10+ 元数据同样注入' : ''
  if (r.kind === 'dolby_vision' && r.source.profile !== 7) {
    return `${proxy}。试编码先注入样片的 RPU，两边都应用杜比视界映射${plus}`
  }
  if (r.kind === 'dolby_vision') return `${proxy}。P7 按 HDR10 基础层评分${plus}`
  if (r.kind === 'hdr10_plus') return `${proxy}。试编码先注入样片的 HDR10+ 元数据`
  return proxy
}

/** "预设 slow（3/4）· 试编码 9（质量 27）· 样片 2/4 评分中"; the name needs the recommendation. */
export function stageText(p: RecommendProgress, rec?: Recommendation | null): string {
  switch (p.stage) {
    case 'analyzing':
      return p.percent > 0 ? `分析原片（${Math.round((p.percent / 8) * 100)}%）` : '分析原片'
    case 'sampling':
      return p.samples ? `准备样片 ${p.sample}/${p.samples}` : '准备样片'
    case 'encoding':
    case 'scoring': {
      const trial = `试编码 ${p.trial}（质量 ${p.quality ?? '—'}）· 样片 ${p.sample}/${p.samples} ${
        p.stage === 'encoding' ? '编码中' : '评分中'
      }`
      if (!p.presets || p.presets < 2 || !p.preset) return trial
      const name = (rec?.presets ?? [])[p.preset - 1]?.preset
      return `${name ? `预设 ${name}（${p.preset}/${p.presets}）· ` : `预设 ${p.preset}/${p.presets} · `}${trial}`
    }
  }
}

export function formatBitrate(bps: number): string {
  if (!Number.isFinite(bps) || bps <= 0) return '—'
  return bps >= 1e6 ? `${(bps / 1e6).toFixed(bps >= 1e7 ? 0 : 1)} Mbps` : `${Math.round(bps / 1e3)} kbps`
}

/** Two decimals, rounded down: 94.9996 is short of a target of 95 and must not read "95.00". */
export function formatScore(s: number | null | undefined): string {
  return s === null || s === undefined ? '—' : (Math.floor(s * 100 + 1e-9) / 100).toFixed(2)
}

/** Where a sample comes from: "12:30 起 10 秒" or the file name. */
export function sampleWhere(s: Sample): string {
  if (s.kind === 'file') return baseName(s.path)
  return `${formatDuration(s.start_s ?? 0)} 起 ${Math.round(s.duration_s)} 秒`
}

export interface TrialRow {
  preset: string
  quality: number
  scores: { text: string; low: boolean }[]
  min: string
  size: string
  status: '进行中' | '达标' | '未达标'
}

/** The trial table, in the order the trials ran. */
export function trialRows(rec: Recommendation): TrialRow[] {
  return rec.trials.map((t) => ({
    preset: t.preset ?? '—',
    quality: t.quality,
    scores: t.scores.map((s) => ({ text: formatScore(s), low: s !== null && s !== undefined && s < rec.target_vmaf })),
    min: formatScore(t.min_score),
    size: t.size_ratio !== null && t.size_ratio !== undefined ? `${Math.round(t.size_ratio * 100)}%` : '—',
    status: !t.done ? '进行中' : t.passed ? '达标' : '未达标',
  }))
}

/** "2.3× 实时", or "—" before anything was encoded. */
export function formatSpeed(speed: number | null | undefined): string {
  if (!speed || speed <= 0) return '—'
  return `${speed.toFixed(speed >= 10 ? 0 : speed >= 1 ? 1 : 2)}× 实时`
}

/** A rough encode time: "约 40 分钟", "约 3.5 小时". */
export function formatEstimate(secs: number | null | undefined): string {
  if (!secs || secs <= 0) return '—'
  if (secs < 90 * 60) return `约 ${Math.max(1, Math.round(secs / 60))} 分钟`
  return `约 ${(secs / 3600).toFixed(secs >= 10 * 3600 ? 0 : 1)} 小时`
}

export interface PresetRow {
  /** null for an encoder without presets. */
  preset: string | null
  name: string
  /** The cheapest quality reaching the target, when one did. */
  quality: number | null
  min: string
  size: string
  bitrate: string
  speed: string
  full: string
  overTimeout: boolean
  front: boolean
  status: '进行中' | '等待' | '无达标' | null
}

/** One row per compared preset, in the order they ran (fastest first). */
export function presetRows(rec: Recommendation): PresetRow[] {
  return (rec.presets ?? []).map((p: PresetResult) => {
    const b = p.best ?? null
    return {
      preset: p.preset ?? null,
      name: p.preset ?? '无预设',
      quality: b?.quality ?? null,
      min: b ? formatScore(b.min_score) : '—',
      size: b ? `${Math.round(b.size_ratio * 100)}%` : '—',
      bitrate: b ? formatBitrate(b.bitrate_bps) : '—',
      speed: formatSpeed(p.speed),
      full: formatEstimate(p.full_secs),
      overTimeout: p.over_timeout,
      front: p.front,
      status: p.done ? (b ? null : '无达标') : p.trials > 0 ? '进行中' : '等待',
    }
  })
}

/**
 * The preset to offer first: the smallest cheapest candidate on the front
 * that finishes within the encode timeout (else the smallest at all).
 */
export function suggestedPreset(rec: Recommendation): string | null | undefined {
  const fits = (rec.presets ?? []).filter((p) => p.best)
  const pick = (list: PresetResult[]) =>
    [...list].sort((a, b) => a.best!.video_bytes - b.best!.video_bytes)[0]
  const best = pick(fits.filter((p) => p.front && !p.over_timeout)) ?? pick(fits)
  return best === undefined ? undefined : best.preset ?? null
}

export interface CandidateView {
  quality: number
  title: string
  detail: string
}

/** One preset's candidates, cheapest first, with a name for where each stands. */
export function candidateViews(rec: Recommendation, preset: string | null): CandidateView[] {
  const list = rec.candidates.filter((c) => (c.preset ?? null) === preset)
  const n = list.length
  return list.map((c: RecommendCandidate, i) => ({
    quality: c.quality,
    title: `质量 ${c.quality} · ${n === 1 ? '唯一达标' : i === 0 ? '最省空间' : i === n - 1 ? '最稳妥' : '折中'}`,
    detail: `最低样片分 ${formatScore(c.min_score)} · 视频码率 ${formatBitrate(c.bitrate_bps)}，约为原样片的 ${Math.round(
      c.size_ratio * 100,
    )}%`,
  }))
}

/** The profile id and name the server would pick for this choice. */
export function defaultChoice(t: Task, preset: string | null, quality: number): { id: string; name: string } {
  const file = baseName(t.path).replace(/\.[^.]+$/, '')
  if (preset === null) return { id: `rec-${t.id}-q${quality}`, name: `推荐：${file} 质量 ${quality}` }
  return { id: `rec-${t.id}-${preset}-q${quality}`, name: `推荐：${file} 预设 ${preset} 质量 ${quality}` }
}

/** One line on where a recommendation stands (task cards and lists). */
export function recommendSummary(t: Task): string {
  const rec = t.recommendation
  switch (t.state) {
    case 'queued':
      return '等待空闲的 CPU 配额（推荐一次只运行一个）'
    case 'running':
      return '试编码和评分中'
    case 'awaiting_choice': {
      const presets = rec?.presets ?? []
      if (presets.length > 1) {
        const ok = presets.filter((p) => p.best).length
        return `${presets.length} 个预设中 ${ok} 个达到 VMAF ${rec?.target_vmaf ?? ''}，等待选择`
      }
      return `${rec?.candidates.length ?? 0} 个候选达到 VMAF ${rec?.target_vmaf ?? ''}，等待选择`
    }
    case 'chosen': {
      const c = t.choice
      if (!c) return '已选择'
      const what = c.preset ? `预设 ${c.preset}，质量 ${c.quality}` : `质量 ${c.quality}`
      return `已保存为方案 ${c.profile_id}（${what}）${c.transcode_task ? `，转码任务 #${c.transcode_task}` : ''}`
    }
    case 'discarded':
      return '没有选择候选'
    case 'expired':
      return '超过保留期，没有选择候选'
    case 'skipped':
      return t.outcome?.outcome === 'skipped' ? t.outcome.reason.message : '已跳过'
    case 'cancelled':
      return '已取消'
    default:
      return t.error ?? ''
  }
}
