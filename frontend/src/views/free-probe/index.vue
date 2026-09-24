<script setup lang="ts">
import type { FreeProbeExchange, ProbeHeader } from '@/api/modules/free-probe'
import type { SelectOption } from '@/components/base/BaseSelect.vue'

import { Plus, Trash2 } from '@lucide/vue'
import { useStorage } from '@vueuse/core'
import { computed, onMounted, onScopeDispose, ref, shallowRef, watch } from 'vue'
import { getAccounts, getProxies } from '@/api'
import { probeBodyUrl, sendFreeProbe } from '@/api/modules/free-probe'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import FormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseIconButton from '@/components/base/BaseIconButton.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BasePageHeader from '@/components/base/BasePageHeader.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseTextarea from '@/components/base/BaseTextarea.vue'
import { useCopyText } from '@/composables/useCopyText'
import { errorMessage } from '@/utils/async'

interface ProbeHeaderDraft {
  id: number
  enabled: boolean
  name: string
  value: string
  encoding: string
}

interface FreeProbeDraft {
  mode: string
  model: string
  method: string
  url: string
  accountId: string
  useAccountHeaders: boolean
  proxyId: string
  proxyUrl: string
  timeout: string
  body: string
  bodyEncoding: string
  headers: ProbeHeaderDraft[]
}

const defaultHeaders: ProbeHeaderDraft[] = [{ id: 1, enabled: true, name: 'Content-Type', value: 'application/json', encoding: 'text' }]
const storedDraft = useStorage<FreeProbeDraft>('codex-proxy-rs-free-probe', {
  mode: 'websocket_prewarm',
  model: 'gpt-5.4',
  method: 'POST',
  url: '',
  accountId: '',
  useAccountHeaders: true,
  proxyId: '',
  proxyUrl: '',
  timeout: '60',
  body: '',
  bodyEncoding: 'text',
  headers: defaultHeaders,
})
const mode = shallowRef(storedDraft.value.mode)
const model = shallowRef(storedDraft.value.model)
const method = shallowRef(storedDraft.value.method)
const url = shallowRef(storedDraft.value.url)
const accountId = shallowRef(storedDraft.value.accountId)
const useAccountHeaders = shallowRef(storedDraft.value.useAccountHeaders)
const proxyId = shallowRef(storedDraft.value.proxyId)
const proxyUrl = shallowRef(storedDraft.value.proxyUrl)
const timeout = shallowRef(storedDraft.value.timeout)
const body = shallowRef(storedDraft.value.body)
const bodyEncoding = shallowRef(storedDraft.value.bodyEncoding)
let nextHeaderId = 1
const headers = ref<ProbeHeaderDraft[]>(
  (storedDraft.value.headers?.length ? storedDraft.value.headers : defaultHeaders).map(header => ({ ...header, id: nextHeaderId++ })),
)
const accounts = shallowRef<SelectOption[]>([{ label: '不使用账号', value: '' }])
const proxies = shallowRef<SelectOption[]>([{ label: '直连', value: '' }, { label: '自定义代理地址', value: 'custom' }])
const loadingCatalog = shallowRef(false)
const catalogError = shallowRef('')
const busy = shallowRef(false)
const error = shallowRef('')
const result = shallowRef<FreeProbeExchange | null>(null)
const bodyId = shallowRef('')
const receivedBytes = shallowRef(0)
let preview = ''
const side = shallowRef('response')
const view = shallowRef('text')
const copyText = useCopyText()
let controller: AbortController | undefined
const catalogController = new AbortController()
const encodings = [{ label: '文本 UTF-8', value: 'text' }, { label: 'Base64', value: 'base64' }]
const views = [...encodings, { label: '十六进制', value: 'hex' }]

function saveDraft() {
  storedDraft.value = {
    mode: mode.value,
    model: model.value,
    method: method.value,
    url: url.value,
    accountId: accountId.value,
    useAccountHeaders: useAccountHeaders.value,
    proxyId: proxyId.value,
    proxyUrl: proxyUrl.value,
    timeout: timeout.value,
    body: body.value,
    bodyEncoding: bodyEncoding.value,
    headers: headers.value.map(header => ({ ...header })),
  }
}

function accountIdentifier(account: Awaited<ReturnType<typeof getAccounts>>['items'][number]) {
  return account.email?.trim() || account.accountId?.trim() || account.userId?.trim() || account.id
}

function bytes(value: string) {
  return Uint8Array.from(atob(value), char => char.charCodeAt(0))
}

function encode(value: string, encoding: string) {
  if (encoding === 'base64') {
    bytes(value)
    return value
  }
  const data = new TextEncoder().encode(value)
  let binary = ''
  for (const byte of data)
    binary += String.fromCharCode(byte)
  return btoa(binary)
}

function display(value: string) {
  if (view.value === 'base64')
    return value
  const data = bytes(value)
  if (view.value === 'hex')
    return Array.from(data, byte => byte.toString(16).padStart(2, '0')).join(' ')
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(data)
  }
  catch {
    return `非 UTF-8 字节，请切换 Base64 或十六进制视图\nBase64: ${value}`
  }
}

const displayedHeaders = computed<ProbeHeader[]>(() => {
  if (!result.value)
    return []
  return side.value === 'request' ? result.value.requestHeaders : result.value.responseHeaders
})
const displayedBody = computed(() => {
  if (!result.value)
    return ''
  return side.value === 'request' ? result.value.requestBodyBase64 : result.value.responseBodyBase64
})

async function loadCatalog() {
  loadingCatalog.value = true
  catalogError.value = ''
  try {
    const [accountRows, proxyRows] = await Promise.all([
      (async () => {
        const rows = [{ label: '不使用账号', value: '' }]
        for (let page = 1; ; page++) {
          const response = await getAccounts({ page, pageSize: 200 }, { signal: catalogController.signal, silent: true })
          rows.push(...response.items.map(item => ({ label: item.name || item.provider, description: accountIdentifier(item), value: item.id })))
          if (page >= response.page.totalPages)
            return rows
        }
      })(),
      (async () => {
        const rows = [{ label: '直连', value: '' }, { label: '自定义代理地址', value: 'custom' }]
        for (let page = 1; ; page++) {
          const response = await getProxies({ page, pageSize: 200 }, { signal: catalogController.signal, silent: true })
          rows.push(...response.items.map(item => ({ label: item.name, value: item.id })))
          if (page >= response.page.totalPages)
            return rows
        }
      })(),
    ])
    accounts.value = accountRows
    proxies.value = proxyRows
    if (accountId.value && !accountRows.some(option => option.value === accountId.value))
      accountId.value = ''
    if (proxyId.value && proxyId.value !== 'custom' && !proxyRows.some(option => option.value === proxyId.value))
      proxyId.value = ''
  }
  catch (cause) {
    if (!catalogController.signal.aborted)
      catalogError.value = errorMessage(cause, '加载账号和代理失败')
  }
  finally {
    loadingCatalog.value = false
  }
}

watch([mode, model, method, url, accountId, useAccountHeaders, proxyId, proxyUrl, timeout, body, bodyEncoding, headers], saveDraft, { deep: true })

async function send() {
  if (busy.value)
    return
  error.value = ''
  result.value = null
  bodyId.value = ''
  receivedBytes.value = 0
  preview = ''
  const timeoutSeconds = Number(timeout.value)
  if (!Number.isSafeInteger(timeoutSeconds) || timeoutSeconds < 0 || timeout.value.trim() === '') {
    error.value = '超时秒数必须是非负整数'
    return
  }
  controller = new AbortController()
  busy.value = true
  try {
    if (mode.value === 'websocket_prewarm' && !accountId.value) {
      error.value = 'WebSocket 预热必须选择 OpenAI OAuth 账号'
      return
    }
    if (mode.value === 'websocket_prewarm' && !model.value.trim()) {
      error.value = '请填写预热模型'
      return
    }
    await sendFreeProbe({
      accountId: accountId.value || null,
      useAccountHeaders: Boolean(accountId.value) && useAccountHeaders.value,
      proxyId: proxyId.value && proxyId.value !== 'custom' ? proxyId.value : null,
      proxyUrl: proxyId.value === 'custom' ? proxyUrl.value : null,
      method: method.value,
      url: mode.value === 'websocket_prewarm' ? 'wss://chatgpt.com/backend-api/codex/responses' : url.value,
      mode: mode.value === 'websocket_prewarm' ? 'websocket_prewarm' : 'http',
      model: mode.value === 'websocket_prewarm' ? model.value.trim() : null,
      headers: headers.value.filter(header => header.enabled).map(header => ({
        name: header.name,
        valueBase64: encode(header.value, header.encoding),
      })),
      bodyBase64: encode(body.value, bodyEncoding.value),
      timeoutSeconds,
    }, (event) => {
      if (event.type === 'prepared') {
        bodyId.value = event.id
      }
      else if (event.type === 'headers') {
        result.value = event.exchange
      }
      else if (event.type === 'progress' && result.value) {
        receivedBytes.value = event.receivedBytes
        preview += atob(event.previewBase64)
        result.value = { ...result.value, responseBodyBase64: btoa(preview) }
      }
      else if (event.type === 'complete' && result.value) {
        result.value = { ...result.value, elapsedMs: event.elapsedMs, error: event.error }
      }
    }, { signal: controller.signal, silent: true })
  }
  catch (cause) {
    error.value = controller.signal.aborted ? '已停止探测，已接收的正文可下载；已发出的请求无法撤回' : errorMessage(cause, '探测失败，请检查请求和 Base64 格式')
  }
  finally {
    busy.value = false
  }
}

function reuseHeaders() {
  if (!result.value)
    return
  headers.value = result.value.requestHeaders.filter(header => !result.value!.automaticRequestHeaders.includes(header.name.toLowerCase())).map(header => ({
    id: nextHeaderId++,
    enabled: true,
    name: header.name,
    value: header.valueBase64,
    encoding: 'base64',
  }))
  useAccountHeaders.value = false
}

function downloadBody() {
  if (side.value === 'response') {
    const link = document.createElement('a')
    link.href = probeBodyUrl(bodyId.value)
    link.download = 'response-body.bin'
    link.click()
    return
  }
  const href = URL.createObjectURL(new Blob([bytes(displayedBody.value)], { type: 'application/octet-stream' }))
  const link = document.createElement('a')
  link.href = href
  link.download = `${side.value}-body.bin`
  link.click()
  setTimeout(() => URL.revokeObjectURL(href), 1000)
}

onMounted(loadCatalog)
onScopeDispose(() => {
  controller?.abort()
  catalogController.abort()
})
</script>

<template>
  <div class="grid min-w-0 gap-4">
    <BasePageHeader title="自由探测" description="默认用 Responses WebSocket 预热收取 State，也可改发自定义 HTTP" />
    <BaseCard padding="compact">
      <form class="grid gap-4" @submit.prevent="send">
        <div class="grid gap-3 sm:grid-cols-[220px_1fr_auto] sm:items-end">
          <FormItem label="探测方式">
            <BaseSelect v-model="mode" :options="[{ label: 'WebSocket 预热', value: 'websocket_prewarm' }, { label: '自定义 HTTP', value: 'http' }]" aria-label="探测方式" :disabled="busy" />
          </FormItem>
          <FormItem v-if="mode === 'websocket_prewarm'" label="模型">
            <BaseInput v-model="model" aria-label="预热模型" placeholder="gpt-5.4" required :disabled="busy" />
          </FormItem>
          <FormItem v-else label="方法">
            <BaseInput v-model="method" aria-label="请求方法" required :disabled="busy" />
          </FormItem>
          <BaseButton type="submit" :loading="busy">
            {{ mode === 'websocket_prewarm' ? '开始预热' : '发送请求' }}
          </BaseButton>
        </div>
        <FormItem v-if="mode === 'http'" label="请求地址">
          <BaseInput v-model="url" aria-label="请求地址" placeholder="https://example.com/path" required :disabled="busy" />
        </FormItem>
        <p v-else class="m-0 text-cp-sm text-cp-text-secondary">
          在所选出口打开 Responses WebSocket 预热。若上游把模型降到更快模型并给出 312 长度的票，会丢掉这张票，再在同一条连接上用不续接的工具回合重取一次。只有请求模型自己的合格长度才会写入缓存。
        </p>
        <div class="grid gap-3 md:grid-cols-3">
          <FormItem label="账号">
            <BaseSelect v-model="accountId" :options="accounts" aria-label="探测账号" :disabled="busy || loadingCatalog" />
          </FormItem>
          <FormItem label="出口代理">
            <BaseSelect v-model="proxyId" :options="proxies" aria-label="出口代理" :disabled="busy || loadingCatalog" />
          </FormItem>
          <FormItem :label="mode === 'websocket_prewarm' ? '超时秒数 · 0 表示 120 秒' : '超时秒数 · 0 表示不限'">
            <BaseInput v-model="timeout" type="number" min="0" aria-label="超时秒数" :disabled="busy" />
          </FormItem>
        </div>
        <BaseInput v-if="proxyId === 'custom'" v-model="proxyUrl" aria-label="自定义代理地址" placeholder="http://user:password@host:port 或 socks5h://host:port" :disabled="busy" />
        <div v-if="accountId && mode === 'http'" class="grid gap-2">
          <BaseCheckbox v-model="useAccountHeaders" label="使用账号认证报头，同名自定义报头优先" show-label :disabled="busy" />
          <span v-if="useAccountHeaders" class="text-cp-xs text-cp-text-secondary">所选账号凭据将发送至上方请求地址，并在本次结果中明文显示</span>
        </div>
        <div v-if="catalogError" class="flex items-center gap-2 text-cp-sm text-cp-error" role="alert">
          {{ catalogError }}<BaseButton variant="ghost" :loading="loadingCatalog" @click="loadCatalog">
            重试加载
          </BaseButton>
        </div>
        <div v-if="mode === 'http'" class="flex items-center justify-between gap-2">
          <h2 class="m-0 text-cp-sm font-heavy">
            请求头
          </h2>
          <BaseButton variant="ghost" :disabled="busy" @click="headers.push({ id: nextHeaderId++, enabled: true, name: '', value: '', encoding: 'text' })">
            <Plus class="size-4" />添加报头
          </BaseButton>
        </div>
        <template v-if="mode === 'http'">
          <div v-for="(header, index) in headers" :key="header.id" class="grid grid-cols-[24px_1fr_40px] items-center gap-2 md:grid-cols-[24px_minmax(140px,1fr)_minmax(180px,2fr)_140px_40px]">
            <BaseCheckbox v-model="header.enabled" :label="`启用报头 ${index + 1}`" :disabled="busy" />
            <BaseInput v-model="header.name" :aria-label="`报头 ${index + 1} 名称`" placeholder="Header-Name" :disabled="busy" />
            <BaseInput v-model="header.value" class="col-start-2 md:col-auto" :aria-label="`报头 ${index + 1} 值`" placeholder="值，可留空或添加同名报头" :disabled="busy" />
            <BaseSelect v-model="header.encoding" class="col-start-2 md:col-auto" :options="encodings" :aria-label="`报头 ${index + 1} 编码`" :disabled="busy" />
            <BaseIconButton class="col-start-3 row-start-1 md:col-auto md:row-auto" :label="`删除报头 ${index + 1}`" :disabled="busy" @click="headers.splice(index, 1)">
              <Trash2 class="size-4" />
            </BaseIconButton>
          </div>
        </template>
        <FormItem :label="mode === 'websocket_prewarm' ? '预热正文 JSON，留空使用默认' : '请求体'">
          <template #extra>
            <BaseSelect id="probe-body-encoding" v-model="bodyEncoding" :options="encodings" aria-label="请求体编码" :disabled="busy" />
          </template>
          <BaseTextarea v-model="body" :rows="7" class="font-mono" placeholder="任意正文，二进制内容可使用 Base64" :disabled="busy" />
        </FormItem>
        <div v-if="busy" class="flex items-center gap-3 text-cp-sm" aria-live="polite">
          正在探测 · 已接收 {{ receivedBytes }} B<BaseButton variant="ghost" @click="controller?.abort()">
            停止探测
          </BaseButton>
        </div>
        <p v-if="error" class="m-0 text-cp-sm text-cp-error" role="alert">
          {{ error }}
        </p>
      </form>
    </BaseCard>
    <BaseCard v-if="result" padding="compact" title="交换结果">
      <template #actions>
        <BaseButton variant="ghost" :disabled="busy" @click="reuseHeaders">
          载入实际请求头继续编辑
        </BaseButton>
      </template>
      <div class="grid min-w-0 gap-4">
        <p class="m-0 break-all text-cp-sm">
          {{ result.method }} {{ result.url }}
        </p>
        <div class="flex flex-wrap items-center gap-3 text-cp-sm" aria-live="polite">
          <strong>{{ result.statusCode ?? '无响应' }}</strong><span>{{ result.httpVersion ?? '' }}</span><span>{{ result.elapsedMs }} ms</span>
          <span v-if="result.turnStateStored">已写入 State，长度 {{ result.turnStateLength }}</span>
          <span v-else-if="result.turnStateLength">观察到 State，长度 {{ result.turnStateLength }}，未写入缓存</span>
          <span v-if="result.error" class="text-cp-error">{{ result.error }}</span>
        </div>
        <div class="flex flex-wrap gap-2">
          <BaseSelect v-model="side" :options="[{ label: '响应', value: 'response' }, { label: '请求', value: 'request' }]" aria-label="交换方向" />
          <BaseSelect v-model="view" :options="views" aria-label="结果显示格式" />
          <BaseButton variant="ghost" @click="copyText(displayedHeaders.map(header => `${header.name}: ${display(header.valueBase64)}`).join('\n'), { successText: '已复制报头', emptyErrorText: '无报头' })">
            复制报头
          </BaseButton>
        </div>
        <div class="overflow-x-auto rounded-cp bg-cp-fill-quaternary p-3">
          <dl class="m-0 grid gap-2 font-mono text-cp-xs">
            <div v-for="(header, index) in displayedHeaders" :key="index" class="grid gap-1 sm:grid-cols-[200px_minmax(0,1fr)]">
              <dt class="break-all font-heavy">
                {{ header.name }}
              </dt><dd class="m-0 whitespace-pre-wrap break-all">
                {{ display(header.valueBase64) }}
              </dd>
            </div>
          </dl>
          <span v-if="!displayedHeaders.length" class="text-cp-sm text-cp-text-secondary">无报头</span>
        </div>
        <div class="flex flex-wrap items-center justify-between gap-2">
          <h2 class="m-0 text-cp-sm font-heavy">
            正文 · {{ side === 'response' ? receivedBytes : bytes(displayedBody).length }} B
          </h2>
          <div class="flex gap-2">
            <BaseButton variant="ghost" @click="copyText(display(displayedBody), { successText: '已复制正文', emptyErrorText: '空正文' })">
              复制预览
            </BaseButton><BaseButton variant="ghost" @click="downloadBody">
              下载原始字节
            </BaseButton>
          </div>
        </div>
        <p v-if="side === 'response'" class="m-0 text-cp-xs text-cp-text-secondary">
          预览前 64 KiB，下载可获取全部已接收字节 · 临时保留最近 32 次，结束后 30 分钟过期
        </p>
        <pre class="cp-scrollbar m-0 max-h-[600px] overflow-auto rounded-cp bg-cp-fill-quaternary p-3 font-mono text-cp-xs whitespace-pre-wrap break-all">{{ display(displayedBody) || '空正文' }}</pre>
      </div>
    </BaseCard>
  </div>
</template>
