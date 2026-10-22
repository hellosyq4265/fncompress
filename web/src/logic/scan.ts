// Progress of a folder batch preview or submission: the server reports it
// with `scan_progress` events carrying the id the request sent.
import type { ScanProgress } from '../api/types'
import type { ProgressView } from './format'

/** An id for one request. Not `crypto.randomUUID`: plain-HTTP pages lack it. */
export function newScanId(): string {
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`
}

/**
 * Bar and line for the progress so far. Without events yet (or without the
 * event stream at all) the bar just runs.
 */
export function scanView(p: ScanProgress | null, submitting: boolean): ProgressView {
  if (!p) return { percent: null, text: submitting ? '正在加入队列…' : '正在检查文件…' }
  switch (p.phase) {
    case 'walking':
      return { percent: null, text: '正在列出文件夹里的视频…' }
    case 'screening':
      return {
        percent: p.total ? Math.floor((p.done / p.total) * 100) : 100,
        text: `正在检查 ${p.done} / ${p.total} 个文件`,
      }
    case 'queueing':
      return { percent: 100, text: `正在加入队列（${p.total} 个文件）` }
  }
}
