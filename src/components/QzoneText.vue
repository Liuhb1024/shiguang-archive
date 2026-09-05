<script setup lang="ts">
import { parseQzoneText } from "../utils/qzoneText";

defineProps<{ value?: string }>();
</script>

<template>
  <template v-for="(part, index) in parseQzoneText(value)" :key="`${index}-${part.type}-${part.value}`">
    <span v-if="part.type === 'emoji'" class="qzone-emoji-missing" :title="`QQ 表情 ${part.value}`">[表情]</span>
    <span v-else :class="{ 'qzone-mention': part.type === 'mention' }" :title="part.uin ? `QQ ${part.uin}` : undefined">{{ part.value }}</span>
  </template>
</template>
