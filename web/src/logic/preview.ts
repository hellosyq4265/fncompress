// What the "new transcode" form tells the user before anything is queued:
// the single-file preview turned into notes and a go/no-go, and a folder
// batch's skipped files grouped by reason.
import type { Attempt, Preview, SkippedFile, VolumeEstimate } from '../api/types'
import { formatTime } from './format'
import { SPACE_NOTE, estimateLine, isShort } from './space'

export type NoteKind = 'error' | 'warning' | 'info'

export interface Note {
  kind: NoteKind
  title: string
  text?: string
}

export interface PreviewAdvice {
  notes: Note[]
  /** Submitting would queue a task that does something. */
  canSubmit: boolean
  /** The file carries the processed marker and `force` is not set. */
  needsForce: boolean
  /** The file's volume may not have room: submitting asks first. */
  shortSpace: VolumeEstimate | null
}

export const ATTEMPT_LABEL: Record<Attempt['kind'], string> = {
  skipped: '保护性跳过',
  rejected: '校验未通过',
  insufficient_savings: '收益不足',
}

export function attemptLine(a: Attempt, fingerprint: string): string {
  const same = a.fingerprint === fingerprint ? '（同一方案设置）' : ''
  return `${formatTime(a.at)} ${ATTEMPT_LABEL[a.kind]}${same}：${a.message}`
}

export function adviseOnPreview(p: Preview, force: boolean): PreviewAdvice {
  const notes: Note[] = []
  let canSubmit = true
  let needsForce = false

  if (p.active_task != null) {
    canSubmit = false
    notes.push({
      kind: 'error',
      title: `这个文件已有进行中或待确认的任务 #${p.active_task}`,
      text: '等它结束，或在任务页处理后再提交。',
    })
  }
  if (p.marker && !force) {
    canSubmit = false
    needsForce = true
    notes.push({
      kind: 'warning',
      title: `已被 fncompress 处理过（方案 ${p.marker.profile}）`,
      text: '再次压缩通常收益很小。确实要处理，请勾选「强制处理」。',
    })
  } else if (p.marker && force) {
    notes.push({ kind: 'info', title: `将忽略已处理标记（方案 ${p.marker.profile}）` })
  }
  // The route reports the marker as a skip too; the note above covers it.
  if (p.skip && p.skip.code !== 'already_processed') {
    canSubmit = false
    notes.push({ kind: 'error', title: '会被保护性跳过，不会编码', text: p.skip.message })
  }
  const same = p.attempts.filter((a) => a.fingerprint === p.fingerprint)
  if (same.length > 0) {
    notes.push({
      kind: 'warning',
      title: `同一方案设置已尝试过 ${same.length} 次`,
      text: '结果很可能相同；可以换个方案，或仍然提交。',
    })
  }
  // Only worth saying when the file would be queued at all.
  const shortSpace = canSubmit && p.space && isShort(p.space) ? p.space : null
  if (shortSpace) {
    notes.push({
      kind: 'warning',
      title: '存储空间可能不足',
      text: `${estimateLine(shortSpace, false)}。${SPACE_NOTE}`,
    })
  }
  return { notes, canSubmit, needsForce, shortSpace }
}

export const SKIP_LABEL: Record<string, string> = {
  active_task: '已有进行中或待确认的任务',
  attempted: '同一方案尝试过（可勾选「包含尝试过的文件」）',
  already_processed: '已处理过（可勾选「强制处理」）',
  probe_failed: '探测失败',
  unreadable: '无法读取',
}

export interface SkipGroup {
  code: string
  label: string
  files: SkippedFile[]
}

/** Skipped files grouped by reason, largest group first. */
export function groupSkipped(skipped: SkippedFile[]): SkipGroup[] {
  const groups = new Map<string, SkippedFile[]>()
  for (const s of skipped) groups.set(s.code, [...(groups.get(s.code) ?? []), s])
  return [...groups.entries()]
    .map(([code, files]) => ({ code, label: SKIP_LABEL[code] ?? code, files }))
    .sort((a, b) => b.files.length - a.files.length || a.code.localeCompare(b.code))
}

/** Path relative to `dir` for listing batch files compactly. */
export function relativeTo(dir: string, path: string): string {
  const d = dir.replace(/[\\/]+$/, '')
  if (path.startsWith(d) && (path[d.length] === '/' || path[d.length] === '\\')) return path.slice(d.length + 1)
  return path
}
