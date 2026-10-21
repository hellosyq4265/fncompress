// The single SSE connection to /api/events. The server sends `ready` first on
// every (re)connect and `lagged` when this client missed events; both mean
// "reload what you show", because events may have been missed.
//
// A reverse proxy that buffers whole responses (seen: the router in front of
// a domain name) holds the stream back, so `ready` never arrives. Then the
// page polls instead: `poll` every few seconds until `ready` comes.
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
  'task_progress',
  'config_changed',
  'monitor_status',
  'space_status',
] as const satisfies readonly ServerEvent['event'][]

const CLOSED = 2

export interface ConnectOptions {
  url?: string
  create?: (url: string) => EventSourceLike
  /** Delay before reopening a connection the browser gave up on. */
  retryMs?: number
  /** Without `ready` for this long, start polling (when `poll` is given). */
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
  const pollAfterMs = opts.pollAfterMs ?? 5000
  const pollMs = opts.pollMs ?? 5000
  const setTimer = opts.setTimer ?? ((fn, ms) => setTimeout(fn, ms))
  const clearTimer = opts.clearTimer ?? ((t) => clearTimeout(t as ReturnType<typeof setTimeout>))

  let source: EventSourceLike | null = null
  let timer: unknown = null
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
  const stopPolling = () => {
    if (pollTimer !== null) clearTimer(pollTimer)
    pollTimer = null
    polling = false
  }

  const open = () => {
    timer = null
    const s = create(url)
    source = s
    report('connecting')
    schedulePoll()
    s.addEventListener('ready', () => {
      stopPolling()
      h.state('open')
      h.resync()
    })
    s.addEventListener('lagged', () => h.resync())
    for (const name of EVENT_NAMES) {
      s.addEventListener(name, (ev) => {
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
      if (stopped) return
      report('reconnecting')
      schedulePoll()
      // The browser retries by itself unless it closed the stream (e.g. the
      // server answered with an error page); then we reopen it ourselves.
      if (s.readyState === CLOSED && timer === null) {
        s.close()
        timer = setTimer(open, retryMs)
      }
    }
  }

  open()
  return () => {
    stopped = true
    if (timer !== null) clearTimer(timer)
    stopPolling()
    source?.close()
  }
}
