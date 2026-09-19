<script setup lang="ts">
import type { AccountRow } from '../../constants'
import { RefreshCw, TriangleAlert } from '@lucide/vue'
import { toRef } from 'vue'

import BaseButton from '@/components/base/BaseButton.vue'
import BaseEmpty from '@/components/base/BaseEmpty.vue'
import BaseModal from '@/components/base/BaseModal/index.vue'
import { useAccountPersonalInfo } from '../../composables/useAccountPersonalInfo'
import AccountProfileActivityInsights from './ActivityInsights.vue'
import AccountProfileMetrics from './Metrics.vue'
import AccountProfileHero from './ProfileHero.vue'
import AccountProfileSkeleton from './Skeleton.vue'
import AccountSubscription from './Subscription.vue'
import AccountProfileTokenActivity from './TokenActivity.vue'

const props = defineProps<{ account: AccountRow }>()
const open = defineModel<boolean>({ required: true })
const accountId = toRef(() => props.account.id)
const { profile, subscription, loading, error, load } = useAccountPersonalInfo(accountId, open)
</script>

<template>
  <BaseModal v-model="open" title="个人信息" size="xl">
    <div class="flex min-h-0 min-w-0 flex-col gap-6 pb-2">
      <div class="grid min-w-0 grid-cols-1 gap-4 rounded-cp-lg bg-cp-fill-alter p-4 sm:p-5 lg:grid-cols-[minmax(310px,1.05fr)_minmax(0,1.95fr)] lg:items-center">
        <AccountProfileHero :account="account" :profile="profile" />
        <AccountProfileMetrics :profile="profile" :loading="loading" />
      </div>

      <section v-if="subscription" class="grid min-w-0 grid-cols-1 gap-4" aria-labelledby="profile-subscription-title">
        <h3 id="profile-subscription-title" class="m-0 text-cp font-heavy text-cp-text">
          订阅信息
        </h3>
        <AccountSubscription :subscription="subscription" />
      </section>

      <AccountProfileSkeleton v-if="loading && !profile" />
      <BaseEmpty
        v-else-if="error && !profile"
        title="个人资料加载失败"
        :description="error"
        :icon="TriangleAlert"
        surface="none"
      />
      <template v-else-if="profile">
        <BaseEmpty
          v-if="profile.hasStatsError"
          title="个人统计暂不可用"
          description="本次未获取到统计数据，可以点击刷新信息重试"
          :icon="TriangleAlert"
          surface="none"
        />
        <template v-else>
          <AccountProfileTokenActivity :daily-usage="profile.dailyUsage" />
          <AccountProfileActivityInsights :insights="profile.activityInsights" />
        </template>
        <p v-if="error" role="status" class="m-0 text-cp-sm text-cp-warning">
          本次信息刷新未完成，保留已获取的结果
        </p>
      </template>
    </div>
    <template #footer>
      <BaseButton variant="secondary" @click="open = false">
        关闭
      </BaseButton>
      <BaseButton :aria-busy="loading" :aria-disabled="loading" @click="load">
        <template #icon>
          <RefreshCw class="size-4" :class="loading ? 'animate-spin motion-reduce:animate-none' : undefined" />
        </template>
        刷新信息
      </BaseButton>
    </template>
  </BaseModal>
</template>
