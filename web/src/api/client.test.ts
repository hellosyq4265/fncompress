import { afterEach, describe, expect, it, vi } from 'vitest'
import { ApiError } from '../logic/errors'
import { api, query } from './client'

function mockFetch(status: number, body: unknown) {
  const fn = vi.fn(
    async (_url: string, _init?: RequestInit) =>
      new Response(body === undefined ? null : JSON.stringify(body), { status }),
  )
  vi.stubGlobal('fetch', fn)
  return fn
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('client', () => {
  it('builds queries without empty values', () => {
    expect(query({ a: 1, b: undefined, c: '', d: 'x y', e: null })).toBe('?a=1&d=x+y')
    expect(query({})).toBe('')
  })

  it('uses relative URLs and joins states', async () => {
    const f = mockFetch(200, { tasks: [] })
    await api.tasks({ state: ['queued', 'running'], limit: 5 })
    expect(f.mock.calls[0][0]).toBe('api/tasks?state=queued%2Crunning&limit=5')
  })

  it('sends JSON bodies and handles 204', async () => {
    const f = mockFetch(204, undefined)
    await expect(api.deleteTask(3)).resolves.toBeUndefined()
    expect(f.mock.calls[0][1]?.method).toBe('DELETE')
    const g = mockFetch(201, { batch_id: null, tasks: [], skipped: [] })
    await api.createTask({ source: 'manual', path: '/a.mkv' })
    expect(g.mock.calls[0][1]?.headers).toEqual({ 'content-type': 'application/json' })
    expect(JSON.parse(String(g.mock.calls[0][1]?.body))).toEqual({ source: 'manual', path: '/a.mkv' })
  })

  it('throws ApiError with the server message', async () => {
    mockFetch(409, { error: { code: 'not_pending', message: '任务 #3 不在待确认状态' } })
    const e = await api.release(3).catch((e: unknown) => e)
    expect(e).toBeInstanceOf(ApiError)
    expect((e as ApiError).message).toBe('任务 #3 不在待确认状态')
  })

  it('escapes profile ids', async () => {
    const f = mockFetch(204, undefined)
    await api.deleteProfile('a b')
    expect(f.mock.calls[0][0]).toBe('api/profiles/a%20b')
  })
})
