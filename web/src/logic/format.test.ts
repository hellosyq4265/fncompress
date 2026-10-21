import { describe, expect, it } from 'vitest'
import type { Outcome } from '../api/types'
import { candidate, report, task } from '../test/fixtures'
import { baseName, dirName, formatBytes, formatDuration, formatUntil, outcomeSummary, progressView } from './format'

describe('format', () => {
  it('formats sizes and durations', () => {
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(5 * 1024 ** 3)).toBe('5.0 GB')
    expect(formatBytes(300 * 1024 ** 2)).toBe('300 MB')
    expect(formatDuration(75)).toBe('1:15')
    expect(formatDuration(3725)).toBe('1:02:05')
    expect(formatDuration(-1)).toBe('—')
  })

  it('splits paths', () => {
    expect(baseName('/vol1/a/b.mkv')).toBe('b.mkv')
    expect(baseName('C:\\x\\y.mp4')).toBe('y.mp4')
    expect(dirName('/vol1/a/b.mkv')).toBe('/vol1/a')
    expect(dirName('/b.mkv')).toBe('/')
  })

  it('says how far off a deadline is', () => {
    const now = 1_000_000 * 1000
    expect(formatUntil(1_000_000 + 30, now)).toBe('即将')
    expect(formatUntil(1_000_000 + 600, now)).toBe('10 分钟后')
    expect(formatUntil(1_000_000 + 7200, now)).toBe('2 小时后')
    expect(formatUntil(1_000_000 + 86400 * 14, now)).toBe('14 天后')
  })

  it('describes progress with speed and time left', () => {
    expect(progressView(null)).toEqual({ percent: null, text: '准备中' })
    expect(progressView({ id: 1, out_time_s: 60, duration_s: 600, percent: 10, speed: 2 })).toEqual({
      percent: 10,
      text: '1:00 / 10:00 · 2.0x · 剩余 4:30',
    })
    expect(progressView({ id: 1, out_time_s: 5, percent: null }).percent).toBeNull()
  })

  it('summarises outcomes', () => {
    expect(outcomeSummary(task(1, { quota: 'cpu' }))).toBe('等待空闲的 CPU 配额')
    expect(outcomeSummary(task(1, { state: 'failed', error: 'ffmpeg 崩溃' }))).toBe('ffmpeg 崩溃')
    const skipped: Outcome = { outcome: 'skipped', reason: { code: 'dolby_vision', message: '杜比视界暂不支持' } }
    expect(outcomeSummary(task(1, { state: 'skipped', outcome: skipped }))).toBe('杜比视界暂不支持')

    const savings = { source_size: 2 * 1024 ** 3, candidate_size: 1024 ** 3, saved_pct: 50 }
    const published = { outcome: 'published', savings, report: report(), published: {} } as unknown as Outcome
    expect(outcomeSummary(task(1, { state: 'published', outcome: published }))).toBe('节省 50.0%（2.0 GB → 1.0 GB）')
    expect(outcomeSummary(task(1, { state: 'released', outcome: published }))).toMatch(/^已人工放行，节省 50\.0%/)

    const low: Outcome = {
      outcome: 'insufficient_savings',
      savings: { ...savings, saved_pct: 3.2 },
      min_savings_pct: 5,
      report: report(),
    }
    expect(outcomeSummary(task(1, { state: 'insufficient_savings', outcome: low }))).toBe('只节省 3.2%，低于 5%，保留源片')

    const hard = candidate(report([{ check: 'frames', severity: 'hard', message: '帧数不同' }]))
    const rejected: Outcome = { outcome: 'rejected', candidate: hard }
    expect(outcomeSummary(task(1, { state: 'pending', pending_reason: 'rejected', outcome: rejected }))).toBe(
      '1 项硬性检查未通过（可节省 40.0%）',
    )
    const verified: Outcome = { outcome: 'verified', candidate: candidate() }
    expect(outcomeSummary(task(1, { state: 'pending', pending_reason: 'observation', outcome: verified }))).toBe(
      '观察模式：校验通过，等待确认（可节省 40.0%）',
    )
    expect(outcomeSummary(task(1, { state: 'discarded', outcome: rejected }))).toBe('成品已丢弃，源片未变')
  })
})
