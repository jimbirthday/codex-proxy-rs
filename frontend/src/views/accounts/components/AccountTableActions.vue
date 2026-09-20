<script setup lang="ts">
import type { AccountRow } from '../constants'
import { KeyRound, MoreHorizontal, Pencil, RefreshCw, RotateCcw, Trash2, Wifi } from '@lucide/vue'

import BaseButton from '@/components/base/BaseButton.vue'
import BaseIconButton from '@/components/base/BaseIconButton.vue'
import BaseMenuItem from '@/components/base/BaseMenuItem.vue'
import BasePopover from '@/components/base/BasePopover.vue'

defineProps<{
  account: AccountRow
  deleting: boolean
  recovering: boolean
  refreshing: boolean
  testing: boolean
  mobile?: boolean
}>()

const emit = defineEmits<{
  edit: [account: AccountRow]
  delete: [account: AccountRow]
  recover: [accountId: string]
  test: [account: AccountRow]
  refresh: [accountId: string]
  reauthorize: [account: AccountRow]
}>()
</script>

<template>
  <div class="relative flex items-center justify-start" :class="mobile ? 'gap-2 [&>button]:min-h-11' : 'gap-1'">
    <template v-if="mobile">
      <BaseButton class="flex-1 px-2!" @click="emit('edit', account)">
        <Pencil class="size-4" /> 编辑
      </BaseButton>
      <BaseButton class="flex-1 px-2!" :loading="testing" @click="emit('test', account)">
        测试连接
      </BaseButton>
    </template>
    <BaseIconButton
      v-else
      variant="ghost"
      size="sm"
      label="编辑账号"
      @click.stop="emit('edit', account)"
    >
      <Pencil class="size-3.5 text-cp-link" />
    </BaseIconButton>

    <BaseIconButton
      v-if="!mobile"
      variant="ghost"
      size="sm"
      label="删除账号"
      :disabled="deleting"
      @click.stop="emit('delete', account)"
    >
      <Trash2 class="size-3.5 text-cp-error" />
    </BaseIconButton>

    <BasePopover placement="bottom-end">
      <template #trigger="{ open }">
        <BaseButton v-if="mobile" class="min-h-11 px-3!" :aria-expanded="open" aria-label="更多账号操作">
          更多 <MoreHorizontal class="size-4" />
        </BaseButton>
        <BaseIconButton v-else variant="ghost" size="sm" label="更多操作" :pressed="open">
          <MoreHorizontal class="size-4" />
        </BaseIconButton>
      </template>

      <template #default="{ close }">
        <div class="w-44 p-1.5" :class="mobile ? '[&>button]:min-h-11' : undefined">
          <BaseMenuItem
            v-if="!mobile"
            :loading="testing"
            :disabled="testing"
            @click.stop="(close(), emit('test', account))"
          >
            <template #icon>
              <Wifi class="size-3.5 text-cp-text-quaternary" />
            </template>
            测试连接
          </BaseMenuItem>
          <BaseMenuItem
            v-if="account.authenticationKind === 'oauth'"
            :loading="refreshing"
            :disabled="refreshing"
            @click.stop="(close(), emit('refresh', account.id))"
          >
            <template #loading>
              <RefreshCw class="size-3.5 animate-spin text-cp-text-quaternary motion-reduce:animate-none" />
            </template>
            <template #icon>
              <RefreshCw class="size-3.5 text-cp-text-quaternary" />
            </template>
            刷新令牌
          </BaseMenuItem>
          <BaseMenuItem v-if="account.authenticationKind === 'oauth'" @click.stop="(close(), emit('reauthorize', account))">
            <template #icon>
              <KeyRound class="size-3.5 text-cp-text-quaternary" />
            </template>
            重新授权
          </BaseMenuItem>
          <BaseMenuItem
            :loading="recovering"
            :disabled="recovering"
            @click.stop="(close(), emit('recover', account.id))"
          >
            <template #icon>
              <RotateCcw class="size-3.5 text-cp-text-quaternary" />
            </template>
            恢复状态
          </BaseMenuItem>
          <BaseMenuItem v-if="mobile" :disabled="deleting" @click.stop="(close(), emit('delete', account))">
            <template #icon>
              <Trash2 class="size-4 text-cp-error-text" />
            </template>
            删除账号
          </BaseMenuItem>
        </div>
      </template>
    </BasePopover>
  </div>
</template>
