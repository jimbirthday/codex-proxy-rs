import {
  buildCodexConfigFiles,
  CODEX_DEFAULT_MODEL,
  CODEX_WEBSOCKET_ENABLED_BY_DEFAULT,
} from './codexConfig.ts'

export interface CodexCcSwitchImportInput {
  apiKey: string
  baseUrl: string
  providerName: string
  websocketEnabled?: boolean
}

function buildUsageScript(apiKey: string, baseUrl: string) {
  // CC Switch 的占位符直接替换 JS 源码，不能安全承载含引号的自定义 Key。
  // 按本次导入值生成字符串字面量，并转义左花括号，避免值中的占位符被二次替换。
  const url = JSON.stringify(`${baseUrl}/usage`).replaceAll('{', '\\u007b')
  const authorization = JSON.stringify(`Bearer ${apiKey}`).replaceAll('{', '\\u007b')
  // 不限额时省略 total / remaining，避免把无限额度显示成余额为零。
  return `({
  request: {
    url: ${url},
    method: "GET",
    headers: { Authorization: ${authorization} }
  },
  extractor: function(response) {
    return [["daily", "日额度"], ["weekly", "周额度"]].map(function(entry) {
      var budget = response[entry[0]];
      var result = {
        planName: entry[1],
        isValid: true,
        used: Number(budget.used),
        unit: response.unit
      };
      if (budget.total === null) {
        result.extra = "不限额";
      } else {
        result.total = Number(budget.total);
        result.remaining = Number(budget.remaining);
      }
      return result;
    });
  }
})`
}

export function buildCodexCcSwitchImportDeeplink(input: CodexCcSwitchImportInput): string {
  const configFiles = buildCodexConfigFiles({
    apiKey: input.apiKey,
    baseUrl: input.baseUrl,
    websocketEnabled: input.websocketEnabled ?? CODEX_WEBSOCKET_ENABLED_BY_DEFAULT,
  })
  // 与界面共用原生生图配置；保留 auth 载荷和独立字段，兼容旧版 CCSwitch。
  const config = encodeBase64(JSON.stringify({
    auth: configFiles.auth,
    config: configFiles.configToml,
  }))
  const entries: [string, string][] = [
    ['resource', 'provider'],
    ['app', 'codex'],
    ['model', CODEX_DEFAULT_MODEL],
    ['name', input.providerName],
    ['homepage', configFiles.baseUrl],
    ['endpoint', configFiles.baseUrl],
    ['apiKey', input.apiKey],
    ['configFormat', 'json'],
    ['config', config],
    ['usageEnabled', 'true'],
    ['usageScript', encodeBase64(buildUsageScript(input.apiKey, configFiles.baseUrl))],
    ['usageAutoInterval', '30'],
  ]

  return `ccswitch://v1/import?${new URLSearchParams(entries).toString()}`
}

function encodeBase64(value: string) {
  const bytes = new TextEncoder().encode(value)
  let binary = ''
  for (const byte of bytes)
    binary += String.fromCharCode(byte)
  return btoa(binary)
}
