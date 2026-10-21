<script setup lang="ts">
import { computed } from 'vue'
import { NBadge, NTooltip } from 'naive-ui'
import { connection } from '../stores/live'

const view = computed(() => {
  switch (connection.value) {
    case 'open':
      return { type: 'success' as const, text: '实时', tip: '已连接，进度实时更新' }
    case 'polling':
      return {
        type: 'info' as const,
        text: '定时刷新',
        tip: '实时连接被中间的代理拦住了（例如路由器反向代理缓冲了响应），改为每 5 秒刷新一次；进度照常更新，只是不那么及时',
      }
    case 'connecting':
      return { type: 'info' as const, text: '连接中', tip: '正在连接 fncompress 服务' }
    default:
      return { type: 'warning' as const, text: '重连中', tip: '与服务的连接断开，正在重连；恢复后会自动刷新' }
  }
})
</script>

<template>
  <NTooltip>
    <template #trigger>
      <span class="conn"><NBadge dot :type="view.type" :processing="connection !== 'open' && connection !== 'polling'" /> {{ view.text }}</span>
    </template>
    {{ view.tip }}
  </NTooltip>
</template>

<style scoped>
.conn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  opacity: 0.8;
  white-space: nowrap;
}
</style>
