<script setup lang="ts">
// Everything about one task, live from the store; opened from a card.
import { computed } from 'vue'
import { NA, NAlert, NDescriptions, NDescriptionsItem, NDivider, NDrawer, NDrawerContent, NProgress, NTag } from 'naive-ui'
import type { Candidate, Profile } from '../api/types'
import {
  ENCODER_LABEL,
  SOURCE_LABEL,
  STATE_LABEL,
  STATE_TAG,
  baseName,
  formatBytes,
  formatPct,
  formatTime,
  formatUntil,
  outcomeSummary,
  progressView,
} from '../logic/format'
import { reportOf } from '../logic/report'
import { tasks } from '../stores/tasks'
import ReportView from './ReportView.vue'
import RoutePlanView from './RoutePlanView.vue'
import TaskActions from './TaskActions.vue'

const props = defineProps<{ id: number | null; mobile?: boolean }>()
const emit = defineEmits<{ close: [] }>()

const t = computed(() => (props.id === null ? undefined : tasks.get(props.id)))
const report = computed(() => (t.value ? reportOf(t.value) : null))
const candidate = computed<Candidate | null>(() => {
  const o = t.value?.outcome
  if (t.value?.candidate) return t.value.candidate
  return o && (o.outcome === 'rejected' || o.outcome === 'verified') ? o.candidate : null
})
const savings = computed(() => {
  const o = t.value?.outcome
  if (candidate.value) return candidate.value.savings
  return o && (o.outcome === 'published' || o.outcome === 'insufficient_savings') ? o.savings : null
})
const published = computed(() => {
  const o = t.value?.outcome
  return o?.outcome === 'published' ? o.published : null
})
const progress = computed(() => progressView(t.value?.progress))

function profileText(p: Profile | undefined): string {
  if (!p) return ''
  const parts = [ENCODER_LABEL[p.encoder] ?? p.encoder, `质量 ${p.quality}`]
  if (p.preset) parts.push(`预设 ${p.preset}`)
  if (p.container !== 'keep') parts.push(`转为 ${p.container.toUpperCase()}`)
  if (p.resize !== 'keep') parts.push(`最高 ${p.resize.max_height}p`)
  return parts.join(' · ')
}

const PENDING_REASON = { rejected: '有硬性检查未通过', observation: '观察模式（校验通过，但前 N 个文件都要人工确认）' }
</script>

<template>
  <NDrawer :show="!!t" :width="mobile ? '100%' : 600" placement="right" @update:show="(v: boolean) => !v && emit('close')">
    <NDrawerContent v-if="t" :title="baseName(t.path)" closable :native-scrollbar="false">
      <div class="head">
        <NTag :type="STATE_TAG[t.state]" :bordered="false">{{ STATE_LABEL[t.state] }}</NTag>
        <span class="summary">{{ t.state === 'running' ? progress.text : outcomeSummary(t) }}</span>
      </div>
      <NProgress
        v-if="t.state === 'running'"
        type="line"
        :percentage="progress.percent ?? 0"
        :processing="progress.percent === null"
        :show-indicator="progress.percent !== null"
        style="margin: 8px 0"
      />
      <TaskActions :task="t" size="medium" class="actions" />
      <NAlert v-if="t.error && t.state !== 'failed'" type="error" :title="t.error" style="margin-top: 12px" />

      <NDescriptions :column="1" label-placement="left" size="small" class="desc">
        <NDescriptionsItem label="任务">#{{ t.id }} · {{ SOURCE_LABEL[t.source] }}<template v-if="t.batch_id"> · 批次 #{{ t.batch_id }}</template></NDescriptionsItem>
        <NDescriptionsItem label="源片"><span class="path">{{ t.path }}</span></NDescriptionsItem>
        <NDescriptionsItem label="方案">{{ t.profile_id }}（{{ profileText(t.snapshot.profile) }}）</NDescriptionsItem>
        <NDescriptionsItem v-if="t.type === 'recommend'" label="样片推荐">
          只搜索质量参数；<NA :href="`#/recommend?task=${t.id}`">在样片推荐页查看样片、试编码和候选</NA>
        </NDescriptionsItem>
        <NDescriptionsItem v-if="t.force" label="强制">已处理标记被忽略</NDescriptionsItem>
        <NDescriptionsItem label="时间">
          创建 {{ formatTime(t.created_at) }}<template v-if="t.started_at"> · 开始 {{ formatTime(t.started_at) }}</template
          ><template v-if="t.finished_at"> · 结束 {{ formatTime(t.finished_at) }}</template>
        </NDescriptionsItem>
        <NDescriptionsItem v-if="t.state === 'pending' && t.pending_reason" label="待确认">
          {{ PENDING_REASON[t.pending_reason] }}<template v-if="t.pending_until">；{{ formatUntil(t.pending_until) }}（{{ formatTime(t.pending_until) }}）自动删除成品</template>
        </NDescriptionsItem>
        <NDescriptionsItem v-if="savings" label="大小">
          {{ formatBytes(savings.source_size) }} → {{ formatBytes(savings.candidate_size) }}（节省 {{ formatPct(savings.saved_pct) }}）
        </NDescriptionsItem>
        <NDescriptionsItem v-if="candidate && t.state === 'pending'" label="成品"><span class="path">{{ candidate.path }}</span></NDescriptionsItem>
        <NDescriptionsItem v-if="published" label="已发布到"><span class="path">{{ published.target }}</span></NDescriptionsItem>
        <NDescriptionsItem v-if="published" label="源片移到"><span class="path">{{ published.recycled.file }}</span></NDescriptionsItem>
      </NDescriptions>

      <template v-if="candidate">
        <NDivider title-placement="left">路线</NDivider>
        <RoutePlanView :plan="candidate.plan" />
      </template>
      <template v-if="report">
        <NDivider title-placement="left">校验</NDivider>
        <ReportView :report="report" />
      </template>
    </NDrawerContent>
  </NDrawer>
</template>

<style scoped>
.head {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.summary {
  font-size: 14px;
}
.actions {
  margin-top: 12px;
}
.desc {
  margin-top: 16px;
}
.path {
  word-break: break-all;
  font-family: ui-monospace, monospace;
  font-size: 12px;
}
</style>
