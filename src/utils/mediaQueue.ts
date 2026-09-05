export function createMediaQueue(concurrency = 3, maximumWaitMs = 20_000) {
  const limit = Math.max(1, Math.min(4, Math.floor(concurrency) || 1));
  type Job = { key: string; start: () => void; reject: (reason: Error) => void; timer?: ReturnType<typeof setTimeout> };
  let running = 0;
  const pending: Job[] = [];
  const pump = () => {
    while (running < limit && pending.length) {
      running += 1;
      const job = pending.shift()!;
      clearTimeout(job.timer);
      job.start();
    }
  };
  return {
    run<T>(key: string, task: () => Promise<T>, priority = false): Promise<T> {
      if (pending.length >= 128) return Promise.reject(new Error("下载排队已满，请稍后重试"));
      return new Promise<T>((resolve, reject) => {
        const job: Job = { key, reject, start: () => {
          let work: Promise<T>;
          try { work = task(); } catch (error) { work = Promise.reject(error); }
          void work.then(resolve, reject).finally(() => { running -= 1; pump(); });
        } };
        job.timer = setTimeout(() => {
          const index = pending.indexOf(job);
          if (index >= 0) {
            pending.splice(index, 1);
            reject(new Error("媒体排队超时，请点击后手动重试"));
          }
        }, maximumWaitMs);
        if (priority) pending.unshift(job); else pending.push(job);
        pump();
      });
    },
    promote(key: string) {
      const index = pending.findIndex(job => job.key === key);
      if (index > 0) pending.unshift(pending.splice(index, 1)[0]!);
    },
    clearPending() {
      for (const job of pending.splice(0)) {
        clearTimeout(job.timer);
        job.reject(new Error("操作已取消：页面已切换"));
      }
    },
  };
}
