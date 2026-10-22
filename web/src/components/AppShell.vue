<script setup lang="ts">
// One layout tree for every width: crossing the phone breakpoint only shows
// or hides the sider and the top bar, so the page (open dialogs, half-filled
// forms) is never rebuilt.
import { computed, h, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { NAlert, NBadge, NButton, NDrawer, NDrawerContent, NMenu, NScrollbar, useThemeVars, type MenuOption } from 'naive-ui'
import { pages } from '../pages'
import { taskShortageLine } from '../logic/space'
import { countByView } from '../logic/tasks'
import { space, startLive } from '../stores/live'
import { tasks } from '../stores/tasks'
import { useIsMobile } from '../stores/ui'
import ConnectionBadge from './ConnectionBadge.vue'
import RiskNotice from './RiskNotice.vue'

const route = useRoute()
const router = useRouter()
const mobile = useIsMobile()
const theme = useThemeVars()
const drawer = ref(false)

onMounted(startLive)

const counts = computed(() => countByView(tasks.values()))
// Queued transcodes waiting for free space, shown on every page.
const shortages = computed(() => space.value.shortages)

// Running/queued next to 任务, outputs awaiting a decision next to 监控.
const badgeFor = (path: string): { n: number; type: 'info' | 'warning' } | null => {
  if (path === '/tasks' && counts.value.active > 0) return { n: counts.value.active, type: 'info' }
  if (path === '/monitor' && counts.value.pending > 0) return { n: counts.value.pending, type: 'warning' }
  return null
}

const menuOptions = computed<MenuOption[]>(() =>
  pages.map((p) => ({
    key: p.path,
    label: () => {
      const b = badgeFor(p.path)
      return h('span', { class: 'menu-label' }, [
        p.title,
        b ? h(NBadge, { value: b.n, type: b.type, style: 'margin-left: 8px' }) : null,
      ])
    },
  })),
)
const active = computed(() => route.path)
const title = computed(() => (route.meta.title as string | undefined) ?? 'fncompress')

watch(title, (t) => (document.title = `${t} · fncompress`), { immediate: true })
watch(mobile, (m) => !m && (drawer.value = false))

function go(key: string) {
  drawer.value = false
  void router.push(key)
}
</script>

<template>
  <div class="shell">
    <aside v-if="!mobile" class="sider" :style="{ borderRight: `1px solid ${theme.dividerColor}` }">
      <div class="brand">fncompress</div>
      <NMenu :options="menuOptions" :value="active" @update:value="go" />
      <div class="sider-foot"><ConnectionBadge /></div>
    </aside>
    <div class="main">
      <header
        v-if="mobile"
        class="mobile-header"
        :style="{ background: theme.bodyColor, borderBottom: `1px solid ${theme.dividerColor}` }"
      >
        <NButton quaternary size="small" aria-label="菜单" @click="drawer = true">☰</NButton>
        <span class="mobile-title">{{ title }}</span>
        <ConnectionBadge />
      </header>
      <NScrollbar class="scroll">
        <main class="content" :class="{ phone: mobile }">
          <h2 v-if="!mobile" class="page-title">{{ title }}</h2>
          <NAlert v-if="shortages.length" type="warning" title="存储空间不足，部分转码任务在排队等待" class="space-alert">
            <div v-for="s in shortages" :key="s.volume">{{ taskShortageLine(s) }}</div>
            <div class="space-hint">
              每个文件按源片大小计算最坏情况，并留出设置中的保留空间。释放空间（如清空回收站、处理待确认的成品）后会自动继续。
            </div>
          </NAlert>
          <RouterView />
        </main>
      </NScrollbar>
    </div>
    <NDrawer v-model:show="drawer" placement="left" :width="220">
      <NDrawerContent title="fncompress" body-content-style="padding: 0">
        <NMenu :options="menuOptions" :value="active" @update:value="go" />
      </NDrawerContent>
    </NDrawer>
    <RiskNotice />
  </div>
</template>

<style scoped>
.shell {
  display: flex;
  height: 100vh;
}
.sider {
  flex: 0 0 200px;
  display: flex;
  flex-direction: column;
}
.brand {
  padding: 18px 24px 10px;
  font-weight: 600;
  font-size: 17px;
}
.sider-foot {
  margin-top: auto;
  padding: 12px 24px;
}
.main {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.scroll {
  flex: 1;
  min-height: 0;
}
.content {
  padding: 20px 28px;
  max-width: 1200px;
}
.content.phone {
  padding: 12px;
}
.page-title {
  margin: 0 0 16px;
  font-size: 20px;
  font-weight: 600;
}
.space-alert {
  margin-bottom: 16px;
}
.space-hint {
  margin-top: 4px;
  opacity: 0.75;
}
.mobile-header {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
}
.mobile-title {
  flex: 1;
  font-weight: 600;
}
:deep(.menu-label) {
  display: inline-flex;
  align-items: center;
}
</style>
