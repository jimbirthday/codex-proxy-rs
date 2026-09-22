<script setup lang="ts">
import type { TurnStatePolicy, TurnStateProbeSchedule } from '@/api/modules/turn-state-policy'
import { ChevronDown } from '@lucide/vue'
import { computed } from 'vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseNumberInput from '@/components/base/BaseNumberInput.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'

const props = defineProps<{ disabled: boolean, verified: boolean }>()
const schedule = defineModel<TurnStateProbeSchedule>('schedule', { required: true })
const state = defineModel<TurnStatePolicy>('state', { required: true })
function stringList(model: () => string[], update: (value: string[]) => void) {
  return computed({ get: () => model().join(', '), set: value => update(value.split(',').map(item => item.trim()).filter(Boolean)) })
}
function numberList(model: () => number[], update: (value: number[]) => void) {
  return computed({ get: () => model().join(', '), set: value => update(value.split(',').map(item => Number(item.trim())).filter(Number.isFinite)) })
}
const stateHeadersText = stringList(() => state.value.responseHeaderNames, v => state.value.responseHeaderNames = v)
const statePointersText = stringList(() => state.value.responseJsonPointers, v => state.value.responseJsonPointers = v)
const acceptedLengthsText = numberList(() => state.value.acceptedLengths, v => state.value.acceptedLengths = v)
const invalidationStatusesText = numberList(() => state.value.invalidationStatuses, v => state.value.invalidationStatuses = v)
</script>

<template>
  <div class="grid gap-3">
    <details class="group rounded-cp-lg bg-cp-fill-quaternary">
      <summary class="flex min-h-11 cursor-pointer list-none items-center justify-between gap-3 px-3.5 py-3 text-cp-sm font-bold text-cp-text">
        调度、频率与退避
        <ChevronDown class="size-4 transition-transform duration-150 group-open:rotate-180 motion-reduce:transition-none" />
      </summary>
      <div class="grid gap-4 px-3.5 pb-4 sm:grid-cols-2 xl:grid-cols-4">
        <BaseFormItem label="后台扫描" description="检查到期任务的频率">
          <BaseNumberInput v-model="schedule.scanIntervalSeconds" label="后台扫描" unit="秒" :min="10" :max="3600" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="轮次最短间隔">
          <BaseNumberInput v-model="schedule.roundMinIntervalSeconds" label="轮次最短间隔" unit="秒" :min="1" :max="86400" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="请求间隔">
          <BaseNumberInput v-model="schedule.requestSpacingMilliseconds" label="请求间隔" unit="毫秒" :min="100" :max="300000" :step="100" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="单请求超时">
          <BaseNumberInput v-model="schedule.requestTimeoutSeconds" label="单请求超时" unit="秒" :min="1" :max="300" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="业务活跃窗口">
          <BaseNumberInput v-model="schedule.activityWindowSeconds" label="业务活跃窗口" unit="秒" :min="60" :max="604800" :step="60" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="预算窗口">
          <BaseNumberInput v-model="schedule.budgetWindowSeconds" label="预算窗口" unit="秒" :min="1" :max="3600" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="窗口请求上限">
          <BaseNumberInput v-model="schedule.budgetLimit" label="窗口请求上限" unit="次" :min="1" :max="100" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="全局探测并发">
          <BaseNumberInput v-model="schedule.maxConcurrency" label="全局探测并发" :min="1" :max="16" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="失败退避起点">
          <BaseNumberInput v-model="schedule.retryInitialSeconds" label="失败退避起点" unit="秒" :min="1" :max="3600" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="失败退避上限">
          <BaseNumberInput v-model="schedule.retryMaxSeconds" label="失败退避上限" unit="秒" :min="1" :max="86400" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="代理冷却起点">
          <BaseNumberInput v-model="schedule.proxyCooldownInitialSeconds" label="代理冷却起点" unit="秒" :min="1" :max="3600" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="代理冷却上限">
          <BaseNumberInput v-model="schedule.proxyCooldownMaxSeconds" label="代理冷却上限" unit="秒" :min="1" :max="86400" :disabled="props.disabled" />
        </BaseFormItem>
      </div>
    </details>

    <details class="group rounded-cp-lg bg-cp-fill-quaternary">
      <summary class="flex min-h-11 cursor-pointer list-none items-center justify-between gap-3 px-3.5 py-3 text-cp-sm font-bold text-cp-text">
        State 提取与有效期
        <ChevronDown class="size-4 transition-transform duration-150 group-open:rotate-180 motion-reduce:transition-none" />
      </summary>
      <div class="grid gap-4 px-3.5 pb-4 sm:grid-cols-2 xl:grid-cols-3">
        <div class="flex flex-wrap gap-x-7 gap-y-3 sm:col-span-2 xl:col-span-3">
          <BaseSwitch v-model="state.captureBusinessResponses" label="采集业务响应 State" show-label :disabled="props.disabled || props.verified" />
          <BaseSwitch v-model="state.captureProbeResponses" label="采集探测响应 State" show-label :disabled="props.disabled || props.verified" />
          <BaseSwitch v-model="state.injectionEnabled" label="注入后续请求" show-label :disabled="props.disabled" />
        </div>
        <BaseFormItem label="响应头来源" description="多个名称使用英文逗号分隔">
          <BaseInput v-model="stateHeadersText" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="JSON Pointer" description="多个路径使用英文逗号分隔">
          <BaseInput v-model="statePointersText" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="允许长度" description="例如 292, 332">
          <BaseInput v-model="acceptedLengthsText" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="有效期">
          <BaseNumberInput v-model="state.ttlSeconds" label="State 有效期" unit="秒" :min="60" :max="604800" :step="60" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="提前续采">
          <BaseNumberInput v-model="state.renewBeforeSeconds" label="State 提前续采" unit="秒" :min="0" :max="604799" :step="60" :disabled="props.disabled" />
        </BaseFormItem>
        <BaseFormItem label="失效状态码" description="多个状态码使用英文逗号分隔">
          <BaseInput v-model="invalidationStatusesText" :disabled="props.disabled" />
        </BaseFormItem>
        <div class="sm:col-span-2 xl:col-span-3">
          <BaseCheckbox v-model="state.requireServedModelMatch" label="只接受响应模型与请求模型一致的 State" show-label :disabled="props.disabled" />
        </div>
      </div>
    </details>
  </div>
</template>
