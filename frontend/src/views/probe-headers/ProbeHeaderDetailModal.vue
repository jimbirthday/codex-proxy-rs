<script setup lang="ts">
import type { TurnStateProbeExchangeDetail, TurnStateProbeHeader } from '@/api'

import { Copy, Eye, LoaderCircle, ShieldAlert } from '@lucide/vue'
import { computed, shallowRef, watch } from 'vue'

import BaseButton from '@/components/base/BaseButton.vue'
import BaseConfirmModal from '@/components/base/BaseConfirmModal.vue'
import BaseEmpty from '@/components/base/BaseEmpty.vue'
import BaseIconButton from '@/components/base/BaseIconButton.vue'
import BaseModal from '@/components/base/BaseModal/index.vue'
import { useCopyText } from '@/composables/useCopyText'
import { formatDateTime } from '@/utils/date'

const props = defineProps<{
  detail: TurnStateProbeExchangeDetail | null
  loading: boolean
  revealing: boolean
}>()
const emit = defineEmits<{ reveal: [] }>()
const open = defineModel<boolean>({ default: false })
const revealConfirmOpen = shallowRef(false)
const copyText = useCopyText()
const headerSides = ['request', 'response'] as const

const summary = computed(() => props.detail?.summary ?? null)
const hasSensitiveHeaders = computed(() => Boolean(props.detail
  && [...props.detail.requestHeaders, ...props.detail.responseHeaders].some(header => header.sensitive)))

watch(open, (value) => {
  if (!value)
    revealConfirmOpen.value = false
})
watch(() => props.detail?.revealed, (revealed) => {
  if (revealed)
    revealConfirmOpen.value = false
})

function decodeHeaderValue(header: TurnStateProbeHeader) {
  if (!header.valueBase64)
    return `已隐藏 · ${header.byteLength} B`
  try {
    const binary = window.atob(header.valueBase64)
    const bytes = Uint8Array.from(binary, char => char.charCodeAt(0))
    const text = new TextDecoder('utf-8', { fatal: true }).decode(bytes)
    if (!/[\r\n]/.test(text))
      return text
  }
  catch {}
  return `Base64: ${header.valueBase64}`
}

function copyHeader(header: TurnStateProbeHeader) {
  if (!header.valueBase64)
    return
  const display = decodeHeaderValue(header)
  const value = display.startsWith('Base64: ') ? header.valueBase64 : display
  void copyText(value, {
    successText: `已复制 ${header.name}`,
    emptyErrorText: '报头值为空',
  })
}

function stateSummary(count: number, length: number | null, valid: boolean) {
  if (count === 0)
    return '未出现'
  if (count > 1)
    return `${count} 个重复值`
  return `${length ?? 0} B · ${valid ? '有效 292' : '非 292'}`
}

function triggerLabel(trigger: TurnStateProbeExchangeDetail['summary']['trigger']) {
  return trigger === 'automatic_renewal' ? '后台自动轮换' : '管理员手动探测'
}

function outcomeLabel(value: string) {
  switch (value) {
    case 'state_acquired': return '已取得 State'
    case 'upstream_312': return '上游撤销 State'
    case 'response_without_state': return '响应无有效 State'
    case 'connection_failed': return '连接失败'
    case 'timed_out': return '请求超时'
    case 'response_read_timed_out': return '响应读取超时'
    case 'response_read_failed': return '响应读取失败'
    default: return value
  }
}
</script>

<template>
  <BaseModal
    v-model="open"
    title="探测报头详情"
    :description="summary ? `${formatDateTime(summary.startedAt)} · ${summary.model}` : '读取请求与响应报头'"
    size="xl"
    :dismissible="!revealing"
  >
    <div v-if="loading" class="flex min-h-72 items-center justify-center gap-2 text-cp-sm font-emphasis text-cp-text-secondary" aria-live="polite">
      <LoaderCircle class="size-4.5 animate-spin motion-reduce:animate-none" />
      正在读取报头详情
    </div>

    <BaseEmpty
      v-else-if="!detail"
      title="详情不可用"
      description="记录可能尚未入库或已经过期"
      :icon="ShieldAlert"
      surface="none"
    />

    <div v-else class="grid gap-4">
      <div class="grid gap-2 rounded-cp bg-cp-fill-quaternary p-3 text-cp-sm sm:grid-cols-2 xl:grid-cols-3">
        <div>
          <span class="text-cp-xs font-heavy text-cp-text-quaternary">账号 / 模型</span>
          <p class="mt-1 mb-0 wrap-anywhere font-mono font-emphasis text-cp-text">
            {{ detail.summary.accountId }} · {{ detail.summary.model }}
          </p>
        </div>
        <div>
          <span class="text-cp-xs font-heavy text-cp-text-quaternary">代理 / HTTP</span>
          <p class="mt-1 mb-0 wrap-anywhere font-emphasis text-cp-text">
            {{ detail.summary.targetLabel }} · {{ detail.summary.httpVersion ?? '无版本' }} · {{ detail.summary.statusCode ?? '无响应' }} · {{ detail.summary.latencyMs }} ms
          </p>
        </div>
        <div>
          <span class="text-cp-xs font-heavy text-cp-text-quaternary">触发 / 结果</span>
          <p class="mt-1 mb-0 wrap-anywhere font-emphasis text-cp-text">
            {{ triggerLabel(detail.summary.trigger) }} · {{ outcomeLabel(detail.summary.outcome) }}
          </p>
        </div>
        <div>
          <span class="text-cp-xs font-heavy text-cp-text-quaternary">请求 ID</span>
          <p class="mt-1 mb-0 wrap-anywhere font-mono font-emphasis text-cp-text">
            {{ detail.summary.requestId }}
          </p>
        </div>
        <div>
          <span class="text-cp-xs font-heavy text-cp-text-quaternary">请求 State</span>
          <p class="mt-1 mb-0 font-mono font-emphasis text-cp-text">
            {{ stateSummary(detail.summary.requestTurnState.count, detail.summary.requestTurnState.byteLength, detail.summary.requestTurnState.valid292) }}
          </p>
        </div>
        <div>
          <span class="text-cp-xs font-heavy text-cp-text-quaternary">响应 State</span>
          <p class="mt-1 mb-0 font-mono font-emphasis text-cp-text">
            {{ stateSummary(detail.summary.responseTurnState.count, detail.summary.responseTurnState.byteLength, detail.summary.responseTurnState.valid292) }}
          </p>
        </div>
      </div>

      <p v-if="!detail.revealed && hasSensitiveHeaders" class="m-0 rounded-cp bg-cp-warning-container px-3 py-2.5 text-cp-sm font-emphasis text-cp-warning-on-container">
        认证、Cookie、API Key 与 Turn State 默认隐藏，显示原文操作会写入管理员审计
      </p>

      <div class="grid min-w-0 gap-4 lg:grid-cols-2">
        <section v-for="side in headerSides" :key="side" class="min-w-0 overflow-hidden rounded-cp bg-cp-fill-quaternary">
          <header class="flex items-center justify-between gap-3 px-3 py-2.5">
            <h3 class="m-0 text-cp font-heavy text-cp-text">
              {{ side === 'request' ? '请求头' : '响应头' }}
            </h3>
            <span class="font-mono text-cp-xs text-cp-text-quaternary">
              {{ side === 'request' ? detail.summary.requestHeaderCount : detail.summary.responseHeaderCount }} 项
            </span>
          </header>
          <div class="max-h-112 divide-y divide-cp-split overflow-auto bg-cp-bg-container">
            <div
              v-for="(header, index) in side === 'request' ? detail.requestHeaders : detail.responseHeaders"
              :key="`${header.name}-${index}`"
              class="grid min-w-0 grid-cols-[minmax(112px,0.72fr)_minmax(0,1.6fr)_36px] items-start gap-2 px-3 py-2.5"
            >
              <code class="wrap-anywhere font-mono text-cp-xs font-bold text-cp-text">{{ header.name }}</code>
              <code
                class="wrap-anywhere whitespace-pre-wrap font-mono text-cp-xs leading-[1.45]"
                :class="header.valueBase64 ? 'text-cp-text-secondary' : 'text-cp-warning-text'"
              >
                {{ decodeHeaderValue(header) }}
              </code>
              <BaseIconButton
                variant="ghost"
                size="sm"
                :label="`复制 ${header.name}`"
                :disabled="!header.valueBase64"
                @click="copyHeader(header)"
              >
                <Copy class="size-3.5" />
              </BaseIconButton>
            </div>
            <p v-if="(side === 'request' ? detail.requestHeaders : detail.responseHeaders).length === 0" class="m-0 px-3 py-8 text-center text-cp-sm text-cp-text-quaternary">
              没有捕获到报头
            </p>
          </div>
        </section>
      </div>
    </div>

    <template #footer>
      <BaseButton v-if="detail && !detail.revealed && hasSensitiveHeaders" variant="soft" :disabled="revealing" @click="revealConfirmOpen = true">
        <template #icon>
          <Eye class="size-4" />
        </template>
        显示敏感原文
      </BaseButton>
      <BaseButton variant="secondary" :disabled="revealing" @click="open = false">
        关闭
      </BaseButton>
    </template>
  </BaseModal>

  <BaseConfirmModal
    v-model="revealConfirmOpen"
    title="显示敏感报头原文"
    description="该操作会记录管理员审计，请避免在截图、日志或工单中泄露内容"
    confirm-text="确认显示"
    :loading="revealing"
    @confirm="emit('reveal')"
  />
</template>
