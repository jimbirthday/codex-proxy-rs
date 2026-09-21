import type { Account, TurnStateOverviewEntry, TurnStateSnapshot } from '@/api'
import { computed, onMounted, onScopeDispose, ref, shallowRef, watch } from 'vue'

import {
  getAccountModels,
  getAccounts,
  getAccountTurnState,
  getTurnStateOverview,
  probeAccountTurnState,
  refreshAccountModels,
} from '@/api'
import { toast } from '@/components/base/BaseToast'
import { errorMessage } from '@/utils/async'

interface SelectOption {
  label: string
  value: string
  description?: string
}

export function useTurnStateProbe() {
  const manualEnabled = shallowRef(false)
  const accounts = ref<Account[]>([])
  const models = ref<SelectOption[]>([])
  const selectedAccountId = shallowRef('')
  const selectedModelId = shallowRef('')
  const snapshot = shallowRef<TurnStateSnapshot | null>(null)
  const overview = shallowRef<TurnStateOverviewEntry[]>([])
  const loadingAccounts = shallowRef(false)
  const loadingOverview = shallowRef(false)
  const loadingModels = shallowRef(false)
  const refreshingModels = shallowRef(false)
  const loadingSnapshot = shallowRef(false)
  const probing = shallowRef(false)
  const error = shallowRef('')

  let accountsController: AbortController | undefined
  let overviewController: AbortController | undefined
  let detailController: AbortController | undefined
  let probeController: AbortController | undefined
  let selectionVersion = 0
  let requestedModel: { accountId: string, modelId: string } | undefined

  const accountOptions = computed<SelectOption[]>(() => accounts.value.map(account => ({
    label: account.email || account.name || account.label || account.id,
    value: account.id,
    description: account.planTypeDisplay,
  })))
  const selectedAccount = computed(() =>
    accounts.value.find(account => account.id === selectedAccountId.value) ?? null,
  )
  const canProbe = computed(() =>
    manualEnabled.value && Boolean(selectedAccountId.value && selectedModelId.value) && !probing.value,
  )

  function cancelDetails() {
    selectionVersion += 1
    detailController?.abort()
    probeController?.abort()
    detailController = undefined
    probeController = undefined
    loadingModels.value = false
    refreshingModels.value = false
    loadingSnapshot.value = false
    probing.value = false
  }

  async function loadAccounts() {
    accountsController?.abort()
    const controller = new AbortController()
    accountsController = controller
    loadingAccounts.value = true
    error.value = ''
    try {
      const loaded: Account[] = []
      let page = 1
      let totalPages = 1
      do {
        const result = await getAccounts(
          { page, pageSize: 200, provider: 'openai' },
          { signal: controller.signal, silent: true },
        )
        loaded.push(...result.items.filter(account => account.authenticationKind === 'oauth'))
        totalPages = result.page.totalPages
        page += 1
      } while (page <= totalPages)

      accounts.value = loaded
      if (!loaded.some(account => account.id === selectedAccountId.value))
        selectedAccountId.value = loaded[0]?.id ?? ''
    }
    catch (cause) {
      if (!controller.signal.aborted)
        error.value = errorMessage(cause, '加载 OAuth 账号失败')
    }
    finally {
      if (accountsController === controller) {
        accountsController = undefined
        loadingAccounts.value = false
      }
    }
  }

  async function loadOverview() {
    overviewController?.abort()
    const controller = new AbortController()
    overviewController = controller
    loadingOverview.value = true
    try {
      const result = await getTurnStateOverview({ signal: controller.signal, silent: true })
      if (!controller.signal.aborted)
        overview.value = result
    }
    catch (cause) {
      if (!controller.signal.aborted)
        error.value = errorMessage(cause, '加载状态总览失败')
    }
    finally {
      if (overviewController === controller) {
        overviewController = undefined
        loadingOverview.value = false
      }
    }
  }

  async function loadModels(refresh = false) {
    const accountId = selectedAccountId.value
    cancelDetails()
    if (!accountId) {
      selectedModelId.value = ''
      models.value = []
      snapshot.value = null
      return
    }
    const version = selectionVersion
    const controller = new AbortController()
    detailController = controller
    if (refresh)
      refreshingModels.value = true
    else
      loadingModels.value = true
    error.value = ''
    const previousModel = selectedModelId.value
    if (!refresh) {
      selectedModelId.value = ''
      models.value = []
      snapshot.value = null
    }
    try {
      const result = await (refresh ? refreshAccountModels : getAccountModels)(
        { accountId },
        { signal: controller.signal, silent: true },
      )
      if (version !== selectionVersion)
        return
      models.value = result.models.map(model => ({
        label: model.label || model.id,
        value: model.id,
      }))
      const historicalModel = requestedModel?.accountId === accountId ? requestedModel.modelId : undefined
      const targetModel = historicalModel ?? previousModel
      requestedModel = undefined
      // 仅显式点击的历史模型可补入目录，避免把上一账号的模型带入新账号。
      if (historicalModel && !models.value.some(model => model.value === historicalModel))
        models.value.push({ label: `${targetModel}（历史记录）`, value: targetModel })
      selectedModelId.value = models.value.some(model => model.value === targetModel)
        ? targetModel
        : models.value[0]?.value ?? ''
      if (refresh)
        toast.success(`已刷新 ${models.value.length} 个上游模型`)
      else if (models.value.length === 0)
        error.value = '该账号的模型目录为空'
    }
    catch (cause) {
      if (!controller.signal.aborted && version === selectionVersion)
        error.value = errorMessage(cause, '加载模型目录失败')
    }
    finally {
      if (version === selectionVersion && detailController === controller) {
        detailController = undefined
        loadingModels.value = false
        refreshingModels.value = false
      }
    }
  }

  async function loadSnapshot() {
    const accountId = selectedAccountId.value
    const modelId = selectedModelId.value
    if (!accountId || !modelId) {
      snapshot.value = null
      return
    }
    const version = selectionVersion
    detailController?.abort()
    const controller = new AbortController()
    detailController = controller
    loadingSnapshot.value = true
    error.value = ''
    try {
      const result = await getAccountTurnState(
        { accountId, modelId },
        { signal: controller.signal, silent: true },
      )
      if (version === selectionVersion)
        snapshot.value = result
    }
    catch (cause) {
      if (!controller.signal.aborted && version === selectionVersion)
        error.value = errorMessage(cause, '加载探测记录失败')
    }
    finally {
      if (version === selectionVersion && detailController === controller) {
        detailController = undefined
        loadingSnapshot.value = false
      }
    }
  }

  async function runProbe() {
    const accountId = selectedAccountId.value
    const modelId = selectedModelId.value
    if (!canProbe.value)
      return
    const version = selectionVersion
    probeController?.abort()
    const controller = new AbortController()
    probeController = controller
    probing.value = true
    error.value = ''
    try {
      const result = await probeAccountTurnState(
        { accountId, modelId },
        { signal: controller.signal, silent: true },
      )
      if (version !== selectionVersion)
        return
      snapshot.value = await getAccountTurnState(
        { accountId, modelId },
        { signal: controller.signal, silent: true },
      )
      await loadOverview()
      if (result.activeTargetId)
        toast.success('Turn State 已就绪，将从下一次匹配的业务请求开始应用')
      else
        toast.warning('代理探测已完成，但没有代理取得 Turn State')
    }
    catch (cause) {
      if (version === selectionVersion)
        error.value = errorMessage(cause, '状态探测失败')
    }
    finally {
      if (version === selectionVersion) {
        probeController = undefined
        probing.value = false
      }
    }
  }

  function selectModel(accountId: string, modelId: string) {
    if (accountId !== selectedAccountId.value) {
      requestedModel = { accountId, modelId }
      selectedAccountId.value = accountId
    }
    else if (loadingModels.value || refreshingModels.value) {
      requestedModel = { accountId, modelId }
    }
    else {
      if (!models.value.some(model => model.value === modelId))
        models.value.push({ label: `${modelId}（历史记录）`, value: modelId })
      selectedModelId.value = modelId
    }
  }

  watch(selectedAccountId, () => {
    void loadModels()
  })
  watch(selectedModelId, () => {
    // 模型目录切换时的清空不应取消正在加载的新账号目录。
    if (!selectedModelId.value) {
      snapshot.value = null
      return
    }
    cancelDetails()
    void loadSnapshot()
  })
  onMounted(() => {
    void loadAccounts()
    void loadOverview()
  })
  onScopeDispose(() => {
    accountsController?.abort()
    overviewController?.abort()
    cancelDetails()
  })

  return {
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
  }
}
