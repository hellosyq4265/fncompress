// The single SSE connection to /api/events. The server sends `ready` first on
// every (re)connect and `lagged` when this client missed events; both mean
// "reload what you show", because events may have been missed. While idle it
// sends `ping` every few seconds, so silence means the stream is stuck.
//
// A reverse proxy that buffers responses (seen: the router in front of a
// domain name) holds the stream back: `ready` comes late or never, and events
// after it arrive in bursts whenever the proxy's buffer fills. So a stream
// counts as live only if `ready` comes within a few seconds of opening it.
// Otherwise the page closes it, polls every few seconds and tries the stream
// again a while later.
import type { ServerEvent } from '../api/types'

/** The parts of EventSource used here (tests pass a fake). */
export interface EventSourceLike {
  readonly readyState: number
  onopen: ((ev: Event) => unknown) | null
  onerror: ((ev: Event) => unknown) | null
  addEventListener(type: string, listener: (ev: MessageEvent) => void): void
  close(): void
}

export type ConnectionState = 'connecting' | 'open' | 'reconnecting' | 'polling'

export interface EventHandlers {
  event(e: ServerEvent): void
  /** After `ready` (each connect) and `lagged`. */
  resync(): void
  state(s: ConnectionState): void
  /** While the stream is not open: reload what changes on its own. */
  poll?(): void
}

export const EVENT_NAMES = [
  'task_created',
  'task_updated',
  'task_deleted',
  'batch_created',
  'scan_progress',
  'task_progress',
  'config_changed',
  'monitor_status',
  'space_status',
] as const satisfies readonly ServerEvent['event'][]

export interface ConnectOptions {
  url?: string
  create?: (url: string) => EventSourceLike
  /** Delay before reopening a stream that failed. */
  retryMs?: number
  /** `ready` later than this after opening: the stream is held back. */
  readyMs?: number
  /** Nothing at all (pings included) for this long after `ready`: reconnect. */
  quietMs?: number
  /** After a held-back stream, wait this long before trying it again. */
  retestMs?: number
  /** After a failure, wait this long before polling. */
  pollAfterMs?: number
  /** Between polls. */
  pollMs?: number
  setTimer?: (fn: () => void, ms: number) => unknown
  clearTimer?: (t: unknown) => void
}

/** Opens the stream; returns a function that closes it for good. */
export function connectEvents(h: EventHandlers, opts: ConnectOptions = {}): () => void {
  const url = opts.url ?? 'api/events'
  const create = opts.create ?? ((u: string) => new EventSource(u) as EventSourceLike)
  const retryMs = opts.retryMs ?? 3000
  const readyMs = opts.readyMs ?? 5000
  const quietMs = opts.quietMs ?? 20000
  const retestMs = opts.retestMs ?? 120000
  const pollAfterMs = opts.pollAfterMs ?? 5000
  const pollMs = opts.pollMs ?? 5000
  const setTimer = opts.setTimer ?? ((fn, ms) => setTimeout(fn, ms))
  const clearTimer = opts.clearTimer ?? ((t) => clearTimeout(t as ReturnType<typeof setTimeout>))

  let source: EventSourceLike | null = null
  let reopenTimer: unknown = null
  let watchdog: unknown = null
  let stopped = false
  let pollTimer: unknown = null
  let polling = false

  // While polling, the badge says so instead of "connecting" over and over.
  const report = (s: ConnectionState) => h.state(polling ? 'polling' : s)

  const tick = () => {
    pollTimer = null
    if (stopped) return
    if (!polling) {
      polling = true
      h.state('polling')
    }
    h.poll?.()
    pollTimer = setTimer(tick, pollMs)
  }
  const schedulePoll = () => {
    if (h.poll && pollTimer === null && !stopped) pollTimer = setTimer(tick, pollAfterMs)
  }
  const pollNow = () => {
    if (!h.poll || polling || stopped) return
    if (pollTimer !== null) clearTimer(pollTimer)
    tick()
  }
  const stopPolling = () => {
    if (pollTimer !== null) clearTimer(pollTimer)
    pollTimer = null
    polling = false
  }

  const clearWatchdog = () => {
    if (watchdog !== null) clearTimer(watchdog)
    watchdog = null
  }
  /** Closes the current stream and opens a new one after `ms`. */
  const reopenIn = (ms: number) => {
    clearWatchdog()
    source?.close()
    source = null
    if (reopenTimer === null && !stopped) reopenTimer = setTimer(open, ms)
  }

  const open = () => {
    reopenTimer = null
    const s = create(url)
    source = s
    let ready = false
    report('connecting')

    const watch = (ms: number) => {
      clearWatchdog()
      watchdog = setTimer(() => {
        watchdog = null
        if (source !== s) return
        if (ready) {
          // Went silent: the connection is stuck somewhere.
          report('reconnecting')
          schedulePoll()
          reopenIn(0)
        } else {
          // Held back by a proxy; what it lets through later is stale.
          pollNow()
          reopenIn(retestMs)
        }
      }, ms)
    }
    watch(readyMs)
    const alive = () => {
      if (source === s && ready) watch(quietMs)
    }

    s.addEventListener('ready', () => {
      if (source !== s) return
      ready = true
      alive()
      stopPolling()
      h.state('open')
      h.resync()
    })
    s.addEventListener('ping', alive)
    s.addEventListener('lagged', () => {
      if (source !== s) return
      alive()
      h.resync()
    })
    for (const name of EVENT_NAMES) {
      s.addEventListener(name, (ev) => {
        if (source !== s) return
        alive()
        let data: unknown
        try {
          data = JSON.parse(ev.data)
        } catch {
          return
        }
        h.event(data as ServerEvent)
      })
    }
    s.onerror = () => {
      if (stopped || source !== s) return
      report('reconnecting')
      schedulePoll()
      // Reopened here rather than by the browser, so each attempt's `ready`
      // deadline counts from when it started.
      reopenIn(retryMs)
    }
  }

  open()
  return () => {
    stopped = true
    if (reopenTimer !== null) clearTimer(reopenTimer)
    clearWatchdog()
    stopPolling()
    source?.close()
  }
}
