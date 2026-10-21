<script setup lang="ts">
// 样片推荐, step by step (decisions doc): the source, the profile to start
// from and the presets to compare, the samples, the VMAF target; then the
// samples and trials as they are measured, and the preset and quality to
// save as a profile. `?task=` shows one
// recommendation; without it the wizard and the recent ones are shown.
import { computed, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import {
  NAlert,
  NButton,
  NCard,
  NCheckbox,
  NCheckboxGroup,
  NEmpty,
  NForm,
  NFormItem,
  NInput,
  NInputGroup,
  NInputNumber,
  NRadioButton,
  NRadioGroup,
  NSpace,
  NSpin,
  NStep,
  NSteps,
  NTag,
  useMessage,
} from 'naive-ui'
import { api } from '../api/client'
import type { Preview, Task } from '../api/types'
import PathPicker from '../components/PathPicker.vue'
import ProfileSelect from '../components/ProfileSelect.vue'
import RecommendResult from '../components/RecommendResult.vue'
import { errorMessage } from '../logic/errors'
import { ENCODER_LABEL, STATE_LABEL, STATE_TAG, baseName, outcomeSummary } from '../logic/format'
import { rangeView } from '../logic/range'
import { isSlowPreset, presetsFastFirst } from '../logic/profile'
import {
  defaultPresets,
  emptyForm,
  formProblem,
  presetsText,
  samplesText,
  scoringNote,
  slowPresetsNote,
  sortPresets,
  toRequest,
} from '../logic/recommend'
import { profiles, settings, system } from '../stores/resources'
import { tasks, tasksLoaded } from '../stores/tasks'

const route = useRoute()
const router = useRouter()
const message = useMessage()
system.use()
profiles.use()
settings.use()

const scorer = computed(() => system.data.value?.scorer ?? null)

// ── One recommendation (?task=) ────────────────────────────────────────────
const taskId = computed(() => (route.query.task ? Number(route.query.task) : null))
const loadError = ref<string | null>(null)
const current = computed<Task | undefined>(() => (taskId.value === null ? undefined : tasks.get(taskId.value)))

// Older recommendations are not in the store; fetch once, SSE keeps it current.
watch(
  [taskId, tasksLoaded],
  async ([id, loaded]) => {
    loadError.value = null
    if (id === null || !loaded || tasks.has(id)) return
    try {
      const t = await api.task(id)
      if (!tasks.has(id)) tasks.set(id, t)
    } catch (e) {
      loadError.value = errorMessage(e)
    }
  },
  { immediate: true },
)

function show(id: number | null) {
  void router.push({ query: id === null ? {} : { task: String(id) } })
}

const recent = computed(() =>
  [...tasks.values()]
    .filter((t) => t.type === 'recommend')
    .sort((a, b) => {
      const wait = (t: Task) => (t.state === 'awaiting_choice' ? 0 : t.state === 'running' || t.state === 'queued' ? 1 : 2)
      return wait(a) - wait(b) || b.id - a.id
    })
    .slice(0, 20),
)

// ── The wizard ─────────────────────────────────────────────────────────────
const form = ref(emptyForm())
const step = ref(1)
const picker = ref<'source' | 'sample' | null>(null)
const submitting = ref(false)

// The route of the source with the chosen profile: a recommendation for a
// source that would be skipped is of no use.
const preview = ref<Preview | null>(null)
const previewError = ref<string | null>(null)
const previewing = ref(false)
let generation = 0
watch(
  () => [form.value.path.trim(), form.value.profileId] as const,
  async ([path, profileId]) => {
    const mine = ++generation
    preview.value = null
    previewError.value = null
    if (!path) return
    previewing.value = true
    try {
      const p = await api.preview({ path, profile_id: profileId, force: true })
      if (mine === generation) preview.value = p
    } catch (e) {
      if (mine === generation) previewError.value = errorMessage(e)
    } finally {
      if (mine === generation) previewing.value = false
    }
  },
)

// HDR and Dolby Vision sources are scored through libplacebo.
const range = computed(() => rangeView(preview.value?.plan?.dynamic_range))
const scoring = computed(() => scoringNote(preview.value?.plan?.dynamic_range))
const proxyMissing = computed(() => !!scoring.value && !!scorer.value && !scorer.value.hdr_proxy)
const sourceReady = computed(
  () => !!preview.value && !preview.value.skip && !previewing.value && !proxyMissing.value,
)
const problem = computed(() => formProblem(form.value))

// Presets: the encoder comes from the route, the profile's own preset from
// the saved profile. Both changing resets the choice to the defaults.
const ownPreset = computed(() => {
  const id = form.value.profileId ?? settings.data.value?.server.default_profile
  return (profiles.data.value ?? []).find((p) => p.id === id)?.profile.preset ?? null
})
const encoder = computed(() => preview.value?.plan?.encoder ?? null)
watch(
  [encoder, ownPreset],
  ([e, own]) => {
    form.value.encoder = e
    form.value.presets = e ? defaultPresets(e, own) : []
  },
  { immediate: true },
)
const presetOptions = computed(() => {
  const e = encoder.value
  if (!e) return []
  const own = ownPreset.value
  const all = presetsFastFirst(e)
  const list = own && !all.includes(own) ? [...all, own] : all
  return list.map((p) => ({ value: p, own: p === own, slow: isSlowPreset(e, p) }))
})
function setPresets(v: (string | number)[]) {
  if (encoder.value) form.value.presets = sortPresets(encoder.value, v.map(String))
}
const slowNote = computed(() => slowPresetsNote(form.value))
const planText = computed(() => {
  const p = preview.value?.plan
  if (!p) return ''
  return `${ENCODER_LABEL[p.encoder] ?? p.encoder} · 输出 ${p.width}×${p.height}`
})

function picked(p: string) {
  if (picker.value === 'source') form.value.path = p
  else if (picker.value === 'sample' && !form.value.files.includes(p)) form.value.files.push(p)
}

async function submit() {
  submitting.value = true
  try {
    const r = await api.createTask(toRequest(form.value))
    const id = r.tasks[0]?.id
    message.success(`样片推荐已加入队列（任务 #${id}）`)
    form.value = emptyForm()
    step.value = 1
    if (id !== undefined) show(id)
  } catch (e) {
    message.error(errorMessage(e), { duration: 6000 })
  } finally {
    submitting.value = false
  }
}

const STEPS = ['原片与方案', '样片', '目标', '试编码与评分', '选择候选']
const resultStep = computed(() => {
  const s = current.value?.state
  if (s === 'awaiting_choice' || s === 'chosen') return 5
  return 4
})
const resultStatus = computed(() => {
  const s = current.value?.state
  if (s === 'failed' || s === 'cancelled' || s === 'skipped') return 'error'
  if (s === 'chosen') return 'finish'
  return 'process'
})
</script>

<template>
  <div class="page">
    <NAlert v-if="scorer && !scorer.available" type="error" class="note">
      <template #header>评分工具不可用，暂时不能发起样片推荐</template>
      {{ scorer.ffmpeg }}：{{ scorer.error }}。请在「设置与诊断」里填写评分运行时的 ffmpeg（带 libvmaf）。
    </NAlert>

    <!-- One recommendation -->
    <template v-if="taskId !== null">
      <NSteps :current="resultStep" :status="resultStatus" size="small" class="steps">
        <NStep v-for="s in STEPS" :key="s" :title="s" />
      </NSteps>
      <NButton text class="back" @click="show(null)">← 新建推荐 / 全部推荐</NButton>
      <NAlert v-if="loadError" type="error" :title="loadError" />
      <NCard v-else-if="current" size="small">
        <RecommendResult :task="current" />
      </NCard>
      <NSpin v-else :show="true"><div class="placeholder" /></NSpin>
    </template>

    <!-- The wizard -->
    <template v-else>
      <NSteps :current="step" size="small" class="steps">
        <NStep v-for="s in STEPS" :key="s" :title="s" />
      </NSteps>

      <NCard size="small" class="card">
        <NForm label-placement="top" :show-feedback="false" class="form">
          <template v-if="step === 1">
            <NFormItem label="原片">
              <NInputGroup>
                <NInput v-model:value="form.path" clearable placeholder="/vol1/影视/电影.mkv" />
                <NButton @click="picker = 'source'">浏览…</NButton>
              </NInputGroup>
            </NFormItem>
            <NFormItem label="从哪个配置方案开始">
              <ProfileSelect v-model:value="form.profileId" clearable />
            </NFormItem>
            <div class="hint">编码器、封装和分辨率沿用这个方案；预设不沿用，在下面选几个对比，结果出来后再选预设和质量。</div>
            <NSpin :show="previewing">
              <NAlert v-if="previewError" type="error" :title="previewError" class="note" />
              <NAlert v-else-if="preview?.skip" type="warning" class="note">
                <template #header>按这个方案不能转码这个原片</template>
                {{ preview.skip.message }}。推荐的结论用不上，换一个方案或原片。
              </NAlert>
              <template v-else-if="preview?.plan">
                <NAlert type="success" :title="planText" class="note">
                  <template v-if="range">
                    <NTag size="small" :type="range.type" :bordered="false">{{ range.tag }}</NTag>
                    {{ scoring }}
                  </template>
                </NAlert>
                <NAlert v-if="proxyMissing" type="error" class="note">
                  <template #header>HDR 显示代理不可用，不能为这个原片推荐</template>
                  {{ scorer?.hdr_proxy_error }}。评分运行时的 ffmpeg 需要 libplacebo 和可用的 Vulkan 设备。
                </NAlert>
              </template>
            </NSpin>
            <template v-if="preview?.plan && !preview.skip">
              <NFormItem v-if="presetOptions.length" label="对比哪些预设">
                <NCheckboxGroup :value="form.presets" @update:value="setPresets">
                  <NSpace :size="[14, 6]">
                    <NCheckbox v-for="o in presetOptions" :key="o.value" :value="o.value">
                      {{ o.value }}<span v-if="o.own" class="dim">（方案当前）</span><span v-if="o.slow" class="dim">（慢）</span>
                    </NCheckbox>
                  </NSpace>
                </NCheckboxGroup>
              </NFormItem>
              <div class="hint">
                <template v-if="presetOptions.length">
                  从快到慢排列。每个预设各搜一遍质量值，样片共用；越慢的预设通常越省空间、越费时。后面的预设从前一个找到的质量值附近开始，一般只要两三组试编码。只勾一个就只用这个预设。
                </template>
                <template v-else>{{ presetsText(form) }}。</template>
              </div>
              <NAlert v-if="slowNote" type="warning" :title="slowNote" class="note" />
            </template>
          </template>

          <template v-else-if="step === 2">
            <NFormItem label="样片">
              <NRadioGroup v-model:value="form.sampleMode">
                <NRadioButton value="auto">从原片自动挑选</NRadioButton>
                <NRadioButton value="files">用现成的样片文件</NRadioButton>
              </NRadioGroup>
            </NFormItem>
            <template v-if="form.sampleMode === 'auto'">
              <NFormItem label="怎么挑">
                <NRadioGroup v-model:value="form.method">
                  <NRadioButton value="bitrate">按码率（快）</NRadioButton>
                  <NRadioButton value="scene">按场景分析（慢）</NRadioButton>
                </NRadioGroup>
              </NFormItem>
              <div class="hint">
                <template v-if="form.method === 'bitrate'">
                  只读数据包，不解码。挑码率最高的片段（最难压缩），再加上码率居中和偏低的片段；避开片头片尾各 5%。
                </template>
                <template v-else>
                  解码全片，按细节、运动、明暗反差和亮度挑复杂又彼此不同的场景。长片要多花十几到几十分钟。
                </template>
              </div>
              <NSpace :size="24">
                <NFormItem label="数量">
                  <NInputNumber v-model:value="form.count" :min="1" :max="8" :precision="0" class="num" />
                </NFormItem>
                <NFormItem label="每个长度（秒）">
                  <NInputNumber v-model:value="form.secs" :min="2" :max="60" :precision="0" class="num" />
                </NFormItem>
              </NSpace>
            </template>
            <template v-else>
              <NFormItem label="样片文件">
                <NSpace vertical :size="6" class="files">
                  <div v-for="(f, i) in form.files" :key="f" class="file">
                    <span class="mono">{{ f }}</span>
                    <NButton text type="error" @click="form.files.splice(i, 1)">移除</NButton>
                  </div>
                  <NButton size="small" :disabled="form.files.length >= 8" @click="picker = 'sample'">添加样片…</NButton>
                </NSpace>
              </NFormItem>
              <div class="hint">样片要和原片同分辨率、同位深和色度采样，并且是 SDR。</div>
            </template>
          </template>

          <template v-else>
            <NFormItem label="VMAF 目标">
              <NInputNumber v-model:value="form.target" :min="50" :max="100" :step="0.5" class="num" />
            </NFormItem>
            <div class="hint">
              每个样片的平均 VMAF 都要达到目标；候选按「最低样片分」比较。95 接近看不出差别，93 更省空间。
            </div>
            <div class="summary">
              <div><span class="k">原片</span>{{ form.path }}</div>
              <div><span class="k">方案</span>{{ planText || '默认方案' }}</div>
              <div><span class="k">预设</span>{{ presetsText(form) }}</div>
              <div><span class="k">样片</span>{{ samplesText(form) }}</div>
              <div class="dim">
                推荐任务占一个 CPU 配额，一次只运行一个；对比的预设越多、越慢，耗时越长。结论只覆盖被测样片，不是整片的画质保证。等待选择期间，自动监控暂缓处理这个原片。
              </div>
            </div>
          </template>
        </NForm>

        <div class="nav">
          <NButton v-if="step > 1" @click="step--">上一步</NButton>
          <NButton
            v-if="step < 3"
            type="primary"
            :disabled="step === 1 && (!sourceReady || problem === '至少选择一个预设')"
            @click="step++"
          >
            下一步
          </NButton>
          <NButton
            v-else
            type="primary"
            :disabled="!!problem || !sourceReady || !scorer?.available"
            :loading="submitting"
            @click="submit"
          >
            开始推荐
          </NButton>
          <span v-if="step === 3 && problem" class="dim">{{ problem }}</span>
        </div>
      </NCard>

      <NCard title="最近的推荐" size="small" class="card">
        <NEmpty v-if="!recent.length" description="还没有样片推荐" />
        <div v-for="t in recent" :key="t.id" class="row" @click="show(t.id)">
          <span class="row-name" :title="t.path">{{ baseName(t.path) }}</span>
          <NTag size="small" :type="STATE_TAG[t.state]" :bordered="false">{{ STATE_LABEL[t.state] }}</NTag>
          <span class="row-summary">{{ outcomeSummary(t) }}</span>
        </div>
      </NCard>

      <PathPicker
        :show="picker !== null"
        mode="file"
        :start="picker === 'source' ? form.path : form.files[form.files.length - 1] ?? form.path"
        @update:show="(v: boolean) => !v && (picker = null)"
        @pick="picked"
      />
    </template>
  </div>
</template>

<style scoped>
.page {
  max-width: 900px;
}
.steps {
  margin-bottom: 16px;
}
.back {
  margin-bottom: 10px;
}
.card + .card {
  margin-top: 16px;
}
.form {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.note {
  margin-top: 6px;
}
.hint {
  font-size: 12px;
  opacity: 0.65;
}
.num {
  width: 140px;
}
.files {
  width: 100%;
}
.file {
  display: flex;
  gap: 10px;
  align-items: center;
}
.mono {
  font-family: ui-monospace, monospace;
  font-size: 12px;
  word-break: break-all;
}
.summary {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 13px;
}
.k {
  display: inline-block;
  width: 3em;
  opacity: 0.6;
}
.dim {
  font-size: 12px;
  opacity: 0.65;
}
.nav {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 18px;
}
.row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 6px 0;
  cursor: pointer;
  border-bottom: 1px solid rgba(128, 128, 128, 0.15);
}
.row:last-child {
  border-bottom: none;
}
.row-name {
  font-weight: 600;
  max-width: 280px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.row-summary {
  flex: 1;
  min-width: 0;
  font-size: 13px;
  opacity: 0.8;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.placeholder {
  min-height: 120px;
}
</style>
