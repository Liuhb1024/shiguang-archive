<script setup lang="ts">
import { computed, ref, watch, onBeforeUnmount } from "vue";
import { contentEvidence } from "../utils/contentEvidence";
import { openArchivedOriginal } from "../utils/qzone";
const props = defineProps<{ content?: string; recovered?: boolean; isBlog?: boolean; canOpenOriginal?: boolean; recordId?: number }>();
const evidence = computed(() => contentEvidence(props.content));
const opening = ref(false);
const openError = ref("");
let revision = 0;
watch(() => props.recordId, () => { revision += 1; opening.value = false; openError.value = ""; });
onBeforeUnmount(() => { revision += 1; });
async function openOriginal() {
  if (opening.value || !props.canOpenOriginal || props.recordId === undefined) return;
  const current = revision;
  opening.value = true;
  openError.value = "";
  try { await openArchivedOriginal(props.recordId); }
  catch { if (current === revision) openError.value = "无法打开原文，请确认当前账号和记录仍然可用。"; }
  finally { if (current === revision) opening.value = false; }
}
</script>

<template>
  <aside v-if="content || isBlog" class="content-evidence" :class="{ 'possible-summary': evidence.possibleSummary }">
    <i class="pi pi-info-circle" aria-hidden="true" />
    <div class="content-source-copy">
    <strong v-if="isBlog">QQ 空间日志 · 当前展示交互记录中的文字</strong>
    <span v-if="recovered">已从同条动态的历史通知找到更长文本；不是新联网获取，也不保证是最新版本或完整全文。</span>
    <span v-else-if="evidence.possibleSummary">当前文字可能是接口摘要（{{ evidence.characters }} 字），末尾省略号不是页面折叠。尚未获取到可验证的完整正文。</span>
    <span v-else>已显示当前记录保存的 {{ evidence.characters }} 字；交互记录不等于已验证的完整全文。</span>
    <template v-if="isBlog">
      <button v-if="canOpenOriginal" type="button" class="content-source-open" :disabled="opening" @click="openOriginal">{{ opening ? '正在打开…' : '在 QQ 官方页面核验原文' }} <i class="pi pi-external-link" aria-hidden="true" /></button>
      <small>{{ canOpenOriginal ? '使用系统浏览器，可能需要重新登录；不会导出 APP 登录凭据。' : '未保存通过身份校验的官方原文链接。' }} 原文不存在时，无法仅从摘要还原。</small>
      <small v-if="openError" role="alert">{{ openError }}</small>
    </template>
    </div>
  </aside>
</template>
