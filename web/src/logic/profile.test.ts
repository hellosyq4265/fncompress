import { describe, expect, it } from 'vitest'
import type { SavedProfile } from '../api/types'
import {
  defaultPreset,
  describeProfile,
  emptyProfileForm,
  formToMonitor,
  formToProfile,
  monitorToForm,
  presetsFor,
  profileProblems,
  profileToForm,
  qualityHint,
  qsvInsteadOf,
} from './profile'

const saved: SavedProfile = {
  id: 'hq',
  name: '高画质',
  profile: {
    id: 'hq',
    encoder: 'libx265',
    quality: 20,
    preset: 'slow',
    container: 'mkv',
    resize: { max_height: 1080 },
    threads: 4,
  },
}

describe('profile form', () => {
  it('knows each encoder', () => {
    expect(presetsFor('libx264')).toContain('ultrafast')
    expect(presetsFor('hevc_qsv')).not.toContain('ultrafast')
    expect(presetsFor('hevc_vaapi')).toEqual([])
    expect(presetsFor('libsvtav1')).toHaveLength(14)
    expect(defaultPreset('libsvtav1')).toBe('8')
    expect(defaultPreset('h264_vaapi')).toBeNull()
    expect(defaultPreset('hevc_qsv')).toBe('medium')
    expect(qualityHint('hevc_qsv')).toContain('global_quality')
    expect(qualityHint('h264_vaapi')).toContain('QP')
  })

  it('round-trips a saved profile', () => {
    const f = profileToForm(saved)
    expect(f.maxHeight).toBe(1080)
    expect(formToProfile(f)).toEqual(saved.profile)
  })

  it('drops presets the encoder does not take and empty limits', () => {
    const f = { ...profileToForm(saved), encoder: 'hevc_vaapi' as const, maxHeight: null, threads: 0 }
    expect(formToProfile(f)).toMatchObject({ preset: null, resize: 'keep', threads: null })
  })

  it('checks the fields the server checks', () => {
    const f = emptyProfileForm()
    expect(profileProblems(f, true)).toHaveLength(2)
    expect(profileProblems({ ...f, id: 'a b', name: 'x' }, true)).toEqual(['方案 id 只能用 1–40 个字母、数字、- 和 _'])
    expect(profileProblems({ ...f, name: 'x', quality: 70 }, false)).toEqual(['质量参数应为 0–63 的整数'])
    expect(profileProblems({ ...f, id: 'ok-1', name: 'x' }, true)).toEqual([])
  })

  it('describes a profile in one line', () => {
    expect(describeProfile(saved.profile)).toEqual(['质量 20', '预设 slow', '转为 MKV', '最高 1080p', '4 线程'])
    expect(describeProfile({ ...saved.profile, preset: null, container: 'keep', resize: 'keep', threads: null })).toEqual([
      '质量 20',
      '预设 medium',
      '保持原封装',
    ])
  })
})

describe('monitor form', () => {
  it('turns exclude lines into globs', () => {
    const f = monitorToForm({ id: 1, path: '/vol1/TV', enabled: true, profile_id: 'hq', recursive: true, excludes: ['**/Extras/**'] })
    expect(f.excludes).toBe('**/Extras/**')
    expect(formToMonitor({ ...f, path: ' /vol1/TV ', excludes: ' **/Extras/**\n\n*.sample.mkv \n' })).toEqual({
      path: '/vol1/TV',
      enabled: true,
      profile_id: 'hq',
      recursive: true,
      excludes: ['**/Extras/**', '*.sample.mkv'],
    })
  })
})

describe('qsvInsteadOf', () => {
  it('suggests the QSV encoder for the same codec when it works', () => {
    const working = new Set(['hevc_qsv', 'h264_qsv', 'hevc_vaapi'] as const)
    expect(qsvInsteadOf('hevc_vaapi', working)).toBe('hevc_qsv')
    expect(qsvInsteadOf('h264_vaapi', working)).toBe('h264_qsv')
  })

  it('stays quiet when QSV did not pass the trial encode, or the encoder is not VAAPI', () => {
    const working = new Set(['hevc_qsv'] as const)
    expect(qsvInsteadOf('av1_vaapi', working)).toBeNull()
    expect(qsvInsteadOf('hevc_qsv', working)).toBeNull()
    expect(qsvInsteadOf('libx265', working)).toBeNull()
  })
})
