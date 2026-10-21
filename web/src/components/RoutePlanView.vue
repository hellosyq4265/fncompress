<script setup lang="ts">
// A route plan: what will be encoded, into which container, and what the
// container change cannot carry over.
import { computed } from 'vue'
import { NAlert, NTag } from 'naive-ui'
import type { RoutePlan } from '../api/types'
import { ENCODER_LABEL } from '../logic/format'
import { rangeView } from '../logic/range'

const props = defineProps<{ plan: RoutePlan }>()
const range = computed(() => rangeView(props.plan.dynamic_range))
</script>

<template>
  <div class="plan">
    <div v-if="range" class="range">
      <NTag size="small" :type="range.type" :bordered="false">{{ range.tag }}</NTag>
      <span>{{ range.detail }}</span>
    </div>
    <div>
      {{ ENCODER_LABEL[plan.encoder] ?? plan.encoder }} 重编码第 {{ plan.video_index }} 条视频流（{{ plan.width }}×{{ plan.height }}，{{ plan.pix_fmt }}），其余轨道原样复制
    </div>
    <div>
      输出 {{ plan.container.toUpperCase() }}<template v-if="plan.container_changed">（换封装，发布为 <code>{{ plan.target }}</code>）</template>
    </div>
    <NAlert v-if="plan.expected_losses.length" type="warning" title="换封装会丢失这些轨道属性" style="margin-top: 8px">
      <ul>
        <li v-for="l in plan.expected_losses" :key="`${l.stream}:${l.property}`">{{ l.reason }}</li>
      </ul>
    </NAlert>
    <NAlert v-for="w in plan.warnings" :key="w" type="info" style="margin-top: 8px">{{ w }}</NAlert>
  </div>
</template>

<style scoped>
.plan {
  font-size: 14px;
  line-height: 1.7;
}
.range {
  display: flex;
  gap: 8px;
  align-items: center;
  flex-wrap: wrap;
}
code {
  font-size: 12px;
  word-break: break-all;
}
ul {
  margin: 0;
  padding-left: 18px;
}
</style>
