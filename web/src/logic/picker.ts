// What the path picker shows for an empty listing. Empty roots mean nothing
// is readable: on fnOS the app user reads only the directories authorized
// for it, not the volume roots (ADR 0012).
import type { Listing } from '../api/types'

export const AUTHORIZE_HINT = '没有可以读取的位置。请在 fnOS「应用设置 → 授权目录」里给 fncompress 授权视频所在的文件夹，再重新打开。'

export type EmptyView = { kind: 'authorize'; text: string } | { kind: 'empty'; text: string }

export function emptyView(listing: Listing, mode: 'file' | 'dir'): EmptyView | null {
  if (listing.dirs.length || listing.files.length) return null
  if (listing.path === '') return { kind: 'authorize', text: AUTHORIZE_HINT }
  return { kind: 'empty', text: mode === 'file' ? '这里没有子文件夹或视频文件' : '这里没有子文件夹' }
}
