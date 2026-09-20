<script setup lang="ts">
import type { AccountListDetails } from '../composables/useAccountListDetails'
import type { AccountRow } from '../constants'
import { computed } from 'vue'
import BaseButton from '@/components/base/BaseButton.vue'
import { useUiClock } from '@/composables/useUiClock'
import { formatDateTime } from '@/utils/date'
import { supportsAccountListDetails } from '../composables/useAccountListDetails'
import { accountResetCreditsSummary } from '../composables/useAccountResetCredits'

const props = defineProps<{
  account: AccountRow
  details?: AccountListDetails
}>()
const emit = defineEmits<{ refresh: [] }>()
const now = useUiClock()
const supported = computed(() => supportsAccountListDetails(props.account))
const credits = computed(() => accountResetCreditsSummary(props.account.id))
const weeklyWindows = computed(() => props.account.quota.windows.filter(window => window.windowSeconds === 7 * 24 * 60 * 60))
const busy = computed(() => props.details?.loading || props.details?.queued || credits.value.loading || credits.value.consuming)
const subscription = computed(() => props.details?.subscription)
const expired = computed(() => subscription.value && Date.parse(subscription.value.expiresAt) <= now.value.getTime())
const subscriptionText = computed(() => {
  if (!supported.value)
    return props.account.authenticationKind === 'api_key' ? '不适用' : '暂不支持查询'
  if (subscription.value)
    return formatDateTime(subscription.value.expiresAt, '未提供', 'Asia/Shanghai')
  if (props.details?.loading)
    return '查询中…'
  if (props.details?.failed)
    return '查询失败'
  return props.details?.checkedAt ? '未提供' : '等待查询…'
})
const creditText = computed(() => {
  if (!supported.value)
    return '不适用'
  if (credits.value.snapshot)
    return `${credits.value.snapshot.availableCount} 次`
  if (credits.value.loading)
    return '查询中…'
  if (credits.value.loadError)
    return '查询失败'
  return '等待查询…'
})
</script>

<template>
  <div class="min-w-0 py-1 text-cp-sm">
    <dl class="m-0 grid grid-cols-[auto_minmax(0,1fr)] items-start gap-x-3 gap-y-2">
      <dt class="text-cp-text-secondary">
        周额度重置
      </dt>
      <dd class="m-0 min-w-0 text-right font-mono tabular-nums text-cp-text">
        <div v-for="window in weeklyWindows" :key="window.key" class="break-words">
          <span v-if="weeklyWindows.length > 1" class="mr-1 font-sans text-cp-xs text-cp-text-secondary">{{ window.limitName || window.labelDisplay }}</span>
          {{ window.resetAtDisplay === '—' || window.resetAtDisplay === '-' ? '未提供' : window.resetAtDisplay }}
        </div>
        <span v-if="!weeklyWindows.length" class="font-sans text-cp-text-secondary">{{ account.authenticationKind === 'api_key' ? '不适用' : '未提供周额度' }}</span>
      </dd>
      <dt class="text-cp-text-secondary">
        账号到期
      </dt>
      <dd class="m-0 min-w-0 text-right" :class="expired ? 'text-cp-warning-text' : 'text-cp-text'">
        <span class="font-mono tabular-nums">{{ subscriptionText }}</span>
        <span v-if="details?.failed && subscription" class="block text-cp-xs text-cp-warning-text">上次查询结果</span>
        <span v-if="subscription" class="mt-0.5 block text-cp-xs text-cp-text-secondary">
          {{ expired ? '订阅已到期' : '订阅结束时间' }}{{ subscription.willRenew === true ? ' · 自动续费' : '' }}
        </span>
      </dd>
      <dt class="text-cp-text-secondary">
        可重置次数
      </dt>
      <dd class="m-0 text-right font-mono font-emphasis tabular-nums text-cp-text">
        {{ creditText }}
        <span v-if="credits.loadError && credits.snapshot" class="block font-sans text-cp-xs text-cp-warning-text">上次查询结果</span>
      </dd>
    </dl>
    <div class="mt-1 flex flex-wrap items-center justify-between gap-x-2 text-cp-xs text-cp-text-secondary">
      <span>北京时间</span>
      <BaseButton v-if="supported" variant="ghost" size="sm" class="min-h-11 px-2! xl:min-h-7" :loading="!!busy" @click="emit('refresh')">
        {{ details?.failed || credits.loadError ? '查询失败，重试' : '刷新到期与次数' }}
      </BaseButton>
    </div>
  </div>
</template>
