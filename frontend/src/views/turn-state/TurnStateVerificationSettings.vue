<script setup lang="ts">
import type { TurnStateSuccessRules, TurnStateVerification } from '@/api/modules/turn-state-policy'
import { computed, reactive, watch } from 'vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseNumberInput from '@/components/base/BaseNumberInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import BaseTextarea from '@/components/base/BaseTextarea.vue'

const props = defineProps<{ disabled: boolean, candidateLimit: number, budgetLimit: number }>()
const emit = defineEmits<{ validity: [valid: boolean] }>()
const value = defineModel<TurnStateVerification>({ required: true })
const ruleText = reactive({ mintSuccess: '', reuseSuccess: '' })
const errors = reactive({ mintSuccess: '', reuseSuccess: '' })
const stages = [{ key: 'mintSuccess', label: '铸票成功条件' }, { key: 'reuseSuccess', label: '复用成功条件' }] as const
for (const { key } of stages) {
  watch(() => value.value[key], (rule) => {
    ruleText[key] = JSON.stringify(rule, null, 2)
    errors[key] = ''
    emit('validity', !errors.mintSuccess && !errors.reuseSuccess)
  }, { immediate: true, deep: true })
}
const cookieNames = computed({
  get: () => value.value.requiredCookieNames.join(', '),
  set: text => value.value.requiredCookieNames = text.split(',').map(v => v.trim()).filter(Boolean),
})
const requestsPerCandidate = computed(() => value.value.mode === 'acquire_only' ? 1 : 1 + value.value.reuseCount)
function updateRule(key: 'mintSuccess' | 'reuseSuccess', text: string) {
  ruleText[key] = text
  try {
    const rule = JSON.parse(text) as TurnStateSuccessRules
    if (!rule || !Number.isInteger(rule.statusMin) || !Number.isInteger(rule.statusMax)
      || typeof rule.requireCompleted !== 'boolean' || typeof rule.requireModelMatch !== 'boolean'
      || !Array.isArray(rule.expectedModels) || !Array.isArray(rule.jsonPredicates)) {
      throw new Error('条件格式不正确')
    }
    value.value[key] = rule
    errors[key] = ''
  }
  catch { errors[key] = '请输入完整合法的成功条件' }
  emit('validity', !errors.mintSuccess && !errors.reuseSuccess)
}
</script>

<template>
  <section class="grid gap-4" aria-label="铸票与复用验证">
    <div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
      <BaseFormItem label="获取模式">
        <BaseSelect v-model="value.mode" :disabled="disabled" :options="[{ value: 'mint_and_validate', label: '铸票并连续复用验证' }, { value: 'acquire_only', label: '仅获取 State' }]" />
      </BaseFormItem>
      <BaseFormItem label="连续复用次数">
        <BaseNumberInput v-model="value.reuseCount" label="连续复用次数" :min="1" :max="10" :disabled="disabled || value.mode === 'acquire_only'" />
      </BaseFormItem>
      <BaseFormItem label="业务代理">
        <BaseSelect v-model="value.businessProxy" :disabled="disabled" :options="[{ value: 'follow_verified', label: '新会话跟随验证成功的代理' }, { value: 'match_account', label: '仅匹配账号当前代理' }]" />
      </BaseFormItem>
      <BaseFormItem label="铸票到首次复用间隔">
        <BaseNumberInput v-model="value.mintToReuseDelayMilliseconds" label="铸票到首次复用间隔" unit="毫秒" :min="0" :max="300000" :disabled="disabled" />
      </BaseFormItem>
      <BaseFormItem label="复用请求间隔">
        <BaseNumberInput v-model="value.reuseSpacingMilliseconds" label="复用请求间隔" unit="毫秒" :min="0" :max="300000" :disabled="disabled" />
      </BaseFormItem>
      <BaseFormItem label="整轮超时">
        <BaseNumberInput v-model="value.roundTimeoutSeconds" label="整轮超时" unit="秒" :min="1" :max="3600" :disabled="disabled" />
      </BaseFormItem>
    </div>
    <BaseFormItem label="必需 Cookie" description="多个名称用英文逗号分隔，验证期间始终使用本次铸票取得的值">
      <BaseInput v-model="cookieNames" :disabled="disabled" />
    </BaseFormItem>
    <BaseSwitch v-model="value.stopOnFirstFailure" label="首次失败后停止当前候选" show-label :disabled="disabled" />
    <p class="m-0 text-cp-sm" :class="props.budgetLimit < requestsPerCandidate ? 'text-cp-error' : 'text-cp-text-secondary'">
      每个候选最多 {{ requestsPerCandidate }} 次请求，本轮最多 {{ requestsPerCandidate * props.candidateLimit }} 次，窗口预算 {{ props.budgetLimit }} 次
    </p>
    <details class="rounded-cp-lg bg-cp-fill-quaternary">
      <summary class="min-h-11 cursor-pointer px-3.5 py-3 text-cp-sm font-bold text-cp-text">
        成功条件
      </summary>
      <div class="grid gap-4 px-3.5 pb-4 xl:grid-cols-2">
        <BaseFormItem v-for="stage in stages" :key="stage.key" :label="stage.label" description="expectedModels 留空时匹配本轮模型，jsonPredicates 支持 pointer 与 values" :error="errors[stage.key]">
          <BaseTextarea :model-value="ruleText[stage.key]" class="font-mono" :rows="12" :disabled="disabled" @update:model-value="updateRule(stage.key, $event)" />
        </BaseFormItem>
      </div>
    </details>
  </section>
</template>
