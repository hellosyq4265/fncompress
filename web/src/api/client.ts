// Thin typed wrapper over fetch. URLs are relative ("api/..."), so the app
// also works behind a path prefix (fnOS iframe); non-2xx answers throw
// ApiError with the server's Chinese message.
import { errorFromBody } from '../logic/errors'
import type * as T from './types'

type Method = 'GET' | 'POST' | 'PUT' | 'DELETE'

export async function request<R>(method: Method, path: string, body?: unknown): Promise<R> {
  const res = await fetch(path, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!res.ok) throw errorFromBody(res.status, await res.text())
  if (res.status === 204) return undefined as R
  return (await res.json()) as R
}

/** `?a=1&b=x`, skipping undefined values; '' when nothing is left. */
export function query(params: Record<string, string | number | boolean | undefined | null>): string {
  const q = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') q.set(k, String(v))
  }
  const s = q.toString()
  return s ? `?${s}` : ''
}

export interface TaskQuery {
  state?: T.TaskState[]
  source?: T.TaskSource
  batch_id?: number
  before?: number
  limit?: number
}

const get = <R>(path: string) => request<R>('GET', path)
const post = <R>(path: string, body?: unknown) => request<R>('POST', path, body)
const put = <R>(path: string, body: unknown) => request<R>('PUT', path, body)
const del = (path: string) => request<void>('DELETE', path)

export const api = {
  tasks: (q: TaskQuery = {}) =>
    get<{ tasks: T.Task[] }>(`api/tasks${query({ ...q, state: q.state?.join(',') })}`),
  task: (id: number) => get<T.Task>(`api/tasks/${id}`),
  createTask: (body: T.CreateTask) => post<T.Created>('api/tasks', body),
  preview: (body: T.PreviewRequest) => post<T.Preview>('api/tasks/preview', body),
  previewBatch: (body: T.BatchRequest) => post<T.BatchPreview>('api/tasks/preview-batch', body),
  cancel: (id: number) => post<T.Task>(`api/tasks/${id}/cancel`),
  retry: (id: number) => post<T.Task>(`api/tasks/${id}/retry`),
  release: (id: number) => post<T.Task>(`api/tasks/${id}/release`),
  discard: (id: number) => post<T.Task>(`api/tasks/${id}/discard`),
  choose: (id: number, body: T.ChooseRequest) => post<T.ChooseResult>(`api/tasks/${id}/choose`, body),
  deleteTask: (id: number) => del(`api/tasks/${id}`),

  settings: () => get<T.Settings>('api/settings'),
  putSettings: (u: T.SettingsUpdate) => put<T.Settings>('api/settings', u),
  resetObservation: () => post<T.Settings>('api/settings/observation/reset'),
  acknowledgeRisk: () => post<T.Settings>('api/settings/risk/acknowledge'),

  profiles: () => get<{ profiles: T.SavedProfile[] }>('api/profiles'),
  createProfile: (p: T.SavedProfile) => post<T.SavedProfile>('api/profiles', p),
  putProfile: (id: string, u: T.ProfileUpdate) =>
    put<T.SavedProfile>(`api/profiles/${encodeURIComponent(id)}`, u),
  deleteProfile: (id: string) => del(`api/profiles/${encodeURIComponent(id)}`),

  monitors: () => get<T.MonitorList>('api/monitors'),
  createMonitor: (m: T.MonitorInput) => post<T.MonitorDir>('api/monitors', m),
  putMonitor: (id: number, m: T.MonitorInput) => put<T.MonitorDir>(`api/monitors/${id}`, m),
  deleteMonitor: (id: number) => del(`api/monitors/${id}`),
  scanMonitors: () => post<T.MonitorStatus>('api/monitors/scan'),

  attempts: (q: { path_prefix?: string; limit?: number } = {}) =>
    get<{ attempts: T.Attempt[] }>(`api/attempts${query(q)}`),
  deleteAttempt: (id: number) => del(`api/attempts/${id}`),
  recycle: () => get<T.RecycleList>('api/recycle'),
  emptyRecycle: () => post<{ removed: unknown[] }>('api/recycle/empty'),
  verifyStats: () => get<T.VerifyStats>('api/verify-stats'),
  system: () => get<T.System>('api/system'),
  /** Directory listing for path pickers; the volumes without a path. */
  fs: (path?: string) => get<T.Listing>(`api/fs${query({ path })}`),
}
