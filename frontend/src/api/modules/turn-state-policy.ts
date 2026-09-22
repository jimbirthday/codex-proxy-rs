import type { TurnStateProbe } from './accounts'
import request from '../request'

export interface TurnStateProbeSchedule {
  scanIntervalSeconds: number
  roundMinIntervalSeconds: number
  requestSpacingMilliseconds: number
  requestTimeoutSeconds: number
  activityWindowSeconds: number
  budgetWindowSeconds: number
  budgetLimit: number
  maxConcurrency: number
  retryInitialSeconds: number
  retryMaxSeconds: number
  proxyCooldownInitialSeconds: number
  proxyCooldownMaxSeconds: number
}

export interface TurnStateProbeRequestHeader {
  name: string
  value: string
}

export interface TurnStateProbeRequest {
  body: Record<string, unknown> | null
  compression: 'none' | 'zstd'
  compressionLevel: number
  instructions: string | null
  inputText: string
  reasoningEffort: string | null
  stream: boolean
  store: boolean
  parallelToolCalls: boolean | null
  include: string[]
  serviceTier: string | null
  extraBody: Record<string, unknown>
  extraHeaders: TurnStateProbeRequestHeader[]
  maxResponseBodyBytes: number
}

export interface TurnStatePolicy {
  captureBusinessResponses: boolean
  captureProbeResponses: boolean
  injectionEnabled: boolean
  responseHeaderNames: string[]
  responseJsonPointers: string[]
  acceptedLengths: number[]
  ttlSeconds: number
  renewBeforeSeconds: number
  invalidationStatuses: number[]
  requireServedModelMatch: boolean
}

export type ResponseHeaderCarrySource = 'business_response' | 'turn_state_probe'
export type ResponseHeaderValueSelection = 'first' | 'last' | 'all'
export type ResponseHeaderMergeMode = 'if_absent' | 'replace' | 'append'
export type ResponseHeaderCarryScope = 'account' | 'account_model' | 'account_model_proxy'
export type ResponseHeaderMissingBehavior = 'keep' | 'clear'
export type ResponseHeaderTransform = 'direct' | 'set_cookie_to_cookie'

export interface ResponseHeaderCarryRule {
  id: string
  name: string
  enabled: boolean
  captureEnabled: boolean
  injectionEnabled: boolean
  clearOnDisable: boolean
  sources: ResponseHeaderCarrySource[]
  sourceHeader: string
  targetHeader: string
  transform: ResponseHeaderTransform
  valueSelection: ResponseHeaderValueSelection
  mergeMode: ResponseHeaderMergeMode
  scope: ResponseHeaderCarryScope
  accountIds: string[]
  models: string[]
  ttlSeconds: number
  missingBehavior: ResponseHeaderMissingBehavior
  captureStatusMin: number
  captureStatusMax: number
  invalidationStatuses: number[]
  maxValueBytes: number
  maxValues: number
}

export interface TurnStateSuccessRules {
  statusMin: number
  statusMax: number
  requireCompleted: boolean
  requireModelMatch: boolean
  expectedModels: string[]
  jsonPredicates: Array<{ pointer: string, values: unknown[] }>
}
export interface TurnStateVerification {
  mode: 'acquire_only' | 'mint_and_validate'
  reuseCount: number
  mintToReuseDelayMilliseconds: number
  reuseSpacingMilliseconds: number
  roundTimeoutSeconds: number
  stopOnFirstFailure: boolean
  requiredCookieNames: string[]
  businessProxy: 'follow_verified' | 'match_account'
  mintSuccess: TurnStateSuccessRules
  reuseSuccess: TurnStateSuccessRules
}
export interface TurnStateProbePolicy {
  schemaVersion: number
  verification: TurnStateVerification
  reuseRequest: TurnStateProbeRequest
  manualEnabled: boolean
  automaticEnabled: boolean
  mode: 'smart' | 'fixed' | 'pool' | 'random'
  proxyIds: string[]
  candidateLimit: number
  schedule: TurnStateProbeSchedule
  request: TurnStateProbeRequest
  state: TurnStatePolicy
  responseHeaderCarry: { rules: ResponseHeaderCarryRule[] }
}

export interface ResponseHeaderCarryStatus {
  totalEntries: number
  rules: Array<{
    ruleId: string
    cachedEntries: number
    lastUpdatedAt: string | null
  }>
}

export interface TurnStateRuntimeClear {
  accountId?: string
  model?: string
  ruleId?: string
  responseHeaders: boolean
  turnState: boolean
}

export function getTurnStateProbePolicy() {
  return request<TurnStateProbePolicy>({
    url: '/api/admin/settings/turn-state-probe',
    method: 'GET',
    silent: true,
  })
}

export function updateTurnStateProbePolicy(data: TurnStateProbePolicy) {
  return request<TurnStateProbePolicy>({
    url: '/api/admin/settings/turn-state-probe',
    method: 'POST',
    data,
    silent: true,
  })
}

export function getTurnStateRuntimeStatus() {
  return request<ResponseHeaderCarryStatus>({
    url: '/api/admin/settings/turn-state-probe/runtime',
    method: 'GET',
    silent: true,
  })
}

export function clearTurnStateRuntime(data: TurnStateRuntimeClear) {
  return request<{ cleared: boolean }>({
    url: '/api/admin/settings/turn-state-probe/runtime',
    method: 'POST',
    data,
    silent: true,
  })
}

export function getTurnStateProbeDefaults() {
  return request<TurnStateProbePolicy>({ url: '/api/admin/settings/turn-state-probe/defaults', method: 'GET' })
}

export function previewTurnStatePolicy(policy: TurnStateProbePolicy, model: string) {
  return request<Record<string, unknown>>({ url: '/api/admin/settings/turn-state-probe/preview', method: 'POST', data: { policy, model } })
}
export function testTurnStatePolicy(policy: TurnStateProbePolicy, accountId: string, model: string) {
  return request<TurnStateProbe>({ url: '/api/admin/accounts/turn-state/test', method: 'POST', data: { policy, accountId, model }, timeout: 3600000 })
}
