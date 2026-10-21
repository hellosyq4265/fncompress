// The one task queue as the UI sees it: a map kept current by SSE events
// (ADR 0002; polled only while a proxy holds the stream back), plus the
// selectors the pages use.
import type { ServerEvent, Task, TaskProgress, TaskState } from '../api/types'

export type TaskMap = Map<number, Task>

/**
 * Applies one server event to the map. Returns true when the map changed.
 * Progress for a task we do not hold, or that is no longer running, is
 * ignored (it may arrive just after the final update).
 */
export function applyEvent(tasks: TaskMap, e: ServerEvent): boolean {
  switch (e.event) {
    case 'task_created':
    case 'task_updated': {
      const old = tasks.get(e.task.id)
      // Keep the latest progress if the update was for a still-running task.
      const progress = e.task.state === 'running' ? (e.task.progress ?? old?.progress) : undefined
      tasks.set(e.task.id, { ...e.task, progress })
      return true
    }
    case 'task_deleted':
      return tasks.delete(e.id)
    case 'task_progress': {
      const t = tasks.get(e.id)
      if (!t || t.state !== 'running') return false
      const { event: _event, ...progress } = e
      t.progress = progress as TaskProgress
      return true
    }
    default:
      return false
  }
}

/**
 * Reloading after `ready` / `lagged`: events that arrive while the lists are
 * being fetched are held and replayed on top of the fresh lists, so a slow
 * fetch never rolls a task back to an older state.
 */
export class TaskSync {
  private held: ServerEvent[] | null = null

  constructor(readonly tasks: TaskMap) {}

  get resyncing(): boolean {
    return this.held !== null
  }

  begin(): void {
    this.held ??= []
  }

  event(e: ServerEvent): void {
    if (this.held) this.held.push(e)
    else applyEvent(this.tasks, e)
  }

  /** Replaces everything with `fresh`, then replays what was held. */
  finish(fresh: Task[]): void {
    const held = this.held ?? []
    this.held = null
    this.tasks.clear()
    for (const t of fresh) this.tasks.set(t.id, t)
    for (const e of held) applyEvent(this.tasks, e)
  }

  /** The fetch failed: keep what we had and apply what was held. */
  abort(): void {
    const held = this.held ?? []
    this.held = null
    for (const e of held) applyEvent(this.tasks, e)
  }
}

/**
 * A poll without the event stream (`poll` in events.ts): takes the fresh
 * unfinished tasks and the newest finished page, and drops held unfinished
 * tasks that are in neither (they finished or were deleted). Older finished
 * tasks the user paged in stay, unlike a full reload.
 */
export function mergePolled(tasks: TaskMap, active: Task[], finished: Task[]): void {
  const fresh = new Set([...active, ...finished].map((t) => t.id))
  for (const [id, t] of tasks) {
    if (ACTIVE_STATES.includes(t.state) && !fresh.has(id)) tasks.delete(id)
  }
  for (const t of [...active, ...finished]) tasks.set(t.id, t)
}

/** Unfinished states; the reload fetches all of these, however many. */
export const ACTIVE_STATES: TaskState[] = ['queued', 'running', 'pending', 'awaiting_choice']
export const FINISHED_STATES: TaskState[] = [
  'published',
  'skipped',
  'insufficient_savings',
  'released',
  'discarded',
  'expired',
  'failed',
  'cancelled',
  'chosen',
]

export type View = 'active' | 'pending' | 'finished'

export function viewOf(state: TaskState): View {
  if (state === 'queued' || state === 'running') return 'active'
  if (state === 'pending' || state === 'awaiting_choice') return 'pending'
  return 'finished'
}

/**
 * Tasks of one view in display order: running first, then queued in the
 * order they will run; pending soonest-to-expire first; finished newest first.
 */
export function tasksIn(tasks: Iterable<Task>, view: View): Task[] {
  const list = [...tasks].filter((t) => viewOf(t.state) === view)
  switch (view) {
    case 'active':
      return list.sort((a, b) => {
        if (a.state !== b.state) return a.state === 'running' ? -1 : 1
        return a.id - b.id
      })
    case 'pending':
      return list.sort((a, b) => (a.pending_until ?? 0) - (b.pending_until ?? 0) || a.id - b.id)
    case 'finished':
      return list.sort((a, b) => b.id - a.id)
  }
}

export function countByView(tasks: Iterable<Task>): Record<View, number> {
  const n: Record<View, number> = { active: 0, pending: 0, finished: 0 }
  for (const t of tasks) n[viewOf(t.state)]++
  return n
}

/** Oldest finished task id we hold, for paging further back. */
export function oldestFinishedId(tasks: Iterable<Task>): number | undefined {
  let min: number | undefined
  for (const t of tasks) {
    if (viewOf(t.state) === 'finished' && (min === undefined || t.id < min)) min = t.id
  }
  return min
}

export interface Actions {
  cancel: boolean
  retry: boolean
  remove: boolean
  release: boolean
  discard: boolean
  /** Open the recommendation to pick a candidate. */
  choose: boolean
}

/** Which buttons a task offers (mirrors the server's rules). */
export function actionsFor(t: Task): Actions {
  const finished = viewOf(t.state) === 'finished'
  return {
    cancel: t.state === 'queued' || t.state === 'running',
    retry: finished,
    remove: finished,
    release: t.state === 'pending' && !!t.candidate,
    discard: t.state === 'pending' || t.state === 'awaiting_choice',
    choose: t.state === 'awaiting_choice',
  }
}
