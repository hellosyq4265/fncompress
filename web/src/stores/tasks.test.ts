import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Task, TaskState } from '../api/types'
import { task } from '../test/fixtures'
import { countByView } from '../logic/tasks'

// A fake server that lists like the real one: newest first, at most 500.
const server = vi.hoisted(() => ({ tasks: [] as Task[] }))
const api = vi.hoisted(() => ({
  tasks: vi.fn(async (q: { state?: TaskState[]; limit?: number; before?: number }) => {
    const list = server.tasks
      .filter((t) => !q.state || q.state.includes(t.state))
      .filter((t) => q.before === undefined || t.id < q.before)
      .sort((a, b) => b.id - a.id)
      .slice(0, Math.min(q.limit ?? 100, 500))
    return { tasks: list.map((t) => ({ ...t })) }
  }),
}))
vi.mock('../api/client', () => ({ api }))

const { tasks, reloadTasks, pollTasks, taskEvent } = await import('./tasks')

/** 2 running (oldest ids), then `queued` more queued behind them. */
function queue(queued: number) {
  server.tasks = [task(1, { state: 'running' }), task(2, { state: 'running' })]
  for (let i = 0; i < queued; i++) server.tasks.push(task(3 + i, { state: 'queued' }))
}

describe('a queue longer than one page', () => {
  beforeEach(() => queue(600))

  it('the reload holds the running tasks', async () => {
    await reloadTasks()
    expect(tasks.get(1)?.state).toBe('running')
    expect(tasks.get(2)?.state).toBe('running')
  })

  it('a poll keeps the running tasks', async () => {
    await reloadTasks()
    await pollTasks()
    expect(tasks.get(1)?.state).toBe('running')
  })

  it('progress of a running task reaches the store after a reload', async () => {
    await reloadTasks()
    taskEvent({ event: 'task_progress', id: 1, percent: 42 } as never)
    expect(tasks.get(1)?.progress).toMatchObject({ percent: 42 })
  })

  it('the count follows the queue as tasks finish', async () => {
    await reloadTasks()
    const before = countByView(tasks.values()).active
    server.tasks[0] = { ...server.tasks[0], state: 'published' }
    await pollTasks()
    expect(countByView(tasks.values()).active).toBe(before - 1)
  })
})
