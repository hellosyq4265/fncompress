import { afterEach, describe, expect, it, vi } from 'vitest'
import { defineComponent, h, nextTick, onUnmounted } from 'vue'
import { flushPromises, mount } from '@vue/test-utils'
import { createMemoryHistory, createRouter } from 'vue-router'
import { NDialogProvider, NMessageProvider } from 'naive-ui'

// No server in tests: the shell's SSE connection is a no-op.
vi.mock('../stores/live', () => ({
  startLive: () => {},
  connection: { value: 'open' },
  space: { value: { shortages: [], waiting_tasks: [] } },
}))

// The risk notice reads the server settings; it has its own test.
vi.mock('./RiskNotice.vue', () => ({ default: { render: () => null } }))

import AppShell from './AppShell.vue'

/** A matchMedia whose answer the test flips. */
function fakeMedia(initial: boolean) {
  const listeners = new Set<(e: { matches: boolean }) => void>()
  const media = {
    matches: initial,
    addEventListener: (_: string, fn: (e: { matches: boolean }) => void) => listeners.add(fn),
    removeEventListener: (_: string, fn: (e: { matches: boolean }) => void) => listeners.delete(fn),
  }
  vi.stubGlobal('matchMedia', () => media)
  return (matches: boolean) => {
    media.matches = matches
    listeners.forEach((fn) => fn({ matches }))
  }
}

afterEach(() => vi.unstubAllGlobals())

describe('AppShell', () => {
  it('keeps the page mounted when the width crosses the phone breakpoint', async () => {
    const setMobile = fakeMedia(false)
    let unmounted = 0
    const Page = defineComponent({
      setup() {
        onUnmounted(() => unmounted++)
        return () => h('div', { class: 'page' }, 'page body')
      },
    })
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/tasks', component: Page, meta: { title: '任务' } }],
    })
    await router.push('/tasks')
    const w = mount(
      defineComponent({ setup: () => () => h(NMessageProvider, () => h(NDialogProvider, () => h(AppShell))) }),
      { global: { plugins: [router] } },
    )
    await flushPromises()
    expect(w.find('.page').exists()).toBe(true)
    expect(w.find('aside').exists()).toBe(true)

    setMobile(true)
    await nextTick()
    expect(w.find('aside').exists()).toBe(false)
    expect(w.find('header').exists()).toBe(true)
    setMobile(false)
    await nextTick()
    expect(w.find('.page').exists()).toBe(true)
    expect(unmounted).toBe(0)
  })
})
