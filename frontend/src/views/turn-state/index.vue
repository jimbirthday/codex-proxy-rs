<script setup lang="ts">
import { Activity, Clock3, DatabaseZap, PlayCircle, RefreshCw, RotateCw, ShieldCheck } from '@lucide/vue'
import { computed, shallowRef } from 'vue'

import { clearTurnStateRuntime } from '@/api/modules/turn-state-policy'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseIconButton from '@/components/base/BaseIconButton.vue'
import BasePageHeader from '@/components/base/BasePageHeader.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import { toast } from '@/components/base/BaseToast'
import { errorMessage } from '@/utils/async'
import { formatDateTime } from '@/utils/date'
import TurnStateOverview from './TurnStateOverview.vue'
import TurnStatePolicyCard from './TurnStatePolicyCard.vue'
import TurnStateProbeHistory from './TurnStateProbeHistory.vue'
import { useTurnStateProbe } from './useTurnStateProbe'

const {
  selectModel,
  accounts,
  accountOptions,
  models,
  selectedAccount,
  selectedAccountId,
  selectedModelId,
  snapshot,
  overview,
  loadingAccounts,
  loadingOverview,
  loadingModels,
  refreshingModels,
  loadingSnapshot,
  probing,
  canProbe,
  manualEnabled,
  error,
  loadAccounts,
  loadOverview,
  loadModels,
  loadSnapshot,
  runProbe,
} = useTurnStateProbe()
const clearingState = shallowRef(false)

function refreshOverview() {
  void loadAccounts()
  void loadOverview()
}

const stateActive = computed(() => Boolean(
  snapshot.value?.stateExpiresAt
  && new Date(snapshot.value.stateExpiresAt).getTime() > Date.now(),
))
const history = computed(() => snapshot.value?.probeHistory ?? [])
const latestProbe = computed(() => history.value[0] ?? null)
const activeTarget = computed(() => {
  const probe = latestProbe.value
  if (!probe?.activeTargetId)
    return '—'
  return probe.attempts.find(attempt => attempt.targetId === probe.activeTargetId)?.targetLabel ?? probe.activeTargetId
})

const applicationLabel = computed(() => {
  if (!stateActive.value)
    return '未获取'
  return snapshot.value?.stateFirstAppliedAt ? '已用于请求' : '待首次使用'
})

function sourceLabel(source: string | null | undefined) {
  if (source === 'manual_probe')
    return '管理员手动探测'
  if (source === 'automatic_renewal')
    return '后台自动轮换'
  if (source === 'upstream_response')
    return '业务响应采集'
  return '—'
}

function invalidationMessage(reason: string | null | undefined) {
  return reason === 'upstream_312' || reason === 'probe_upstream_312'
    ? '上游返回 312，当前模型的 State 已失效'
    : '当前模型的 State 已失效'
}

async function clearSelectedState() {
  if (!selectedAccountId.value || !selectedModelId.value || clearingState.value)
    return
  clearingState.value = true
  error.value = ''
  try {
    await clearTurnStateRuntime({
      accountId: selectedAccountId.value,
      model: selectedModelId.value,
      responseHeaders: false,
      turnState: true,
    })
    await Promise.all([loadSnapshot(), loadOverview()])
    toast.success('所选账号与模型的 Turn State 已清空')
  }
  catch (cause) {
    error.value = errorMessage(cause, '清空 Turn State 失败')
  }
  finally {
    clearingState.value = false
  }
}
</script>

<template>
  <div class="w-full pb-6">
    <BasePageHeader
      class="relative max-md:pl-12 max-md:[&>div:last-child]:absolute max-md:[&>div:last-child]:top-0 max-md:[&>div:last-child]:right-0"
      title="Codex 状态探测"
      description="按 OAuth 账号与模型通过代理采集、轮换并检查结果"
    >
      <template #actions>
        <BaseIconButton
          variant="ghost"
          size="md"
          label="刷新 OAuth 账号"
          :loading="loadingAccounts || loadingOverview"
          :disabled="probing"
          @click="refreshOverview"
        >
          <template #loading>
            <RefreshCw class="size-4 animate-spin motion-reduce:animate-none" />
          </template>
          <RefreshCw class="size-4" />
        </BaseIconButton>
      </template>
    </BasePageHeader>

    <TurnStatePolicyCard :account-id="selectedAccountId" :model="selectedModelId" @manual-enabled="manualEnabled = $event" />

    <TurnStateOverview
      :accounts="accounts"
      :entries="overview"
      :loading="loadingAccounts || loadingOverview"
      :selected-account-id="selectedAccountId"
      @select-account="selectedAccountId = $event"
      @select-model="selectModel"
    />

    <div class="mt-5 grid min-w-0 gap-4 xl:grid-cols-[minmax(320px,0.82fr)_minmax(0,1.8fr)]">
      <BaseCard title="探测目标" description="选择账号和上游模型，使用已保存的探测策略">
        <div class="grid gap-4">
          <div class="grid gap-2">
            <span id="turn-state-account-label" class="text-cp-sm font-heavy text-cp-text-secondary">OAuth 账号</span>
            <BaseSelect
              v-model="selectedAccountId"
              aria-labelledby="turn-state-account-label"
              :options="accountOptions"
              :disabled="loadingAccounts || probing"
              :placeholder="loadingAccounts ? '加载账号中...' : '选择 OAuth 账号'"
              empty-text="暂无 OpenAI OAuth 账号"
            />
          </div>

          <div class="grid gap-2">
            <span class="flex min-h-8 items-center justify-between gap-3">
              <span id="turn-state-model-label" class="text-cp-sm font-heavy text-cp-text-secondary">上游模型</span>
              <BaseIconButton
                variant="ghost"
                size="sm"
                label="刷新所选账号的模型目录"
                :loading="refreshingModels"
                :disabled="!selectedAccount || loadingModels || probing"
                @click="loadModels(true)"
              >
                <template #loading>
                  <RefreshCw class="size-3.5 animate-spin motion-reduce:animate-none" />
                </template>
                <RefreshCw class="size-3.5" />
              </BaseIconButton>
            </span>
            <BaseSelect
              v-model="selectedModelId"
              aria-labelledby="turn-state-model-label"
              :options="models"
              :disabled="!selectedAccount || loadingModels || refreshingModels || probing"
              :placeholder="loadingModels ? '加载模型中...' : '选择上游模型'"
              empty-text="该账号没有可用模型"
            />
          </div>

          <p v-if="error" role="alert" class="m-0 rounded-cp bg-cp-error-container px-3 py-2.5 text-cp-sm font-emphasis text-cp-error-on-container">
            {{ error }}
          </p>

          <BaseButton
            variant="primary"
            size="lg"
            :loading="probing"
            :disabled="!canProbe || loadingModels || loadingSnapshot"
            @click="runProbe"
          >
            <template #icon>
              <RotateCw class="size-4.5" />
            </template>
            {{ probing ? '正在探测候选代理' : manualEnabled ? '通过代理探测并替换' : '手动探测未开启' }}
          </BaseButton>
        </div>
      </BaseCard>

      <BaseCard title="当前状态" :description="selectedModelId || '等待选择模型'">
        <template #actions>
          <BaseButton size="sm" variant="ghost" :loading="clearingState" :disabled="!selectedAccountId || !selectedModelId || !snapshot" @click="clearSelectedState">
            清空当前 State
          </BaseButton>
        </template>
        <div class="grid gap-3 sm:grid-cols-2 xl:grid-cols-3" aria-live="polite" :aria-busy="loadingSnapshot || undefined">
          <div class="min-h-23 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
            <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary">
              <ShieldCheck class="size-3.5" />
              请求使用
            </span>
            <p class="mt-3 mb-0 text-cp-lg font-heavy" :class="stateActive && snapshot?.stateFirstAppliedAt ? 'text-cp-success-text' : stateActive ? 'text-cp-warning-text' : 'text-cp-text'">
              {{ loadingSnapshot ? '读取中' : applicationLabel }}
            </p>
          </div>
          <div class="min-h-23 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
            <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary">
              <PlayCircle class="size-3.5" />
              首次发送
            </span>
            <p class="mt-3 mb-0 break-words font-mono text-cp-sm font-emphasis text-cp-text">
              {{ formatDateTime(snapshot?.stateFirstAppliedAt) }}
            </p>
          </div>
          <div class="min-h-23 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
            <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary">
              <DatabaseZap class="size-3.5" />
              采集时间
            </span>
            <p class="mt-3 mb-0 break-words font-mono text-cp-sm font-emphasis text-cp-text">
              {{ formatDateTime(snapshot?.stateCapturedAt) }}
            </p>
          </div>
          <div class="min-h-23 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
            <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary">
              <RotateCw class="size-3.5" />
              续采窗口
            </span>
            <p class="mt-3 mb-0 break-words font-mono text-cp-sm font-emphasis text-cp-text">
              {{ formatDateTime(snapshot?.nextRotationAt) }}
            </p>
          </div>
          <div class="min-h-23 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
            <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary">
              <Clock3 class="size-3.5" />
              过期时间
            </span>
            <p class="mt-3 mb-0 break-words font-mono text-cp-sm font-emphasis text-cp-text">
              {{ formatDateTime(snapshot?.stateExpiresAt) }}
            </p>
          </div>
          <div class="min-h-23 rounded-cp bg-cp-fill-quaternary px-3.5 py-3">
            <span class="inline-flex items-center gap-2 text-cp-xs font-heavy text-cp-text-quaternary">
              <Activity class="size-3.5" />
              State 来源
            </span>
            <p class="mt-3 mb-0 truncate text-cp-sm font-heavy text-cp-text" :title="`${sourceLabel(snapshot?.stateSource)} · ${activeTarget}`">
              {{ sourceLabel(snapshot?.stateSource) }}
            </p>
          </div>
        </div>

        <p
          v-if="snapshot?.invalidatedAt"
          class="mt-3 mb-0 rounded-cp bg-cp-warning-container px-3 py-2.5 text-cp-sm font-emphasis text-cp-warning-on-container"
        >
          {{ invalidationMessage(snapshot.invalidationReason) }} · {{ formatDateTime(snapshot.invalidatedAt) }}
        </p>
      </BaseCard>
    </div>

    <BaseCard
      class="mt-4"
      title="探测记录"
      description="当前账号与模型最近 20 次代理探测结果"
    >
      <TurnStateProbeHistory :history="history" :loading="loadingSnapshot" />
    </BaseCard>
  </div>
</template>
