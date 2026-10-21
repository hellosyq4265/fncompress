<script setup lang="ts">
// A verification report: hard and soft findings listed, recorded-only ones
// folded away, then what the FFmpeg checks measured.
import { computed } from 'vue'
import { NCollapse, NCollapseItem, NTag } from 'naive-ui'
import type { Report } from '../api/types'
import { SEVERITY_LABEL, SEVERITY_TAG } from '../logic/format'
import { checkLabel, groupFindings, inspectionLines } from '../logic/report'

const props = defineProps<{ report: Report }>()
const groups = computed(() => groupFindings(props.report.findings))
const main = computed(() => groups.value.filter((g) => g.severity !== 'info'))
const info = computed(() => groups.value.find((g) => g.severity === 'info'))
const lines = computed(() => inspectionLines(props.report))
const VERDICT = { passed: '全部通过', passed_with_warnings: '通过（有软性项）', rejected: '未通过' } as const
const VERDICT_TAG = { passed: 'success', passed_with_warnings: 'warning', rejected: 'error' } as const
</script>

<template>
  <div class="report">
    <div class="verdict">
      校验结论 <NTag size="small" :type="VERDICT_TAG[report.verdict]" :bordered="false">{{ VERDICT[report.verdict] }}</NTag>
    </div>
    <div v-for="g in main" :key="g.severity" class="group">
      <div v-for="(f, i) in g.findings" :key="i" class="finding">
        <NTag size="tiny" :type="SEVERITY_TAG[f.severity]" :bordered="false">{{ SEVERITY_LABEL[f.severity] }}</NTag>
        <div>
          <div class="check">{{ checkLabel(f.check) }}</div>
          <div class="msg">{{ f.message }}</div>
        </div>
      </div>
    </div>
    <ul v-if="lines.length" class="lines">
      <li v-for="l in lines" :key="l">{{ l }}</li>
    </ul>
    <NCollapse v-if="info">
      <NCollapseItem :title="`记录项 ${info.findings.length} 条（不影响结论）`" name="info">
        <div v-for="(f, i) in info.findings" :key="i" class="finding small">
          <div class="check">{{ checkLabel(f.check) }}</div>
          <div class="msg">{{ f.message }}</div>
        </div>
      </NCollapseItem>
    </NCollapse>
  </div>
</template>

<style scoped>
.verdict {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
}
.finding {
  display: flex;
  gap: 8px;
  align-items: flex-start;
  padding: 6px 0;
}
.finding.small {
  display: block;
  padding: 4px 0;
  font-size: 13px;
}
.check {
  font-weight: 500;
}
.msg {
  font-size: 13px;
  opacity: 0.8;
  word-break: break-all;
}
.lines {
  margin: 8px 0;
  padding-left: 18px;
  font-size: 13px;
  opacity: 0.85;
}
</style>
