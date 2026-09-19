<script setup lang="ts">
import type { TurnStateProbe, TurnStateProbeAttempt } from '@/api'
import { CheckCircle2, Clock3, Route, XCircle } from '@lucide/vue'

import BaseEmpty from '@/components/base/BaseEmpty.vue'
import { formatDateTime } from '@/utils/date'

defineProps<{
  history: TurnStateProbe[]
  loading: boolean
}>()

function duration(probe: TurnStateProbe) {
  const startedAt = new Date(probe.startedAt).getTime()
  const finishedAt = new Date(probe.finishedAt).getTime()
  return Number.isFinite(startedAt) && Number.isFinite(finishedAt)
    ? `${Math.max(0, finishedAt - startedAt)} ms`
    : '—'
}

function attemptStatus(attempt: TurnStateProbeAttempt) {
  if (attempt.stateAcquired)
    return '已取得 State'
  if (attempt.statusCode !== null)
    return `HTTP ${attempt.statusCode}`
  return '请求失败'
}

function triggerLabel(trigger: TurnStateProbe['trigger']) {
  return trigger === 'automatic_renewal' ? '后台自动轮换' : '管理员手动探测'
}
</script>

<template>
  <div aria-live="polite" :aria-busy="loading || undefined">
    <div v-if="loading" class="flex min-h-44 items-center justify-center gap-2 text-cp-sm font-emphasis text-cp-text-secondary">
      <Clock3 class="size-4 animate-pulse motion-reduce:animate-none" />
      正在加载探测记录
    </div>

    <BaseEmpty
      v-else-if="history.length === 0"
      title="暂无探测记录"
      description="选择账号和模型后执行探测，代理结果会显示在这里"
      :icon="Route"
      surface="none"
    />

    <div v-else class="divide-y divide-cp-split">
      <article v-for="probe in history" :key="`${probe.startedAt}-${probe.finishedAt}`" class="py-5 first:pt-0 last:pb-0">
        <header class="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
          <div class="flex min-w-0 items-start gap-3">
            <span
              class="inline-flex size-9 shrink-0 items-center justify-center rounded-cp"
              :class="probe.activeTargetId ? 'bg-cp-success-container text-cp-success' : 'bg-cp-error-container text-cp-error'"
            >
              <CheckCircle2 v-if="probe.activeTargetId" class="size-4.5" />
              <XCircle v-else class="size-4.5" />
            </span>
            <div class="min-w-0">
              <div class="flex flex-wrap items-center gap-2">
                <h3 class="m-0 text-cp-lg leading-[1.2] font-heavy text-cp-text">
                  {{ probe.activeTargetId ? '探测成功' : '未取得 State' }}
                </h3>
                <span class="rounded-cp-sm bg-cp-fill-quaternary px-2 py-1 font-mono text-cp-xs text-cp-text-secondary">
                  {{ probe.model }}
                </span>
              </div>
              <p class="mt-1.5 mb-0 text-cp-sm leading-relaxed font-emphasis text-cp-text-secondary">
                开始 {{ formatDateTime(probe.startedAt) }} · 完成 {{ formatDateTime(probe.finishedAt) }}
                · {{ triggerLabel(probe.trigger) }} · 总耗时 {{ duration(probe) }} · 本轮实际尝试 {{ probe.attempts.length }} 次
                <template v-if="probe.stateExpiresAt">
                  · State 有效至 {{ formatDateTime(probe.stateExpiresAt) }}
                </template>
              </p>
            </div>
          </div>
          <span
            class="w-fit shrink-0 rounded-cp-sm px-2.5 py-1 text-cp-xs font-heavy"
            :class="probe.activeTargetId ? 'bg-cp-success-container text-cp-success-on-container' : 'bg-cp-error-container text-cp-error-on-container'"
          >
            {{ probe.activeTargetId ? '已就绪，待业务请求应用' : '未替换' }}
          </span>
        </header>

        <div class="mt-4 overflow-hidden rounded-cp bg-cp-fill-quaternary">
          <div class="hidden grid-cols-[minmax(140px,1fr)_minmax(180px,1.7fr)_80px_90px_110px] gap-3 px-3 py-2 text-cp-xs font-heavy text-cp-text-quaternary md:grid">
            <span>代理</span>
            <span>结果</span>
            <span>状态码</span>
            <span>耗时</span>
            <span>State</span>
          </div>
          <div class="divide-y divide-cp-split">
            <div
              v-for="attempt in probe.attempts"
              :key="attempt.targetId"
              class="grid gap-2 bg-cp-bg-container px-3 py-3 md:grid-cols-[minmax(140px,1fr)_minmax(180px,1.7fr)_80px_90px_110px] md:items-center md:gap-3"
            >
              <div class="flex min-w-0 items-center gap-2">
                <span class="min-w-0 wrap-anywhere text-cp-sm font-heavy text-cp-text">{{ attempt.targetLabel }}</span>
                <span
                  v-if="probe.activeTargetId === attempt.targetId"
                  class="shrink-0 rounded-cp-sm bg-cp-success-container px-1.5 py-0.5 text-[11px] font-heavy text-cp-success-on-container"
                >
                  已采用
                </span>
              </div>
              <p class="m-0 wrap-anywhere text-cp-sm leading-[1.35] font-emphasis text-cp-text-secondary">
                {{ attempt.message }}
              </p>
              <span class="font-mono text-cp-sm text-cp-text-secondary">
                <span class="font-sans font-heavy text-cp-text-quaternary md:hidden">状态码 </span>
                {{ attempt.statusCode ?? '—' }}
              </span>
              <span class="font-mono text-cp-sm text-cp-text-secondary">
                <span class="font-sans font-heavy text-cp-text-quaternary md:hidden">耗时 </span>
                {{ attempt.latencyMs }} ms
              </span>
              <span
                class="inline-flex w-fit items-center gap-1.5 text-cp-xs font-heavy"
                :class="attempt.stateAcquired ? 'text-cp-success-text' : 'text-cp-error-text'"
              >
                <CheckCircle2 v-if="attempt.stateAcquired" class="size-3.5" />
                <XCircle v-else class="size-3.5" />
                {{ attemptStatus(attempt) }}
              </span>
            </div>
          </div>
        </div>
      </article>
    </div>
  </div>
</template>
