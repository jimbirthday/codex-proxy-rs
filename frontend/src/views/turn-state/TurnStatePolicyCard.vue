<script setup lang="ts">
import type { OutboundProxyRecord } from '@/api'
import type { TurnStateProbePolicy } from '@/api/modules/turn-state-policy'
import { computed, onMounted, onScopeDispose, ref, shallowRef } from 'vue'
import { getProxies } from '@/api'
import { getTurnStateProbePolicy, updateTurnStateProbePolicy } from '@/api/modules/turn-state-policy'
import BaseButton from '@/components/base/BaseButton.vue'
import BaseCard from '@/components/base/BaseCard.vue'
import BaseCheckbox from '@/components/base/BaseCheckbox.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseInput from '@/components/base/BaseInput.vue'
import BaseSelect from '@/components/base/BaseSelect.vue'
import BaseSwitch from '@/components/base/BaseSwitch.vue'
import { toast } from '@/components/base/BaseToast'
import { errorMessage } from '@/utils/async'

const emit = defineEmits<{ manualEnabled: [enabled: boolean] }>()

const policy = ref<TurnStateProbePolicy>()
const saved = shallowRef<TurnStateProbePolicy>()
const proxies = shallowRef<OutboundProxyRecord[]>([])
const loading = shallowRef(true)
const saving = shallowRef(false)
const error = shallowRef('')
const search = shallowRef('')
const selectedRandomScope = ref('all')
let disposed = false
let loadVersion = 0
const controller = new AbortController()
const modes = [
  { value: 'smart', label: '智能选择' },
  { value: 'fixed', label: '固定代理' },
  { value: 'pool', label: '指定代理池' },
  { value: 'random', label: '随机代理' },
]
const descriptions = {
  smart: '优先账号绑定代理和历史成功代理，失败后轮换其他代理',
  fixed: '只通过指定代理探测，失败后等待冷却',
  pool: '只在选中的代理池内轮换，成功后停止本轮',
  random: '从可用代理中随机选择，本轮不会重复尝试同一代理',
}
const showPool = computed(() => policy.value?.mode === 'pool' || (policy.value?.mode === 'random' && selectedRandomScope.value === 'selected'))
const proxyOptions = computed(() => proxies.value.map(proxy => ({ value: proxy.id, label: proxy.name, description: proxy.endpoint })))
const filteredProxies = computed(() => {
  const query = search.value.trim().toLowerCase()
  return proxies.value.filter(proxy => `${proxy.name} ${proxy.endpoint} ${proxy.lastTest?.exitIpv4 ?? ''} ${proxy.lastTest?.exitIpv6 ?? ''}`.toLowerCase().includes(query))
})
const fixedProxy = computed({
  get: () => policy.value?.proxyIds[0] ?? '',
  set: (value: string) => {
    if (policy.value)
      policy.value.proxyIds = value ? [value] : []
  },
})
const candidateLimit = computed({
  get: () => String(policy.value?.candidateLimit ?? 3),
  set: (value: string) => {
    if (policy.value)
      policy.value.candidateLimit = Number(value)
  },
})
const validation = computed(() => {
  const value = policy.value
  if (!value)
    return ''
  if ((value.mode === 'fixed' || showPool.value) && value.proxyIds.length === 0)
    return '请选择探测代理'
  if (value.proxyIds.length > 200)
    return '最多选择 200 个代理'
  if (value.proxyIds.some(id => !proxies.value.some(proxy => proxy.id === id)))
    return '部分已选代理不存在，请刷新并重新选择'
  return ''
})
const dirty = computed(() => JSON.stringify(policy.value) !== JSON.stringify(saved.value))
const summary = computed(() => {
  if (!saved.value)
    return loading.value ? '正在读取已保存策略' : '未读取到探测设置'
  const value = saved.value
  const name = modes.find(mode => mode.value === value.mode)?.label
  const scope = value.mode === 'fixed'
    ? proxies.value.find(proxy => proxy.id === value.proxyIds[0])?.name ?? '代理不存在'
    : value.proxyIds.length ? `${value.proxyIds.length} 个代理` : '全部已保存代理'
  return `${value.manualEnabled ? '手动探测已开启' : '手动探测已关闭'} · ${value.automaticEnabled ? '自动续采已开启' : '自动续采已关闭'} · ${name} · ${scope} · 每轮最多 ${value.candidateLimit} 次`
})

const proxyMode = computed({
  get: () => policy.value?.mode ?? 'smart',
  set: (mode: string) => {
    if (!policy.value)
      return
    policy.value.mode = mode as TurnStateProbePolicy['mode']
    if (mode === 'fixed') {
      policy.value.proxyIds = policy.value.proxyIds.slice(0, 1)
      policy.value.candidateLimit = 1
    }
    if (mode === 'smart' || (mode === 'random' && selectedRandomScope.value === 'all'))
      policy.value.proxyIds = []
  },
})
const randomScope = computed({
  get: () => selectedRandomScope.value,
  set: (scope: string) => {
    selectedRandomScope.value = scope
    if (scope === 'all' && policy.value?.mode === 'random')
      policy.value.proxyIds = []
  },
})
function toggleProxy(id: string, checked: boolean) {
  if (!policy.value)
    return
  policy.value.proxyIds = checked ? [...policy.value.proxyIds, id] : policy.value.proxyIds.filter(value => value !== id)
}
async function load() {
  const version = ++loadVersion
  loading.value = true
  emit('manualEnabled', false)
  error.value = ''
  try {
    const [value, items] = await Promise.all([
      getTurnStateProbePolicy(),
      (async () => {
        const items: OutboundProxyRecord[] = []
        let page = 1
        let totalPages = 1
        do {
          const result = await getProxies({ page, pageSize: 200 }, { signal: controller.signal, silent: true })
          items.push(...result.items)
          totalPages = result.page.totalPages
          page++
        } while (page <= totalPages)
        return items
      })(),
    ])
    if (disposed || version !== loadVersion)
      return
    proxies.value = items
    selectedRandomScope.value = value.proxyIds.length ? 'selected' : 'all'
    saved.value = structuredClone(value)
    policy.value = value
    emit('manualEnabled', value.manualEnabled)
  }
  catch (cause) {
    if (!disposed && version === loadVersion)
      error.value = errorMessage(cause, '加载探测设置失败')
  }
  finally {
    if (!disposed && version === loadVersion)
      loading.value = false
  }
}
async function save() {
  if (!policy.value || validation.value || saving.value)
    return
  saving.value = true
  error.value = ''
  try {
    const value = await updateTurnStateProbePolicy({ ...policy.value, proxyIds: [...policy.value.proxyIds] })
    if (disposed)
      return
    saved.value = structuredClone(value)
    policy.value = value
    emit('manualEnabled', value.manualEnabled)
    toast.success('探测策略已保存，新轮次将使用此策略')
  }
  catch (cause) {
    if (!disposed)
      error.value = errorMessage(cause, '保存探测策略失败')
  }
  finally {
    if (!disposed)
      saving.value = false
  }
}
onMounted(load)
onScopeDispose(() => {
  disposed = true
  controller.abort()
})
</script>

<template>
  <BaseCard class="mb-5" title="探测与续采设置" description="手动探测沿用已保存策略，修改后从新轮次生效">
    <p class="m-0 mb-4 break-words text-cp-sm text-cp-text-secondary" role="status">
      {{ summary }}
    </p>
    <p v-if="error" role="alert" class="mb-3 text-cp-sm text-cp-error-text">
      {{ error }}
    </p>
    <BaseButton v-if="!policy && !loading" @click="load">
      重新加载
    </BaseButton>
    <div v-if="policy" class="grid min-w-0 gap-4">
      <div class="flex flex-wrap gap-x-8 gap-y-3">
        <BaseSwitch v-model="policy.manualEnabled" label="手动探测" show-label :disabled="saving || loading" />
        <BaseSwitch v-model="policy.automaticEnabled" label="自动续采" show-label :disabled="saving || loading" />
      </div>
      <div class="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
        <BaseFormItem label="代理策略">
          <BaseSelect v-model="proxyMode" :options="modes" :disabled="saving || loading" />
        </BaseFormItem>
        <BaseFormItem v-if="policy.mode === 'fixed'" label="固定代理">
          <BaseSelect v-model="fixedProxy" :options="proxyOptions" :disabled="saving || loading" placeholder="选择一个代理" />
        </BaseFormItem>
        <BaseFormItem v-if="policy.mode === 'random'" label="随机范围">
          <BaseSelect v-model="randomScope" :options="[{ value: 'all', label: '全部已保存代理' }, { value: 'selected', label: '指定代理池' }]" :disabled="saving || loading" />
        </BaseFormItem>
        <BaseFormItem label="每轮最多尝试">
          <BaseSelect v-model="candidateLimit" :options="[1, 2, 3].map(value => ({ value: String(value), label: `${value} 次` }))" :disabled="policy.mode === 'fixed' || saving || loading" />
        </BaseFormItem>
      </div>
      <p class="m-0 text-cp-sm text-cp-text-secondary">
        {{ descriptions[policy.mode] }}
      </p>
      <div v-if="showPool" class="grid min-w-0 gap-3">
        <div class="flex flex-wrap items-center gap-3">
          <BaseInput v-model="search" class="min-w-0 flex-1" aria-label="搜索探测代理" placeholder="搜索名称、地址或出口 IP" />
          <span class="text-cp-sm text-cp-text-secondary">已选 {{ policy.proxyIds.length }} / {{ proxies.length }}</span>
        </div>
        <div class="grid max-h-72 gap-2 overflow-y-auto rounded-cp border border-cp-split p-3 sm:grid-cols-2">
          <BaseCheckbox v-for="proxy in filteredProxies" :key="proxy.id" class="min-h-12 min-w-0" :model-value="policy.proxyIds.includes(proxy.id)" :label="proxy.name" show-label :disabled="saving || loading" @update:model-value="toggleProxy(proxy.id, $event)">
            <template #label>
              <span class="block break-words leading-normal">{{ proxy.name }}</span>
              <span class="block break-all text-cp-xs leading-normal text-cp-text-secondary">{{ proxy.endpoint }}</span>
              <span class="block break-all text-cp-xs leading-normal text-cp-text-secondary">出口 IP：{{ proxy.lastTest?.exitIpv4 || proxy.lastTest?.exitIpv6 || proxy.lastTest?.exitIp || '未检测' }}</span>
            </template>
          </BaseCheckbox>
          <p v-if="filteredProxies.length === 0" class="m-0 text-cp-sm text-cp-text-secondary">
            没有匹配的代理
          </p>
        </div>
      </div>
      <p v-if="validation" role="alert" class="m-0 text-cp-sm text-cp-warning-text">
        {{ validation }}
      </p>
      <div class="flex flex-wrap items-center gap-3">
        <BaseButton variant="primary" :loading="saving" :disabled="loading || !!validation || !dirty" @click="save">
          保存探测设置
        </BaseButton>
        <BaseButton variant="ghost" :disabled="saving || loading" @click="load">
          重新加载
        </BaseButton>
        <span v-if="dirty" class="text-cp-sm text-cp-warning-text">有未保存的修改</span>
      </div>
    </div>
  </BaseCard>
</template>
