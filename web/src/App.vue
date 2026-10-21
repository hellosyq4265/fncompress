<script setup lang="ts">
import { computed } from 'vue'
import {
  NConfigProvider,
  NDialogProvider,
  NGlobalStyle,
  NMessageProvider,
  darkTheme,
  dateZhCN,
  useOsTheme,
  zhCN,
  type GlobalThemeOverrides,
} from 'naive-ui'
import AppShell from './components/AppShell.vue'

const os = useOsTheme()
const theme = computed(() => (os.value === 'dark' ? darkTheme : null))
const overrides: GlobalThemeOverrides = { common: { borderRadius: '6px' } }
</script>

<template>
  <NConfigProvider :theme="theme" :theme-overrides="overrides" :locale="zhCN" :date-locale="dateZhCN">
    <NGlobalStyle />
    <NMessageProvider>
      <NDialogProvider>
        <AppShell />
      </NDialogProvider>
    </NMessageProvider>
  </NConfigProvider>
</template>

<style>
html,
body,
#app {
  margin: 0;
  height: 100%;
}
</style>
