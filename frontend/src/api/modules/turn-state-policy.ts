import request from '../request'

export interface TurnStateProbePolicy {
  manualEnabled: boolean
  automaticEnabled: boolean
  mode: 'smart' | 'fixed' | 'pool' | 'random'
  proxyIds: string[]
  candidateLimit: number
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
