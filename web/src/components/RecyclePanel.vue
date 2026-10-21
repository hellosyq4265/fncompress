<script setup lang="ts">
// Replaced sources in the recycle directories (one-click empty) and the
// attempt records that keep monitoring from retrying a file.
import { computed, onMounted, ref } from 'vue'
import { NAlert, NButton, NCard, NCollapse, NCollapseItem, NEmpty, NInput, NTag, useDialog, useMessage } from 'naive-ui'
import { api } from '../api/client'
import type { Attempt, RecycleList } from '../api/types'
import { errorMessage } from '../logic/errors'
import { baseName, formatBytes, formatTime } from '../logic/format'
import { ATTEMPT_LABEL } from '../logic/preview'
import { recycleTotals } from '../logic/settings'
import { onResync } from '../stores/live'
import { settings } from '../stores/resources'

const message = useMessage()
const dialog = useDialog()
settings.use()

const recycle = ref<RecycleList | null>(null)
const recycleError = ref<string | null>(null)
const attempts = ref<Attempt[] | null>(null)
const attemptsError = ref<string | null>(null)
const prefix = ref('')

async function loadRecycle() {
  try {
    recycle.value = await api.recycle()
    recycleError.value = null
  } catch (e) {
    recycleError.value = errorMessage(e)
  }
}
async function loadAttempts() {
  try {
    attempts.value = (await api.attempts({ path_prefix: prefix.value.trim() || undefined, limit: 500 })).attempts
    attemptsError.value = null
  } catch (e) {
    attemptsError.value = errorMessage(e)
  }
}
const loadAll = () => Promise.all([loadRecycle(), loadAttempts()])
onMounted(loadAll)
onResync(loadAll)

const totals = computed(() => (recycle.value ? recycleTotals(recycle.value) : { files: 0, bytes: 0 }))
const retention = computed(() => settings.data.value?.global.recycle_retention_days)

function empty() {
  dialog.warning({
    title: '清空所有回收目录？',
    content: `将永久删除 ${totals.value.files} 个被替换的源片（${formatBytes(totals.value.bytes)}），无法恢复。`,
    positiveText: '永久删除',
    negativeText: '取消',
    onPositiveClick: async () => {
      try {
        const r = await api.emptyRecycle()
        message.success(`已删除 ${r.removed.length} 个文件`)
        await loadRecycle()
      } catch (e) {
        message.error(errorMessage(e), { duration: 6000 })
      }
    },
  })
}

async function forget(a: Attempt) {
  try {
    await api.deleteAttempt(a.id)
    attempts.value = attempts.value?.filter((x) => x.id !== a.id) ?? null
    message.success('已删除；监控会重新考虑这个文件')
  } catch (e) {
    message.error(errorMessage(e))
  }
}
</script>

<template>
  <div class="wrap">
    <NCard size="small" class="card">
      <template #header>回收目录</template>
      <template #header-extra>
        <NButton size="small" type="error" secondary :disabled="totals.files === 0" @click="empty">一键清空</NButton>
      </template>
      <NAlert v-if="recycleError" type="error" :title="recycleError" />
      <template v-else-if="recycle">
        <div class="line">
          {{ totals.files }} 个被替换的源片，共 {{ formatBytes(totals.bytes) }}<template v-if="retention">；保留 {{ retention }} 天后自动删除</template>
        </div>
        <NCollapse v-if="recycle.dirs.some((d) => d.entries.length)">
          <NCollapseItem v-for="d in recycle.dirs.filter((d) => d.entries.length)" :key="d.dir" :title="`${d.dir}（${d.entries.length}）`" :name="d.dir">
            <div v-for="e in d.entries" :key="e.id" class="entry">
              <span class="name" :title="e.original_path">{{ baseName(e.original_path) }}</span>
              <span class="dim">{{ formatBytes(e.identity.size) }} · {{ formatTime(e.recycled_at) }}</span>
            </div>
          </NCollapseItem>
        </NCollapse>
      </template>
    </NCard>

    <NCard size="small" class="card">
      <template #header>尝试记录</template>
      <div class="line dim">
        保护性跳过、校验未通过、收益不足的文件按「文件 + 方案设置」记录，自动监控不会再用同样的设置重试。删除一条记录，监控就会重新考虑那个文件。
      </div>
      <NInput v-model:value="prefix" clearable placeholder="只看这个目录下的文件，例如 /vol1/影视" class="filter" @keyup.enter="loadAttempts" @clear="loadAttempts" @blur="loadAttempts" />
      <NAlert v-if="attemptsError" type="error" :title="attemptsError" />
      <NEmpty v-else-if="attempts && attempts.length === 0" description="没有尝试记录" />
      <div v-else-if="attempts" class="attempts">
        <div v-for="a in attempts" :key="a.id" class="attempt">
          <div class="head">
            <NTag size="small" :bordered="false" :type="a.kind === 'rejected' ? 'error' : a.kind === 'skipped' ? 'warning' : 'default'">
              {{ ATTEMPT_LABEL[a.kind] }}
            </NTag>
            <span class="name" :title="a.path">{{ baseName(a.path) }}</span>
            <span class="dim">{{ formatTime(a.at) }}</span>
            <NButton size="tiny" quaternary @click="forget(a)">删除</NButton>
          </div>
          <div class="msg">{{ a.message }}</div>
          <div class="path">{{ a.path }}</div>
        </div>
      </div>
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
  margin: 4px 0 8px;
  font-size: 13px;
}
.dim {
  opacity: 0.65;
  font-size: 12px;
}
.entry {
  display: flex;
  gap: 12px;
  padding: 3px 0;
  font-size: 13px;
}
.name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.filter {
  margin-bottom: 10px;
}
.attempts {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.attempt .head {
  display: flex;
  align-items: center;
  gap: 8px;
}
.msg {
  font-size: 13px;
  margin-top: 2px;
}
.path {
  font-size: 11px;
  opacity: 0.5;
  font-family: ui-monospace, monospace;
  word-break: break-all;
}
</style>
