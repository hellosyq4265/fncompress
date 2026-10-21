// Settings form and diagnostics helpers.
import type { GlobalSettings, RecycleList, ServerSettings, VerifyStats } from '../api/types'
import { checkLabel } from './report'

export interface SettingsForm {
  global: GlobalSettings
  server: ServerSettings
}

/** A deep copy to edit without touching the loaded settings. */
export function toForm(s: { global: GlobalSettings; server: ServerSettings }): SettingsForm {
  return JSON.parse(JSON.stringify({ global: s.global, server: s.server }))
}

/** Whether the form differs from what the server has. */
export function isDirty(form: SettingsForm, saved: { global: GlobalSettings; server: ServerSettings }): boolean {
  return JSON.stringify(normalize(form)) !== JSON.stringify(normalize(toForm(saved)))
}

/**
 * The request body; an empty recycle directory means "next to each source",
 * an empty scoring ffmpeg means "the ffmpeg above", an empty dovi_tool means
 * "the one next to the scoring ffmpeg, else on PATH".
 */
export function normalize(form: SettingsForm): SettingsForm {
  const f = toForm(form)
  const dir = f.global.recycle_dir?.trim()
  f.global.recycle_dir = dir ? dir : null
  const scorer = f.global.scoring_ffmpeg?.trim()
  f.global.scoring_ffmpeg = scorer ? scorer : null
  const dovi = f.global.dovi_tool?.trim()
  f.global.dovi_tool = dovi ? dovi : null
  f.global.ffmpeg = f.global.ffmpeg.trim()
  f.global.ffprobe = f.global.ffprobe.trim()
  return sortKeys(f)
}

function sortKeys<T>(v: T): T {
  if (Array.isArray(v)) return v.map(sortKeys) as T
  if (v && typeof v === 'object') {
    return Object.fromEntries(
      Object.keys(v)
        .sort()
        .map((k) => [k, sortKeys((v as Record<string, unknown>)[k])]),
    ) as T
  }
  return v
}

export interface ObservationView {
  /** 0–100. */
  percent: number
  text: string
  active: boolean
}

export function observation(observed: number, n: number): ObservationView {
  if (n === 0) return { percent: 100, text: '已关闭：校验通过的成品直接发布', active: false }
  if (observed >= n) return { percent: 100, text: `已观察满 ${n} 个，之后校验通过的成品直接发布`, active: false }
  return {
    percent: Math.round((observed / n) * 100),
    text: `已观察 ${observed} / ${n} 个；在此之前所有成品都要人工确认`,
    active: true,
  }
}

export interface CheckRow {
  check: string
  label: string
  hard: number
  soft: number
  info: number
}

/** One row per check, the most frequent hard ones first. */
export function checkRows(stats: VerifyStats): CheckRow[] {
  const rows = new Map<string, CheckRow>()
  for (const c of stats.checks) {
    const row = rows.get(c.check) ?? { check: c.check, label: checkLabel(c.check), hard: 0, soft: 0, info: 0 }
    row[c.severity] += c.count
    rows.set(c.check, row)
  }
  return [...rows.values()].sort(
    (a, b) => b.hard - a.hard || b.soft - a.soft || b.info - a.info || a.check.localeCompare(b.check),
  )
}

export function recycleTotals(r: RecycleList): { files: number; bytes: number } {
  let files = 0
  let bytes = 0
  for (const d of r.dirs) {
    files += d.entries.length
    for (const e of d.entries) bytes += e.identity.size
  }
  return { files, bytes }
}
