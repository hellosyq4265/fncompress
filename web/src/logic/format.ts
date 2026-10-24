// Labels and number formatting shared by the pages.
import type { Encoder, Recommendation, Severity, Task, TaskProgress, TaskSource, TaskState } from '../api/types'
import { recommendSummary, stageText } from './recommend'

export const STATE_LABEL: Record<TaskState, string> = {
  queued: '排队中',
  running: '运行中',
  published: '已发布',
  skipped: '已跳过',
  insufficient_savings: '收益不足',
  pending: '待确认',
  released: '已放行',
  discarded: '已丢弃',
  expired: '已过期',
  failed: '失败',
  cancelled: '已取消',
  awaiting_choice: '待选择',
  chosen: '已选择',
}

export type TagType = 'default' | 'info' | 'success' | 'warning' | 'error'

export const STATE_TAG: Record<TaskState, TagType> = {
  queued: 'default',
  running: 'info',
  published: 'success',
  skipped: 'warning',
  insufficient_savings: 'default',
  pending: 'warning',
  released: 'success',
  discarded: 'default',
  expired: 'default',
  failed: 'error',
  cancelled: 'default',
  awaiting_choice: 'warning',
  chosen: 'success',
}

export const SOURCE_LABEL: Record<TaskSource, string> = {
  manual: '手动',
  folder_batch: '文件夹批次',
  monitor: '自动监控',
}

export const ENCODER_LABEL: Record<Encoder, string> = {
  libx264: 'x264（CPU）',
  libx265: 'x265（CPU）',
  libsvtav1: 'SVT-AV1（CPU）',
  h264_qsv: 'H.264 QSV（核显）',
  hevc_qsv: 'HEVC QSV（核显）',
  av1_qsv: 'AV1 QSV（核显）',
  h264_vaapi: 'H.264 VAAPI（核显）',
  hevc_vaapi: 'HEVC VAAPI（核显）',
  av1_vaapi: 'AV1 VAAPI（核显）',
}

export const SEVERITY_LABEL: Record<Severity, string> = {
  hard: '硬性',
  soft: '软性',
  info: '记录',
}

export const SEVERITY_TAG: Record<Severity, TagType> = {
  hard: 'error',
  soft: 'warning',
  info: 'default',
}

export function formatBytes(n: number): string {
  if (!Number.isFinite(n)) return '—'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let v = n
  let i = 0
  while (Math.abs(v) >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${i === 0 ? v : v.toFixed(v >= 100 ? 0 : 1)} ${units[i]}`
}

/** 75 → "1:15", 3725 → "1:02:05". */
export function formatDuration(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) return '—'
  const s = Math.round(secs)
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const ss = String(s % 60).padStart(2, '0')
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`
}

export function formatPct(p: number, digits = 1): string {
  return `${p.toFixed(digits)}%`
}

/** Unix seconds → local "MM-DD HH:mm" (with the year when not this year). */
export function formatTime(unix: number, now = Date.now()): string {
  const d = new Date(unix * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  const md = `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
  return d.getFullYear() === new Date(now).getFullYear() ? md : `${d.getFullYear()}-${md}`
}

/** Unix seconds in the future → "3 天后" / "5 小时后" / "即将". */
export function formatUntil(unix: number, now = Date.now()): string {
  const secs = unix - now / 1000
  if (secs <= 60) return '即将'
  if (secs < 3600) return `${Math.round(secs / 60)} 分钟后`
  if (secs < 86400 * 2) return `${Math.round(secs / 3600)} 小时后`
  return `${Math.round(secs / 86400)} 天后`
}

/** The file name of a path, for either separator. */
export function baseName(path: string): string {
  const parts = path.split(/[\\/]/)
  return parts[parts.length - 1] || path
}

export function dirName(path: string): string {
  const i = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return i > 0 ? path.slice(0, i) : i === 0 ? path.slice(0, 1) : ''
}

export interface ProgressView {
  /** 0–100, or null when the duration is unknown. */
  percent: number | null
  /** e.g. "12:30 / 45:00 · 2.3x · 剩余 14:20". */
  text: string
}

/** `rec` names the preset of a recommendation's progress. */
export function progressView(p: TaskProgress | null | undefined, rec?: Recommendation | null): ProgressView {
  if (!p) return { percent: null, text: '准备中' }
  const percent = p.percent ?? null
  if (p.recommend) return { percent, text: stageText(p.recommend, rec) }
  if (p.predict) return { percent, text: `预判收益中 · 样片 ${p.predict.sample}/${p.predict.samples}` }
  const parts = [p.duration_s ? `${formatDuration(p.out_time_s)} / ${formatDuration(p.duration_s)}` : formatDuration(p.out_time_s)]
  if (p.speed && p.speed > 0) {
    parts.push(`${p.speed.toFixed(p.speed >= 10 ? 0 : 1)}x`)
    if (p.duration_s) parts.push(`剩余 ${formatDuration((p.duration_s - p.out_time_s) / p.speed)}`)
  }
  return { percent: percent === null ? null : Math.min(100, Math.max(0, percent)), text: parts.join(' · ') }
}

/** One line on how a task ended (or why it waits). */
export function outcomeSummary(t: Task): string {
  if (t.type === 'recommend') return recommendSummary(t)
  const o = t.outcome
  switch (t.state) {
    case 'queued':
      return '等待空闲的 ' + (t.quota === 'gpu' ? 'GPU' : 'CPU') + ' 配额'
    case 'running':
      return '编码和校验中'
    case 'failed':
      return t.error ?? '失败'
    case 'cancelled':
      return '已取消'
  }
  if (!o) return t.error ?? ''
  switch (o.outcome) {
    case 'skipped':
      return o.reason.message
    case 'insufficient_savings':
      return `只节省 ${formatPct(o.savings.saved_pct)}，低于 ${formatPct(o.min_savings_pct, 0)}，保留源片`
    case 'predicted_no_gain':
      return `预判只节省约 ${formatPct(o.estimate.saved_pct)}，低于 ${formatPct(o.min_savings_pct, 0)}，没有转码，保留源片`
    case 'published':
      return `${t.state === 'released' ? '已人工放行，' : ''}节省 ${formatPct(o.savings.saved_pct)}（${formatBytes(o.savings.source_size)} → ${formatBytes(o.savings.candidate_size)}）`
    case 'rejected':
    case 'verified': {
      const c = o.candidate
      const hard = c.report.findings.filter((f) => f.severity === 'hard').length
      const saved = `可节省 ${formatPct(c.savings.saved_pct)}`
      if (t.state === 'discarded') return '成品已丢弃，源片未变'
      if (t.state === 'expired') return '超过保留期，成品已删除'
      if (t.pending_reason === 'observation' || hard === 0) return `观察模式：校验通过，等待确认（${saved}）`
      return `${hard} 项硬性检查未通过（${saved}）`
    }
  }
}
