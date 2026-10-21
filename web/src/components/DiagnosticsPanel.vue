<script setup lang="ts">
// Observation mode, verification statistics, the startup encoder trial and
// server facts: what is needed to judge whether the checks are calibrated.
import { computed, onMounted, ref } from 'vue'
import { NAlert, NButton, NCard, NDescriptions, NDescriptionsItem, NProgress, NSpace, NTable, NTag, useDialog, useMessage } from 'naive-ui'
import { api } from '../api/client'
import type { VerifyStats } from '../api/types'
import { errorMessage } from '../logic/errors'
import { ENCODER_LABEL, formatTime } from '../logic/format'
import { checkRows, observation } from '../logic/settings'
import { onResync } from '../stores/live'
import { settings, system } from '../stores/resources'

const message = useMessage()
const dialog = useDialog()
settings.use()
system.use()

const stats = ref<VerifyStats | null>(null)
const statsError = ref<string | null>(null)
const loading = ref(false)
async function loadStats() {
  loading.value = true
  // Every finished task can move the observation count, and none of them
  // sends a config event: read it again along with the statistics.
  void settings.reload()
  try {
    stats.value = await api.verifyStats()
    statsError.value = null
  } catch (e) {
    statsError.value = errorMessage(e)
  } finally {
    loading.value = false
  }
}
onMounted(loadStats)
onResync(loadStats)

const obs = computed(() => {
  const s = settings.data.value
  return s ? observation(s.observed, s.server.observe_first_n) : null
})
const rows = computed(() => (stats.value ? checkRows(stats.value) : []))
const encoders = computed(() => system.data.value?.encoders ?? [])
const tools = computed(() => {
  const s = system.data.value
  if (!s) return []
  return [
    { name: '评分 ffmpeg', path: s.scorer.ffmpeg, available: s.scorer.available, version: s.scorer.version, error: s.scorer.error },
    { name: 'dovi_tool', path: s.dovi.path, available: s.dovi.available, version: s.dovi.version, error: s.dovi.error },
  ]
})

function resetObservation() {
  dialog.warning({
    title: '重新开始观察？',
    content: '观察计数清零，接下来的文件又会全部进入待确认。',
    positiveText: '清零',
    negativeText: '取消',
    onPositiveClick: async () => {
      try {
        await api.resetObservation()
        message.success('观察计数已清零')
      } catch (e) {
        message.error(errorMessage(e))
      }
    },
  })
}
</script>

<template>
  <div class="wrap">
    <NCard title="观察模式" size="small" class="card">
      <template v-if="obs && settings.data.value">
        <NProgress type="line" :percentage="obs.percent" :status="obs.active ? 'info' : 'success'" :show-indicator="false" />
        <div class="line">{{ obs.text }}</div>
        <NButton size="small" secondary :disabled="settings.data.value.observed === 0" @click="resetObservation">
          观察计数清零
        </NButton>
      </template>
    </NCard>

    <NCard size="small" class="card">
      <template #header>校验统计</template>
      <template #header-extra><NButton size="small" quaternary :loading="loading" @click="loadStats">刷新</NButton></template>
      <NAlert v-if="statsError" type="error" :title="statsError" />
      <template v-else-if="stats">
        <div class="line">
          共 {{ stats.reports }} 份校验报告：全部通过 {{ stats.passed }}，有软性项 {{ stats.passed_with_warnings }}，未通过 {{ stats.rejected }}
        </div>
        <NTable v-if="rows.length" size="small" :single-line="false" class="table">
          <thead>
            <tr><th>检查项</th><th class="n">硬性</th><th class="n">软性</th><th class="n">记录</th></tr>
          </thead>
          <tbody>
            <tr v-for="r in rows" :key="r.check">
              <td>{{ r.label }} <span class="id">{{ r.check }}</span></td>
              <td class="n" :class="{ hard: r.hard }">{{ r.hard || '' }}</td>
              <td class="n" :class="{ soft: r.soft }">{{ r.soft || '' }}</td>
              <td class="n">{{ r.info || '' }}</td>
            </tr>
          </tbody>
        </NTable>
        <div v-else class="line dim">还没有触发过任何检查项。</div>
      </template>
    </NCard>

    <NCard title="编码器试编" size="small" class="card">
      <div v-if="encoders.length === 0" class="line dim">服务以 --no-encoder-probe 启动，没有试编；所有编码器都当作可用。</div>
      <NTable v-else size="small" :single-line="false">
        <tbody>
          <tr v-for="e in encoders" :key="e.encoder">
            <td>{{ ENCODER_LABEL[e.encoder] ?? e.encoder }}</td>
            <td>
              <NTag size="small" :type="e.available ? 'success' : 'default'" :bordered="false">
                {{ e.available ? `可用（${e.packets} 个包）` : '不可用' }}
              </NTag>
            </td>
            <td class="err">{{ e.error ?? '' }}</td>
          </tr>
        </tbody>
      </NTable>
      <div class="line dim">启动时每个编码器真实试编 1 秒，确实出包才算可用。核显编码器需要运行用户在 render 组里。</div>
    </NCard>

    <NCard title="外部工具" size="small" class="card">
      <NTable v-if="system.data.value" size="small" :single-line="false">
        <tbody>
          <tr v-for="t in tools" :key="t.name">
            <td>{{ t.name }}</td>
            <td><span class="mono">{{ t.path }}</span></td>
            <td>
              <NTag size="small" :type="t.available ? 'success' : 'warning'" :bordered="false">
                {{ t.available ? `可用（${t.version ?? '未知版本'}）` : '不可用' }}
              </NTag>
            </td>
            <td class="err">{{ t.error ?? '' }}</td>
          </tr>
        </tbody>
      </NTable>
      <div class="line dim">评分 ffmpeg 用于样片推荐；dovi_tool 是杜比视界片源转码所需，不可用时这类片源会被保护性跳过。</div>
    </NCard>

    <NCard title="系统" size="small" class="card">
      <NDescriptions v-if="system.data.value" :column="1" label-placement="left" size="small">
        <NDescriptionsItem label="版本">{{ system.data.value.version }}</NDescriptionsItem>
        <NDescriptionsItem label="数据目录"><span class="mono">{{ system.data.value.data_dir }}</span></NDescriptionsItem>
        <NDescriptionsItem label="启动时间">{{ formatTime(system.data.value.started_at) }}</NDescriptionsItem>
        <NDescriptionsItem label="运行中的任务">
          <NSpace v-if="system.data.value.running_tasks.length" :size="4">
            <span v-for="id in system.data.value.running_tasks" :key="id">#{{ id }}</span>
          </NSpace>
          <span v-else>无</span>
        </NDescriptionsItem>
      </NDescriptions>
      <div class="line dim">API 文档：<a href="api/openapi.json" target="_blank">api/openapi.json</a></div>
    </NCard>
  </div>
</template>

<style scoped>
.wrap {
  max-width: 820px;
}
.card {
  margin-bottom: 12px;
}
.line {
  margin: 8px 0;
  font-size: 13px;
}
.dim {
  opacity: 0.65;
}
.table {
  margin-top: 8px;
}
.n {
  width: 64px;
  text-align: right;
}
.hard {
  color: #d03050;
  font-weight: 600;
}
.soft {
  color: #f0a020;
}
.id {
  font-size: 11px;
  opacity: 0.5;
  font-family: ui-monospace, monospace;
}
.err {
  font-size: 12px;
  opacity: 0.7;
  word-break: break-all;
}
.mono {
  font-family: ui-monospace, monospace;
  font-size: 12px;
  word-break: break-all;
}
</style>
