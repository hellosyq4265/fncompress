<script setup lang="ts">
// One recommendation, live from the task store: progress while it runs,
// the samples and every trial as they are measured, how the compared
// presets did, then the preset and quality to save as a profile
// (optionally transcoding the source right away).
import { computed, ref, watch } from 'vue'
import {
  NA,
  NAlert,
  NButton,
  NCheckbox,
  NDivider,
  NForm,
  NFormItem,
  NInput,
  NProgress,
  NRadio,
  NRadioGroup,
  NSpace,
  NTable,
  NTag,
  useDialog,
  useMessage,
} from 'naive-ui'
import { api } from '../api/client'
import type { Task } from '../api/types'
import { errorMessage } from '../logic/errors'
import { taskEvent } from '../stores/tasks'
import { ENCODER_LABEL, STATE_LABEL, STATE_TAG, baseName, formatUntil, outcomeSummary, progressView } from '../logic/format'
import {
  SCORING_LABEL,
  candidateViews,
  defaultChoice,
  formatBitrate,
  presetRows,
  sampleWhere,
  suggestedPreset,
  trialRows,
} from '../logic/recommend'
import TaskActions from './TaskActions.vue'

const props = defineProps<{ task: Task }>()

const message = useMessage()
const dialog = useDialog()

const t = computed(() => props.task)
const rec = computed(() => t.value.recommendation ?? null)
const progress = computed(() => progressView(t.value.progress, rec.value))
const rows = computed(() => (rec.value ? trialRows(rec.value) : []))
const presets = computed(() => (rec.value ? presetRows(rec.value) : []))
// More than one preset: the trial table names each trial's preset.
const compared = computed(() => presets.value.length > 1)
const setup = computed(() => {
  const r = rec.value
  const p = t.value.snapshot.profile
  if (!r) return p ? `${ENCODER_LABEL[p.encoder] ?? p.encoder}` : ''
  const parts = [ENCODER_LABEL[r.encoder] ?? r.encoder]
  const names = presets.value.map((p) => p.preset).filter((p): p is string => p !== null)
  if (names.length > 1) parts.push(`对比预设 ${names.join(' / ')}`)
  else if (names.length === 1) parts.push(`预设 ${names[0]}`)
  parts.push(`输出 ${r.width}×${r.height}`, `质量 ${r.range[0]}–${r.range[1]} 内搜索`, `模型 ${r.model}`)
  if (r.scoring && r.scoring !== 'direct') parts.push(`评分：${SCORING_LABEL[r.scoring]}`)
  return parts.join(' · ')
})

// The choice form: a preset, then one of its qualities; id and name follow
// the candidate until edited.
const preset = ref<string | null>(null)
const candidates = computed(() => (rec.value ? candidateViews(rec.value, preset.value) : []))
const quality = ref<number | null>(null)
const profileId = ref('')
const name = ref('')
const transcode = ref(true)
const edited = ref(false)
const saving = ref(false)

watch(
  () => (t.value.state === 'awaiting_choice' && rec.value ? suggestedPreset(rec.value) : undefined),
  (suggested) => {
    const ok = presets.value.some((p) => p.preset === preset.value && p.quality !== null)
    if (suggested !== undefined && !ok) preset.value = suggested
  },
  { immediate: true },
)
function pickPreset(p: (typeof presets.value)[number]) {
  if (t.value.state === 'awaiting_choice' && p.quality !== null) preset.value = p.preset
}
watch(
  candidates,
  (list) => {
    if (quality.value === null || !list.some((c) => c.quality === quality.value)) {
      quality.value = list[0]?.quality ?? null
    }
  },
  { immediate: true },
)
watch(
  [preset, quality, () => t.value.id],
  ([p, q]) => {
    if (q === null || edited.value) return
    const d = defaultChoice(t.value, p, q)
    profileId.value = d.id
    name.value = d.name
  },
  { immediate: true },
)

async function save() {
  if (quality.value === null) return
  saving.value = true
  try {
    const r = await api.choose(t.value.id, {
      preset: preset.value,
      quality: quality.value,
      profile_id: profileId.value.trim() || null,
      name: name.value.trim() || null,
      transcode: transcode.value,
    })
    // Applied at once; the SSE events may lag behind a proxy.
    taskEvent({ event: 'task_updated', task: r.task })
    if (r.transcode) taskEvent({ event: 'task_created', task: r.transcode })
    if (r.transcode) {
      message.success(`已保存为配置方案「${r.profile.name}」，转码任务 #${r.transcode.id} 已加入队列`)
    } else if (r.transcode_error) {
      message.warning(`已保存为配置方案「${r.profile.name}」，但转码没有加入队列：${r.transcode_error}`, {
        duration: 8000,
      })
    } else {
      message.success(`已保存为配置方案「${r.profile.name}」`)
    }
  } catch (e) {
    message.error(errorMessage(e), { duration: 6000 })
  } finally {
    saving.value = false
  }
}

function giveUp() {
  dialog.warning({
    title: '放弃这次推荐？',
    content: `不保存任何方案；自动监控随后照常处理 ${baseName(t.value.path)}。`,
    positiveText: '放弃',
    negativeText: '再想想',
    onPositiveClick: async () => {
      try {
        taskEvent({ event: 'task_updated', task: await api.discard(t.value.id) })
        message.success('已放弃，没有保存方案')
      } catch (e) {
        message.error(errorMessage(e), { duration: 6000 })
      }
    },
  })
}
</script>

<template>
  <div class="result">
    <div class="head">
      <span class="name" :title="t.path">{{ baseName(t.path) }}</span>
      <NTag :type="STATE_TAG[t.state]" :bordered="false">{{ STATE_LABEL[t.state] }}</NTag>
      <TaskActions :task="t" :only="['cancel', 'retry', 'remove']" />
    </div>
    <div class="dim path">{{ t.path }}</div>
    <div class="dim">{{ setup }}<template v-if="rec"> · 目标 VMAF {{ rec.target_vmaf }}</template></div>

    <template v-if="t.state === 'running'">
      <NProgress
        type="line"
        :percentage="progress.percent ?? 0"
        :processing="progress.percent === null"
        :show-indicator="progress.percent !== null"
        class="bar"
      >
        {{ progress.percent?.toFixed(0) }}%
      </NProgress>
      <div class="stage">{{ progress.text }}</div>
    </template>
    <NAlert v-else-if="t.state === 'queued'" type="info" :title="outcomeSummary(t)" class="block" />
    <NAlert
      v-else-if="t.state === 'failed' || t.state === 'skipped' || t.state === 'cancelled'"
      :type="t.state === 'failed' ? 'error' : 'warning'"
      :title="outcomeSummary(t)"
      class="block"
    />
    <NAlert v-else-if="t.state === 'chosen' && t.choice" type="success" class="block">
      <template #header>
        已保存为配置方案 {{ t.choice.profile_id }}（<template v-if="t.choice.preset">预设 {{ t.choice.preset }}，</template>质量
        {{ t.choice.quality }}）
      </template>
      可以在<NA href="#/profiles">配置方案</NA>里查看，或给监控目录换用它。
      <template v-if="t.choice.transcode_task">
        原片的转码：<NA :href="`#/tasks?task=${t.choice.transcode_task}`">任务 #{{ t.choice.transcode_task }}</NA>。
      </template>
    </NAlert>
    <NAlert v-else-if="t.state === 'discarded' || t.state === 'expired'" type="default" :title="outcomeSummary(t)" class="block" />

    <template v-if="rec && rec.samples.length">
      <NDivider title-placement="left">样片（{{ rec.samples.length }}）</NDivider>
      <NTable size="small" :single-line="false" class="table">
        <thead>
          <tr>
            <th>#</th>
            <th>位置</th>
            <th>原码率</th>
            <th>为什么选它</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(s, i) in rec.samples" :key="i">
            <td>{{ i + 1 }}</td>
            <td>{{ sampleWhere(s) }}</td>
            <td>{{ formatBitrate(s.bitrate_bps) }}</td>
            <td>{{ s.reason }}</td>
          </tr>
        </tbody>
      </NTable>
    </template>

    <template v-if="rec && presets.length && (compared || rows.length)">
      <NDivider title-placement="left">预设对比</NDivider>
      <NTable size="small" :single-line="false" class="table">
        <thead>
          <tr>
            <th v-if="t.state === 'awaiting_choice'"></th>
            <th>预设</th>
            <th>最省的质量</th>
            <th>最低样片分</th>
            <th>体积</th>
            <th>视频码率</th>
            <th>编码速度</th>
            <th>整片转码</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="p in presets"
            :key="p.name"
            :class="{ off: p.status === null && !p.front, pick: t.state === 'awaiting_choice' && p.quality !== null }"
            @click="pickPreset(p)"
          >
            <td v-if="t.state === 'awaiting_choice'">
              <NRadio :checked="preset === p.preset" :disabled="p.quality === null" />
            </td>
            <td>{{ p.name }}</td>
            <td>{{ p.quality ?? '—' }}</td>
            <td>{{ p.min }}</td>
            <td>{{ p.size }}</td>
            <td>{{ p.bitrate }}</td>
            <td>{{ p.speed }}</td>
            <td :class="{ low: p.overTimeout }">{{ p.full }}</td>
            <td>
              <NSpace :size="4" :wrap="false">
                <NTag v-if="p.status" size="small" :bordered="false" :type="p.status === '进行中' ? 'info' : 'default'">
                  {{ p.status }}
                </NTag>
                <NTag v-if="p.front && compared" size="small" :bordered="false" type="success">前沿</NTag>
                <NTag v-if="p.overTimeout" size="small" :bordered="false" type="error">会超时</NTag>
              </NSpace>
            </td>
          </tr>
        </tbody>
      </NTable>
      <div class="hint">
        每个预设各自搜索达到目标的最省质量值。体积是试编码相对原样片的视频大小；编码速度是样片的视频秒数除以编码耗时，整片转码按这个速度推算。<template
          v-if="compared"
          >「前沿」：没有另一个预设又快又小；其余的淡化显示。</template
        >速度受同时在跑的其他任务影响。<template v-if="presets.some((p) => p.overTimeout)">
          「会超时」：按推算的耗时，转码会超过编码超时（片长 × {{ t.snapshot.global.encode_timeout_factor }} + 30 分钟）而失败。</template
        >
      </div>
    </template>

    <template v-if="rec && rows.length">
      <NDivider title-placement="left">试编码</NDivider>
      <NTable size="small" :single-line="false" class="table">
        <thead>
          <tr>
            <th v-if="compared">预设</th>
            <th>质量</th>
            <th v-for="(_, i) in rec.samples" :key="i">样片 {{ i + 1 }}</th>
            <th>最低样片分</th>
            <th>体积</th>
            <th>结论</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="r in rows" :key="`${r.preset}-${r.quality}`">
            <td v-if="compared">{{ r.preset }}</td>
            <td>{{ r.quality }}</td>
            <td v-for="(s, i) in r.scores" :key="i" :class="{ low: s.low }">{{ s.text }}</td>
            <td>{{ r.min }}</td>
            <td>{{ r.size }}</td>
            <td>
              <NTag size="small" :bordered="false" :type="r.status === '达标' ? 'success' : r.status === '进行中' ? 'info' : 'default'">
                {{ r.status }}
              </NTag>
            </td>
          </tr>
        </tbody>
      </NTable>
      <div class="hint">
        体积是试编码结果相对原样片的视频大小。困难的样片先测；一个样片低于目标，这组就不再测其余样片（显示为 —）。<template
          v-if="compared"
          >后面的预设从前一个预设找到的质量值附近开始搜索。</template
        >
      </div>
    </template>

    <template v-if="t.state === 'awaiting_choice' && candidates.length">
      <NDivider title-placement="left">
        选择候选<template v-if="preset !== null">（预设 {{ preset }}<template v-if="compared">，在上表换预设</template>）</template>
      </NDivider>
      <NRadioGroup v-model:value="quality" class="candidates">
        <NSpace vertical :size="10">
          <NRadio v-for="c in candidates" :key="c.quality" :value="c.quality">
            <div class="cand-title">{{ c.title }}</div>
            <div class="cand-detail">{{ c.detail }}</div>
          </NRadio>
        </NSpace>
      </NRadioGroup>
      <NForm label-placement="left" label-width="auto" :show-feedback="false" class="choose">
        <NFormItem label="方案名称">
          <NInput v-model:value="name" @update:value="edited = true" />
        </NFormItem>
        <NFormItem label="方案 id">
          <NInput v-model:value="profileId" placeholder="字母、数字、- 和 _" @update:value="edited = true" />
        </NFormItem>
        <NFormItem label=" ">
          <NCheckbox v-model:checked="transcode">立即用此方案转码原片</NCheckbox>
        </NFormItem>
      </NForm>
      <NSpace :size="10" class="block">
        <NButton type="primary" :loading="saving" :disabled="quality === null" @click="save">保存为配置方案</NButton>
        <NButton @click="giveUp">放弃</NButton>
      </NSpace>
      <div class="hint">
        结论只覆盖这几个样片，不是整片的画质保证。等待选择期间，自动监控暂缓处理这个原片<template v-if="t.pending_until">；{{ formatUntil(t.pending_until) }}没有选择会自动结束</template>。
      </div>
    </template>
  </div>
</template>

<style scoped>
.head {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.name {
  font-size: 16px;
  font-weight: 600;
  word-break: break-all;
}
.dim {
  margin-top: 4px;
  font-size: 13px;
  opacity: 0.7;
}
.path {
  font-family: ui-monospace, monospace;
  font-size: 12px;
  word-break: break-all;
}
.bar {
  margin-top: 14px;
}
.stage {
  margin-top: 6px;
  font-size: 13px;
}
.block {
  margin-top: 14px;
}
.table {
  font-size: 13px;
}
.low {
  color: #d03050;
  font-weight: 600;
}
.off {
  opacity: 0.5;
}
.pick {
  cursor: pointer;
}
.hint {
  margin-top: 8px;
  font-size: 12px;
  opacity: 0.65;
}
.cand-title {
  font-weight: 600;
}
.cand-detail {
  font-size: 13px;
  opacity: 0.8;
}
.choose {
  margin-top: 16px;
  max-width: 520px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
</style>
