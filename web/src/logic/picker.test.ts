import { describe, expect, it } from 'vitest'
import type { Listing } from '../api/types'
import { AUTHORIZE_HINT, emptyView } from './picker'

const listing = (path: string, dirs: string[] = []): Listing => ({
  path,
  parent: null,
  dirs: dirs.map((d) => ({ name: d, path: d })),
  files: [],
  truncated: false,
})

describe('emptyView', () => {
  it('shows nothing when there are entries', () => {
    expect(emptyView(listing('', ['/vol1/影音']), 'file')).toBeNull()
    expect(emptyView(listing('/vol1/影音', ['a']), 'dir')).toBeNull()
  })

  it('points at the authorized directories when no root is readable', () => {
    expect(emptyView(listing(''), 'file')).toEqual({ kind: 'authorize', text: AUTHORIZE_HINT })
    expect(AUTHORIZE_HINT).toContain('应用设置 → 授权目录')
  })

  it('says an ordinary directory is empty', () => {
    expect(emptyView(listing('/vol1/a'), 'file')?.text).toBe('这里没有子文件夹或视频文件')
    expect(emptyView(listing('/vol1/a'), 'dir')?.text).toBe('这里没有子文件夹')
  })
})
