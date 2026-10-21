<script setup lang="ts">
// Automatic monitoring: master switch, directories with their watch state,
// and the pending outputs waiting for a decision (decisions doc).
import { computed, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import {
  NAlert,
  NButton,
  NCard,
  NEmpty,
  NSpace,
  NSpin,
  NSwitch,
  NTab,
  NTabs,
  NTag,
  useDialog,
  useMessage,
} from 'naive-ui'
import { api } from '../api/client'
import type { MonitorDir } from '../api/types'
import MonitorDialog from '../components/MonitorDialog.vue'
import TaskCard from '../components/TaskCard.vue'
import TaskDetail from '../components/TaskDetail.vue'
import { errorMessage } from '../logic/errors'
import { formatTime } from '../logic/format'
import { monitorShortageLine } from '../logic/space'
import { tasksIn } from '../logic/tasks'
import { monitors, profiles, saveServer } from '../stores/resources'
import { tasks } from '../stores/tasks'
import { useIsMobile } from '../stores/ui'

const route = useRoute()
const router = useRouter()
const message = useMessage()
const dialog = useDialog()
const mobile = useIsMobile()

monitors.use()
profiles.use()

type Tab = 'dirs' | 'pending'
const tab = computed<Tab>(() => (route.query.tab === 'pending' ? 'pending' : 'dirs'))
const openId = computed(() => (route.query.task ? Number(route.query.task) : null))
function setQuery(q: Record<string, string | undefined>) {
  void router.replace({ query: { ...route.query, ...q } })
}

const data = computed(() => monitors.data.value)
const status = computed(() => data.value?.status)
const pending = computed(() => tasksIn(tasks.values(), 'pending'))
const profileName = (id: string) => profiles.data.value?.find((p) => p.id === id)?.name ?? id

function watchState(m: MonitorDir): { type: 'success' | 'warning' | 'default'; text: string; error?: string } {
  if (!data.value?.enabled) return { type: 'default', text: '总开关已关闭' }
  if (!m.enabled) return { type: 'default', text: '已停用' }
  const w = status.value?.dirs.find((d) => d.id === m.id)
  if (!w) return { type: 'default', text: '启动中' }
  if (w.watching) return { type: 'success', text: '监听中' }
  return { type: 'warning', text: '只靠定期扫描', error: w.error ?? undefined }
}

const switching = ref(false)
async function toggleMaster(on: boolean) {
  switching.value = true
  try {
    await saveServer({ monitor_enabled: on })
    message.success(on ? '自动监控已开启' : '自动监控已关闭，正在运行的任务不受影响')
  } catch (e) {
    message.error(errorMessage(e))
  } finally {
    switching.value = false
  }
}

const scanning = ref(false)
async function scan() {
  scanning.value = true
  try {
    await api.scanMonitors()
    message.success('已开始扫描')
  } catch (e) {
    message.error(errorMessage(e))
  } finally {
    scanning.value = false
  }
}

async function toggleDir(m: MonitorDir, enabled: boolean) {
  try {
    await api.putMonitor(m.id, { ...m, enabled })
  } catch (e) {
    message.error(errorMessage(e))
  }
}

const editorOpen = ref(false)
const editing = ref<MonitorDir | null>(null)
function edit(m: MonitorDir | null) {
  editing.value = m
  editorOpen.value = true
}

function remove(m: MonitorDir) {
  dialog.warning({
    title: '不再监控这个文件夹？',
    content: `${m.path}。已排队的任务和已有的尝试记录不受影响。`,
    positiveText: '移除',
    negativeText: '取消',
    onPositiveClick: async () => {
      try {
        await api.deleteMonitor(m.id)
        message.success('已移除')
      } catch (e) {
        message.error(errorMessage(e))
      }
    },
  })
}
</script>

<template>
  <NTabs :value="tab" type="line" @update:value="(v: Tab) => setQuery({ tab: v === 'dirs' ? undefined : v })">
    <NTab name="dirs">监控目录</NTab>
    <NTab name="pending">待确认成品<span v-if="pending.length" class="count">{{ pending.length }}</span></NTab>
  </NTabs>

  <template v-if="tab === 'dirs'">
    <NAlert v-if="monitors.error.value" type="error" :title="monitors.error.value" class="gap" />
    <NSpin v-else-if="!data" class="gap" />
    <template v-else>
      <NCard size="small" class="gap">
        <div class="master">
          <NSwitch :value="data.enabled" :loading="switching" @update:value="toggleMaster" />
          <div class="master-text">
            <div class="master-title">自动监控{{ data.enabled ? '已开启' : '已关闭' }}</div>
            <div v-if="data.enabled && status" class="sub">
              <template v-if="status.scanning">正在扫描 · </template>
              <template v-if="status.last_scan_at">上次全量扫描 {{ formatTime(status.last_scan_at) }} · </template>
              等待稳定 {{ status.settling }} 个文件 · 本次启动已入队 {{ status.queued }} 个
            </div>
            <div v-else class="sub">关闭后不再监听和扫描；手动转码不受影响。</div>
          </div>
          <NSpace :size="8">
            <NButton :disabled="!data.enabled" :loading="scanning || status?.scanning" @click="scan">立即扫描</NButton>
            <NButton type="primary" @click="edit(null)">添加目录</NButton>
          </NSpace>
        </div>
      </NCard>

      <NAlert
        v-if="data.enabled && status?.space_paused.length"
        type="warning"
        title="存储空间不足，以下存储上的自动监控已暂停"
        class="gap"
      >
        <div v-for="s in status.space_paused" :key="s.volume">{{ monitorShortageLine(s) }}</div>
        <div class="sub">
          新文件加上已排队的任务会超出剩余空间（含设置中的保留空间），所以先不加入队列。每分钟检查一次，空间足够后自动恢复。
        </div>
      </NAlert>

      <NEmpty v-if="data.monitors.length === 0" description="还没有监控目录。添加后，新放进去的视频稳定一段时间就会自动压缩。" class="empty" />
      <div v-else class="list">
        <NCard v-for="m in data.monitors" :key="m.id" size="small">
          <div class="row">
            <NSwitch size="small" :value="m.enabled" @update:value="(v: boolean) => toggleDir(m, v)" />
            <span class="path" :title="m.path">{{ m.path }}</span>
            <NTag size="small" :type="watchState(m).type" :bordered="false">{{ watchState(m).text }}</NTag>
          </div>
          <div class="meta">
            方案：{{ profileName(m.profile_id) }} · {{ m.recursive ? '包含子文件夹' : '只看这一层' }}
            <template v-if="m.excludes.length"> · 排除：{{ m.excludes.join('，') }}</template>
          </div>
          <div v-if="watchState(m).error" class="err">监听失败：{{ watchState(m).error }}</div>
          <NSpace :size="8" class="actions">
            <NButton size="small" secondary @click="edit(m)">编辑</NButton>
            <NButton size="small" secondary @click="remove(m)">移除</NButton>
          </NSpace>
        </NCard>
      </div>
    </template>
  </template>

  <template v-else>
    <div class="sub gap">
      校验有硬性项未通过、或处在观察模式的成品在这里等你决定：放行会替换源片（源片进回收目录），丢弃则删除成品、源片不变。
    </div>
    <NEmpty v-if="pending.length === 0" description="没有等待确认的成品" class="empty" />
    <div v-else class="list">
      <TaskCard
        v-for="t in pending"
        :key="t.id"
        :task="t"
        @open="(id) => setQuery({ task: String(id) })"
        @batch="(id) => router.push({ path: '/tasks', query: { batch: String(id) } })"
      />
    </div>
  </template>

  <MonitorDialog v-model:show="editorOpen" :editing="editing" />
  <TaskDetail :id="openId" :mobile="mobile" @close="setQuery({ task: undefined })" />
</template>

<style scoped>
.count {
  margin-left: 6px;
  font-size: 12px;
  opacity: 0.7;
}
.gap {
  margin-top: 16px;
}
.empty {
  margin-top: 40px;
}
.master {
  display: flex;
  align-items: center;
  gap: 14px;
  flex-wrap: wrap;
}
.master-text {
  flex: 1;
  min-width: 200px;
}
.master-title {
  font-weight: 600;
}
.sub {
  font-size: 13px;
  opacity: 0.7;
}
.list {
  display: flex;
  flex-direction: column;
  gap: 10px;
  margin-top: 12px;
}
.row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.path {
  flex: 1;
  min-width: 0;
  font-weight: 600;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.meta {
  margin-top: 4px;
  font-size: 13px;
  opacity: 0.75;
}
.err {
  margin-top: 4px;
  font-size: 13px;
  color: #d03050;
}
.actions {
  margin-top: 10px;
}
</style>
