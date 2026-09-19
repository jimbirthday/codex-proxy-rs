<script setup lang="ts">
import BaseCard from '@/components/base/BaseCard.vue'
import BaseFormItem from '@/components/base/BaseForm/FormItem.vue'
import BaseForm from '@/components/base/BaseForm/index.vue'
import BaseInput from '@/components/base/BaseInput.vue'

const maxWaitingPerKey = defineModel<string>('maxWaitingPerKey', { required: true })
const maxWaitingPerAccount = defineModel<string>('maxWaitingPerAccount', { required: true })
const concurrencyWaitTimeoutSeconds = defineModel<string>('concurrencyWaitTimeoutSeconds', { required: true })
</script>

<template>
  <BaseCard title="并发队列">
    <BaseForm class="max-w-6xl sm:grid-cols-2">
      <BaseFormItem label="密钥队列容量" description="每个客户端密钥允许等待的最大请求数，0 表示不排队">
        <BaseInput v-model="maxWaitingPerKey" aria-label="密钥队列容量" type="number" min="0" max="1000" step="1" />
      </BaseFormItem>
      <BaseFormItem label="账号队列容量" description="每个上游账号允许等待的最大请求数，0 表示不排队">
        <BaseInput v-model="maxWaitingPerAccount" aria-label="账号队列容量" type="number" min="0" max="1000" step="1" />
      </BaseFormItem>
      <BaseFormItem label="排队超时（秒）" description="密钥队列与账号队列共用的等待时限，从首次入队起计时，1～120 秒">
        <BaseInput v-model="concurrencyWaitTimeoutSeconds" aria-label="排队超时（秒）" type="number" min="1" max="120" step="1" />
      </BaseFormItem>
    </BaseForm>
  </BaseCard>
</template>
