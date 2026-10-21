import { describe, expect, it } from 'vitest'
import type { Report } from '../api/types'
import { candidate, report, task } from '../test/fixtures'
import { checkLabel, groupFindings, inspectionLines, reportOf } from './report'

describe('report', () => {
  it('groups findings hard first and drops empty tiers', () => {
    const groups = groupFindings([
      { check: 'time_base', severity: 'info', message: 'a' },
      { check: 'frame_count', severity: 'hard', message: 'b' },
      { check: 'av_offset', severity: 'hard', message: 'c' },
    ])
    expect(groups.map((g) => [g.severity, g.findings.length])).toEqual([
      ['hard', 2],
      ['info', 1],
    ])
  })

  it('names checks in Chinese and passes unknown ids through', () => {
    expect(checkLabel('spot_check_ssim')).toBe('画面抽检')
    expect(checkLabel('new_check')).toBe('new_check')
  })

  it('finds the report wherever the outcome keeps it', () => {
    const r = report([{ check: 'decode', severity: 'hard', message: 'x' }])
    expect(reportOf(task(1, { outcome: { outcome: 'rejected', candidate: candidate(r) } }))).toBe(r)
    expect(reportOf(task(1, { outcome: { outcome: 'skipped', reason: { code: 'hdr', message: 'HDR' } } }))).toBeNull()
    expect(reportOf(task(1))).toBeNull()
    expect(reportOf(task(1, { candidate: candidate(r) }))).toBe(r)
  })

  it('summarises the FFmpeg inspection', () => {
    const r: Report = {
      ...report(),
      inspection: {
        decode: { hardware: true, errors: [] },
        spot_checks: [
          { at_s: 30, frames: 48, ssim: 0.991 },
          { at_s: 95, frames: 48, ssim: 0.962 },
        ],
        decoded_frames: [1000, 1000],
        stream_hashes: [
          { index: 1, source: 'a', candidate: 'a' },
          { index: 2, source: 'b', candidate: 'c' },
        ],
        errors: [['decode', 'ffmpeg 超时']],
      },
    }
    expect(inspectionLines(r)).toEqual([
      '完整解码（核显）：无错误',
      '画面抽检 2 处，最低 SSIM 0.962（1:35 处）',
      '解码帧数：源片 1000，成品 1000',
      '复制流哈希：1/2 条一致',
      '完整解码未能运行：ffmpeg 超时',
    ])
    expect(inspectionLines(report())).toEqual([])
  })
})
