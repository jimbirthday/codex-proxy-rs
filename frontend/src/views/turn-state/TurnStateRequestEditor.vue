<script setup lang="ts">
import type { TurnStateProbeRequest } from '@/api/modules/turn-state-policy'
import { ChevronDown, Plus, Trash2 } from '@lucide/vue'
import { ref, watch } from 'vue'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseNumberInput from '@/components/base/BaseNumberInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseTextarea from '@/components/base/BaseTextarea.vue'

const { disabled } = defineProps<{ title: string, disabled: boolean }>()
const emit = defineEmits<{ validity: [valid: boolean] }>()
const request = defineModel<TurnStateProbeRequest>({ required: true })
const text = ref('')
const error = ref('')
watch(() => request.value.body, (body) => {
  text.value = JSON.stringify(body ?? {
    ...request.value.extraBody,
    model: '$model',
    input: [{ role: 'user', content: [{ type: 'input_text', text: request.value.inputText }] }],
    stream: request.value.stream,
    store: request.value.store,
    ...(request.value.instructions ? { instructions: request.value.instructions } : {}),
    ...(request.value.reasoningEffort ? { reasoning: { effort: request.value.reasoningEffort } } : {}),
    ...(request.value.parallelToolCalls !== null ? { parallel_tool_calls: request.value.parallelToolCalls } : {}),
    include: request.value.include,
    ...(request.value.serviceTier ? { service_tier: request.value.serviceTier } : {}),
  }, null, 2)
  error.value = ''
  emit('validity', true)
}, { immediate: true, deep: true })
function update(value: string) {
  text.value = value
  try {
    const body: unknown = JSON.parse(value)
    if (!body || Array.isArray(body) || typeof body !== 'object')
      throw new Error('请求体必须是 JSON 对象')
    request.value.body = body as Record<string, unknown>
    error.value = ''
    emit('validity', true)
  }
  catch {
    error.value = '请输入合法的 JSON 对象'
    emit('validity', false)
  }
}
</script>

<template>
  <details class="group rounded-cp-lg bg-cp-fill-quaternary">
    <summary class="flex min-h-11 cursor-pointer list-none items-center justify-between gap-3 px-3.5 py-3 text-cp-sm font-bold text-cp-text">
      {{ title }}
      <ChevronDown class="size-4 transition-transform group-open:rotate-180 motion-reduce:transition-none" />
    </summary>
    <div class="grid gap-4 px-3.5 pb-4">
      <BaseFormItem label="完整请求体" description="支持任意 JSON 字段，$model 自动替换为本轮选定模型" :error="error">
        <BaseTextarea :model-value="text" class="font-mono" :rows="16" :disabled="disabled" @update:model-value="update" />
      </BaseFormItem>
      <div class="grid gap-4 sm:grid-cols-3">
        <BaseFormItem label="压缩方式">
          <BaseSelect v-model="request.compression" :options="[{ value: 'none', label: '不压缩' }, { value: 'zstd', label: 'Zstandard' }]" :disabled="disabled" />
        </BaseFormItem>
        <BaseFormItem label="压缩级别">
          <BaseNumberInput v-model="request.compressionLevel" label="压缩级别" :min="-7" :max="22" :disabled="disabled || request.compression === 'none'" />
        </BaseFormItem>
        <BaseFormItem label="响应正文上限">
          <BaseNumberInput v-model="request.maxResponseBodyBytes" label="响应正文上限" unit="字节" :min="1024" :max="1048576" :disabled="disabled" />
        </BaseFormItem>
      </div>
      <div class="flex items-center justify-between gap-3">
        <span class="text-cp-sm font-bold text-cp-text-secondary">请求头覆盖</span>
        <BaseButton size="sm" variant="ghost" :disabled="disabled || request.extraHeaders.length >= 32" @click="request.extraHeaders.push({ name: '', value: '' })">
          <template #icon>
            <Plus class="size-3.5" />
          </template>
          添加
        </BaseButton>
      </div>
      <div v-for="(header, index) in request.extraHeaders" :key="index" class="grid gap-2 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]">
        <BaseInput v-model="header.name" aria-label="请求头名称" placeholder="请求头名称" :disabled="disabled" />
        <BaseInput v-model="header.value" aria-label="请求头值" placeholder="请求头值" :disabled="disabled" />
        <BaseButton size="sm" variant="ghost" :disabled="disabled" @click="request.extraHeaders.splice(index, 1)">
          <template #icon>
            <Trash2 class="size-3.5" />
          </template>
          删除
        </BaseButton>
      </div>
    </div>
  </details>
</template>
