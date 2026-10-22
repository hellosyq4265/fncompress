import { describe, expect, it } from 'vitest'
import type { ScanProgress } from '../api/types'
import { newScanId, scanView } from './scan'

const at = (phase: ScanProgress['phase'], done: number, total: number): ScanProgress => ({
  scan_id: 's',
  phase,
  done,
  total,
})

describe('scanView', () => {
  it('runs without a figure until the server reports', () => {
    expect(scanView(null, false)).toEqual({ percent: null, text: '正在检查文件…' })
    expect(scanView(null, true).percent).toBeNull()
    expect(scanView(at('walking', 0, 0), false).percent).toBeNull()
  })

  it('counts checked files', () => {
    expect(scanView(at('screening', 37, 120), false)).toEqual({ percent: 30, text: '正在检查 37 / 120 个文件' })
    expect(scanView(at('screening', 0, 0), false).percent).toBe(100)
  })

  it('ends with queueing', () => {
    expect(scanView(at('queueing', 5, 5), true)).toEqual({ percent: 100, text: '正在加入队列（5 个文件）' })
  })
})

describe('newScanId', () => {
  it('differs each time', () => {
    expect(newScanId()).not.toBe(newScanId())
  })
})
