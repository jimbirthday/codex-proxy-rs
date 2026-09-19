import type { MaybeRefOrGetter } from 'vue'
import { usePreferredReducedMotion } from '@vueuse/core'
import { nextTick, toValue, watch } from 'vue'

export function useTableColumnMotion(
  table: MaybeRefOrGetter<HTMLTableElement | null>,
  columnKeys: () => string[],
) {
  const preferredMotion = usePreferredReducedMotion()

  watch([columnKeys, preferredMotion], async ([keys, motion], [previousKeys], onCleanup) => {
    const root = toValue(table)
    if (!root || motion === 'reduce'
      || keys.length !== previousKeys.length
      || keys.every((key, index) => key === previousKeys[index])
      || keys.some(key => !previousKeys.includes(key))) {
      return
    }

    // 只处理已有列换位；在列宽更新前记录位置，固定列不参与移动。
    const elements = Array.from(root.querySelectorAll<HTMLElement>('[data-column-motion]'))
      .filter(element => element.closest('table') === root)
    const positions = new Map(elements.map(element => [element, element.getBoundingClientRect().left]))
    const animations: Animation[] = []
    let cancelled = false
    onCleanup(() => {
      cancelled = true
      animations.forEach(animation => animation.cancel())
    })

    await nextTick()
    if (cancelled || !root.isConnected)
      return

    // 先统一读取新位置，再播放位移，避免逐个单元格交替读写布局。
    const movements = Array.from(positions)
      .filter(([element]) => element.isConnected)
      .map(([element, left]) => ({ element, offset: left - element.getBoundingClientRect().left }))
    for (const { element, offset } of movements) {
      if (Math.abs(offset) < 0.5)
        continue
      animations.push(element.animate([
        { transform: `translateX(${offset}px)` },
        { transform: 'translateX(0)' },
      ], { duration: 200, easing: 'cubic-bezier(0.2, 0, 0, 1)' }))
    }
  })
}
