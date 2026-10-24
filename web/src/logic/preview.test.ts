import { describe, expect, it } from 'vitest'
import type { Attempt, Preview } from '../api/types'
import { adviseOnPreview, attemptLine, groupSkipped, relativeTo } from './preview'

const identity = { dev: 1, ino: 2, size: 3, mtime_ns: 4 }

function preview(over: Partial<Preview> = {}): Preview {
  return {
    path: '/vol1/a.mkv',
    identity,
    profile_id: 'default',
    fingerprint: 'fp1',
    attempts: [],
    plan: null,
    skip: null,
    ...over,
  }
}

function attempt(fingerprint: string, kind: Attempt['kind'] = 'rejected'): Attempt {
  return { id: 1, identity, fingerprint, path: '/vol1/a.mkv', kind, message: '帧数不同', at: 1_700_000_000 }
}

describe('adviseOnPreview', () => {
  it('lets a clean file through', () => {
    expect(adviseOnPreview(preview(), false)).toEqual({ notes: [], canSubmit: true, needsForce: false, shortSpace: null })
  })

  it('warns when the volume may be short, only if the file would be queued', () => {
    const GB = 1024 ** 3
    const space = { volume: '/vol1', free_bytes: 5 * GB, needed_bytes: 40 * GB, files: 1 }
    const a = adviseOnPreview(preview({ space }), false)
    expect(a.canSubmit).toBe(true)
    expect(a.shortSpace).toEqual(space)
    expect(a.notes[0].title).toBe('存储空间可能不足')
    expect(a.notes[0].text).toMatch(/^\/vol1：剩余 5\.0 GB，需要 40\.0 GB。/)
    const fits = adviseOnPreview(preview({ space: { ...space, free_bytes: 50 * GB } }), false)
    expect(fits).toMatchObject({ notes: [], shortSpace: null })
    const busy = adviseOnPreview(preview({ space, active_task: 7 }), false)
    expect(busy.shortSpace).toBeNull()
    expect(busy.notes).toHaveLength(1)
  })

  it('asks for force on processed files', () => {
    const p = preview({
      marker: { profile: 'hq', version: '1' },
      skip: { code: 'already_processed', message: '已被 fncompress 处理过' },
    })
    const a = adviseOnPreview(p, false)
    expect(a.notes).toHaveLength(1)
    expect(a.canSubmit).toBe(false)
    expect(a.needsForce).toBe(true)
    expect(a.notes[0].title).toContain('方案 hq')
    const forced = adviseOnPreview(p, true)
    expect(forced.canSubmit).toBe(true)
    expect(forced.notes[0].kind).toBe('info')
  })

  it('blocks protective skips and files with a task', () => {
    const skip = adviseOnPreview(preview({ skip: { code: 'dolby_vision', message: '杜比视界 M5 支持' } }), false)
    expect(skip.canSubmit).toBe(false)
    expect(skip.notes).toEqual([{ kind: 'error', title: '会被保护性跳过，不会编码', text: '杜比视界 M5 支持' }])
    const busy = adviseOnPreview(preview({ active_task: 7 }), false)
    expect(busy.canSubmit).toBe(false)
    expect(busy.notes[0].title).toContain('#7')
  })

  it('warns about attempts with the same settings only', () => {
    const other = adviseOnPreview(preview({ attempts: [attempt('fp0')] }), false)
    expect(other.notes).toEqual([])
    const same = adviseOnPreview(preview({ attempts: [attempt('fp1'), attempt('fp1')] }), false)
    expect(same.canSubmit).toBe(true)
    expect(same.notes[0].title).toBe('同一方案设置已尝试过 2 次')
    expect(attemptLine(attempt('fp1'), 'fp1')).toMatch(/校验未通过（同一方案设置）：帧数不同$/)
    expect(attemptLine(attempt('fp0', 'insufficient_savings'), 'fp1')).toMatch(/收益不足：帧数不同$/)
    expect(attemptLine(attempt('fp0', 'predicted_no_gain'), 'fp1')).toMatch(/预判收益不足：帧数不同$/)
  })
})

describe('batch helpers', () => {
  it('groups skipped files by reason', () => {
    const g = groupSkipped([
      { path: '/a', code: 'attempted', message: '' },
      { path: '/b', code: 'already_processed', message: '' },
      { path: '/c', code: 'already_processed', message: '' },
      { path: '/d', code: 'mystery', message: '' },
    ])
    expect(g.map((x) => [x.code, x.files.length])).toEqual([
      ['already_processed', 2],
      ['attempted', 1],
      ['mystery', 1],
    ])
    expect(g[0].label).toContain('强制处理')
    expect(g[2].label).toBe('mystery')
  })

  it('shortens paths under the batch directory', () => {
    expect(relativeTo('/vol1/TV', '/vol1/TV/S1/e1.mkv')).toBe('S1/e1.mkv')
    expect(relativeTo('/vol1/TV/', '/vol1/TV/e1.mkv')).toBe('e1.mkv')
    expect(relativeTo('/vol1/TV', '/vol1/TVx/e1.mkv')).toBe('/vol1/TVx/e1.mkv')
    expect(relativeTo('C:\\m', 'C:\\m\\a.mkv')).toBe('a.mkv')
  })
})
