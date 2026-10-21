<script setup lang="ts">
// Saved profiles: the encoder settings manual tasks and monitored
// directories refer to. Changes apply to new tasks at once.
import { computed, ref } from 'vue'
import { NAlert, NButton, NCard, NEmpty, NSpace, NSpin, NTag, useDialog, useMessage } from 'naive-ui'
import { api } from '../api/client'
import type { SavedProfile } from '../api/types'
import ProfileDialog from '../components/ProfileDialog.vue'
import { errorMessage } from '../logic/errors'
import { ENCODER_LABEL } from '../logic/format'
import { describeProfile } from '../logic/profile'
import { monitors, profiles, saveServer, settings, system } from '../stores/resources'

const message = useMessage()
const dialog = useDialog()

profiles.use()
settings.use()
monitors.use()
system.use()

const defaultId = computed(() => settings.data.value?.server.default_profile)
const unavailable = computed(
  () => new Set((system.data.value?.encoders ?? []).filter((e) => !e.available).map((e) => e.encoder)),
)
const usedBy = (id: string) => (monitors.data.value?.monitors ?? []).filter((m) => m.profile_id === id).length
const list = computed(() =>
  [...(profiles.data.value ?? [])].sort((a, b) => (a.id === defaultId.value ? -1 : b.id === defaultId.value ? 1 : a.name.localeCompare(b.name))),
)

const editorOpen = ref(false)
const editing = ref<SavedProfile | null>(null)
function edit(p: SavedProfile | null) {
  editing.value = p
  editorOpen.value = true
}

async function makeDefault(p: SavedProfile) {
  try {
    await saveServer({ default_profile: p.id })
    message.success(`「${p.name}」已设为默认方案`)
  } catch (e) {
    message.error(errorMessage(e))
  }
}

function remove(p: SavedProfile) {
  dialog.warning({
    title: `删除方案「${p.name}」？`,
    content: '已排队的任务不受影响；以后无法再选这个方案。',
    positiveText: '删除',
    negativeText: '取消',
    onPositiveClick: async () => {
      try {
        await api.deleteProfile(p.id)
        message.success('已删除')
      } catch (e) {
        message.error(errorMessage(e), { duration: 6000 })
      }
    },
  })
}
</script>

<template>
  <NSpace justify="space-between" align="center" class="bar">
    <span class="sub">新任务立即使用修改后的方案；已排队的任务使用入队时的设置。</span>
    <NButton type="primary" @click="edit(null)">新建方案</NButton>
  </NSpace>
  <NAlert v-if="profiles.error.value" type="error" :title="profiles.error.value" />
  <NSpin v-else-if="!profiles.data.value" />
  <NEmpty v-else-if="list.length === 0" description="还没有配置方案" />
  <div v-else class="grid">
    <NCard v-for="p in list" :key="p.id" size="small">
      <div class="head">
        <span class="name">{{ p.name }}</span>
        <NTag v-if="p.id === defaultId" size="small" type="success" :bordered="false">默认</NTag>
        <NTag v-if="unavailable.has(p.profile.encoder)" size="small" type="error" :bordered="false">编码器不可用</NTag>
      </div>
      <div class="id">id：{{ p.id }}</div>
      <div class="enc">{{ ENCODER_LABEL[p.profile.encoder] ?? p.profile.encoder }}</div>
      <div class="desc">{{ describeProfile(p.profile).join(' · ') }}</div>
      <div v-if="usedBy(p.id)" class="used">{{ usedBy(p.id) }} 个监控目录在用</div>
      <NSpace class="actions" :size="8">
        <NButton size="small" secondary @click="edit(p)">编辑</NButton>
        <NButton v-if="p.id !== defaultId" size="small" secondary @click="makeDefault(p)">设为默认</NButton>
        <NButton v-if="p.id !== defaultId" size="small" secondary @click="remove(p)">删除</NButton>
      </NSpace>
    </NCard>
  </div>
  <ProfileDialog v-model:show="editorOpen" :editing="editing" />
</template>

<style scoped>
.bar {
  margin-bottom: 16px;
}
.sub {
  font-size: 13px;
  opacity: 0.7;
}
.grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
  gap: 12px;
}
.head {
  display: flex;
  align-items: center;
  gap: 8px;
}
.name {
  font-weight: 600;
  font-size: 15px;
}
.id {
  font-size: 12px;
  opacity: 0.55;
  font-family: ui-monospace, monospace;
}
.enc {
  margin-top: 8px;
}
.desc {
  font-size: 13px;
  opacity: 0.8;
}
.used {
  margin-top: 4px;
  font-size: 12px;
  opacity: 0.65;
}
.actions {
  margin-top: 12px;
}
</style>
