<script setup lang="ts">
import { computed, onMounted, shallowRef, useTemplateRef } from 'vue'
import { renderMarkdown } from './markdown'
import markdownStyles from './markdown.css?inline'

const props = defineProps<{
  source?: string | null
}>()

const host = useTemplateRef<HTMLElement>('host')
const contentRoot = shallowRef<HTMLElement | null>(null)
const html = computed(() => renderMarkdown(props.source))

onMounted(() => {
  if (!host.value)
    return

  const shadowRoot = host.value.attachShadow({ mode: 'open' })
  const style = document.createElement('style')
  const content = document.createElement('div')
  style.textContent = markdownStyles
  shadowRoot.append(style, content)
  contentRoot.value = content
})
</script>

<template>
  <div ref="host" class="min-w-0">
    <Teleport v-if="contentRoot" :to="contentRoot">
      <div class="markdown-body" v-html="html" />
    </Teleport>
  </div>
</template>
