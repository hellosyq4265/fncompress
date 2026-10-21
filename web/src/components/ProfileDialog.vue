<script setup lang="ts">
// Create or edit a saved profile.
import { computed, ref, watch } from 'vue'
import {
  NAlert,
  NButton,
  NForm,
  NFormItem,
  NInput,
  NInputNumber,
  NModal,
  NRadioButton,
  NRadioGroup,
  NSelect,
  NSpace,
  useDialog,
  useMessage,
} from 'naive-ui'
import { api } from '../api/client'
import type { Encoder, SavedProfile } from '../api/types'
import { errorMessage } from '../logic/errors'
import { ENCODER_LABEL } from '../logic/format'
import {
  ENCODERS,
  defaultPreset,
  emptyProfileForm,
  formToProfile,
  isGpu,
  presetsFor,
  profileProblems,
  profileToForm,
  qualityHint,
  qsvInsteadOf,
  type ProfileForm,
} from '../logic/profile'
import { system } from '../stores/resources'

const props = defineProps<{ show: boolean; editing: SavedProfile | null }>()
const emit = defineEmits<{ 'update:show': [v: boolean]; saved: [p: SavedProfile] }>()
const message = useMessage()
const dialog = useDialog()

system.use()
const form = ref<ProfileForm>(emptyProfileForm())
const tried = ref(false)
const saving = ref(false)

const isNew = computed(() => !props.editing)
const problems = computed(() => profileProblems(form.value, isNew.value))

const unavailable = computed(
  () => new Set((system.data.value?.encoders ?? []).filter((e) => !e.available).map((e) => e.encoder)),
)
const working = computed(
  () => new Set((system.data.value?.encoders ?? []).filter((e) => e.available).map((e) => e.encoder)),
)
const betterQsv = computed(() => qsvInsteadOf(form.value.encoder, working.value))
const encoderOptions = computed(() =>
  ENCODERS.map((e) => ({
    label: ENCODER_LABEL[e] + (unavailable.value.has(e) ? '（试编失败，不可用）' : ''),
    value: e,
    disabled: unavailable.value.has(e) && e !== form.value.encoder,
  })),
)
watch(
  () => props.show,
  (show) => {
    if (!show) return
    form.value = props.editing ? profileToForm(props.editing) : emptyProfileForm()
    // A new profile starts on an encoder the trial encode found working.
    if (!props.editing && unavailable.value.has(form.value.encoder)) {
      form.value.encoder = ENCODERS.find((e) => !unavailable.value.has(e)) ?? form.value.encoder
    }
    tried.value = false
  },
)

const presetOptions = computed(() => {
  const def = defaultPreset(form.value.encoder)
  return [
    { label: def ? `编码器默认（${def}）` : '编码器默认', value: '' },
    ...presetsFor(form.value.encoder).map((p) => ({ label: p, value: p })),
  ]
})
const HEIGHTS = [2160, 1440, 1080, 720, 480]
const heightOptions = [
  { label: '保持原分辨率', value: 0 },
  ...HEIGHTS.map((h) => ({ label: `最高 ${h}p（只缩小，不放大）`, value: h })),
]

function useEncoder(e: Encoder) {
  form.value.encoder = e
  if (form.value.preset && !presetsFor(e).includes(form.value.preset)) form.value.preset = null
}

// Picking VAAPI on a machine where QSV works: offer QSV, the user decides.
function setEncoder(e: Encoder) {
  useEncoder(e)
  const qsv = qsvInsteadOf(e, working.value)
  if (!qsv) return
  dialog.info({
    title: `建议改用 ${ENCODER_LABEL[qsv]}`,
    content:
      '这台机器的 QSV 试编通过。在 Intel 核显上两者用的是同一个硬件编码器，但 QSV 按画面内容调整量化（ICQ）、有预设可选；' +
      'VAAPI 方案用固定 QP，同样画质下文件通常更大。VAAPI 编码稍快，主要用于没有 QSV 的显卡。',
    positiveText: `改用 ${ENCODER_LABEL[qsv]}`,
    negativeText: '继续用 VAAPI',
    onPositiveClick: () => useEncoder(qsv),
  })
}

async function save() {
  tried.value = true
  if (problems.value.length) return
  saving.value = true
  try {
    const profile = formToProfile(form.value)
    const saved = props.editing
      ? await api.putProfile(props.editing.id, { name: form.value.name.trim(), profile })
      : await api.createProfile({ id: form.value.id, name: form.value.name.trim(), profile })
    message.success(props.editing ? '方案已保存，新任务立即使用' : '方案已创建')
    emit('saved', saved)
    emit('update:show', false)
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
    :title="editing ? `编辑方案：${editing.name}` : '新建配置方案'"
    style="width: min(560px, 96vw)"
    @update:show="(v: boolean) => emit('update:show', v)"
  >
    <NForm label-placement="top" :show-feedback="false" class="form">
      <NFormItem label="名称">
        <NInput v-model:value="form.name" placeholder="例如：电影 · 核显 HEVC" />
      </NFormItem>
      <NFormItem v-if="isNew" label="方案 id">
        <NInput v-model:value="form.id" placeholder="例如 movies-hevc" />
      </NFormItem>
      <div v-if="isNew" class="hint">写进成品的已处理标记，创建后不能修改；只能用字母、数字、- 和 _。</div>
      <NFormItem label="编码器">
        <NSelect :value="form.encoder" :options="encoderOptions" @update:value="setEncoder" />
      </NFormItem>
      <div class="hint">
        {{ isGpu(form.encoder) ? '占用 GPU 配额' : '占用 CPU 配额' }}
        <template v-if="betterQsv">
          · 这台机器可用 {{ ENCODER_LABEL[betterQsv] }}，同样画质下通常更省空间
          <NButton text type="primary" size="tiny" @click="useEncoder(betterQsv)">改用</NButton>
        </template>
      </div>
      <NFormItem label="质量">
        <NInputNumber v-model:value="form.quality" :min="0" :max="63" :precision="0" style="width: 160px" />
      </NFormItem>
      <div class="hint">{{ qualityHint(form.encoder) }}</div>
      <NFormItem v-if="presetsFor(form.encoder).length" label="预设">
        <NSelect
          :value="form.preset ?? ''"
          :options="presetOptions"
          @update:value="(v: string) => (form.preset = v || null)"
        />
      </NFormItem>
      <NFormItem label="输出封装">
        <NRadioGroup v-model:value="form.container">
          <NRadioButton value="keep">保持原封装</NRadioButton>
          <NRadioButton value="mkv">MKV</NRadioButton>
          <NRadioButton value="mp4">MP4</NRadioButton>
        </NRadioGroup>
      </NFormItem>
      <div v-if="form.container !== 'keep'" class="hint">
        换封装时成品使用新扩展名，部分轨道属性可能无法保留（新建转码的预览会列出）。
      </div>
      <NFormItem label="分辨率">
        <NSelect
          :value="form.maxHeight ?? 0"
          :options="heightOptions"
          @update:value="(v: number) => (form.maxHeight = v || null)"
        />
      </NFormItem>
      <NFormItem label="编码线程（可选）">
        <NInputNumber v-model:value="form.threads" :min="0" :max="64" clearable placeholder="由 FFmpeg 决定" style="width: 200px" />
      </NFormItem>
    </NForm>
    <NAlert v-if="tried && problems.length" type="error" class="problems">
      <div v-for="p in problems" :key="p">{{ p }}</div>
    </NAlert>
    <div v-if="editing" class="hint note">已排队的任务使用入队时的设置，不受影响。</div>
    <template #footer>
      <NSpace justify="end">
        <NButton @click="emit('update:show', false)">取消</NButton>
        <NButton type="primary" :loading="saving" @click="save">保存</NButton>
      </NSpace>
    </template>
  </NModal>
</template>

<style scoped>
.form {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.hint {
  margin-top: -6px;
  font-size: 12px;
  opacity: 0.65;
}
.problems {
  margin-top: 12px;
}
/* Below the form, not under a field: no pull-up towards the input. */
.hint.note {
  margin-top: 12px;
}
</style>
