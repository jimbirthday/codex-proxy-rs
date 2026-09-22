<script setup lang="ts">
import type { ResponseHeaderCarryRule, ResponseHeaderCarrySource, ResponseHeaderCarryStatus } from '@/api/modules/turn-state-policy'
import { ChevronDown, Plus, Trash2 } from '@lucide/vue'
import { computed } from 'vue'

import BaseButton from '@/components/base/BaseButton.vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseNumberInput from '@/components/base/BaseNumberInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import { formatDateTime } from '@/utils/date'

const props = defineProps<{
  disabled: boolean
  status?: ResponseHeaderCarryStatus
  clearingRuleId?: string
}>()
const emit = defineEmits<{
  clearRule: [ruleId: string]
  clearAll: []
}>()
const rules = defineModel<ResponseHeaderCarryRule[]>({ required: true })

const statusByRule = computed(() => new Map(props.status?.rules.map(item => [item.ruleId, item])))

function newRule(): ResponseHeaderCarryRule {
  return {
    id: `header_${crypto.randomUUID().replaceAll('-', '').slice(0, 16)}`,
    name: '新响应头规则',
    enabled: true,
    captureEnabled: true,
    injectionEnabled: true,
    clearOnDisable: false,
    sources: ['business_response'],
    sourceHeader: '',
    targetHeader: '',
    transform: 'direct',
    valueSelection: 'last',
    mergeMode: 'if_absent',
    scope: 'account_model',
    accountIds: [],
    models: [],
    ttlSeconds: 3600,
    missingBehavior: 'keep',
    captureStatusMin: 200,
    captureStatusMax: 299,
    invalidationStatuses: [],
    maxValueBytes: 8192,
    maxValues: 4,
  }
}

function addRule() {
  rules.value.push(newRule())
}

function toggleSource(rule: ResponseHeaderCarryRule, source: ResponseHeaderCarrySource, checked: boolean) {
  rule.sources = checked ? [...new Set([...rule.sources, source])] : rule.sources.filter(value => value !== source)
}

function setInvalidationStatuses(rule: ResponseHeaderCarryRule, value: string) {
  rule.invalidationStatuses = value.split(',').map(item => Number(item.trim())).filter(Number.isFinite)
}

function setList(rule: ResponseHeaderCarryRule, field: 'accountIds' | 'models', value: string) {
  rule[field] = [...new Set(value.split(',').map(item => item.trim()).filter(Boolean))]
}

function setTransform(rule: ResponseHeaderCarryRule, value: string) {
  rule.transform = value as ResponseHeaderCarryRule['transform']
  if (value === 'set_cookie_to_cookie') {
    rule.sourceHeader = 'set-cookie'
    rule.targetHeader = 'cookie'
    rule.valueSelection = 'all'
    rule.mergeMode = 'replace'
  }
}
</script>

<template>
  <section class="grid gap-3" aria-labelledby="response-header-carry-title">
    <div class="flex flex-wrap items-start justify-between gap-3">
      <div>
        <h3 id="response-header-carry-title" class="m-0 text-cp font-bold text-cp-text">
          响应头续带
        </h3>
        <p class="mt-1 mb-0 text-cp-sm text-cp-text-secondary">
          值仅保存在当前进程内，并按账号与凭据版本严格隔离
        </p>
      </div>
      <div class="flex flex-wrap gap-2">
        <BaseButton size="sm" variant="ghost" :disabled="props.disabled || !props.status?.totalEntries" @click="emit('clearAll')">
          清空全部缓存<span v-if="props.status">（{{ props.status.totalEntries }}）</span>
        </BaseButton>
        <BaseButton size="sm" :disabled="props.disabled || rules.length >= 64" @click="addRule">
          <template #icon>
            <Plus class="size-3.5" />
          </template>
          添加规则
        </BaseButton>
      </div>
    </div>

    <p v-if="rules.length === 0" class="m-0 rounded-cp bg-cp-fill-quaternary px-3.5 py-4 text-cp-sm text-cp-text-secondary">
      尚未配置响应头续带规则
    </p>

    <details v-for="(rule, index) in rules" :key="rule.id" class="group rounded-cp-lg bg-cp-fill-quaternary">
      <summary class="flex min-h-12 cursor-pointer list-none items-center justify-between gap-3 px-3.5 py-3">
        <span class="min-w-0">
          <span class="block truncate text-cp-sm font-bold text-cp-text">{{ rule.name || '未命名规则' }}</span>
          <span class="block truncate font-mono text-cp-xs text-cp-text-quaternary">{{ rule.sourceHeader || '未设置来源' }} → {{ rule.targetHeader || '未设置目标' }}</span>
        </span>
        <span class="flex shrink-0 items-center gap-3 text-cp-xs text-cp-text-secondary">
          缓存 {{ statusByRule.get(rule.id)?.cachedEntries ?? 0 }}
          <ChevronDown class="size-4 transition-transform duration-150 group-open:rotate-180 motion-reduce:transition-none" />
        </span>
      </summary>
      <div class="grid gap-4 px-3.5 pb-4">
        <div class="flex flex-wrap gap-x-7 gap-y-3">
          <BaseSwitch v-model="rule.enabled" label="启用规则" show-label :disabled="props.disabled" />
          <BaseSwitch v-model="rule.captureEnabled" label="采集" show-label :disabled="props.disabled || !rule.enabled" />
          <BaseSwitch v-model="rule.injectionEnabled" label="注入" show-label :disabled="props.disabled || !rule.enabled" />
          <BaseSwitch v-model="rule.clearOnDisable" label="停用时清空" show-label :disabled="props.disabled" />
        </div>
        <div class="flex flex-wrap gap-x-7 gap-y-3">
          <BaseCheckbox :model-value="rule.sources.includes('business_response')" label="采集业务响应" show-label :disabled="props.disabled" @update:model-value="toggleSource(rule, 'business_response', $event)" />
          <BaseCheckbox :model-value="rule.sources.includes('turn_state_probe')" label="采集 State 探测响应" show-label :disabled="props.disabled" @update:model-value="toggleSource(rule, 'turn_state_probe', $event)" />
        </div>
        <div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          <BaseFormItem label="规则名称" required>
            <BaseInput v-model="rule.name" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="来源响应头" required>
            <BaseInput v-model="rule.sourceHeader" class="font-mono" placeholder="x-session-context" :disabled="props.disabled || rule.transform === 'set_cookie_to_cookie'" />
          </BaseFormItem>
          <BaseFormItem label="目标请求头" required>
            <BaseInput v-model="rule.targetHeader" class="font-mono" placeholder="x-session-context" :disabled="props.disabled || rule.transform === 'set_cookie_to_cookie'" />
          </BaseFormItem>
          <BaseFormItem label="值转换">
            <BaseSelect :model-value="rule.transform" :options="[{ value: 'direct', label: '原值续带' }, { value: 'set_cookie_to_cookie', label: 'Set-Cookie 转 Cookie' }]" :disabled="props.disabled" @update:model-value="setTransform(rule, $event)" />
          </BaseFormItem>
          <BaseFormItem label="值选择">
            <BaseSelect v-model="rule.valueSelection" :options="[{ value: 'first', label: '第一个值' }, { value: 'last', label: '最后一个值' }, { value: 'all', label: '全部值' }]" :disabled="props.disabled || rule.transform === 'set_cookie_to_cookie'" />
          </BaseFormItem>
          <BaseFormItem label="注入方式">
            <BaseSelect v-model="rule.mergeMode" :options="[{ value: 'if_absent', label: '请求未携带时注入' }, { value: 'replace', label: '覆盖请求值' }, { value: 'append', label: '追加请求值' }]" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="账号内作用域">
            <BaseSelect v-model="rule.scope" :options="[{ value: 'account', label: '账号' }, { value: 'account_model', label: '账号 + 模型' }, { value: 'account_model_proxy', label: '账号 + 模型 + 代理' }]" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="限定账号" description="留空匹配全部；多个账号 ID 使用英文逗号分隔">
            <BaseInput :model-value="rule.accountIds.join(', ')" class="font-mono" placeholder="留空匹配全部账号" :disabled="props.disabled" @update:model-value="setList(rule, 'accountIds', $event)" />
          </BaseFormItem>
          <BaseFormItem label="限定模型" description="留空匹配全部；多个规范化模型 ID 使用英文逗号分隔">
            <BaseInput :model-value="rule.models.join(', ')" class="font-mono" placeholder="gpt-5.4, gpt-5.4-mini" :disabled="props.disabled" @update:model-value="setList(rule, 'models', $event)" />
          </BaseFormItem>
          <BaseFormItem label="缓存有效期">
            <BaseNumberInput v-model="rule.ttlSeconds" label="缓存有效期" unit="秒" :min="1" :max="604800" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="响应缺失时">
            <BaseSelect v-model="rule.missingBehavior" :options="[{ value: 'keep', label: '保留旧值' }, { value: 'clear', label: '清除旧值' }]" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="失效状态码" description="多个状态码使用英文逗号分隔">
            <BaseInput :model-value="rule.invalidationStatuses.join(', ')" placeholder="401, 403" :disabled="props.disabled" @update:model-value="setInvalidationStatuses(rule, $event)" />
          </BaseFormItem>
          <BaseFormItem label="采集状态码下限">
            <BaseNumberInput v-model="rule.captureStatusMin" label="采集状态码下限" :min="100" :max="599" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="采集状态码上限">
            <BaseNumberInput v-model="rule.captureStatusMax" label="采集状态码上限" :min="100" :max="599" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="单值大小上限">
            <BaseNumberInput v-model="rule.maxValueBytes" label="单值大小上限" unit="字节" :min="1" :max="65536" :disabled="props.disabled" />
          </BaseFormItem>
          <BaseFormItem label="多值数量上限">
            <BaseNumberInput v-model="rule.maxValues" label="多值数量上限" :min="1" :max="16" :disabled="props.disabled" />
          </BaseFormItem>
        </div>
        <div class="flex flex-wrap items-center justify-between gap-3">
          <span class="text-cp-xs text-cp-text-quaternary">最近更新：{{ formatDateTime(statusByRule.get(rule.id)?.lastUpdatedAt) }}</span>
          <div class="flex flex-wrap gap-2">
            <BaseButton size="sm" variant="ghost" :loading="props.clearingRuleId === rule.id" :disabled="props.disabled || !(statusByRule.get(rule.id)?.cachedEntries)" @click="emit('clearRule', rule.id)">
              清空缓存
            </BaseButton>
            <BaseButton size="sm" variant="destructive" :disabled="props.disabled" @click="rules.splice(index, 1)">
              <template #icon>
                <Trash2 class="size-3.5" />
              </template>
              删除规则
            </BaseButton>
          </div>
        </div>
      </div>
    </details>
  </section>
</template>
