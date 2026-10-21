<script setup lang="ts">
// Shown over every page until the user confirms it once. The confirmation
// lives in the server settings, so the gateway and LAN entries and every
// browser share it.
import { computed, ref } from 'vue'
import { NAlert, NButton, NCheckbox, NModal, useMessage } from 'naive-ui'
import { api } from '../api/client'
import { errorMessage } from '../logic/errors'
import { settings } from '../stores/resources'

const message = useMessage()
const s = settings.use().data
// Not while the settings are unknown: a failed load must not lock the app.
const show = computed(() => s.value !== null && !s.value.risk_acknowledged)
const recycleDays = computed(() => s.value?.global.recycle_retention_days ?? 14)
// Files the observation mode will still hold for review.
const observeLeft = computed(() => Math.max(0, (s.value?.server.observe_first_n ?? 0) - (s.value?.observed ?? 0)))
const understood = ref(false)
const saving = ref(false)

async function acknowledge() {
  saving.value = true
  try {
    settings.data.value = await api.acknowledgeRisk()
  } catch (e) {
    message.error(errorMessage(e), { duration: 6000 })
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <NModal
    :show="show"
    preset="card"
    title="使用前请了解风险"
    :closable="false"
    :mask-closable="false"
    :close-on-esc="false"
    style="width: min(560px, 96vw)"
  >
    <div class="notice">
      <NAlert type="error" :show-icon="false" class="warn">
        fncompress 还在测试阶段，可能有没发现的问题。转码通过校验后会<b>替换源片</b>，视频有可能损坏或丢失内容。
        <b>不要用在重要的、没有备份的视频上。</b>
      </NAlert>
      <p>已有的保护能降低风险，但不能保证绝对安全：</p>
      <ul>
        <li>成品要通过校验（时长、音画同步、帧数、画质等）才替换；校验发现不了所有问题。</li>
        <li>
          替换前源片先进回收目录（源片旁边的 <code>.fncompress-recycle</code>），保留 {{ recycleDays }} 天，期间可以手动找回。回收目录和源片在同一块存储上，存储本身出故障时会一起丢失。
        </li>
        <li v-if="observeLeft > 0">
          观察模式：接下来 {{ observeLeft }} 个文件不论校验结果都先等你确认，再决定是否替换。
        </li>
      </ul>
      <p>建议先在测试目录或已经备份的目录上试一批，播放检查成品没有问题，再扩大到更多目录。</p>
      <NCheckbox v-model:checked="understood">我已了解风险，要处理的视频已有备份或可以承受损失</NCheckbox>
    </div>
    <template #footer>
      <div class="footer">
        <NButton type="primary" :disabled="!understood" :loading="saving" @click="acknowledge">
          我已了解，开始使用
        </NButton>
      </div>
    </template>
  </NModal>
</template>

<style scoped>
.notice p {
  margin: 0 0 10px;
}
.notice ul {
  margin: 0 0 10px;
  padding-left: 20px;
}
.notice li {
  margin-bottom: 4px;
}
.warn {
  margin-bottom: 12px;
}
.footer {
  display: flex;
  justify-content: flex-end;
}
</style>
