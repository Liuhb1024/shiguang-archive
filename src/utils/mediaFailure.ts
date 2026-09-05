export interface MediaFailure { title: string; hint: string; retryable: boolean }

export function mediaFailure(reason: unknown): MediaFailure {
  const text = String(reason);
  if (/429|频率|频繁|风控|验证码/.test(text)) return { title: "请求受限，已停止", hint: "请稍后再试，不要连续重试", retryable: false };
  if (/403|401|签名.*过期/.test(text)) return { title: "地址失效或无访问权限", hint: "确认原内容仍可访问；现有地址可能需要更新", retryable: false };
  if (/404|占位图|没有.*地址/.test(text)) return { title: "原地址未提供图片", hint: "可能已失效或被删除，当前不能确认可恢复", retryable: false };
  if (/取消|尚未登录|登录失效|会话/.test(text)) return { title: "操作已停止", hint: "请确认当前账号后再操作", retryable: false };
  if (/不允许|安全|域名|HTTPS|公网|跳转|DNS/.test(text)) return { title: "地址被安全策略拦截", hint: "未降低保护或发送登录凭据", retryable: false };
  if (/写入|保存|磁盘|目录|临时|同步/.test(text)) return { title: "本地保存失败", hint: "请检查可用空间和目录权限", retryable: true };
  if (/超时|连接|网络|响应读取/.test(text)) return { title: "网络连接失败", hint: "检查网络后可手动重试", retryable: true };
  if (/非图片|非视频|格式|无法显示/.test(text)) return { title: "媒体格式无法显示", hint: "响应可能不是原始媒体，或当前系统不支持该格式", retryable: false };
  return { title: "媒体加载失败", hint: "未能确认原因，请稍后手动重试", retryable: true };
}
