// Task buttons: confirm what cannot be undone, call the API, show the
// server's message on failure. The list updates from SSE events; a task the
// server answers with is applied at once too, so a card never waits on an
// event that is late (a buffering proxy) or lost.
import { reactive } from 'vue'
import { useDialog, useMessage } from 'naive-ui'
import { api } from '../api/client'
import type { Task } from '../api/types'
import { errorMessage, isApiError } from '../logic/errors'
import { baseName } from '../logic/format'
import { taskEvent, tasks } from '../stores/tasks'

export type TaskAction = 'cancel' | 'retry' | 'remove' | 'release' | 'discard' | 'choose'

/**
 * The action in flight per task, shared by every card and detail view that
 * shows it: while one runs, none of that task's buttons can be pressed.
 */
const inFlight = reactive(new Map<number, TaskAction>())

/** Errors meaning the task moved on before the click reached the server. */
const STALE = ['not_pending', 'not_awaiting_choice', 'busy', 'task_active', 'not_found']

function applied(t: Task): void {
  taskEvent({ event: 'task_updated', task: t })
}

/** Re-reads a task the UI showed in an old state. */
async function refresh(id: number): Promise<void> {
  try {
    applied(await api.task(id))
  } catch (e) {
    if (isApiError(e, 'not_found')) tasks.delete(id)
  }
}

export function useTaskActions() {
  const dialog = useDialog()
  const message = useMessage()

  async function run(t: Task, action: TaskAction): Promise<void> {
    if (inFlight.has(t.id)) return
    inFlight.set(t.id, action)
    try {
      switch (action) {
        case 'cancel':
          applied(await api.cancel(t.id))
          message.success(t.state === 'running' ? '正在停止编码' : '已取消')
          break
        case 'retry': {
          const created = await api.retry(t.id)
          taskEvent({ event: 'task_created', task: created })
          message.success(`已重新排队（任务 #${created.id}）`)
          break
        }
        case 'remove':
          await api.deleteTask(t.id)
          // The server also sends task_deleted; removing now avoids a flicker.
          tasks.delete(t.id)
          break
        case 'release':
          applied(await api.release(t.id))
          message.success('已放行，成品替换了源片')
          break
        case 'discard':
          applied(await api.discard(t.id))
          message.success(t.type === 'recommend' ? '已放弃，没有保存方案' : '成品已丢弃')
          break
        case 'choose':
          // Hash routing; also works where no router is installed (card tests).
          window.location.hash = `#/recommend?task=${t.id}`
          break
      }
    } catch (e) {
      message.error(errorMessage(e), { duration: 6000 })
      if (STALE.some((code) => isApiError(e, code))) await refresh(t.id)
    } finally {
      inFlight.delete(t.id)
    }
  }

  function confirm(title: string, content: string, positiveText: string, danger: boolean, then: () => Promise<void>) {
    dialog[danger ? 'warning' : 'info']({
      title,
      content,
      positiveText,
      negativeText: '再想想',
      onPositiveClick: () => {
        void then()
      },
    })
  }

  /** Asks first where the action stops work or touches files. */
  function act(t: Task, action: TaskAction): void {
    if (inFlight.has(t.id)) return
    const name = baseName(t.path)
    switch (action) {
      case 'cancel':
        if (t.state !== 'running') return void run(t, action)
        if (t.type === 'recommend') {
          return confirm('停止推荐？', `${name} 的试编码和评分会停止，已有的结果不保留。`, '停止', true, () =>
            run(t, action),
          )
        }
        return confirm('停止编码？', `${name} 正在编码，停止后半成品会被删除，源片不变。`, '停止', true, () =>
          run(t, action),
        )
      case 'release': {
        const days = t.snapshot.global?.recycle_retention_days
        const recycle = days ? `源片移入回收目录，保留 ${days} 天。` : '源片移入回收目录。'
        return confirm('放行这个成品？', `成品会替换 ${name}。${recycle}`, '放行', false, () => run(t, action))
      }
      case 'discard':
        if (t.type === 'recommend') {
          return confirm('放弃这次推荐？', `不保存任何方案；自动监控随后照常处理 ${name}。`, '放弃', true, () =>
            run(t, action),
          )
        }
        return confirm('丢弃这个成品？', `删除压缩结果，${name} 保持不变。`, '丢弃', true, () => run(t, action))
      default:
        return void run(t, action)
    }
  }

  const isBusy = (t: Task, action: TaskAction) => inFlight.get(t.id) === action
  /** Any action of this task is in flight. */
  const isLocked = (t: Task) => inFlight.has(t.id)

  return { act, isBusy, isLocked }
}
