import { describe, expect, it } from 'vitest'
import type { ServerEvent } from '../api/types'
import { candidate, task } from '../test/fixtures'
import {
  TaskSync,
  actionsFor,
  applyEvent,
  countByView,
  foldQueue,
  mergePolled,
  oldestFinishedId,
  tasksIn,
  type TaskMap,
} from './tasks'

const progress = (id: number, percent: number): ServerEvent => ({
  event: 'task_progress',
  id,
  out_time_s: percent,
  duration_s: 100,
  percent,
  speed: 2,
})

describe('applyEvent', () => {
  it('adds, updates and deletes tasks', () => {
    const m: TaskMap = new Map()
    expect(applyEvent(m, { event: 'task_created', task: task(1) })).toBe(true)
    expect(m.get(1)?.state).toBe('queued')
    applyEvent(m, { event: 'task_updated', task: task(1, { state: 'running' }) })
    expect(m.get(1)?.state).toBe('running')
    expect(applyEvent(m, { event: 'task_deleted', id: 1 })).toBe(true)
    expect(m.size).toBe(0)
    expect(applyEvent(m, { event: 'task_deleted', id: 1 })).toBe(false)
  })

  it('adds a whole folder batch at once', () => {
    const m: TaskMap = new Map()
    const tasks = [task(5, { batch_id: 2 }), task(6, { batch_id: 2 })]
    expect(applyEvent(m, { event: 'batch_created', batch_id: 2, tasks })).toBe(true)
    expect([...m.keys()]).toEqual([5, 6])
  })

  it('puts progress on running tasks only', () => {
    const m: TaskMap = new Map([
      [1, task(1, { state: 'running' })],
      [2, task(2)],
    ])
    expect(applyEvent(m, progress(1, 40))).toBe(true)
    expect(m.get(1)?.progress).toEqual({ id: 1, out_time_s: 40, duration_s: 100, percent: 40, speed: 2 })
    expect(applyEvent(m, progress(2, 10))).toBe(false)
    expect(applyEvent(m, progress(9, 10))).toBe(false)
    expect(m.get(2)?.progress).toBeUndefined()
  })

  it('keeps progress across an update while running and drops it after', () => {
    const m: TaskMap = new Map([[1, task(1, { state: 'running' })]])
    applyEvent(m, progress(1, 40))
    applyEvent(m, { event: 'task_updated', task: task(1, { state: 'running' }) })
    expect(m.get(1)?.progress?.percent).toBe(40)
    applyEvent(m, { event: 'task_updated', task: task(1, { state: 'published' }) })
    expect(m.get(1)?.progress).toBeUndefined()
  })

  it('ignores configuration events', () => {
    const m: TaskMap = new Map()
    expect(applyEvent(m, { event: 'config_changed', section: 'settings' })).toBe(false)
  })
})

describe('TaskSync', () => {
  it('replays events that arrive during a reload on top of the fresh list', () => {
    const m: TaskMap = new Map([
      [1, task(1)],
      [5, task(5)],
    ])
    const sync = new TaskSync(m)
    sync.begin()
    expect(sync.resyncing).toBe(true)
    // Arrives while the (older) list is in flight.
    sync.event({ event: 'task_updated', task: task(1, { state: 'running' }) })
    sync.event({ event: 'task_created', task: task(3) })
    expect(m.get(1)?.state).toBe('queued')
    sync.finish([task(1), task(2)])
    expect([...m.keys()].sort()).toEqual([1, 2, 3])
    expect(m.get(1)?.state).toBe('running')
    expect(sync.resyncing).toBe(false)
    sync.event({ event: 'task_deleted', id: 2 })
    expect(m.has(2)).toBe(false)
  })

  it('applies held events to the old map when the reload fails', () => {
    const m: TaskMap = new Map([[1, task(1)]])
    const sync = new TaskSync(m)
    sync.begin()
    sync.begin()
    sync.event({ event: 'task_updated', task: task(1, { state: 'cancelled' }) })
    sync.abort()
    expect(m.get(1)?.state).toBe('cancelled')
  })
})

describe('mergePolled', () => {
  it('refreshes the lists and keeps finished tasks paged in earlier', () => {
    const m: TaskMap = new Map([
      [1, task(1, { state: 'running' })],
      [2, task(2)],
      [3, task(3, { state: 'published' })],
    ])
    // 1 finished, 2 was deleted, 3 is older history, 4 is new.
    mergePolled(m, [task(4)], [task(1, { state: 'published' })])
    expect([...m.keys()].sort()).toEqual([1, 3, 4])
    expect(m.get(1)?.state).toBe('published')
    expect(m.get(3)?.state).toBe('published')
  })
})

describe('selectors', () => {
  const all = [
    task(1, { state: 'published' }),
    task(2, { state: 'queued' }),
    task(3, { state: 'running' }),
    task(4, { state: 'queued' }),
    task(5, { state: 'pending', pending_until: 300 }),
    task(6, { state: 'pending', pending_until: 200 }),
    task(7, { state: 'failed' }),
  ]

  it('orders each view', () => {
    expect(tasksIn(all, 'active').map((t) => t.id)).toEqual([3, 2, 4])
    expect(tasksIn(all, 'pending').map((t) => t.id)).toEqual([6, 5])
    expect(tasksIn(all, 'finished').map((t) => t.id)).toEqual([7, 1])
  })

  it('counts views and finds the paging cursor', () => {
    expect(countByView(all)).toEqual({ active: 3, pending: 2, finished: 2 })
    expect(oldestFinishedId(all)).toBe(1)
    expect(oldestFinishedId([task(2)])).toBeUndefined()
  })

  it('offers the actions the server allows', () => {
    expect(actionsFor(task(1, { state: 'queued' }))).toMatchObject({ cancel: true, retry: false, remove: false })
    expect(actionsFor(task(1, { state: 'running' })).cancel).toBe(true)
    expect(actionsFor(task(1, { state: 'failed' }))).toMatchObject({ cancel: false, retry: true, remove: true })
    expect(actionsFor(task(1, { state: 'pending', candidate: candidate() }))).toMatchObject({
      cancel: false,
      release: true,
      discard: true,
      remove: false,
    })
    expect(actionsFor(task(1, { state: 'pending' })).release).toBe(false)
    const rec = { type: 'recommend' as const }
    expect(actionsFor(task(1, { ...rec, state: 'awaiting_choice' }))).toMatchObject({
      choose: true,
      discard: true,
      release: false,
      remove: false,
      retry: false,
    })
    expect(actionsFor(task(1, { ...rec, state: 'chosen' }))).toMatchObject({ choose: false, retry: true, remove: true })
  })
})

describe('foldQueue', () => {
  const ids = (n: number) => Array.from({ length: n }, (_, i) => i + 1)

  it('shows a short queue whole', () => {
    expect(foldQueue(ids(10), false)).toEqual({ shown: ids(10), hidden: 0 })
  })

  it('folds a long queue to the first 10', () => {
    const { shown, hidden } = foldQueue(ids(600), false)
    expect(shown).toEqual(ids(10))
    expect(hidden).toBe(590)
  })

  it('unfolds to at most 30', () => {
    expect(foldQueue(ids(25), true)).toEqual({ shown: ids(25), hidden: 0 })
    const { shown, hidden } = foldQueue(ids(600), true)
    expect(shown).toEqual(ids(30))
    expect(hidden).toBe(570)
  })
})
