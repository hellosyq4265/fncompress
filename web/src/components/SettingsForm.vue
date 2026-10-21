<script setup lang="ts">
// Global and server settings in one form. New tasks use the saved values at
// once; slot changes apply as running tasks finish (ADR 0003).
import { computed, ref, watch } from 'vue'
import {
  NAlert,
  NButton,
  NCard,
  NCheckbox,
  NCollapse,
  NCollapseItem,
  NForm,
  NFormItem,
  NInput,
  NInputNumber,
  NSpace,
  NSpin,
  useMessage,
  useThemeVars,
} from 'naive-ui'
import { api } from '../api/client'
import { errorMessage } from '../logic/errors'
import { isDirty, normalize, toForm, type SettingsForm } from '../logic/settings'
import { settings } from '../stores/resources'
import ProfileSelect from './ProfileSelect.vue'

const message = useMessage()
const theme = useThemeVars()
settings.use()

const form = ref<SettingsForm | null>(null)
const saving = ref(false)
/** The server's settings changed while the form had unsaved edits. */
const changedElsewhere = ref(false)

const dirty = computed(() => !!form.value && !!settings.data.value && isDirty(form.value, settings.data.value))

watch(
  () => settings.data.value,
  (s) => {
    if (!s) return
    if (!form.value || !dirty.value) {
      form.value = toForm(s)
      changedElsewhere.value = false
    } else {
      changedElsewhere.value = true
    }
  },
  { immediate: true },
)

function reset() {
  if (settings.data.value) form.value = toForm(settings.data.value)
  changedElsewhere.value = false
}

async function save() {
  if (!form.value) return
  saving.value = true
  try {
    const saved = await api.putSettings(normalize(form.value))
    settings.data.value = saved
    form.value = toForm(saved)
    changedElsewhere.value = false
    message.success('设置已保存，新任务立即使用')
  } catch (e) {
    message.error(errorMessage(e), { duration: 6000 })
  } finally {
    saving.value = false
  }
}
</script>

<template>
  <NAlert v-if="settings.error.value && !form" type="error" :title="settings.error.value" />
  <NSpin v-else-if="!form" />
  <div v-else class="wrap">
    <NAlert v-if="changedElsewhere" type="warning" title="设置已在别处被修改" class="card">
      你的改动还没保存。<NButton text type="primary" @click="reset">放弃改动并载入最新设置</NButton>
    </NAlert>
    <NForm label-placement="left" label-width="auto" :show-feedback="false">
      <NCard title="任务" size="small" class="card">
        <div class="fields">
          <NFormItem label="GPU 配额">
            <NInputNumber v-model:value="form.server.gpu_slots" :min="0" :max="16" :precision="0" class="num" />
          </NFormItem>
          <NFormItem label="CPU 配额">
            <NInputNumber v-model:value="form.server.cpu_slots" :min="0" :max="64" :precision="0" class="num" />
          </NFormItem>
          <div class="hint">同时运行的任务数。设为 0 暂停这一类任务；调小不会打断正在运行的任务。</div>
          <NFormItem label="默认方案">
            <ProfileSelect :value="form.server.default_profile" @update:value="(v) => form && v && (form.server.default_profile = v)" />
          </NFormItem>
          <NFormItem label="保留空间（GiB）">
            <NInputNumber v-model:value="form.server.min_free_gib" :min="0" :max="10240" :precision="0" class="num" />
          </NFormItem>
          <div class="hint">
            每个存储卷至少留这么多空间。转码按源片大小估算最坏情况：放不下的任务留在队列里等待，自动监控在该存储上暂停，空间释放后自动继续。
          </div>
        </div>
      </NCard>

      <NCard title="自动监控" size="small" class="card">
        <div class="fields">
          <NFormItem label="全量扫描间隔（分钟）">
            <NInputNumber v-model:value="form.server.scan_interval_mins" :min="1" :precision="0" class="num" />
          </NFormItem>
          <NFormItem label="稳定判定（秒）">
            <NInputNumber v-model:value="form.server.settle_secs" :min="0" :precision="0" class="num" />
          </NFormItem>
          <div class="hint">新文件的大小和修改时间连续这么久不变才处理，避免压缩复制到一半的文件。总开关在监控页。</div>
        </div>
      </NCard>

      <NCard title="收益与保留" size="small" class="card">
        <div class="fields">
          <NFormItem label="最低节省比例（%）">
            <NInputNumber v-model:value="form.global.min_savings_pct" :min="0" :max="99" class="num" />
          </NFormItem>
          <NFormItem label="待确认成品保留（天）">
            <NInputNumber v-model:value="form.global.pending_retention_days" :min="1" :max="3650" :precision="0" class="num" />
          </NFormItem>
          <NFormItem label="回收目录保留（天）">
            <NInputNumber v-model:value="form.global.recycle_retention_days" :min="1" :max="3650" :precision="0" class="num" />
          </NFormItem>
          <NFormItem label="回收目录">
            <NInput
              :value="form.global.recycle_dir ?? ''"
              placeholder="留空：放在每个源片旁边的 .fncompress-recycle"
              @update:value="(v: string) => form && (form.global.recycle_dir = v)"
            />
          </NFormItem>
          <div class="hint">留空最稳妥：回收目录和源片在同一个存储卷上，移动是瞬间完成的。</div>
        </div>
      </NCard>

      <NCard title="校验与观察" size="small" class="card">
        <div class="fields">
          <NFormItem label="观察模式文件数">
            <NInputNumber v-model:value="form.server.observe_first_n" :min="0" :precision="0" class="num" />
          </NFormItem>
          <div class="hint">前 N 个完成校验的文件无论结论都进入待确认，用来确认检查是否合理；0 关闭。计数和清零在「诊断」。</div>
        </div>
        <NCollapse>
          <NCollapseItem title="校验阈值（高级）" name="verify">
            <div class="fields">
              <NFormItem label="音画偏移软性 / 硬性（毫秒）">
                <NSpace :size="8">
                  <NInputNumber v-model:value="form.global.verify.av_offset_soft_ms" :min="0" class="num" />
                  <NInputNumber v-model:value="form.global.verify.av_offset_hard_ms" :min="0" class="num" />
                </NSpace>
              </NFormItem>
              <NFormItem label="流结束时间软性 / 硬性（毫秒）">
                <NSpace :size="8">
                  <NInputNumber v-model:value="form.global.verify.end_soft_ms" :min="0" class="num" />
                  <NInputNumber v-model:value="form.global.verify.end_hard_ms" :min="0" class="num" />
                </NSpace>
              </NFormItem>
              <NFormItem label="画面抽检点数 / 每点秒数">
                <NSpace :size="8">
                  <NInputNumber v-model:value="form.global.verify.spot_checks" :min="0" :precision="0" class="num" />
                  <NInputNumber v-model:value="form.global.verify.spot_check_secs" :min="0.1" :step="0.5" class="num" />
                </NSpace>
              </NFormItem>
              <NFormItem label="SSIM 硬性 / 软性下限">
                <NSpace :size="8">
                  <NInputNumber v-model:value="form.global.verify.ssim_hard" :min="0" :max="1" :step="0.01" class="num" />
                  <NInputNumber v-model:value="form.global.verify.ssim_soft" :min="0" :max="1" :step="0.01" class="num" />
                </NSpace>
              </NFormItem>
              <NFormItem label="检查项">
                <NSpace vertical :size="6">
                  <NCheckbox v-model:checked="form.global.verify.decode_check">完整解码成品，有任何错误就拒绝</NCheckbox>
                  <NCheckbox v-model:checked="form.global.verify.hw_decode">完整解码先试核显（失败退回软件）</NCheckbox>
                  <NCheckbox v-model:checked="form.global.verify.copied_stream_hash">复制流逐字节哈希比对（读两遍全文件，较慢）</NCheckbox>
                </NSpace>
              </NFormItem>
            </div>
          </NCollapseItem>
        </NCollapse>
      </NCard>

      <NCard title="工具" size="small" class="card">
        <div class="fields">
          <NFormItem label="ffmpeg">
            <NInput v-model:value="form.global.ffmpeg" />
          </NFormItem>
          <NFormItem label="ffprobe">
            <NInput v-model:value="form.global.ffprobe" />
          </NFormItem>
          <NFormItem label="评分 ffmpeg">
            <NInput
              :value="form.global.scoring_ffmpeg ?? ''"
              placeholder="留空：用上面的 ffmpeg（需要带 libvmaf）"
              @update:value="(v: string) => form && (form.global.scoring_ffmpeg = v)"
            />
          </NFormItem>
          <NFormItem label="dovi_tool">
            <NInput
              :value="form.global.dovi_tool ?? ''"
              placeholder="留空：用评分 ffmpeg 同目录的 dovi_tool，再找 PATH"
              @update:value="(v: string) => form && (form.global.dovi_tool = v)"
            />
          </NFormItem>
          <NFormItem label="核显设备">
            <NInput v-model:value="form.global.render_device" />
          </NFormItem>
          <NFormItem label="转码解码">
            <NCheckbox v-model:checked="form.global.hw_decode">QSV/VAAPI 转码用核显解码源片（失败退回软件解码）</NCheckbox>
          </NFormItem>
          <NFormItem label="编码超时系数">
            <NInputNumber v-model:value="form.global.encode_timeout_factor" :min="0.1" class="num" />
          </NFormItem>
          <div class="hint">
            超时 = 片长 × 系数 + 30 分钟。评分 ffmpeg 用于样片推荐的 VMAF 评分（评分运行时）；dovi_tool 用于杜比视界片源的 RPU 提取和核对。启动时命令行或环境变量给出的工具路径会覆盖这里。
          </div>
        </div>
      </NCard>
    </NForm>

    <div class="save" :style="{ background: theme.bodyColor }">
      <NSpace>
        <NButton type="primary" :disabled="!dirty" :loading="saving" @click="save">保存设置</NButton>
        <NButton :disabled="!dirty" @click="reset">撤销改动</NButton>
      </NSpace>
    </div>
  </div>
</template>

<style scoped>
.wrap {
  max-width: 820px;
}
.card {
  margin-bottom: 12px;
}
.fields {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.num {
  width: 140px;
}
.hint {
  margin-top: -4px;
  font-size: 12px;
  opacity: 0.65;
}
.save {
  position: sticky;
  bottom: 0;
  padding: 12px 0;
}
</style>
