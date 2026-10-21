<script setup lang="ts">
// Add or edit a monitored directory.
import { ref, watch } from 'vue'
import { NButton, NCheckbox, NForm, NFormItem, NInput, NInputGroup, NModal, NSpace, useMessage } from 'naive-ui'
import { api } from '../api/client'
import type { MonitorDir } from '../api/types'
import { errorMessage } from '../logic/errors'
import { emptyMonitorForm, formToMonitor, monitorToForm, type MonitorForm } from '../logic/profile'
import PathPicker from './PathPicker.vue'
import ProfileSelect from './ProfileSelect.vue'

const props = defineProps<{ show: boolean; editing: MonitorDir | null }>()
const emit = defineEmits<{ 'update:show': [v: boolean] }>()
const message = useMessage()

const form = ref<MonitorForm>(emptyMonitorForm())
const picker = ref(false)
const saving = ref(false)

watch(
  () => props.show,
  (show) => {
    if (show) form.value = props.editing ? monitorToForm(props.editing) : emptyMonitorForm()
  },
)

async function save() {
  if (!form.value.path.trim()) {
    message.error('请选择要监控的文件夹')
    return
  }
  saving.value = true
  try {
    const body = formToMonitor(form.value)
    if (props.editing) await api.putMonitor(props.editing.id, body)
    else await api.createMonitor(body)
    message.success(props.editing ? '已保存，监控立即按新设置运行' : '已添加，马上开始扫描')
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
    :title="editing ? '编辑监控目录' : '添加监控目录'"
    style="width: min(560px, 96vw)"
    @update:show="(v: boolean) => emit('update:show', v)"
  >
    <NForm label-placement="top" :show-feedback="false" class="form">
      <NFormItem label="文件夹">
        <NInputGroup>
          <NInput v-model:value="form.path" placeholder="/vol1/影视/下载" />
          <NButton @click="picker = true">浏览…</NButton>
        </NInputGroup>
      </NFormItem>
      <NFormItem label="配置方案">
        <ProfileSelect v-model:value="form.profileId" clearable />
      </NFormItem>
      <NFormItem label="选项">
        <NSpace vertical :size="6">
          <NCheckbox v-model:checked="form.enabled">启用</NCheckbox>
          <NCheckbox v-model:checked="form.recursive">包含子文件夹</NCheckbox>
        </NSpace>
      </NFormItem>
      <NFormItem label="排除规则（每行一个，可选）">
        <NInput
          v-model:value="form.excludes"
          type="textarea"
          :autosize="{ minRows: 2, maxRows: 6 }"
          placeholder="**/Extras/**&#10;*.sample.mkv"
        />
      </NFormItem>
      <div class="hint">
        相对这个文件夹匹配。目录嵌套时由最深的监控目录决定怎么处理；停用一个子目录就能排除整棵子树。
      </div>
    </NForm>
    <template #footer>
      <NSpace justify="end">
        <NButton @click="emit('update:show', false)">取消</NButton>
        <NButton type="primary" :loading="saving" @click="save">保存</NButton>
      </NSpace>
    </template>
    <PathPicker v-model:show="picker" mode="dir" :start="form.path" @pick="(p) => (form.path = p)" />
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
</style>
