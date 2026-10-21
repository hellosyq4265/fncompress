// Free-space wording: transcodes waiting in the queue, the monitor paused,
// a folder batch that may not fit. The server counts every file at its full
// source size (candidate next to the source, then the recycle or the
// pending output) plus the reserve from the settings.
import type { SpaceShortage, VolumeEstimate } from '../api/types'
import { formatBytes } from './format'

function room(volume: string, free: number, needed: number): string {
  return `${volume}：剩余 ${formatBytes(free)}，需要 ${formatBytes(needed)}`
}

/** One line per volume where queued transcodes wait. */
export function taskShortageLine(s: SpaceShortage): string {
  return `${room(s.volume, s.free_bytes, s.needed_bytes)}，${s.waiting} 个任务在等待`
}

/** One line per volume where the monitor holds files. */
export function monitorShortageLine(s: SpaceShortage): string {
  return `${room(s.volume, s.free_bytes, s.needed_bytes)}，${s.waiting} 个文件在等待`
}

export function isShort(e: VolumeEstimate): boolean {
  return e.free_bytes < e.needed_bytes
}

/** Volumes a folder batch would not fit on, worst case. */
export function shortEstimates(space: readonly VolumeEstimate[]): VolumeEstimate[] {
  return space.filter(isShort)
}

/** A folder batch's line names its file count; a single file's does not. */
export function estimateLine(e: VolumeEstimate, batch = true): string {
  const line = room(e.volume, e.free_bytes, e.needed_bytes)
  return batch ? `${line}（本批 ${e.files} 个文件）` : line
}

export const SPACE_NOTE =
  '按源片大小估算最坏情况，并计入已排队的任务和设置中的保留空间。放不下的任务会留在队列里等待空间，不会写满存储。'
