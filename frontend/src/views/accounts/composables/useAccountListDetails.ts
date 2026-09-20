import type { Ref } from 'vue'
import type { AccountRow } from '../constants'
import type { AccountSubscription } from '@/api'
import { onScopeDispose, reactive, watch } from 'vue'
import { getAccountPersonalInfo } from '@/api'
import { accountResetCreditsSummary, loadAccountResetCreditsSummary } from './useAccountResetCredits'

export interface AccountListDetails {
  subscription: AccountSubscription | null
  loading: boolean
  failed: boolean
  checkedAt: number
  revision: string
  queued: boolean
}

const freshnessMs = 5 * 60 * 1000

export function supportsAccountListDetails(account: AccountRow) {
  return account.provider === 'openai' && account.authenticationKind === 'oauth'
}

export function useAccountListDetails(accounts: Ref<AccountRow[]>) {
  const details = reactive(new Map<string, AccountListDetails>())
  const controller = new AbortController()
  const queued = new Map<string, { account: AccountRow, force: boolean }>()
  const activeIds = new Set<string>()
  let active = 0
  let disposed = false

  async function load(account: AccountRow) {
    const state = details.get(account.id)
    if (!state || state.loading || disposed)
      return
    state.loading = true
    state.failed = false
    try {
      const result = await getAccountPersonalInfo({ accountId: account.id }, {
        silent: true,
        signal: controller.signal,
      })
      if (!disposed)
        state.subscription = result.subscription
    }
    catch {
      if (!disposed)
        state.failed = true
    }
    finally {
      state.loading = false
      state.checkedAt = Date.now()
    }
  }

  function drain() {
    if (disposed)
      return
    while (active < 3 && queued.size > 0) {
      const task = [...queued.values()].find(task => !activeIds.has(task.account.id))
      if (!task)
        break
      const { account, force } = task
      queued.delete(account.id)
      active++
      activeIds.add(account.id)
      // 每个任务顺序读取两项；列表最多三个摘要请求同时进行，翻页后丢弃未开始的任务。
      void (async () => {
        const state = details.get(account.id)
        if (state)
          state.queued = false
        if (state && Date.now() - state.checkedAt >= freshnessMs)
          await load(account)
        if (!disposed && accounts.value.some(row => row.id === account.id)) {
          const credits = accountResetCreditsSummary(account.id)
          if (force || Date.now() - credits.checkedAt >= freshnessMs)
            await loadAccountResetCreditsSummary(account.id)
        }
      })().finally(() => {
        active--
        activeIds.delete(account.id)
        drain()
      })
    }
  }

  function refresh(account: AccountRow) {
    const state = details.get(account.id)
    if (!state || state.loading || state.queued || activeIds.has(account.id))
      return
    state.checkedAt = 0
    state.queued = true
    queued.set(account.id, { account, force: true })
    drain()
  }

  watch(accounts, (rows) => {
    queued.clear()
    for (const account of rows) {
      if (!supportsAccountListDetails(account))
        continue
      const previous = details.get(account.id)
      if (!previous || previous.revision !== account.updatedAt) {
        details.set(account.id, {
          subscription: null,
          loading: false,
          failed: false,
          checkedAt: 0,
          revision: account.updatedAt,
          queued: true,
        })
      }
      queued.set(account.id, { account, force: !!previous && previous.revision !== account.updatedAt })
    }
    drain()
  }, { immediate: true })

  onScopeDispose(() => {
    disposed = true
    queued.clear()
    controller.abort()
  })

  return { details, refresh }
}
