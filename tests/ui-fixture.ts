// Standalone test entry, never imported by src/main.ts or the production build.
// All Tauri calls terminate here: no Rust process, account, or network service.
let account = "A";
let loggedIn = true;
let imageRequests = 0;
const slow = new URLSearchParams(location.search).has("slow");
let releaseImage: () => void = () => {};
const heldImage = new Promise<void>(resolve => { releaseImage = resolve; });
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
const items = () => [0, 1, 2, 3].map(index => ({ key: `${account}-${index}`, dynamicId: index + 1, mediaType: "photo", pictureIndex: 0, url: `local-picture-${index + 1}-0`, publishedAt: 1400000000 + index * 31536000, authorName: `合成账号 ${account}`, content: `合成内容 ${account}：@{uin:10000,nick:测试好友,who:1} [em]e100[/em]` }));
Object.assign(window, {
  __TAURI_OS_PLUGIN_INTERNALS__: { platform: "macos" },
  __TAURI_INTERNALS__: {
    convertFileSrc: () => "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='400' height='300'%3E%3Crect width='400' height='300' fill='%23166d85'/%3E%3Ctext x='70' y='150' fill='white' font-size='24'%3ESYNTHETIC MEDIA%3C/text%3E%3C/svg%3E",
    invoke: async (command: string, args: Record<string, unknown> = {}) => {
      if (command === "get_login_status") return loggedIn ? { status: "success", message: "合成会话", user: { uin: account === "A" ? "10001" : "10002", nickname: `合成账号 ${account}` } } : { status: "loggedOut", message: "已退出" };
      if (command === "list_archived_media") return { items: items(), years: [2014, 2015, 2016, 2017], total: 4 };
      if (command === "open_archived_original") {
        document.querySelector("#fixture-requests")!.textContent = `原文核验请求 ${args.id}`;
        return;
      }
      if (command === "load_archived_image") {
        imageRequests += 1; document.querySelector("#fixture-requests")!.textContent = `图片请求 ${imageRequests}`;
        await pause(100);
        if (slow && args.id === 2) await heldImage;
        if (args.id === 2) throw "QQ 返回了图片不存在占位图";
        if (args.id === 3) throw "QQ 媒体请求超时";
        if (args.id === 4) throw "HTTP 403";
        return "/synthetic-only.png";
      }
      if (command === "get_archived_feed") { const item = items().find(item => item.dynamicId === args.id)!; return { ...item, isBlog: true, canOpenOriginal: true, content: slow ? "合成长文，不依赖图片。\n".repeat(400) + "正文末尾标记" : item.content + "...", id: item.dynamicId, pictureUrls: [item.url], videoUrls: [], likes: [], comments: [], likeCount: 0, commentCount: 0 }; }
      if (command === "logout_qzone") { await pause(350); loggedIn = false; return; }
      if (command === "plugin:app|version") return "test-only";
      if (command === "get_archive_overview") return { dynamics: 0, pictures: 0, comments: 0, likes: 0, databaseBytes: 0 };
      if (command === "get_archive_progress") return { status: "idle", pages: 0, fetched: 0, saved: 0, skipped: 0, message: "合成空任务" };
      throw new Error(`Mock does not implement: ${command}`);
    },
  },
});
// Test-owned localhost preference only; no real terms acceptance is performed.
localStorage.setItem("qzone-archive-disclaimer", "2026-07-18-v1");
const { useAuthStore } = await import("../src/stores/auth");
const { pinia } = await import("../src/stores");
await import("../src/main");
if (slow) {
  const releaseButton = document.createElement("button");
  releaseButton.textContent = "释放合成图片";
  releaseButton.addEventListener("click", releaseImage);
  document.querySelector("#fixture-controls")!.append(releaseButton);
}
document.querySelector("#fixture-switch")!.addEventListener("click", async () => {
  const auth = useAuthStore(pinia);
  await auth.logout();
  account = "B"; loggedIn = true;
  await auth.restoreSession();
});
