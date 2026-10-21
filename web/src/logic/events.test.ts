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
  fail(closed: boolean) {
    this.readyState = closed ? 2 : 0
    this.onerror?.(new Event('error'))
  }
}

function setup() {
  const sources: FakeSource[] = []
  const events: ServerEvent[] = []
  const states: ConnectionState[] = []
  const timers: (() => void)[] = []
  let resyncs = 0
  const stop = connectEvents(
    { event: (e) => events.push(e), resync: () => resyncs++, state: (s) => states.push(s) },
    {
      create: (url) => {
        const s = new FakeSource(url)
        sources.push(s)
        return s
      },
      setTimer: (fn) => timers.push(fn),
      clearTimer: () => {},
    },
  )
  return { sources, events, states, timers, stop, resyncs: () => resyncs }
}

describe('connectEvents', () => {
  it('reloads on ready and lagged, and forwards events', () => {
    const t = setup()
    const s = t.sources[0]
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

  it('lets the browser retry, and reopens a stream the browser closed', () => {
    const t = setup()
    t.sources[0].fail(false)
    expect(t.states.at(-1)).toBe('reconnecting')
    expect(t.timers).toHaveLength(0)
    t.sources[0].fail(true)
    expect(t.sources[0].closed).toBe(true)
    expect(t.timers).toHaveLength(1)
    t.timers[0]()
    expect(t.sources).toHaveLength(2)
    t.sources[1].emit('ready', {})
    expect(t.resyncs()).toBe(1)
    expect(t.states.at(-1)).toBe('open')
  })

  it('polls while ready does not come, and stops once it does', () => {
    const sources: FakeSource[] = []
    const states: ConnectionState[] = []
    const timers = new Map<number, { fn: () => void; ms: number }>()
    let next = 0
    let polls = 0
    const fire = () => {
      const [id, t] = [...timers][0]
      timers.delete(id)
      t.fn()
      return t.ms
    }
    const stop = connectEvents(
      { event: () => {}, resync: () => {}, state: (s) => states.push(s), poll: () => polls++ },
      {
        create: (url) => {
          const s = new FakeSource(url)
          sources.push(s)
          return s
        },
        pollAfterMs: 5000,
        pollMs: 4000,
        setTimer: (fn, ms) => {
          timers.set(++next, { fn, ms })
          return next
        },
        clearTimer: (id) => timers.delete(id as number),
      },
    )
    // A buffering proxy: the stream is open but nothing arrives.
    expect(states).toEqual(['connecting'])
    expect(fire()).toBe(5000)
    expect(polls).toBe(1)
    expect(states).toEqual(['connecting', 'polling'])
    expect(fire()).toBe(4000)
    expect(polls).toBe(2)
    // The browser retries meanwhile: still shown as polling, one poll timer.
    sources[0].fail(false)
    expect(states.at(-1)).toBe('polling')
    expect(timers.size).toBe(1)
    // The proxy finally lets it through.
    sources[0].emit('ready', {})
    expect(states.at(-1)).toBe('open')
    expect(timers.size).toBe(0)
    // Lost again: polling resumes after the delay.
    sources[0].fail(false)
    expect(states.at(-1)).toBe('reconnecting')
    expect(fire()).toBe(5000)
    expect(states.at(-1)).toBe('polling')
    stop()
    expect(timers.size).toBe(0)
  })

  it('closes for good when stopped', () => {
    const t = setup()
    t.stop()
    expect(t.sources[0].closed).toBe(true)
    t.sources[0].fail(true)
    expect(t.timers).toHaveLength(0)
  })
})
