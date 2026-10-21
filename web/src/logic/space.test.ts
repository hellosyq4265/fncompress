import { describe, expect, it } from 'vitest'
import { estimateLine, monitorShortageLine, shortEstimates, taskShortageLine } from './space'

const GB = 1024 ** 3

describe('space', () => {
  it('describes shortages', () => {
    const s = { volume: '/vol1', free_bytes: 12 * GB, needed_bytes: 58 * GB, waiting: 2 }
    expect(taskShortageLine(s)).toBe('/vol1：剩余 12.0 GB，需要 58.0 GB，2 个任务在等待')
    expect(monitorShortageLine(s)).toBe('/vol1：剩余 12.0 GB，需要 58.0 GB，2 个文件在等待')
  })

  it('keeps only volumes a batch would not fit on', () => {
    const fits = { volume: '/vol1', free_bytes: 100 * GB, needed_bytes: 100 * GB, files: 3 }
    const short = { volume: '/vol2', free_bytes: 1 * GB, needed_bytes: 30 * GB, files: 1 }
    expect(shortEstimates([fits, short])).toEqual([short])
    expect(estimateLine(short)).toBe('/vol2：剩余 1.0 GB，需要 30.0 GB（本批 1 个文件）')
  })
})
