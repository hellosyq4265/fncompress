// Server state other than tasks: loaded when a page first needs it, then
// reloaded when the server says it changed (`config_changed`) or after a
// reconnect, so every open page stays current without polling.
import { ref, shallowRef, type Ref, type ShallowRef } from 'vue'
import { api } from '../api/client'
import type { MonitorList, SavedProfile, ServerSettings, Settings, System } from '../api/types'
import { errorMessage } from '../logic/errors'
import { addConfigListener, addMonitorListener, addPollListener, addResyncListener } from './live'

export interface Resource<T> {
  data: ShallowRef<T | null>
  error: Ref<string | null>
  loading: Ref<boolean>
  reload: () => Promise<void>
  /** Starts loading on first use; returns the resource. */
  use: () => Resource<T>
}

/** `poll`: also reload while the event stream is held back and the page polls. */
export function createResource<T>(load: () => Promise<T>, sections: string[], poll = false): Resource<T> {
  const data = shallowRef<T | null>(null)
  const error = ref<string | null>(null)
  const loading = ref(false)
  let started = false
  let generation = 0

  async function reload() {
    const mine = ++generation
    loading.value = true
    try {
      const v = await load()
      if (mine !== generation) return
      data.value = v
      error.value = null
    } catch (e) {
      if (mine === generation) error.value = errorMessage(e)
    } finally {
      if (mine === generation) loading.value = false
    }
  }

  addConfigListener((section) => {
    if (started && sections.includes(section)) void reload()
  })
  addResyncListener(() => {
    if (started) void reload()
  })
  if (poll) {
    addPollListener(() => {
      if (started) void reload()
    })
  }

  const resource: Resource<T> = {
    data,
    error,
    loading,
    reload,
    use() {
      if (!started) {
        started = true
        void reload()
      }
      return resource
    },
  }
  return resource
}

export const profiles = createResource<SavedProfile[]>(async () => (await api.profiles()).profiles, ['profiles'])
export const settings = createResource<Settings>(() => api.settings(), ['settings'])
// The master switch is a server setting, so settings changes reload it too.
// Its status (scanning, paused for space) changes on its own: polled.
export const monitors = createResource<MonitorList>(() => api.monitors(), ['monitors', 'settings'], true)
export const system = createResource<System>(() => api.system(), [])

addMonitorListener((status) => {
  if (monitors.data.value) monitors.data.value = { ...monitors.data.value, status, enabled: status.enabled }
})

/** Changes some server settings; the rest are sent back as they are. */
export async function saveServer(patch: Partial<ServerSettings>): Promise<Settings> {
  const current = settings.data.value ?? (await api.settings())
  const saved = await api.putSettings({ server: { ...current.server, ...patch } })
  settings.data.value = saved
  return saved
}
