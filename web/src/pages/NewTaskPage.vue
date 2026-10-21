<script setup lang="ts">
// New transcode: one file or a folder batch. The server previews before
// anything is queued — route, expected losses, protective skips, earlier
// attempts for a file; queued and skipped files for a folder.
import { computed, h, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import {
  NAlert,
  NButton,
  NCheckbox,
  NCollapse,
  NCollapseItem,
  NForm,
  NFormItem,
  NInput,
  NInputGroup,
  NRadioButton,
  NRadioGroup,
  NSpace,
  NSpin,
  useDialog,
  useMessage,
} from 'naive-ui'
import { api } from '../api/client'
import type { BatchPreview, Preview } from '../api/types'
import PathPicker from '../components/PathPicker.vue'
import ProfileSelect from '../components/ProfileSelect.vue'
import RoutePlanView from '../components/RoutePlanView.vue'
import { errorMessage, isApiError } from '../logic/errors'
import { adviseOnPreview, attemptLine, groupSkipped, relativeTo } from '../logic/preview'
import { SPACE_NOTE, estimateLine, shortEstimates } from '../logic/space'

type Mode = 'file' | 'dir'

const router = useRouter()
const message = useMessage()
const dialog = useDialog()

const mode = ref<Mode>('file')
const path = ref('')
const dir = ref('')
const profileId = ref<string | null>(null)
const force = ref(false)
const recursive = ref(true)
const includeAttempted = ref(false)
const picker = ref(false)

const filePreview = ref<Preview | null>(null)
const batchPreview = ref<BatchPreview | null>(null)
const previewError = ref<string | null>(null)
const previewing = ref(false)
const submitting = ref(false)

const target = computed(() => (mode.value === 'file' ? path.value : dir.value).trim())

// Previews follow the form, debounced; a late answer for an old input is dropped.
let generation = 0
let timer: ReturnType<typeof setTimeout> | undefined
watch(
  [mode, target, profileId, force, recursive, includeAttempted],
  () => {
    clearTimeout(timer)
    generation++
    filePreview.value = null
    batchPreview.value = null
    previewError.value = null
    if (!target.value) return
    previewing.value = true
    timer = setTimeout(runPreview, 500)
  },
)

async function runPreview() {
  const mine = generation
  try {
    if (mode.value === 'file') {
      const p = await api.preview({ path: target.value, profile_id: profileId.value, force: force.value })
      if (mine === generation) filePreview.value = p
    } else {
      const p = await api.previewBatch({
        dir: target.value,
        profile_id: profileId.value,
        recursive: recursive.value,
        force: force.value,
        include_attempted: includeAttempted.value,
      })
      if (mine === generation) batchPreview.value = p
    }
  } catch (e) {
    if (mine === generation) previewError.value = errorMessage(e)
  } finally {
    if (mine === generation) previewing.value = false
  }
}

const advice = computed(() => (filePreview.value ? adviseOnPreview(filePreview.value, force.value) : null))
const skipGroups = computed(() => (batchPreview.value ? groupSkipped(batchPreview.value.skipped) : []))
// Volumes the batch may not fit on: its tasks would wait in the queue.
const shortSpace = computed(() => (batchPreview.value ? shortEstimates(batchPreview.value.space) : []))

function confirmSubmit() {
  const batch = mode.value === 'dir'
  const short = batch ? shortSpace.value : advice.value?.shortSpace ? [advice.value.shortSpace] : []
  if (short.length === 0) {
    void submit()
    return
  }
  dialog.warning({
    title: '存储空间可能不足',
    content: () =>
      h('div', [
        ...short.map((e) => h('div', estimateLine(e, batch))),
        h('div', { style: 'margin-top: 8px' }, SPACE_NOTE),
        h('div', { style: 'margin-top: 8px' }, '仍要加入队列吗？'),
      ]),
    positiveText: '仍然加入',
    negativeText: '取消',
    onPositiveClick: () => void submit(),
  })
}

const canSubmit = computed(() => {
  if (previewing.value || submitting.value) return false
  if (mode.value === 'file') return !!advice.value?.canSubmit
  return (batchPreview.value?.files.length ?? 0) > 0
})
const submitLabel = computed(() =>
  mode.value === 'file' ? '开始转码' : `加入队列（${batchPreview.value?.files.length ?? 0} 个文件）`,
)

async function submit(withForce = force.value) {
  submitting.value = true
  try {
    if (mode.value === 'file') {
      const r = await api.createTask({ source: 'manual', path: target.value, profile_id: profileId.value, force: withForce })
      message.success(`已加入队列（任务 #${r.tasks[0]?.id}）`)
      void router.push('/tasks')
    } else {
      const r = await api.createTask({
        source: 'folder_batch',
        dir: target.value,
        profile_id: profileId.value,
        recursive: recursive.value,
        force: withForce,
        include_attempted: includeAttempted.value,
      })
      const skipped = r.skipped.length ? `，跳过 ${r.skipped.length} 个` : ''
      message.success(`批次 #${r.batch_id}：${r.tasks.length} 个文件已加入队列${skipped}`)
      void router.push({ path: '/tasks', query: r.batch_id ? { batch: String(r.batch_id) } : {} })
    }
  } catch (e) {
    // The preview should have caught it; the file may have changed since.
    if (isApiError(e, 'already_processed') && !withForce) {
      dialog.warning({
        title: '这个文件已被处理过',
        content: `${e.message}。仍要处理吗？`,
        positiveText: '仍然处理',
        negativeText: '取消',
        onPositiveClick: () => {
          force.value = true
          void submit(true)
        },
      })
    } else {
      message.error(errorMessage(e), { duration: 6000 })
    }
  } finally {
    submitting.value = false
  }
}

function picked(p: string) {
  if (mode.value === 'file') path.value = p
  else dir.value = p
}
</script>

<template>
  <div class="page">
    <NForm label-placement="top" :show-feedback="false" class="form">
      <NFormItem label="转码对象">
        <NRadioGroup v-model:value="mode">
          <NRadioButton value="file">单个文件</NRadioButton>
          <NRadioButton value="dir">整个文件夹</NRadioButton>
        </NRadioGroup>
      </NFormItem>
      <NFormItem :label="mode === 'file' ? '视频文件' : '文件夹'">
        <NInputGroup>
          <NInput
            v-if="mode === 'file'"
            v-model:value="path"
            clearable
            placeholder="/vol1/影视/电影.mkv"
          />
          <NInput v-else v-model:value="dir" clearable placeholder="/vol1/影视/剧集" />
          <NButton @click="picker = true">浏览…</NButton>
        </NInputGroup>
      </NFormItem>
      <NFormItem label="配置方案">
        <ProfileSelect v-model:value="profileId" clearable />
      </NFormItem>
      <NFormItem v-if="mode === 'dir'" label="选项">
        <NSpace vertical :size="6">
          <NCheckbox v-model:checked="recursive">包含子文件夹</NCheckbox>
          <NCheckbox v-model:checked="includeAttempted">包含同一方案尝试过的文件</NCheckbox>
          <NCheckbox v-model:checked="force">强制处理已处理过的文件</NCheckbox>
        </NSpace>
      </NFormItem>
      <NFormItem v-else-if="advice?.needsForce || force" label="选项">
        <NCheckbox v-model:checked="force">强制处理（忽略已处理标记）</NCheckbox>
      </NFormItem>
    </NForm>

    <div v-if="target" class="preview">
      <NSpin :show="previewing">
        <NAlert v-if="previewError" type="error" :title="previewError" />

        <template v-else-if="filePreview && advice">
          <NAlert v-for="n in advice.notes" :key="n.title" :type="n.kind" :title="n.title" class="note">
            {{ n.text }}
          </NAlert>
          <div v-if="filePreview.plan" class="block">
            <div class="block-title">将会这样处理</div>
            <RoutePlanView :plan="filePreview.plan" />
          </div>
          <NCollapse v-if="filePreview.attempts.length" class="block">
            <NCollapseItem :title="`以往的尝试（${filePreview.attempts.length}）`" name="a">
              <div v-for="a in filePreview.attempts" :key="a.id" class="line">
                {{ attemptLine(a, filePreview.fingerprint) }}
              </div>
            </NCollapseItem>
          </NCollapse>
        </template>

        <template v-else-if="batchPreview">
          <NAlert :type="batchPreview.files.length ? 'success' : 'warning'" class="note">
            <template #header>
              将加入 {{ batchPreview.files.length }} 个文件<template v-if="batchPreview.skipped.length">，跳过 {{ batchPreview.skipped.length }} 个</template>
            </template>
            加入队列的文件运行时仍会逐个判断路线，不支持的会被保护性跳过。
          </NAlert>
          <NAlert v-if="shortSpace.length" type="warning" title="存储空间可能不足" class="note">
            <div v-for="e in shortSpace" :key="e.volume">{{ estimateLine(e) }}</div>
            <div class="why">{{ SPACE_NOTE }}</div>
          </NAlert>
          <NCollapse class="block">
            <NCollapseItem v-if="batchPreview.files.length" :title="`将加入的文件（${batchPreview.files.length}）`" name="files">
              <div v-for="f in batchPreview.files" :key="f" class="line mono">{{ relativeTo(batchPreview.dir, f) }}</div>
            </NCollapseItem>
            <NCollapseItem v-for="g in skipGroups" :key="g.code" :title="`${g.label}（${g.files.length}）`" :name="g.code">
              <div v-for="s in g.files" :key="s.path" class="line">
                <span class="mono">{{ relativeTo(batchPreview.dir, s.path) }}</span>
                <span v-if="s.message" class="why"> — {{ s.message }}</span>
              </div>
            </NCollapseItem>
          </NCollapse>
        </template>

        <div v-else class="placeholder" />
      </NSpin>
    </div>

    <div class="submit">
      <NButton type="primary" size="large" :disabled="!canSubmit" :loading="submitting" @click="confirmSubmit">
        {{ submitLabel }}
      </NButton>
    </div>

    <PathPicker v-model:show="picker" :mode="mode" :start="target" @pick="picked" />
  </div>
</template>

<style scoped>
.page {
  max-width: 760px;
}
.form {
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.preview {
  margin-top: 20px;
}
.note {
  margin-bottom: 10px;
}
.block {
  margin-top: 12px;
}
.block-title {
  font-weight: 600;
  margin-bottom: 6px;
}
.line {
  font-size: 13px;
  padding: 2px 0;
  word-break: break-all;
}
.mono {
  font-family: ui-monospace, monospace;
  font-size: 12px;
}
.why {
  opacity: 0.7;
}
.placeholder {
  min-height: 60px;
}
.submit {
  margin-top: 20px;
}
</style>
