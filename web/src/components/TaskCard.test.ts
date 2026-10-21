import { describe, expect, it } from 'vitest'
import { defineComponent, h } from 'vue'
import { mount } from '@vue/test-utils'
import { NDialogProvider, NMessageProvider } from 'naive-ui'
import type { Task } from '../api/types'
import { candidate, task } from '../test/fixtures'
import TaskCard from './TaskCard.vue'

// useDialog / useMessage need their providers around the card.
function mountCard(t: Task, mobile = false) {
  const Wrapper = defineComponent({
    setup: () => () =>
      h(NMessageProvider, () => h(NDialogProvider, () => h(TaskCard, { task: t, mobile }))),
  })
  return mount(Wrapper)
}

const buttons = (w: ReturnType<typeof mountCard>) => w.findAll('button').map((b) => b.text())

describe('TaskCard', () => {
  it('shows progress and cancel for a running task', () => {
    const w = mountCard(
      task(1, {
        state: 'running',
        progress: { id: 1, out_time_s: 60, duration_s: 600, percent: 10, speed: 2 },
      }),
    )
    expect(w.text()).toContain('film1.mkv')
    expect(w.text()).toContain('运行中')
    expect(w.text()).toContain('剩余 4:30')
    expect(buttons(w)).toEqual(['取消'])
  })

  it('offers release and discard for a pending output', () => {
    const w = mountCard(
      task(2, {
        state: 'pending',
        pending_reason: 'observation',
        pending_until: Math.floor(Date.now() / 1000) + 86400 * 14,
        candidate: candidate(),
        outcome: { outcome: 'verified', candidate: candidate() },
      }),
    )
    expect(buttons(w)).toEqual(['放行', '丢弃'])
    expect(w.text()).toContain('观察模式')
    expect(w.text()).toContain('14 天后自动删除成品')
  })

  it('offers retry and delete for finished tasks', () => {
    expect(buttons(mountCard(task(3, { state: 'failed', error: 'x' })))).toEqual(['重试', '删除记录'])
  })

  it('keeps only cancel on phones', () => {
    expect(buttons(mountCard(task(4, { state: 'queued' }), true))).toEqual(['取消'])
    expect(buttons(mountCard(task(5, { state: 'failed' }), true))).toEqual([])
    const pending = task(6, { state: 'pending', candidate: candidate() })
    expect(buttons(mountCard(pending, true))).toEqual([])
  })

  it('offers choosing or giving up a recommendation awaiting a choice', () => {
    const w = mountCard(
      task(8, {
        type: 'recommend',
        quota: 'cpu',
        state: 'awaiting_choice',
        pending_until: Math.floor(Date.now() / 1000) + 86400 * 14,
        recommendation: {
          target_vmaf: 95,
          encoder: 'hevc_qsv',
          preset: null,
          width: 1920,
          height: 1080,
          model: 'vmaf_v0.6.1',
          range: [14, 40],
          samples: [],
          trials: [],
          candidates: [
            { quality: 27, min_score: 95.4, scores: [95.4], video_bytes: 1, size_ratio: 0.4, bitrate_bps: 1 },
          ],
        },
      }),
    )
    expect(buttons(w)).toEqual(['选择候选', '放弃'])
    expect(w.text()).toContain('样片推荐')
    expect(w.text()).toContain('1 个候选达到 VMAF 95，等待选择')
    expect(w.text()).toContain('14 天后自动结束')
  })

  it('links the batch', async () => {
    const w = mountCard(task(7, { source: 'folder_batch', batch_id: 12 }))
    expect(w.text()).toContain('批次 #12')
  })
})
