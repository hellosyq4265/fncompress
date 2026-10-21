<script setup lang="ts">
// The buttons a task offers; `only` limits them (phones show cancel only).
import { computed } from 'vue'
import { NButton, NSpace } from 'naive-ui'
import type { Task } from '../api/types'
import { actionsFor } from '../logic/tasks'
import { useTaskActions, type TaskAction } from '../composables/useTaskActions'

const props = defineProps<{ task: Task; only?: TaskAction[]; size?: 'tiny' | 'small' | 'medium' }>()
const { act, isBusy, isLocked } = useTaskActions()

const LABEL: Record<TaskAction, string> = {
  choose: '选择候选',
  release: '放行',
  discard: '丢弃',
  cancel: '取消',
  retry: '重试',
  remove: '删除记录',
}
/** Shown on the button while its request runs (moving files can take a moment). */
const RUNNING: Partial<Record<TaskAction, string>> = {
  release: '放行中…',
  discard: '丢弃中…',
  cancel: '取消中…',
}
const TYPE: Record<TaskAction, 'primary' | 'default' | 'error'> = {
  choose: 'primary',
  release: 'primary',
  discard: 'default',
  cancel: 'default',
  retry: 'default',
  remove: 'default',
}

const shown = computed(() => {
  const allowed = actionsFor(props.task)
  return (Object.keys(LABEL) as TaskAction[]).filter((a) => allowed[a] && (!props.only || props.only.includes(a)))
})
</script>

<template>
  <NSpace v-if="shown.length" :size="8" :wrap-item="false">
    <NButton
      v-for="a in shown"
      :key="a"
      :size="size ?? 'small'"
      :type="TYPE[a]"
      :secondary="TYPE[a] === 'default'"
      :loading="isBusy(task, a)"
      :disabled="isLocked(task) && !isBusy(task, a)"
      @click.stop="act(task, a)"
    >
      {{ isBusy(task, a) && RUNNING[a] ? RUNNING[a] : a === 'discard' && task.type === 'recommend' ? '放弃' : LABEL[a] }}
    </NButton>
  </NSpace>
</template>
