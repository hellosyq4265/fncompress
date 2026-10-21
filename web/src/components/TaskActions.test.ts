import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { defineComponent, h, nextTick } from 'vue'
import { flushPromises, mount } from '@vue/test-utils'
import { NDialogProvider, NMessageProvider } from 'naive-ui'
import type { Task } from '../api/types'
import { ApiError } from '../logic/errors'
import { tasks } from '../stores/tasks'
import { candidate, task } from '../test/fixtures'
import TaskActions from './TaskActions.vue'

const api = vi.hoisted(() => ({ release: vi.fn(), discard: vi.fn(), task: vi.fn() }))
vi.mock('../api/client', () => ({ api }))

function deferred<T>() {
  let resolve!: (v: T) => void
  const promise = new Promise<T>((r) => (resolve = r))
  return { promise, resolve }
}

const pending = (id: number): Task =>
  task(id, { state: 'pending', candidate: candidate(), outcome: { outcome: 'verified', candidate: candidate() } })

/** The card's buttons for the task as the store holds it. */
function mountActions(id: number) {
  const Wrapper = defineComponent({
    setup: () => () =>
      h(NMessageProvider, () =>
        h(NDialogProvider, () => (tasks.get(id) ? h(TaskActions, { task: tasks.get(id)! }) : null)),
      ),
  })
  return mount(Wrapper, { attachTo: document.body })
}

const button = (w: ReturnType<typeof mountActions>, text: string) => w.findAll('button').find((b) => b.text() === text)

/** Clicks the confirm dialog's positive button (teleported to the body). */
async function confirm(text: string) {
  await nextTick()
  const b = [...document.querySelectorAll<HTMLButtonElement>('.n-dialog button')].find((e) => e.textContent?.trim() === text)
  expect(b, `dialog button ${text}`).toBeTruthy()
  b!.click()
  await nextTick()
}

beforeEach(() => {
  tasks.clear()
  vi.resetAllMocks()
})
afterEach(() => {
  document.body.innerHTML = ''
})

describe('TaskActions', () => {
  it('shows releasing, locks the other button, then applies the answer without waiting for SSE', async () => {
    tasks.set(1, pending(1))
    const answer = deferred<Task>()
    api.release.mockReturnValue(answer.promise)
    const w = mountActions(1)

    await button(w, '放行')!.trigger('click')
    await confirm('放行')
    expect(button(w, '放行中…')).toBeTruthy()
    expect(button(w, '丢弃')!.attributes('disabled')).toBeDefined()

    // A second click while the request runs sends nothing.
    await button(w, '丢弃')!.trigger('click')
    expect(api.discard).not.toHaveBeenCalled()

    answer.resolve({ ...pending(1), state: 'released', candidate: null })
    await flushPromises()
    expect(api.release).toHaveBeenCalledTimes(1)
    expect(tasks.get(1)!.state).toBe('released')
    expect(w.findAll('button').map((b) => b.text())).not.toContain('放行')
  })

  it('re-reads a task that was already released elsewhere', async () => {
    tasks.set(2, pending(2))
    api.discard.mockRejectedValue(new ApiError(409, 'not_pending', '任务 #2 没有待确认成品'))
    api.task.mockResolvedValue({ ...pending(2), state: 'released', candidate: null })
    const w = mountActions(2)

    await button(w, '丢弃')!.trigger('click')
    await confirm('丢弃')
    await flushPromises()
    expect(api.task).toHaveBeenCalledWith(2)
    expect(tasks.get(2)!.state).toBe('released')
  })
})
