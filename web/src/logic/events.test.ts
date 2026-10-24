import { describe, expect, it } from 'vitest'
import type { ServerEvent } from '../api/types'
import { connectEvents, type ConnectionState, type EventSourceLike } from './events'

class FakeSource implements EventSourceLike {
  readyState = 0
  onopen: ((ev: Event) => unknown) | null = null
  onerror: ((ev: Event) => unknown) | null = null
  closed = false
  listeners = new Map<string, ((ev: MessageEvent) => void)[]>()
  constructor(readonly url: string) {}
  addEventListener(type: string, fn: (ev: MessageEvent) => void) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn])
  }
  close() {
    this.closed = true
    this.readyState = 2
  }
  emit(type: string, data: unknown) {
    const ev = { data: typeof data === 'string' ? data : JSON.stringify(data) } as MessageEvent
    this.listeners.get(type)?.forEach((f) => f(ev))
  }
  fail() {
    this.onerror?.(new Event('error'))
  }
}

/** Defaults: ready within 5 s, quiet 20 s, retest after 120 s, polls every 4 s. */
function setup() {
  const sources: FakeSource[] = []
  const events: ServerEvent[] = []
  const states: ConnectionState[] = []
  const timers = new Map<number, { fn: () => void; ms: number }>()
  let next = 0
  let resyncs = 0
  let polls = 0
  const stop = connectEvents(
    {
      event: (e) => events.push(e),
      resync: () => resyncs++,
      state: (s) => states.push(s),
      poll: () => polls++,
    },
    {
      create: (url) => {
        const s = new FakeSource(url)
        sources.push(s)
        return s
      },
      pollMs: 4000,
      setTimer: (fn, ms) => {
        timers.set(++next, { fn, ms })
        return next
      },
      clearTimer: (id) => timers.delete(id as number),
    },
  )
  /** Runs the pending timer of `ms` (it must exist). */
  const fire = (ms: number) => {
    const hit = [...timers].find(([, t]) => t.ms === ms)
    if (!hit) throw new Error(`no ${ms} ms timer; pending: ${[...timers.values()].map((t) => t.ms)}`)
    timers.delete(hit[0])
    hit[1].fn()
  }
  const pending = () => [...timers.values()].map((t) => t.ms).sort((a, b) => a - b)
  return {
    sources,
    events,
    states,
    stop,
    fire,
    pending,
    last: () => sources[sources.length - 1],
    resyncs: () => resyncs,
    polls: () => polls,
  }
}

describe('connectEvents', () => {
  it('reloads on ready and lagged, and forwards events', () => {
    const t = setup()
    const s = t.last()
    expect(s.url).toBe('api/events')
    expect(t.states).toEqual(['connecting'])
    s.emit('ready', {})
    expect(t.states).toEqual(['connecting', 'open'])
    expect(t.resyncs()).toBe(1)
    s.emit('task_deleted', { event: 'task_deleted', id: 4 })
    s.emit('task_progress', 'not json')
    expect(t.events).toEqual([{ event: 'task_deleted', id: 4 }])
    s.emit('lagged', { missed: 3 })
    expect(t.resyncs()).toBe(2)
  })

  it('reopens a failed stream itself, polling meanwhile', () => {
    const t = setup()
    t.last().emit('ready', {})
    t.last().fail()
    expect(t.states.at(-1)).toBe('reconnecting')
    expect(t.sources[0].closed).toBe(true)
    // Retry after 3 s; polling would start at 5 s.
    expect(t.pending()).toEqual([3000, 5000])
    t.fire(3000)
    expect(t.sources).toHaveLength(2)
    t.last().emit('ready', {})
    expect(t.states.at(-1)).toBe('open')
    expect(t.resyncs()).toBe(2)
    expect(t.polls()).toBe(0)
    // Only the quiet watchdog is left.
    expect(t.pending()).toEqual([20000])
  })

  it('polls instead of a stream held back by a proxy, and tries it again later', () => {
    const t = setup()
    // A buffering proxy: nothing arrives within 5 s.
    t.fire(5000)
    expect(t.states.at(-1)).toBe('polling')
    expect(t.polls()).toBe(1)
    expect(t.sources[0].closed).toBe(true)
    expect(t.pending()).toEqual([4000, 120000])
    t.fire(4000)
    expect(t.polls()).toBe(2)
    // The retest is held back too: still polling, never shown as live.
    t.fire(120000)
    expect(t.sources).toHaveLength(2)
    t.fire(5000)
    expect(t.sources[1].closed).toBe(true)
    expect(t.states.every((s) => s !== 'open')).toBe(true)
    // The proxy was fixed: the next retest is live and polling stops.
    t.fire(120000)
    t.last().emit('ready', {})
    expect(t.states.at(-1)).toBe('open')
    expect(t.pending()).toEqual([20000])
  })

  it('keeps polling when ready only comes in a late burst', () => {
    const t = setup()
    t.fire(5000)
    expect(t.states.at(-1)).toBe('polling')
    // The proxy's buffer fills: ready and a backlog arrive at once.
    t.sources[0].emit('ready', {})
    t.sources[0].emit('task_progress', { event: 'task_progress', id: 1 })
    expect(t.states.at(-1)).toBe('polling')
    expect(t.events).toEqual([])
    expect(t.resyncs()).toBe(0)
    t.fire(4000)
    expect(t.polls()).toBe(2)
  })

  it('stays open while pings come, and reconnects when the stream goes silent', () => {
    const t = setup()
    t.last().emit('ready', {})
    expect(t.pending()).toEqual([20000])
    t.last().emit('ping', {})
    t.last().emit('task_progress', { event: 'task_progress', id: 1 })
    expect(t.pending()).toEqual([20000])
    expect(t.states.at(-1)).toBe('open')
    t.fire(20000)
    expect(t.states.at(-1)).toBe('reconnecting')
    expect(t.sources[0].closed).toBe(true)
    t.fire(0)
    expect(t.sources).toHaveLength(2)
    t.last().emit('ready', {})
    expect(t.states.at(-1)).toBe('open')
  })

  it('closes for good when stopped', () => {
    const t = setup()
    t.last().fail()
    t.stop()
    expect(t.sources[0].closed).toBe(true)
    expect(t.pending()).toEqual([])
  })
})
