import { API_BASE_URL } from '@/api/constants'

export function resolveServiceRootUrl() {
  const normalizedApiBase = API_BASE_URL.trim().replace(/\/+$/, '')
  if (/^https?:\/\//i.test(normalizedApiBase))
    return normalizedApiBase
  if (typeof window === 'undefined')
    return normalizedApiBase

  const origin = window.location.origin.replace(/\/+$/, '')
  if (!normalizedApiBase)
    return origin
  return `${origin}${normalizedApiBase.startsWith('/') ? normalizedApiBase : `/${normalizedApiBase}`}`
}
