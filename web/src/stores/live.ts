// Starts the one SSE connection and fans events out: tasks go to the task
// store, configuration and monitor events to whoever listens, and which
// queued transcodes wait for free space to `space`.
import { onUnmounted, ref, shallowRef } from 'vue'
import { api } from '../api/client'
import type { MonitorStatus, ScanProgress, SpaceStatus } from '../api/types'
import { connectEvents, type ConnectionState } from '../logic/events'
import { pollTasks, reloadTasks, taskEvent } from './tasks'

export const connection = ref<ConnectionState>('connecting')
export const space = shallowRef<SpaceStatus>({ shortages: [], waiting_tasks: [] })

async function reloadSpace() {
  try {
    space.value = (await api.system()).space
  } catch {
    // The next event or reconnect brings it.
  }
}

type Listener<A extends unknown[]> = Set<(...args: A) => void>
const configListeners: Listener<[string]> = new Set()
const monitorListeners: Listener<[MonitorStatus]> = new Set()
const scanListeners: Listener<[ScanProgress]> = new Set()
const resyncListeners: Listener<[]> = new Set()
const pollListeners: Listener<[]> = new Set()

let stop: (() => void) | null = null

export function startLive(): void {
  if (stop) return
  stop = connectEvents({
    event(e) {
      if (e.event === 'config_changed') configListeners.forEach((f) => f(e.section))
      else if (e.event === 'monitor_status') monitorListeners.forEach((f) => f(e.status))
      else if (e.event === 'space_status') space.value = e.status
      else if (e.event === 'scan_progress') scanListeners.forEach((f) => f(e))
      else taskEvent(e)
    },
    resync() {
      void reloadTasks()
      void reloadSpace()
      resyncListeners.forEach((f) => f())
    },
    state(s) {
      connection.value = s
    },
    // Only what changes without the user: tasks, free space, monitor status.
    poll() {
      void pollTasks()
      void reloadSpace()
      pollListeners.forEach((f) => f())
    },
  })
}

function listen<A extends unknown[]>(set: Listener<A>, fn: (...args: A) => void): void {
  set.add(fn)
  onUnmounted(() => set.delete(fn))
}

/** Outside components (stores): listen for good. */
export function addConfigListener(fn: (section: string) => void): void {
  configListeners.add(fn)
}
export function addResyncListener(fn: () => void): void {
  resyncListeners.add(fn)
}
/** Polled while the event stream is held back (logic/events.ts). */
export function addPollListener(fn: () => void): void {
  pollListeners.add(fn)
}
export function addMonitorListener(fn: (s: MonitorStatus) => void): void {
  monitorListeners.add(fn)
}

/** In a component: `section` is "settings", "profiles" or "monitors". */
export const onConfigChanged = (fn: (section: string) => void) => listen(configListeners, fn)
export const onMonitorStatus = (fn: (s: MonitorStatus) => void) => listen(monitorListeners, fn)
/** In a component: folder batch progress, for every request (match `scan_id`). */
export const onScanProgress = (fn: (p: ScanProgress) => void) => listen(scanListeners, fn)
/** In a component: reload what it shows (reconnect or missed events). */
export const onResync = (fn: () => void) => listen(resyncListeners, fn)
