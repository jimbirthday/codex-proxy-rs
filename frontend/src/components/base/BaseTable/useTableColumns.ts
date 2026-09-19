import type { MaybeRefOrGetter } from 'vue'
import type { BaseTableColumn, TableRow } from './columns'
import { useStorage } from '@vueuse/core'
import { computed, toValue } from 'vue'
import { resolveColumns } from './columns'

export interface TableColumnOption {
  key: string
  label: string
  visible: boolean
  disabled: boolean
  reorderable: boolean
}

function readVisibility(value: string): Record<string, boolean> {
  try {
    const parsed: unknown = JSON.parse(value)
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed))
      return Object.fromEntries(Object.entries(parsed).filter(([, visible]) => typeof visible === 'boolean'))
  }
  catch {
    // 损坏的本地偏好回退到列定义，不影响表格展示。
  }
  return {}
}

function readOrder(value: string): string[] {
  try {
    const parsed: unknown = JSON.parse(value)
    if (Array.isArray(parsed))
      return [...new Set(parsed.filter((key): key is string => typeof key === 'string'))]
  }
  catch {
    // 顺序偏好损坏时回退到列定义，保留独立存储的显隐偏好。
  }
  return []
}

export function useTableColumns<Row extends TableRow>(
  source: MaybeRefOrGetter<BaseTableColumn<Row>[]>,
  tableId: string,
) {
  // 只保存用户覆盖；新增列继续使用自身默认值，不继承旧列表的隐藏状态。
  const overrides = useStorage<Record<string, boolean>>(
    `codex-proxy:table-columns:${tableId}`,
    {},
    undefined,
    {
      shallow: true,
      writeDefaults: false,
      serializer: { read: readVisibility, write: JSON.stringify },
    },
  )
  const order = useStorage<string[]>(
    `codex-proxy:table-column-order:${tableId}`,
    [],
    undefined,
    {
      shallow: true,
      writeDefaults: false,
      serializer: { read: readOrder, write: JSON.stringify },
    },
  )

  const orderedColumns = computed(() => {
    const columns = resolveColumns(toValue(source))
    const ranks = new Map(order.value.map((key, index) => [key, index]))
    const movable = columns.filter(column => column.label && !column.sticky)
      .sort((a, b) => (ranks.get(a.key) ?? Number.MAX_SAFE_INTEGER) - (ranks.get(b.key) ?? Number.MAX_SAFE_INTEGER))

    // 固定列保留原位；新增列沿用定义顺序，接在用户已排序的普通列之后。
    let index = 0
    return columns.map(column => column.label && !column.sticky ? movable[index++]! : column)
  })

  const columnStates = computed(() => {
    const states = orderedColumns.value.map((column) => {
      const override = overrides.value[column.key]
      return {
        column,
        visible: column.hideable === false || (typeof override === 'boolean' ? override : !column.defaultHidden),
      }
    })
    // 即使旧偏好隐藏了全部列，也保留一个可操作的表格。
    if (!states.some(state => state.visible) && states[0])
      states[0].visible = true
    return states
  })
  const visibleColumns = computed(() => columnStates.value.filter(state => state.visible).map(state => state.column))
  const columnOptions = computed<TableColumnOption[]>(() => columnStates.value
    .filter(({ column }) => column.label)
    .map(({ column, visible }) => ({
      key: column.key,
      label: column.label!,
      visible,
      disabled: column.hideable === false || (visible && visibleColumns.value.length === 1),
      reorderable: !column.sticky,
    })))

  function setColumnVisible(key: string, visible: boolean) {
    const option = columnOptions.value.find(option => option.key === key)
    const column = toValue(source).find(column => column.key === key)
    if (!column || !option || option.disabled)
      return

    const next = { ...overrides.value }
    if (visible === !column.defaultHidden)
      delete next[key]
    else next[key] = visible
    overrides.value = next
  }

  function setColumnOrder(keys: string[]) {
    const movable = new Set(columnOptions.value.filter(option => option.reorderable).map(option => option.key))
    order.value = [...new Set(keys.filter(key => movable.has(key)))]
  }

  function resetColumns() {
    overrides.value = {}
    order.value = []
  }

  return { visibleColumns, columnOptions, setColumnVisible, setColumnOrder, resetColumns }
}
