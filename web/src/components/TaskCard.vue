<script setup lang="ts">
import { computed } from 'vue'
import { NCard, NProgress, NTag } from 'naive-ui'
import type { Task } from '../api/types'
import {
  ENCODER_LABEL,
  SOURCE_LABEL,
  STATE_LABEL,
  STATE_TAG,
  baseName,
  dirName,
  formatUntil,
  outcomeSummary,
  progressView,
} from '../logic/format'
import { space } from '../stores/live'
import TaskActions from './TaskActions.vue'

const props = defineProps<{ task: Task; mobile?: boolean }>()
const emit = defineEmits<{ open: [id: number]; batch: [id: number] }>()

const t = computed(() => props.task)
const waitingForSpace = computed(() => t.value.state === 'queued' && space.value.waiting_tasks.includes(t.value.id))
const progress = computed(() => progressView(t.value.progress))
const encoder = computed(() => ENCODER_LABEL[t.value.snapshot.profile?.encoder] ?? t.value.snapshot.profile?.encoder)
</script>

<template>
  <NCard size="small" hoverable class="task-card" @click="emit('open', t.id)">
    <div class="row">
      <div class="name" :title="t.path">{{ baseName(t.path) }}</div>
      <NTag v-if="waitingForSpace" size="small" type="warning" :bordered="false">等待存储空间</NTag>
      <NTag size="small" :type="STATE_TAG[t.state]" :bordered="false">{{ STATE_LABEL[t.state] }}</NTag>
    </div>
    <div class="meta">
      <span v-if="!mobile" class="dir" :title="dirName(t.path)">{{ dirName(t.path) }}</span>
      <span>{{ t.type === 'recommend' ? '样片推荐' : SOURCE_LABEL[t.source] }}</span>
      <a v-if="t.batch_id" class="batch" @click.stop="emit('batch', t.batch_id)">批次 #{{ t.batch_id }}</a>
      <span v-if="!mobile && encoder">{{ encoder }}</span>
    </div>
    <template v-if="t.state === 'running'">
      <NProgress
        type="line"
        :percentage="progress.percent ?? 0"
        :show-indicator="progress.percent !== null"
        :processing="progress.percent === null"
        :height="6"
        class="bar"
      >
        {{ progress.percent?.toFixed(0) }}%
      </NProgress>
      <div class="summary">{{ progress.text }}</div>
    </template>
    <div v-else class="summary" :class="{ error: t.state === 'failed' }">
      {{ outcomeSummary(t) }}
      <span v-if="t.state === 'pending' && t.pending_until" class="until">
        · {{ formatUntil(t.pending_until) }}自动删除成品
      </span>
      <span v-if="t.state === 'awaiting_choice' && t.pending_until" class="until">
        · {{ formatUntil(t.pending_until) }}自动结束
      </span>
    </div>
    <div class="actions">
      <TaskActions :task="t" :only="mobile ? ['cancel'] : undefined" />
    </div>
  </NCard>
</template>

<style scoped>
.task-card {
  cursor: pointer;
}
.row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.name {
  flex: 1;
  min-width: 0;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.meta {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 12px;
  margin-top: 2px;
  font-size: 12px;
  opacity: 0.65;
}
.dir {
  max-width: 420px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.batch {
  text-decoration: underline;
  cursor: pointer;
}
.bar {
  margin-top: 8px;
}
.summary {
  margin-top: 6px;
  font-size: 13px;
  opacity: 0.85;
  word-break: break-all;
}
.summary.error {
  color: var(--n-color-target, #d03050);
  opacity: 1;
}
.until {
  opacity: 0.7;
}
.actions:not(:empty) {
  margin-top: 8px;
}
</style>
