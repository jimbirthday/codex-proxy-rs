<script setup lang="ts">
import type { AccountRow } from '../constants'
import type { BaseTableSort } from '@/components/base/BaseTable/columns'
import { ChevronDown } from '@lucide/vue'
import { computed } from 'vue'
import AccountGroupMarks from '@/components/AccountGroupMarks.vue'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import BaseEmpty from '@/components/base/BaseEmpty.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import ProviderIconGroup from '@/components/ProviderIconGroup.vue'
import { accountColumns } from '../constants'
import AccountPlanBadge from './AccountPlanBadge.vue'
import AccountQuotaSummaryCell from './AccountQuotaSummaryCell/index.vue'
import AccountStatusBadge from './AccountStatusBadge/index.vue'

const props = defineProps<{
  accounts: AccountRow[]
  loading: boolean
  selectedIds: Set<string>
  expandedIds: Set<string>
  allSelected: boolean
  indeterminate: boolean
  sort?: BaseTableSort
}>()
const emit = defineEmits<{
  select: [accountId: string]
  selectAll: []
  expand: [accountId: string]
  sortChange: [sort: BaseTableSort | undefined]
}>()
defineSlots<{
  details: (props: { row: AccountRow }) => unknown
  actions: (props: { row: AccountRow }) => unknown
  expanded: (props: { row: AccountRow }) => unknown
}>()
const sortOptions = [
  { label: '默认排序', value: '' },
  ...accountColumns.filter(column => column.sortable).flatMap((column) => {
    const key = typeof column.sortable === 'string' ? column.sortable : column.key
    return [
      { label: `${column.label} · 升序`, value: `${key}:asc` },
      { label: `${column.label} · 降序`, value: `${key}:desc` },
    ]
  }),
]
const sortValue = computed({
  get: () => props.sort ? `${props.sort.key}:${props.sort.direction}` : '',
  set: (value: string) => {
    const [key, direction] = value.split(':')
    emit('sortChange', key && (direction === 'asc' || direction === 'desc') ? { key, direction } : undefined)
  },
})
</script>

<template>
  <section aria-label="账号列表" :aria-busy="loading" class="min-w-0">
    <div class="mb-3 flex flex-wrap items-center justify-between gap-2">
      <BaseCheckbox
        class="min-h-11"
        :model-value="allSelected"
        :indeterminate="indeterminate"
        :disabled="loading || !accounts.length"
        label="选择本页"
        show-label
        @update:model-value="emit('selectAll')"
      />
      <BaseSelect v-model="sortValue" class="w-40" :disabled="loading" :options="sortOptions" aria-label="账号排序" />
    </div>
    <div v-if="loading" class="grid gap-3 md:grid-cols-2" role="status" aria-label="正在加载账号">
      <div v-for="index in 4" :key="index" class="h-72 animate-pulse rounded-cp-lg bg-cp-fill-alter motion-reduce:animate-none" />
    </div>
    <BaseEmpty v-else-if="!accounts.length" title="暂无账号数据" description="调整筛选条件，或导入新的账号" />
    <div v-else class="grid items-start gap-3 md:grid-cols-2">
      <article
        v-for="account in accounts" :key="account.id"
        class="min-w-0 rounded-cp-lg p-3 sm:p-4"
        :class="selectedIds.has(account.id) ? 'bg-cp-primary-container ring-1 ring-cp-control-outline' : 'bg-cp-fill-alter'"
        :aria-label="account.email || account.name || account.id"
      >
        <header>
          <div class="flex items-start gap-2">
            <div class="min-w-0 flex-1 pt-2">
              <h2 class="m-0 text-cp font-heavy wrap-anywhere text-cp-text">
                {{ account.email || account.name || account.accountId || account.id }}
              </h2>
              <p v-if="account.notes" class="m-0 mt-1 text-cp-sm wrap-anywhere text-cp-text-secondary">
                {{ account.notes }}
              </p>
            </div>
            <BaseCheckbox
              class="min-h-11 min-w-11 justify-center"
              :model-value="selectedIds.has(account.id)"
              :label="`选择 ${account.email || account.name || account.id}`"
              @update:model-value="emit('select', account.id)"
            />
          </div>
          <div class="mt-2 flex flex-wrap items-center gap-2">
            <ProviderIconGroup :provider="account.provider" :authentication-kind="account.authenticationKind" />
            <AccountPlanBadge :authentication-kind="account.authenticationKind" :plan-type="account.planType" :plan-type-display="account.planTypeDisplay" />
            <AccountStatusBadge
              :status="account.status"
              :error-reason="account.errorReason"
              :error-message="account.errorMessage"
              :rate-limited-until="account.quota.rateLimitedUntil"
              :rate-limit-reason="account.quota.rateLimitReason"
              :recovery-probe-required="account.quota.recoveryProbeRequired"
              :next-refresh-at="account.nextRefreshAt"
            />
          </div>
          <div v-if="account.groups.length" class="mt-2">
            <AccountGroupMarks :groups="account.groups" />
          </div>
        </header>
        <div class="my-3 min-w-0 rounded-cp bg-cp-bg-container px-3 py-2">
          <AccountQuotaSummaryCell :account="account" />
          <slot name="details" :row="account" />
        </div>
        <slot name="actions" :row="account" />
        <BaseButton
          variant="ghost" class="mt-2 min-h-11 w-full"
          :aria-expanded="expandedIds.has(account.id)"
          :aria-controls="`account-statistics-${account.id}`"
          @click="emit('expand', account.id)"
        >
          {{ expandedIds.has(account.id) ? '收起用量与详情' : '用量与更多详情' }}
          <ChevronDown class="size-4" :class="expandedIds.has(account.id) ? 'rotate-180' : undefined" />
        </BaseButton>
        <div v-if="expandedIds.has(account.id)" :id="`account-statistics-${account.id}`" class="mt-3 grid min-w-0 gap-3">
          <slot name="expanded" :row="account" />
        </div>
      </article>
    </div>
  </section>
</template>
