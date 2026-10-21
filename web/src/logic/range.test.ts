import { describe, expect, it } from 'vitest'
import type { DoviConfig } from '../api/types'
import { rangeView } from './range'

const dv = (profile: number, compat: number): DoviConfig => ({
  version_major: 1,
  version_minor: 0,
  profile,
  level: 6,
  rpu_present: true,
  el_present: profile === 7,
  bl_present: true,
  bl_compat_id: compat,
  md_compression: 0,
})

describe('rangeView', () => {
  it('says nothing for SDR', () => {
    expect(rangeView({ kind: 'sdr' })).toBeNull()
    expect(rangeView(undefined)).toBeNull()
  })

  it('names HDR10 and HLG', () => {
    expect(rangeView({ kind: 'hdr10' })?.tag).toBe('HDR10')
    expect(rangeView({ kind: 'hlg' })?.detail).toContain('10-bit')
    expect(rangeView({ kind: 'hdr10_plus' })).toMatchObject({ tag: 'HDR10+', type: 'info' })
  })

  it('adds HDR10+ to Dolby Vision', () => {
    const both = rangeView({ kind: 'dolby_vision', source: dv(8, 1), output: dv(8, 1), hdr10_plus: true })
    expect(both?.tag).toBe('杜比视界 P8.1 + HDR10+')
    expect(both?.detail).toContain('HDR10+')
    const p7 = rangeView({ kind: 'dolby_vision', source: dv(7, 6), output: dv(8, 1), hdr10_plus: true })
    expect(p7?.tag).toBe('杜比视界 P7 → P8.1 + HDR10+')
  })

  it('marks the P7 conversion', () => {
    const kept = rangeView({ kind: 'dolby_vision', source: dv(8, 1), output: dv(8, 1) })
    expect(kept).toMatchObject({ tag: '杜比视界 P8.1', type: 'info' })
    expect(rangeView({ kind: 'dolby_vision', source: dv(5, 0), output: dv(5, 0) })?.tag).toBe('杜比视界 P5')
    const p7 = rangeView({ kind: 'dolby_vision', source: dv(7, 6), output: dv(8, 1) })
    expect(p7).toMatchObject({ tag: '杜比视界 P7 → P8.1', type: 'warning' })
    expect(p7?.detail).toContain('转为 P8.1')
  })
})
