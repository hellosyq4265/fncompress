<script setup lang="ts">
// Browse the NAS (`GET /api/fs`) and pick a video file or a directory, so
// paths need not be typed. Starts where the last pick was.
import { computed, ref, watch } from 'vue'
import { NAlert, NButton, NEmpty, NModal, NSpin } from 'naive-ui'
import { api } from '../api/client'
import type { Listing } from '../api/types'
import { errorMessage } from '../logic/errors'
import { dirName, formatBytes } from '../logic/format'
import { emptyView } from '../logic/picker'

const props = defineProps<{ show: boolean; mode: 'file' | 'dir'; start?: string }>()
const emit = defineEmits<{ 'update:show': [v: boolean]; pick: [path: string] }>()

const LAST_DIR = 'fncompress.lastDir'
const remembered = () => {
  try {
    return localStorage.getItem(LAST_DIR) ?? undefined
  } catch {
    return undefined
  }
}
const remember = (dir: string) => {
  try {
    localStorage.setItem(LAST_DIR, dir)
  } catch {
    // Private mode: nothing to remember.
  }
}

const listing = ref<Listing | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)

async function open(path?: string) {
  loading.value = true
  error.value = null
  try {
    listing.value = await api.fs(path)
  } catch (e) {
    error.value = errorMessage(e)
    // A remembered or typed path that went away: fall back to the roots
    // (authorized directories, then the readable volumes).
    if (path && !listing.value) listing.value = await api.fs().catch(() => null)
  } finally {
    loading.value = false
  }
}

watch(
  () => props.show,
  (show) => {
    if (!show) return
    const start = props.start?.trim()
    // A file path starts in its directory.
    const from = start ? (props.mode === 'file' && /\.\w{2,4}$/.test(start) ? dirName(start) : start) : remembered()
    listing.value = null
    void open(from)
  },
  { immediate: true },
)

const atRoots = computed(() => listing.value?.path === '')
const empty = computed(() => (listing.value ? emptyView(listing.value, props.mode) : null))

function pick(path: string) {
  remember(props.mode === 'dir' ? path : dirName(path))
  emit('pick', path)
  emit('update:show', false)
}
</script>

<template>
  <NModal
    :show="show"
    preset="card"
    :title="mode === 'file' ? '选择视频文件' : '选择文件夹'"
    style="width: min(680px, 96vw)"
    :segmented="{ content: true, footer: 'soft' }"
    @update:show="(v: boolean) => emit('update:show', v)"
  >
    <div class="bar">
      <NButton size="small" :disabled="!listing || atRoots" @click="open(listing?.parent ?? undefined)">上一级</NButton>
      <NButton size="small" quaternary :disabled="atRoots" @click="open()">起始位置</NButton>
      <!-- rtl cuts long paths on the left; the inner span keeps the slashes in place -->
      <span class="where" :title="listing?.path"><span class="ltr">{{ atRoots ? '授权目录与存储卷' : listing?.path }}</span></span>
    </div>
    <NAlert v-if="error" type="error" :title="error" class="gap" />
    <NSpin :show="loading">
      <div class="list">
        <template v-if="listing">
          <button v-for="d in listing.dirs" :key="d.path" class="row dir" @click="open(d.path)">
            <span class="icon">📁</span><span class="name">{{ d.name }}</span>
          </button>
          <button
            v-for="f in listing.files"
            :key="f.path"
            class="row file"
            :disabled="mode !== 'file'"
            @click="pick(f.path)"
          >
            <span class="icon">🎞</span><span class="name">{{ f.name }}</span><span class="size">{{ formatBytes(f.size) }}</span>
          </button>
          <NAlert v-if="empty?.kind === 'authorize'" type="warning" :title="empty.text" class="gap" />
          <NEmpty v-else-if="empty" :description="empty.text" class="gap" />
          <div v-if="listing.truncated" class="note">条目太多，只列出了前 5000 个</div>
        </template>
      </div>
    </NSpin>
    <template v-if="mode === 'dir'" #footer>
      <div class="foot">
        <NButton type="primary" :disabled="!listing || atRoots" @click="listing && pick(listing.path)">
          选择这个文件夹
        </NButton>
      </div>
    </template>
  </NModal>
</template>

<style scoped>
.bar {
  display: flex;
  align-items: center;
  gap: 8px;
}
.where {
  flex: 1;
  min-width: 0;
  font-family: ui-monospace, monospace;
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  direction: rtl;
  text-align: left;
}
.ltr {
  direction: ltr;
  unicode-bidi: isolate;
}
.gap {
  margin-top: 12px;
}
.list {
  margin-top: 10px;
  max-height: min(56vh, 520px);
  min-height: 120px;
  overflow-y: auto;
}
.row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 8px 6px;
  border: 0;
  border-radius: 4px;
  background: none;
  color: inherit;
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.row:hover:not(:disabled) {
  background: rgba(128, 128, 128, 0.14);
}
.row:disabled {
  cursor: default;
  opacity: 0.45;
}
.name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.size {
  font-size: 12px;
  opacity: 0.6;
}
.note {
  padding: 8px 6px;
  font-size: 12px;
  opacity: 0.7;
}
.foot {
  display: flex;
  justify-content: flex-end;
}
</style>
