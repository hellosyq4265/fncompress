import { describe, expect, it, vi } from 'vitest'
import { defineComponent, h } from 'vue'
import { flushPromises, mount } from '@vue/test-utils'
import { NMessageProvider, NModal } from 'naive-ui'
import type { Settings } from '../api/types'

const state = vi.hoisted(() => ({ acknowledged: false, acks: 0 }))

vi.mock('../stores/live', () => ({
  addConfigListener: () => {},
  addMonitorListener: () => {},
  addPollListener: () => {},
  addResyncListener: () => {},
}))
vi.mock('../api/client', () => {
  const settings = () =>
    ({
      global: { recycle_retention_days: 14 },
      server: { observe_first_n: 20 },
      observed: 0,
      risk_acknowledged: state.acknowledged,
    }) as unknown as Settings
  return {
    api: {
      settings: async () => settings(),
      acknowledgeRisk: async () => {
        state.acks++
        state.acknowledged = true
        return settings()
      },
    },
  }
})

import RiskNotice from './RiskNotice.vue'

const button = () =>
  [...document.body.querySelectorAll('button')].find((b) => b.textContent?.includes('我已了解，开始使用'))

describe('RiskNotice', () => {
  it('blocks the app until the risk is acknowledged on the server', async () => {
    const w = mount(defineComponent({ setup: () => () => h(NMessageProvider, () => h(RiskNotice)) }), {
      attachTo: document.body,
    })
    await flushPromises()
    expect(w.findComponent(NModal).props('show')).toBe(true)
    expect(document.body.textContent).toContain('不要用在重要的、没有备份的视频上')
    expect(document.body.textContent).toContain('接下来 20 个文件')
    expect(button()?.disabled).toBe(true)

    document.body.querySelector<HTMLElement>('.n-checkbox')!.click()
    await flushPromises()
    expect(button()?.disabled).toBe(false)
    button()!.click()
    await flushPromises()
    expect(state.acks).toBe(1)
    expect(w.findComponent(NModal).props('show')).toBe(false)
    w.unmount()
  })
})
