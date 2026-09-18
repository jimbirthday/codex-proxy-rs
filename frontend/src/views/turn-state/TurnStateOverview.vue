<script setup lang="ts">
import type { Account, TurnStateOverviewEntry } from '@/api'
import { AlertTriangle, CheckCircle2, Clock3, PlayCircle, ShieldCheck } from '@lucide/vue'
import { computed } from 'vue'

import BaseCard from '@/components/base/BaseCard.vue'
import BaseEmpty from '@/components/base/BaseEmpty.vue'
import { formatDateTime } from '@/utils/date'

const props = defineProps<{
  accounts: Account[]
  entries: TurnStateOverviewEntry[]
  loading: boolean
  selectedAccountId: string
}>()

const emit = defineEmits<{
  selectAccount: [accountId: string]
}>()

const entryMap = computed(() => {
  const map = new Map<string, TurnStateOverviewEntry[]>()
  for (const entry of props.entries) {
    const list = map.get(entry.accountId) ?? []
    list.push(entry)
    map.set(entry.accountId, list)
  }
  return map
})

const rows = computed(() => {
  const now = Date.now()
  return props.accounts.map((account) => {
    const entries = entryMap.value.get(account.id) ?? []
    const availableEntries = entries.filter(entry => entry.stateAvailable)
    const appliedEntries = availableEntries.filter(entry => entry.stateFirstAppliedAt)
    const expiringEntries = availableEntries.filter((entry) => {
      const expiresAt = entry.stateExpiresAt ? new Date(entry.stateExpiresAt).getTime() : 0
      return expiresAt > now && expiresAt <= now + 5 * 60 * 1000
    })
    const current = [...availableEntries]
      .sort((left, right) => new Date(right.stateCapturedAt ?? 0).getTime() - new Date(left.stateCapturedAt ?? 0).getTime())[0]
    return {
      account,
      entries,
      availableEntries,
      appliedEntries,
      expiringEntries,
      current,
      status: availableEntries.length === 0 ? 'missing' : appliedEntries.length > 0 ? 'applied' : 'ready',
    }
  })
})

const stats = computed(() => ({
  total: rows.value.length,
  applied: rows.value.filter(row => row.appliedEntries.length > 0).length,
  ready: rows.value.filter(row => row.availableEntries.length > 0 && row.appliedEntries.length === 0).length,
  expiring: rows.value.filter(row => row.expiringEntries.length > 0).length,
  missing: rows.value.filter(row => row.availableEntries.length === 0).length,
}))

function statusLabel(status: string) {
  if (status === 'applied')
    return '已用于请求'
  if (status === 'ready')
    return '待首次使用'
  return '未获取'
}

function sourceLabel(source: string | null | undefined) {
  if (source === 'manual_probe')
    return '管理员手动探测'
  if (source === 'automatic_renewal')
    return '后台自动轮换'
  if (source === 'upstream_response')
    return '业务响应采集'
  return '—'
}
</script>

<template>
  <BaseCard title="账号状态总览" description="区分 State 已获取与已用于请求">
    <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-5" aria-live="polite" :aria-busy="loading || undefined">
      <div class="min-h-22 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
        <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary"><ShieldCheck class="size-3.5" />OAuth 账号</span>
        <p class="mt-2 mb-0 font-mono text-cp-xl font-heavy text-cp-text">
          {{ loading ? '—' : stats.total }}
        </p>
      </div>
      <div class="min-h-22 rounded-cp bg-cp-success-container px-3.5 py-3">
        <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-success-on-container"><CheckCircle2 class="size-3.5" />State 已用于请求</span>
        <p class="mt-2 mb-0 font-mono text-cp-xl font-heavy text-cp-success-on-container">
          {{ loading ? '—' : stats.applied }}
        </p>
      </div>
      <div class="min-h-22 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
        <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary"><PlayCircle class="size-3.5" />等待首次使用</span>
        <p class="mt-2 mb-0 font-mono text-cp-xl font-heavy text-cp-text">
          {{ loading ? '—' : stats.ready }}
        </p>
      </div>
      <div class="min-h-22 rounded-cp bg-cp-warning-container px-3.5 py-3">
        <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-warning-on-container"><Clock3 class="size-3.5" />等待轮换</span>
        <p class="mt-2 mb-0 font-mono text-cp-xl font-heavy text-cp-warning-on-container">
          {{ loading ? '—' : stats.expiring }}
        </p>
      </div>
      <div class="min-h-22 rounded-cp bg-cp-error-container px-3.5 py-3">
        <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-error-on-container"><AlertTriangle class="size-3.5" />未取得 State</span>
        <p class="mt-2 mb-0 font-mono text-cp-xl font-heavy text-cp-error-on-container">
          {{ loading ? '—' : stats.missing }}
        </p>
      </div>
    </div>

    <div v-if="loading" class="mt-5 flex min-h-28 items-center justify-center text-cp-sm font-emphasis text-cp-text-secondary">
      正在加载账号状态
    </div>
    <BaseEmpty
      v-else-if="rows.length === 0"
      class="mt-5"
      title="暂无 OAuth 账号"
      description="先添加 OpenAI OAuth 账号，状态探测会在这里显示覆盖情况"
      :icon="ShieldCheck"
      surface="none"
    />
    <div v-else class="mt-5 overflow-hidden rounded-cp border border-cp-split">
      <div class="hidden grid-cols-[minmax(220px,1.2fr)_minmax(160px,1fr)_minmax(160px,1fr)_minmax(180px,1fr)] gap-4 bg-cp-fill-quaternary px-4 py-2.5 text-cp-xs font-heavy text-cp-text-quaternary md:grid">
        <span>OAuth 账号</span><span>请求状态</span><span>模型覆盖</span><span>来源 / 下次轮换</span>
      </div>
      <div class="divide-y divide-cp-split">
        <button
          v-for="row in rows"
          :key="row.account.id"
          type="button"
          class="grid w-full gap-3 bg-cp-bg-container px-4 py-3.5 text-left transition-colors hover:bg-cp-fill-quaternary focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-cp-primary md:grid-cols-[minmax(220px,1.2fr)_minmax(160px,1fr)_minmax(160px,1fr)_minmax(180px,1fr)] md:items-center md:gap-4"
          :class="row.account.id === selectedAccountId ? 'bg-cp-fill-quaternary' : undefined"
          @click="emit('selectAccount', row.account.id)"
        >
          <span class="min-w-0">
            <span class="block truncate text-cp-sm font-heavy text-cp-text">{{ row.account.email || row.account.name || row.account.id }}</span>
            <span class="mt-1 block text-cp-xs text-cp-text-secondary">{{ row.account.planTypeDisplay }} · {{ row.account.id }}</span>
          </span>
          <span class="min-w-0">
            <span class="mb-1 block text-cp-xs font-heavy text-cp-text-quaternary md:hidden">请求状态</span>
            <span class="flex items-center gap-2 text-cp-sm font-heavy" :class="row.status === 'missing' ? 'text-cp-error-text' : row.status === 'ready' ? 'text-cp-warning-text' : 'text-cp-success-text'">
              <CheckCircle2 v-if="row.status === 'applied'" class="size-4" />
              <PlayCircle v-else-if="row.status === 'ready'" class="size-4" />
              <AlertTriangle v-else class="size-4" />
              {{ statusLabel(row.status) }}
            </span>
          </span>
          <span class="min-w-0 text-cp-sm text-cp-text-secondary">
            <span class="mb-1 block text-cp-xs font-heavy text-cp-text-quaternary md:hidden">模型覆盖</span>
            <span v-if="row.entries.length === 0">尚未探测</span>
            <span v-else class="flex flex-wrap gap-1.5">
              <span v-for="entry in row.entries.slice(0, 3)" :key="entry.model" class="rounded-cp-sm bg-cp-fill-quaternary px-1.5 py-0.5 font-mono text-cp-xs" :class="entry.stateAvailable && entry.stateFirstAppliedAt ? 'text-cp-success-text' : entry.stateAvailable ? 'text-cp-warning-text' : 'text-cp-text-secondary'">
                {{ entry.model }}{{ entry.stateAvailable && entry.stateFirstAppliedAt ? ' · 已使用' : entry.stateAvailable ? ' · 待使用' : ' · 无' }}
              </span>
              <span v-if="row.entries.length > 3" class="text-cp-xs text-cp-text-quaternary">+{{ row.entries.length - 3 }}</span>
            </span>
          </span>
          <span class="text-cp-sm text-cp-text-secondary">
            <span class="mb-1 block text-cp-xs font-heavy text-cp-text-quaternary md:hidden">来源 / 下次轮换</span>
            <span v-if="!row.current">—</span>
            <span v-else>{{ sourceLabel(row.current.stateSource) }}<span class="mt-1 block font-mono text-cp-xs text-cp-text-quaternary">{{ formatDateTime(row.current.nextRotationAt) }}</span></span>
          </span>
        </button>
      </div>
    </div>
  </BaseCard>
</template>
