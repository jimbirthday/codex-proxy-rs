import type { RequestOptions } from '../request'
import { API_BASE_URL } from '../constants'
import { requestStream } from '../request'

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
  automaticRequestHeaders: string[]
  responseBodyBase64: string
  elapsedMs: number
  error: string | null
}

export type FreeProbeEvent
  = | { type: 'connecting' }
    | { type: 'prepared', id: string }
    | { type: 'headers', exchange: FreeProbeExchange }
    | { type: 'progress', receivedBytes: number, previewBase64: string }
    | { type: 'complete', elapsedMs: number, error: string | null }

export function probeBodyUrl(id: string) {
  return `${API_BASE_URL}/api/admin/accounts/free-probe/body?id=${encodeURIComponent(id)}`
}

export async function sendFreeProbe(data: FreeProbeRequest, onEvent: (event: FreeProbeEvent) => void, options: RequestOptions = {}) {
  const stream = await requestStream({
    url: '/api/admin/accounts/free-probe',
    method: 'POST',
    data,
    timeout: 0,
    ...options,
  })
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  let buffer = ''
  let complete = false
  try {
    for (;;) {
      const { done, value } = await reader.read()
      if (done)
        break
      buffer += decoder.decode(value, { stream: true })
      for (;;) {
        const end = buffer.indexOf('\n\n')
        if (end < 0)
          break
        const frame = buffer.slice(0, end)
        buffer = buffer.slice(end + 2)
        const name = frame.split('\n').find(line => line.startsWith('event:'))?.slice(6).trim()
        const payload = frame.split('\n').filter(line => line.startsWith('data:')).map(line => line.slice(5).trimStart()).join('\n')
        if (!name || !payload)
          continue
        const event = JSON.parse(payload)
        if (name === 'error')
          throw new Error(event.message)
        if (name === 'complete')
          complete = true
        onEvent(name === 'headers' ? { type: name, exchange: { ...event, responseBodyBase64: '' } } : { ...event, type: name })
      }
    }
    if (!complete)
      throw new Error('探测连接中断，已接收的正文可下载')
  }
  finally {
    await reader.cancel().catch(() => {})
    reader.releaseLock()
  }
}
