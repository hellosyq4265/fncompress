// Profile and monitor forms: what each encoder accepts, and the conversion
// between form fields and the request bodies.
import type { Encoder, MonitorDir, MonitorInput, Profile, SavedProfile } from '../api/types'

export const ENCODERS: Encoder[] = [
  'hevc_qsv',
  'h264_qsv',
  'av1_qsv',
  'hevc_vaapi',
  'h264_vaapi',
  'av1_vaapi',
  'libx265',
  'libx264',
  'libsvtav1',
]

export const isGpu = (e: Encoder) => e.endsWith('_qsv') || e.endsWith('_vaapi')

/**
 * The QSV encoder to suggest instead of VAAPI `e`, when the trial encode
 * found it working. On Intel both drive the same hardware, but QSV adapts the
 * quantizer to the content (ICQ) and has presets; the VAAPI profile is fixed
 * QP, which usually means bigger files for the same picture.
 */
export function qsvInsteadOf(e: Encoder, available: ReadonlySet<Encoder>): Encoder | null {
  if (!e.endsWith('_vaapi')) return null
  const qsv = e.replace(/_vaapi$/, '_qsv') as Encoder
  return available.has(qsv) ? qsv : null
}

const X26X_PRESETS = ['ultrafast', 'superfast', 'veryfast', 'faster', 'fast', 'medium', 'slow', 'slower', 'veryslow']
const QSV_PRESETS = ['veryfast', 'faster', 'fast', 'medium', 'slow', 'slower', 'veryslow']

/** Presets the encoder takes; empty when it has none (VAAPI). */
export function presetsFor(e: Encoder): string[] {
  if (e === 'libx264' || e === 'libx265') return X26X_PRESETS
  if (e.endsWith('_qsv')) return QSV_PRESETS
  if (e === 'libsvtav1') return Array.from({ length: 14 }, (_, i) => String(i))
  return []
}

/** Presets fastest first (SVT-AV1 counts down), as 样片推荐 runs them. */
export function presetsFastFirst(e: Encoder): string[] {
  const all = presetsFor(e)
  return e === 'libsvtav1' ? [...all].reverse() : all
}

/** What 样片推荐 compares by default; the server's `Encoder::compared_presets`. */
export function comparedPresets(e: Encoder): string[] {
  if (e === 'libx264' || e === 'libx265') return ['fast', 'medium', 'slow']
  if (e.endsWith('_qsv')) return ['veryfast', 'medium', 'slow', 'veryslow']
  if (e === 'libsvtav1') return ['10', '8', '6']
  return []
}

/** Presets that take many times longer than the default on a NAS CPU. */
export function isSlowPreset(e: Encoder, p: string): boolean {
  if (e === 'libx264' || e === 'libx265') return p === 'slower' || p === 'veryslow'
  if (e === 'libsvtav1') return Number(p) <= 4
  return false
}

/** The encoder's default preset, as the server fills it in. */
export function defaultPreset(e: Encoder): string | null {
  if (e === 'libsvtav1') return '8'
  return presetsFor(e).length ? 'medium' : null
}

/** What `quality` means for this encoder; lower is always better. */
export function qualityHint(e: Encoder): string {
  if (e.endsWith('_qsv')) return 'global_quality，越小画质越好、文件越大；常用 22–28'
  if (e.endsWith('_vaapi')) return 'QP，越小画质越好、文件越大；常用 22–28'
  if (e === 'libsvtav1') return 'CRF，越小画质越好、文件越大；常用 28–38'
  return 'CRF，越小画质越好、文件越大；常用 20–26'
}

export const PROFILE_ID = /^[A-Za-z0-9_-]{1,40}$/

export interface ProfileForm {
  id: string
  name: string
  encoder: Encoder
  quality: number
  /** null: the encoder's default. */
  preset: string | null
  container: Profile['container']
  /** null: keep the source size. */
  maxHeight: number | null
  threads: number | null
}

export function emptyProfileForm(): ProfileForm {
  return {
    id: '',
    name: '',
    encoder: 'hevc_qsv',
    quality: 24,
    preset: null,
    container: 'keep',
    maxHeight: null,
    threads: null,
  }
}

export function profileToForm(p: SavedProfile): ProfileForm {
  const r = p.profile.resize
  return {
    id: p.id,
    name: p.name,
    encoder: p.profile.encoder,
    quality: p.profile.quality,
    preset: p.profile.preset,
    container: p.profile.container,
    maxHeight: r === 'keep' ? null : r.max_height,
    threads: p.profile.threads,
  }
}

export function formToProfile(f: ProfileForm): Profile {
  // A preset the new encoder does not know would be passed to FFmpeg as is.
  const preset = f.preset && presetsFor(f.encoder).includes(f.preset) ? f.preset : null
  return {
    id: f.id,
    encoder: f.encoder,
    quality: f.quality,
    preset,
    container: f.container,
    resize: f.maxHeight ? { max_height: f.maxHeight } : 'keep',
    threads: f.threads && f.threads > 0 ? f.threads : null,
  }
}

/** Problems to show before sending; the server checks again. */
export function profileProblems(f: ProfileForm, isNew: boolean): string[] {
  const out: string[] = []
  if (isNew && !PROFILE_ID.test(f.id)) out.push('方案 id 只能用 1–40 个字母、数字、- 和 _')
  if (!f.name.trim()) out.push('请填写方案名称')
  if (!Number.isInteger(f.quality) || f.quality < 0 || f.quality > 63) out.push('质量参数应为 0–63 的整数')
  return out
}

export function describeProfile(p: Profile): string[] {
  const parts = [`质量 ${p.quality}`, `预设 ${p.preset ?? defaultPreset(p.encoder) ?? '无'}`]
  parts.push(p.container === 'keep' ? '保持原封装' : `转为 ${p.container.toUpperCase()}`)
  if (p.resize !== 'keep') parts.push(`最高 ${p.resize.max_height}p`)
  if (p.threads) parts.push(`${p.threads} 线程`)
  return parts
}

export interface MonitorForm {
  path: string
  enabled: boolean
  /** null: the default profile. */
  profileId: string | null
  recursive: boolean
  /** One glob per line. */
  excludes: string
}

export function emptyMonitorForm(): MonitorForm {
  return { path: '', enabled: true, profileId: null, recursive: true, excludes: '' }
}

export function monitorToForm(m: MonitorDir): MonitorForm {
  return {
    path: m.path,
    enabled: m.enabled,
    profileId: m.profile_id,
    recursive: m.recursive,
    excludes: m.excludes.join('\n'),
  }
}

export function formToMonitor(f: MonitorForm): MonitorInput {
  return {
    path: f.path.trim(),
    enabled: f.enabled,
    profile_id: f.profileId,
    recursive: f.recursive,
    excludes: f.excludes
      .split('\n')
      .map((s) => s.trim())
      .filter(Boolean),
  }
}
