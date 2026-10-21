<script setup lang="ts">
// Picks a saved profile; empty means the server's default profile.
import { computed } from 'vue'
import { NSelect } from 'naive-ui'
import type { SavedProfile } from '../api/types'
import { ENCODER_LABEL } from '../logic/format'
import { profiles, settings, system } from '../stores/resources'

defineProps<{ value: string | null; clearable?: boolean }>()
const emit = defineEmits<{ 'update:value': [v: string | null] }>()

profiles.use()
settings.use()
system.use()

const unavailable = computed(() => {
  // Without the startup trial encode every encoder counts as available.
  const probes = system.data.value?.encoders ?? []
  return new Set(probes.filter((e) => !e.available).map((e) => e.encoder))
})
const defaultId = computed(() => settings.data.value?.server.default_profile)

function label(p: SavedProfile): string {
  const parts = [`${p.name}`, `${ENCODER_LABEL[p.profile.encoder] ?? p.profile.encoder} · 质量 ${p.profile.quality}`]
  if (p.id === defaultId.value) parts.push('默认')
  if (unavailable.value.has(p.profile.encoder)) parts.push('编码器不可用')
  return parts.join(' · ')
}

const options = computed(() =>
  (profiles.data.value ?? []).map((p) => ({ label: label(p), value: p.id })),
)
</script>

<template>
  <NSelect
    :value="value"
    :options="options"
    :loading="profiles.loading.value && !profiles.data.value"
    :clearable="clearable"
    :placeholder="defaultId ? `默认方案（${defaultId}）` : '默认方案'"
    @update:value="(v: string | null) => emit('update:value', v)"
  />
</template>
