// The task queue shown everywhere: loaded once, then kept current by SSE
// (or by polling while the stream is held back, see logic/events.ts).
// After `ready` or `lagged` it reloads all unfinished tasks and the newest
// finished ones; older finished tasks are paged in on demand.
import { reactive, ref } from 'vue'
import { api } from '../api/client'
import type { ServerEvent, Task } from '../api/types'
import { errorMessage } from '../logic/errors'
import { ACTIVE_STATES, FINISHED_STATES, TaskSync, mergePolled, oldestFinishedId, type TaskMap } from '../logic/tasks'

/** Finished tasks per page. */
export const PAGE = 50
/** Unfinished tasks are loaded in full, this many per request (the server's cap). */
const ACTIVE_PAGE = 500

export const tasks: TaskMap = reactive(new Map<number, Task>()) as TaskMap
const sync = new TaskSync(tasks)

export const tasksLoaded = ref(false)
export const tasksError = ref<string | null>(null)
export const moreFinished = ref(false)

export function taskEvent(e: ServerEvent): void {
  sync.event(e)
}

/**
 * Every unfinished task, paged newest first. One page is not enough: with a
 * long queue it would hold only the newest queued tasks and miss the running
 * ones, which have the oldest ids.
 */
async function allUnfinished(): Promise<Task[]> {
  const all: Task[] = []
  let before: number | undefined
  for (;;) {
    const page = (await api.tasks({ state: ACTIVE_STATES, limit: ACTIVE_PAGE, before })).tasks
    all.push(...page)
    if (page.length < ACTIVE_PAGE) return all
    before = page[page.length - 1].id
  }
}

let generation = 0
let polls = 0

export async function reloadTasks(): Promise<void> {
  const mine = ++generation
  sync.begin()
  try {
    const [active, finished] = await Promise.all([
      allUnfinished(),
      api.tasks({ state: FINISHED_STATES, limit: PAGE }),
    ])
    // A newer reload started meanwhile; it will finish the sync.
    if (mine !== generation) return
    sync.finish([...active, ...finished.tasks])
    moreFinished.value = finished.tasks.length === PAGE
    tasksError.value = null
    tasksLoaded.value = true
  } catch (e) {
    if (mine !== generation) return
    sync.abort()
    tasksError.value = errorMessage(e)
  }
}

/** Without the event stream: refreshes the lists, keeping paged-in history. */
export async function pollTasks(): Promise<void> {
  if (sync.resyncing) return
  // A reload that starts meanwhile wins; the poll must not cancel it.
  const reloads = generation
  const mine = ++polls
  try {
    const [active, finished] = await Promise.all([
      allUnfinished(),
      api.tasks({ state: FINISHED_STATES, limit: PAGE }),
    ])
    if (mine !== polls || reloads !== generation || sync.resyncing) return
    mergePolled(tasks, active, finished.tasks)
    tasksError.value = null
    tasksLoaded.value = true
  } catch (e) {
    if (mine === polls && reloads === generation) tasksError.value = errorMessage(e)
  }
}

export async function loadMoreFinished(): Promise<void> {
  const before = oldestFinishedId(tasks.values())
  const page = await api.tasks({ state: FINISHED_STATES, limit: PAGE, before })
  for (const t of page.tasks) if (!tasks.has(t.id)) tasks.set(t.id, t)
  moreFinished.value = page.tasks.length === PAGE
}
