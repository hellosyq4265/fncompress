import { describe, expect, it } from 'vitest'
import type { GlobalSettings, ServerSettings } from '../api/types'
import { checkRows, isDirty, normalize, observation, recycleTotals, toForm } from './settings'

const global: GlobalSettings = {
  ffmpeg: '/usr/bin/ffmpeg',
  ffprobe: '/usr/bin/ffprobe',
  scoring_ffmpeg: null,
  dovi_tool: null,
  render_device: '/dev/dri/renderD128',
  hw_decode: true,
  min_savings_pct: 5,
  recycle_dir: null,
  recycle_retention_days: 14,
  pending_retention_days: 14,
  encode_timeout_factor: 30,
  verify: {
    av_offset_soft_ms: 15,
    av_offset_hard_ms: 45,
    end_soft_ms: 50,
    end_hard_ms: 1000,
    decode_check: true,
    hw_decode: true,
    spot_checks: 8,
    spot_check_secs: 2,
    ssim_hard: 0.8,
    ssim_soft: 0.9,
    copied_stream_hash: false,
  },
}
const server: ServerSettings = {
  gpu_slots: 1,
  cpu_slots: 1,
  monitor_enabled: true,
  scan_interval_mins: 360,
  settle_secs: 60,
  observe_first_n: 20,
  default_profile: 'default',
  min_free_gib: 10,
}

describe('settings form', () => {
  it('copies, so edits do not touch the loaded settings', () => {
    const saved = { global, server }
    const f = toForm(saved)
    f.server.gpu_slots = 2
    expect(server.gpu_slots).toBe(1)
    expect(isDirty(f, saved)).toBe(true)
  })

  it('treats a blank recycle directory and padding as unchanged', () => {
    const f = toForm({ global, server })
    f.global.recycle_dir = '  '
    f.global.ffmpeg = ' /usr/bin/ffmpeg '
    expect(isDirty(f, { global, server })).toBe(false)
    expect(normalize(f).global.recycle_dir).toBeNull()
    f.global.recycle_dir = ' /vol1/recycle '
    expect(normalize(f).global.recycle_dir).toBe('/vol1/recycle')
    f.global.scoring_ffmpeg = '  '
    expect(normalize(f).global.scoring_ffmpeg).toBeNull()
    f.global.dovi_tool = ' '
    expect(normalize(f).global.dovi_tool).toBeNull()
  })

  it('notices nested changes', () => {
    const f = toForm({ global, server })
    f.global.verify.ssim_hard = 0.75
    expect(isDirty(f, { global, server })).toBe(true)
  })
})

describe('diagnostics', () => {
  it('describes the observation window', () => {
    expect(observation(5, 20)).toEqual({ percent: 25, text: '已观察 5 / 20 个；在此之前所有成品都要人工确认', active: true })
    expect(observation(20, 20).active).toBe(false)
    expect(observation(0, 0).text).toContain('已关闭')
  })

  it('folds check counts into rows, hard first', () => {
    const rows = checkRows({
      reports: 3,
      passed: 1,
      passed_with_warnings: 1,
      rejected: 1,
      observed: 3,
      observe_first_n: 20,
      checks: [
        { check: 'encoder_sei', severity: 'info', count: 3 },
        { check: 'av_offset', severity: 'soft', count: 1 },
        { check: 'av_offset', severity: 'hard', count: 1 },
        { check: 'timeline', severity: 'soft', count: 2 },
      ],
    })
    expect(rows.map((r) => [r.check, r.hard, r.soft, r.info])).toEqual([
      ['av_offset', 1, 1, 0],
      ['timeline', 0, 2, 0],
      ['encoder_sei', 0, 0, 3],
    ])
    expect(rows[0].label).toBe('音画偏移')
  })

  it('adds up the recycle', () => {
    const entry = (size: number) => ({
      file: 'x',
      id: 'x',
      identity: { dev: 1, ino: 1, size, mtime_ns: 1 },
      original_path: '/a',
      recycled_at: 1,
      replaced_by: '/a',
    })
    expect(recycleTotals({ dirs: [{ dir: '/a', entries: [entry(10), entry(5)] }, { dir: '/b', entries: [entry(1)] }] })).toEqual({
      files: 3,
      bytes: 16,
    })
  })
})
