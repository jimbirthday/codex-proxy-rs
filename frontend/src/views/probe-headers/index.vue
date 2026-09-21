<script setup lang="ts">
import type { TurnStateProbeExchangeDetail, TurnStateProbeExchangeSummary } from '@/api'

import { Eye, Filter, PauseCircle, PlayCircle, RefreshCw, ShieldAlert } from '@lucide/vue'
import { computed, onMounted, onScopeDispose, shallowRef, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'

import {
  getTurnStateCaptureStatus,
  getTurnStateProbeExchange,
  getTurnStateProbeExchanges,
  revealTurnStateProbeExchange,
  startTurnStateCapture,
  stopTurnStateCapture,
} from '@/api'
import { ApiError } from '@/api/request'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import BaseConfirmModal from '@/components/base/BaseConfirmModal.vue'
import BaseIconButton from '@/components/base/BaseIconButton.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BasePageHeader from '@/components/base/BasePageHeader.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseTablePagination from '@/components/base/BaseTable/BaseTablePagination.vue'
import { defineTableColumns } from '@/components/base/BaseTable/columns'
import BaseTable from '@/components/base/BaseTable/index.vue'
import { toast } from '@/components/base/BaseToast'
import { errorMessage } from '@/utils/async'
import { formatDateTime } from '@/utils/date'
import ProbeHeaderDetailModal from './ProbeHeaderDetailModal.vue'

const route = useRoute()
const router = useRouter()
const status = shallowRef<Awaited<ReturnType<typeof getTurnStateCaptureStatus>> | null>(null)
const rows = shallowRef<TurnStateProbeExchangeSummary[]>([])
const total = shallowRef(0)
const page = shallowRef(1)
const pageSize = shallowRef(20)
const loading = shallowRef(true)
const refreshing = shallowRef(false)
const mutating = shallowRef(false)
const error = shallowRef('')
const duration = shallowRef('15')
const accountId = shallowRef('')
const modelId = shallowRef('')
const trigger = shallowRef('')
const statusCode = shallowRef('')
const stateOnly = shallowRef(false)
const now = shallowRef(Date.now())
const detailOpen = shallowRef(false)
const detailLoading = shallowRef(false)
const revealing = shallowRef(false)
const detail = shallowRef<TurnStateProbeExchangeDetail | null>(null)
const startConfirmOpen = shallowRef(false)
let listController: AbortController | undefined
let detailController: AbortController | undefined
let listSequence = 0
let detailSequence = 0

const durationOptions = [
  { label: '15 分钟', value: '15' },
  { label: '1 小时', value: '60' },
  { label: '6 小时', value: '360' },
]
const triggerOptions = [
  { label: '全部触发方式', value: '' },
  { label: '管理员手动探测', value: 'manual_probe' },
  { label: '后台自动轮换', value: 'automatic_renewal' },
]

const columns = defineTableColumns<TurnStateProbeExchangeSummary>([
  { key: 'startedAt', label: '时间', kind: 'datetime', size: 'xl' },
  { key: 'trigger', label: '触发', kind: 'status', size: 'lg' },
  { key: 'accountId', label: '账号', kind: 'mono', size: '2xl' },
  { key: 'model', label: '模型', kind: 'mono', size: 'xl' },
  { key: 'targetLabel', label: '代理', kind: 'text', size: 'xl' },
  { key: 'statusCode', label: 'HTTP', kind: 'status', size: 'sm' },
  { key: 'outcome', label: '结果', kind: 'custom', size: 'xl' },
  { key: 'latencyMs', label: '耗时', kind: 'numeric', size: 'md' },
  { key: 'headers', label: '报头', kind: 'custom', size: 'lg' },
  { key: 'turnState', label: 'Turn State', kind: 'custom', size: 'xl' },
  { key: 'actions', label: '操作', kind: 'actions', size: 'sm', hideable: false },
])

const captureActive = computed(() => Boolean(
  status.value?.enabledUntil && new Date(status.value.enabledUntil).getTime() > now.value,
))
const remaining = computed(() => {
  if (!status.value?.enabledUntil)
    return '未开启'
  const seconds = Math.max(0, Math.ceil((new Date(status.value.enabledUntil).getTime() - now.value) / 1000))
  if (seconds <= 0)
    return '已结束'
  const hours = Math.floor(seconds / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  const rest = seconds % 60
  return hours > 0 ? `${hours} 小时 ${minutes} 分` : `${minutes} 分 ${rest} 秒`
})
const pagination = computed(() => ({
  currentPage: page.value,
  pageSize: pageSize.value,
  total: total.value,
}))
const selectedDurationLabel = computed(() =>
  durationOptions.find(option => option.value === duration.value)?.label ?? '15 分钟',
)

function cancelled(cause: unknown) {
  return cause instanceof ApiError && cause.kind === 'cancelled'
}

function queryParams() {
  const parsedStatus = statusCode.value.trim() ? Number(statusCode.value.trim()) : undefined
  return {
    page: page.value,
    pageSize: pageSize.value,
    accountId: accountId.value.trim() || undefined,
    modelId: modelId.value.trim() || undefined,
    trigger: trigger.value as '' | 'manual_probe' | 'automatic_renewal',
    statusCode: Number.isInteger(parsedStatus) ? parsedStatus : undefined,
    stateOnly: stateOnly.value || undefined,
  }
}

async function loadStatus(silent = false) {
  try {
    status.value = await getTurnStateCaptureStatus({ silent })
  }
  catch (cause) {
    if (!silent)
      error.value = errorMessage(cause, '读取采集状态失败')
  }
}

async function loadRows(background = false) {
  const sequence = ++listSequence
  listController?.abort()
  const controller = new AbortController()
  listController = controller
  if (!background)
    loading.value = true
  error.value = ''
  try {
    const result = await getTurnStateProbeExchanges(queryParams(), {
      signal: controller.signal,
      silent: background,
    })
    if (sequence !== listSequence)
      return
    rows.value = result.items
    total.value = result.total
    page.value = result.page
    pageSize.value = result.pageSize
  }
  catch (cause) {
    if (sequence === listSequence && !background && !cancelled(cause))
      error.value = errorMessage(cause, '加载探测报头失败')
  }
  finally {
    if (sequence === listSequence && !background)
      loading.value = false
  }
}

async function refresh() {
  if (refreshing.value)
    return
  refreshing.value = true
  await Promise.all([loadStatus(true), loadRows(true)])
  refreshing.value = false
}

async function startCapture() {
  if (mutating.value)
    return
  mutating.value = true
  try {
    status.value = await startTurnStateCapture(Number(duration.value) as 15 | 60 | 360)
    startConfirmOpen.value = false
    toast.success('探测报头采集已开启')
  }
  catch (cause) {
    toast.error(errorMessage(cause, '开启采集失败'))
  }
  finally {
    mutating.value = false
  }
}

async function stopCapture() {
  if (mutating.value)
    return
  mutating.value = true
  try {
    status.value = await stopTurnStateCapture()
    toast.success('探测报头采集已停止')
  }
  catch (cause) {
    toast.error(errorMessage(cause, '停止采集失败'))
  }
  finally {
    mutating.value = false
  }
}

function applyFilters() {
  const code = statusCode.value.trim()
  if (code && (!/^\d{3}$/.test(code) || Number(code) < 100)) {
    toast.warning('HTTP 状态码应为 100 至 999')
    return
  }
  page.value = 1
  void loadRows()
}

function resetFilters() {
  accountId.value = ''
  modelId.value = ''
  trigger.value = ''
  statusCode.value = ''
  stateOnly.value = false
  page.value = 1
  void loadRows()
}

function handlePageChange(nextPage: number) {
  page.value = nextPage
  void loadRows()
}

function handlePageSizeChange(nextPageSize: number) {
  pageSize.value = nextPageSize
  page.value = 1
  void loadRows()
}

async function openDetail(id: string) {
  const sequence = ++detailSequence
  detailController?.abort()
  const controller = new AbortController()
  detailController = controller
  detail.value = null
  detailOpen.value = true
  detailLoading.value = true
  try {
    let loaded: TurnStateProbeExchangeDetail | null = null
    for (let attempt = 0; attempt < 4; attempt += 1) {
      try {
        loaded = await getTurnStateProbeExchange(id, {
          signal: controller.signal,
          silent: true,
        })
        break
      }
      catch (cause) {
        if (cancelled(cause) || sequence !== detailSequence || !detailOpen.value)
          return
        if (!(cause instanceof ApiError && cause.status === 404 && attempt < 3))
          throw cause
        await new Promise(resolve => window.setTimeout(resolve, 250))
      }
    }
    if (sequence !== detailSequence || !loaded)
      return
    detail.value = loaded
    if (route.query.exchangeId !== id)
      await router.replace({ query: { ...route.query, exchangeId: id } })
  }
  catch (cause) {
    if (sequence === detailSequence && !cancelled(cause))
      toast.error(errorMessage(cause, '读取报头详情失败'))
  }
  finally {
    if (sequence === detailSequence)
      detailLoading.value = false
  }
}

async function revealDetail() {
  if (!detail.value || revealing.value)
    return
  revealing.value = true
  try {
    detail.value = await revealTurnStateProbeExchange(detail.value.summary.id)
    toast.success('已显示敏感报头原文')
  }
  catch (cause) {
    toast.error(errorMessage(cause, '显示敏感报头失败'))
  }
  finally {
    revealing.value = false
  }
}

function triggerLabel(value: TurnStateProbeExchangeSummary['trigger']) {
  return value === 'automatic_renewal' ? '自动轮换' : '手动探测'
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

function outcomeClass(value: string) {
  if (value === 'state_acquired')
    return 'text-cp-success-text'
  if (value === 'response_without_state' || value === 'upstream_312')
    return 'text-cp-warning-text'
  return 'text-cp-error-text'
}

function formatBytes(value: number) {
  if (value < 1024)
    return `${value} B`
  return `${(value / 1024).toFixed(1)} KiB`
}

function stateLabel(row: TurnStateProbeExchangeSummary) {
  const summary = row.responseTurnState.present ? row.responseTurnState : row.requestTurnState
  if (!summary.present)
    return '无 State 头'
  if (summary.count > 1)
    return `${summary.count} 个重复值`
  return `${summary.byteLength ?? 0} B · ${summary.valid292 ? '有效 292' : '非 292'}`
}

const clockTimer = window.setInterval(() => {
  now.value = Date.now()
}, 1000)
const refreshTimer = window.setInterval(() => {
  if (captureActive.value)
    void refresh()
}, 5000)

watch(detailOpen, (open) => {
  if (open)
    return
  detailSequence += 1
  detailController?.abort()
  detailLoading.value = false
  detail.value = null
  if (route.query.exchangeId) {
    const query = { ...route.query }
    delete query.exchangeId
    void router.replace({ query })
  }
})

onMounted(async () => {
  await Promise.all([loadStatus(), loadRows()])
  const exchangeId = typeof route.query.exchangeId === 'string' ? route.query.exchangeId : ''
  if (exchangeId)
    await openDetail(exchangeId)
})

onScopeDispose(() => {
  listSequence += 1
  detailSequence += 1
  window.clearInterval(clockTimer)
  window.clearInterval(refreshTimer)
  listController?.abort()
  detailController?.abort()
})
</script>

<template>
  <div class="w-full pb-6">
    <BasePageHeader
      class="relative max-md:pl-12 max-md:[&>div:last-child]:absolute max-md:[&>div:last-child]:top-0 max-md:[&>div:last-child]:right-0"
      title="探测报头"
      description="限时采集 Codex State 手动探测与自动轮换的请求头和响应头"
    >
      <template #actions>
        <BaseIconButton variant="ghost" size="md" label="刷新探测报头" :loading="refreshing" @click="refresh">
          <template #loading>
            <RefreshCw class="size-4 animate-spin motion-reduce:animate-none" />
          </template>
          <RefreshCw class="size-4" />
        </BaseIconButton>
      </template>
    </BasePageHeader>

    <BaseCard padding="compact" title="采集窗口" :description="`原始记录保留 ${status?.retentionHours ?? 24} 小时，服务重启后自动关闭`">
      <div class="grid gap-3 lg:grid-cols-[minmax(220px,0.8fr)_minmax(260px,1fr)_auto] lg:items-center">
        <div class="flex min-w-0 items-center gap-3">
          <span class="inline-flex size-10 shrink-0 items-center justify-center rounded-cp" :class="captureActive ? 'bg-cp-success-container text-cp-success' : 'bg-cp-fill-quaternary text-cp-text-quaternary'">
            <PlayCircle v-if="captureActive" class="size-5" />
            <PauseCircle v-else class="size-5" />
          </span>
          <div class="min-w-0">
            <p class="m-0 font-heavy" :class="captureActive ? 'text-cp-success-text' : 'text-cp-text'">
              {{ captureActive ? '正在采集探测报头' : '采集已关闭' }}
            </p>
            <p class="mt-1 mb-0 text-cp-xs font-emphasis text-cp-text-secondary">
              剩余 {{ remaining }}
            </p>
          </div>
        </div>

        <div class="grid grid-cols-2 gap-2 text-cp-xs sm:grid-cols-4">
          <div class="rounded-cp bg-cp-fill-quaternary px-2.5 py-2">
            <span class="text-cp-text-quaternary">队列</span><strong class="mt-1 block font-mono text-cp-text">{{ status?.buffer.queuedItems ?? 0 }}</strong>
          </div>
          <div class="rounded-cp bg-cp-fill-quaternary px-2.5 py-2">
            <span class="text-cp-text-quaternary">已写入</span><strong class="mt-1 block font-mono text-cp-text">{{ status?.buffer.persistedTotal ?? 0 }}</strong>
          </div>
          <div class="rounded-cp bg-cp-fill-quaternary px-2.5 py-2">
            <span class="text-cp-text-quaternary">已丢弃</span><strong class="mt-1 block font-mono" :class="status?.buffer.droppedTotal ? 'text-cp-warning-text' : 'text-cp-text'">{{ status?.buffer.droppedTotal ?? 0 }}</strong>
          </div>
          <div class="rounded-cp bg-cp-fill-quaternary px-2.5 py-2">
            <span class="text-cp-text-quaternary">写入失败</span><strong class="mt-1 block font-mono" :class="status?.buffer.writeFailureTotal ? 'text-cp-error-text' : 'text-cp-text'">{{ status?.buffer.writeFailureTotal ?? 0 }}</strong>
          </div>
        </div>

        <div class="flex items-center justify-end gap-2">
          <BaseSelect v-if="!captureActive" v-model="duration" class="w-32" aria-label="采集时长" :options="durationOptions" :disabled="mutating" />
          <BaseButton v-if="!captureActive" variant="primary" :loading="mutating" @click="startConfirmOpen = true">
            <template #icon>
              <PlayCircle class="size-4" />
            </template>开启采集
          </BaseButton>
          <BaseButton v-else variant="destructive" :loading="mutating" @click="stopCapture">
            <template #icon>
              <PauseCircle class="size-4" />
            </template>停止采集
          </BaseButton>
        </div>
      </div>
    </BaseCard>

    <BaseCard class="mt-4" padding="compact" title="交换记录" description="列表不返回报头原文，点击详情后按需读取">
      <div class="mb-3 grid gap-2 md:grid-cols-2 xl:grid-cols-[1fr_1fr_180px_130px_auto_auto] xl:items-center" role="group" aria-label="探测报头筛选">
        <BaseInput v-model="accountId" placeholder="账号 ID" aria-label="按账号 ID 筛选" />
        <BaseInput v-model="modelId" placeholder="上游模型" aria-label="按上游模型筛选" />
        <BaseSelect v-model="trigger" :options="triggerOptions" aria-label="按触发方式筛选" />
        <BaseInput v-model="statusCode" type="number" placeholder="状态码" aria-label="按 HTTP 状态码筛选" />
        <BaseCheckbox v-model="stateOnly" label="仅看 State" show-label />
        <div class="flex justify-end gap-2">
          <BaseButton variant="ghost" @click="resetFilters">
            重置
          </BaseButton>
          <BaseButton variant="secondary" @click="applyFilters">
            <template #icon>
              <Filter class="size-4" />
            </template>筛选
          </BaseButton>
        </div>
      </div>

      <p v-if="error" role="alert" class="m-0 mb-3 rounded-cp bg-cp-error-container px-3 py-2.5 text-cp-sm font-emphasis text-cp-error-on-container">
        {{ error }}
      </p>
      <BaseTable :columns="columns" :rows="rows" row-key="id" :loading="loading" density="compact" empty-text="当前筛选条件下暂无探测报头">
        <template #startedAt="{ row }">
          <span>{{ formatDateTime(row.startedAt) }}</span>
        </template>
        <template #trigger="{ row }">
          <span class="rounded-cp-sm bg-cp-fill-quaternary px-2 py-1 text-cp-xs font-heavy text-cp-text-secondary">{{ triggerLabel(row.trigger) }}</span>
        </template>
        <template #statusCode="{ row }">
          <span class="font-mono font-bold" :class="row.statusCode && row.statusCode < 300 ? 'text-cp-success-text' : row.statusCode ? 'text-cp-warning-text' : 'text-cp-text-quaternary'">{{ row.statusCode ?? '—' }}</span>
        </template>
        <template #outcome="{ row }">
          <span class="text-cp-xs font-heavy" :class="outcomeClass(row.outcome)">{{ outcomeLabel(row.outcome) }}</span>
        </template>
        <template #latencyMs="{ row }">
          <span>{{ row.latencyMs }} ms</span>
        </template>
        <template #headers="{ row }">
          <span class="font-mono text-cp-xs text-cp-text-secondary">请求 {{ row.requestHeaderCount }} / {{ formatBytes(row.requestHeaderBytes) }}<br>响应 {{ row.responseHeaderCount }} / {{ formatBytes(row.responseHeaderBytes) }}</span>
        </template>
        <template #turnState="{ row }">
          <span class="text-cp-xs font-heavy" :class="row.responseTurnState.valid292 ? 'text-cp-success-text' : row.responseTurnState.present || row.requestTurnState.present ? 'text-cp-warning-text' : 'text-cp-text-quaternary'">{{ stateLabel(row) }}</span>
        </template>
        <template #actions="{ row }">
          <BaseIconButton variant="ghost" size="md" label="查看探测报头详情" @click="openDetail(row.id)">
            <Eye class="size-4" />
          </BaseIconButton>
        </template>
      </BaseTable>
      <BaseTablePagination :pagination="pagination" :loading="loading" @page-change="handlePageChange" @page-size-change="handlePageSizeChange" />
    </BaseCard>

    <p class="mt-3 mb-0 flex items-start gap-2 text-cp-xs leading-relaxed font-emphasis text-cp-text-quaternary">
      <ShieldAlert class="mt-0.5 size-3.5 shrink-0" />
      记录为 reqwest/hyper 可见报头，不包含 HTTP/2 伪头、代理 CONNECT 或 TLS 信息，Base64 只是编码并非加密
    </p>
  </div>

  <ProbeHeaderDetailModal v-model="detailOpen" :detail="detail" :loading="detailLoading" :revealing="revealing" @reveal="revealDetail" />
  <BaseConfirmModal
    v-model="startConfirmOpen"
    title="开启敏感报头采集"
    :description="`将采集认证、Cookie 与 Turn State 原文，持续 ${selectedDurationLabel}，记录保留 24 小时`"
    confirm-text="确认开启"
    :loading="mutating"
    @confirm="startCapture"
  />
</template>
