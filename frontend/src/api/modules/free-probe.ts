import type { RequestOptions } from '../request'
import request from '../request'

export interface ProbeHeader {
  name: string
  valueBase64: string
}

export interface FreeProbeRequest {
  accountId: string | null
  useAccountHeaders: boolean
  proxyId: string | null
  proxyUrl: string | null
  method: string
  url: string
  headers: ProbeHeader[]
  bodyBase64: string
  timeoutSeconds: number
}

export interface FreeProbeExchange {
  method: string
  url: string
  requestHeaders: ProbeHeader[]
  requestBodyBase64: string
  statusCode: number | null
  httpVersion: string | null
  responseHeaders: ProbeHeader[]
  responseBodyBase64: string
  elapsedMs: number
  error: string | null
}

export function sendFreeProbe(data: FreeProbeRequest, options: RequestOptions = {}) {
  return request<FreeProbeExchange>({
    url: '/api/admin/accounts/free-probe',
    method: 'POST',
    data,
    timeout: 0,
    ...options,
  })
}
