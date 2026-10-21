<script setup lang="ts">
// The one task queue (decisions doc): running and queued first, then what
// awaits a decision, then history. Phones get the same list with cancel only.
import { computed, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { NAlert, NButton, NEmpty, NSpin, NTab, NTabs, NTag } from 'naive-ui'
import TaskCard from '../components/TaskCard.vue'
import TaskDetail from '../components/TaskDetail.vue'
import { errorMessage } from '../logic/errors'
import { countByView, tasksIn, type View } from '../logic/tasks'
import { loadMoreFinished, moreFinished, tasks, tasksError, tasksLoaded } from '../stores/tasks'
import { useIsMobile } from '../stores/ui'

const route = useRoute()
const router = useRouter()
const mobile = useIsMobile()

const VIEWS: { key: View; label: string; empty: string }[] = [
  { key: 'active', label: '进行中', empty: '没有排队或运行中的任务' },
  { key: 'pending', label: '待确认', empty: '没有等待确认的成品或待选择的推荐' },
  { key: 'finished', label: '已结束', empty: '还没有结束的任务' },
]

// View, batch filter and open task live in the URL so links and Back work.
const view = computed<View>(() => {
  const v = route.query.view
  return v === 'pending' || v === 'finished' ? v : 'active'
})
const batch = computed(() => (route.query.batch ? Number(route.query.batch) : null))
const openId = computed(() => (route.query.task ? Number(route.query.task) : null))

function setQuery(q: Record<string, string | undefined>) {
  void router.replace({ query: { ...route.query, ...q } })
}

const counts = computed(() => countByView(tasks.values()))
const list = computed(() => {
  const all = tasksIn(tasks.values(), view.value)
  return batch.value === null ? all : all.filter((t) => t.batch_id === batch.value)
})
const current = computed(() => VIEWS.find((v) => v.key === view.value)!)

const loadingMore = ref(false)
const moreError = ref<string | null>(null)
async function more() {
  loadingMore.value = true
  moreError.value = null
  try {
    await loadMoreFinished()
  } catch (e) {
    moreError.value = errorMessage(e)
  } finally {
    loadingMore.value = false
  }
}
</script>

<template>
  <NTabs :value="view" type="line" @update:value="(v: View) => setQuery({ view: v === 'active' ? undefined : v })">
    <NTab v-for="v in VIEWS" :key="v.key" :name="v.key">
      {{ v.label }}<span v-if="v.key !== 'finished' && counts[v.key]" class="count">{{ counts[v.key] }}</span>
    </NTab>
  </NTabs>

  <div v-if="batch !== null" class="filter">
    <NTag closable @close="setQuery({ batch: undefined })">只看批次 #{{ batch }}</NTag>
  </div>

  <NAlert v-if="tasksError" type="error" :title="tasksError" class="gap" />
  <NSpin v-else-if="!tasksLoaded" class="gap" />
  <NEmpty v-else-if="list.length === 0" :description="current.empty" class="empty" />
  <div v-else class="list">
    <TaskCard
      v-for="t in list"
      :key="t.id"
      :task="t"
      :mobile="mobile"
      @open="(id) => setQuery({ task: String(id) })"
      @batch="(id) => setQuery({ batch: String(id) })"
    />
  </div>

  <div v-if="tasksLoaded && view === 'finished' && moreFinished" class="more">
    <NButton :loading="loadingMore" @click="more">加载更早的任务</NButton>
    <div v-if="moreError" class="more-error">{{ moreError }}</div>
  </div>

  <TaskDetail :id="openId" :mobile="mobile" @close="setQuery({ task: undefined })" />
</template>

<style scoped>
.count {
  margin-left: 6px;
  font-size: 12px;
  opacity: 0.7;
}
.filter {
  margin-top: 12px;
}
.gap {
  margin-top: 16px;
}
.empty {
  margin-top: 48px;
}
.list {
  display: flex;
  flex-direction: column;
  gap: 10px;
  margin-top: 12px;
}
.more {
  margin: 16px 0;
  text-align: center;
}
.more-error {
  margin-top: 6px;
  font-size: 13px;
  color: #d03050;
}
</style>
