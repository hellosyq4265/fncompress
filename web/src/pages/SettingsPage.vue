<script setup lang="ts">
import { computed } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { NTab, NTabs } from 'naive-ui'
import DiagnosticsPanel from '../components/DiagnosticsPanel.vue'
import RecyclePanel from '../components/RecyclePanel.vue'
import SettingsForm from '../components/SettingsForm.vue'

type Tab = 'settings' | 'diagnostics' | 'recycle'
const route = useRoute()
const router = useRouter()
const tab = computed<Tab>(() => {
  const t = route.query.tab
  return t === 'diagnostics' || t === 'recycle' ? t : 'settings'
})
const setTab = (t: Tab) => void router.replace({ query: t === 'settings' ? {} : { tab: t } })
</script>

<template>
  <NTabs :value="tab" type="line" class="tabs" @update:value="setTab">
    <NTab name="settings">设置</NTab>
    <NTab name="diagnostics">诊断</NTab>
    <NTab name="recycle">回收与记录</NTab>
  </NTabs>
  <SettingsForm v-if="tab === 'settings'" />
  <DiagnosticsPanel v-else-if="tab === 'diagnostics'" />
  <RecyclePanel v-else />
</template>

<style scoped>
.tabs {
  margin-bottom: 16px;
}
</style>
