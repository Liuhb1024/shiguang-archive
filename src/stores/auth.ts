import { invoke } from "@tauri-apps/api/core";
import { defineStore } from "pinia";
import { computed, ref } from "vue";

export interface LoginUser {
  uin: string;
  nickname: string;
}

interface LoginStatus {
  status: "waiting" | "scanned" | "expired" | "success" | "error" | "loggedOut";
  message: string;
  user?: LoginUser;
}

interface QrLoginStart {
  qrImage: string;
}

const delay = (milliseconds: number) => new Promise((resolve) => setTimeout(resolve, milliseconds));

export const useAuthStore = defineStore("auth", () => {
  const dialogVisible = ref(false);
  const loading = ref(false);
  const qrImage = ref("");
  const status = ref<LoginStatus["status"]>("loggedOut");
  const message = ref("使用手机 QQ 扫码登录");
  const user = ref<LoginUser>();
  let pollingRun = 0;
  const sessionVersion = ref(0);

  const loggedIn = computed(() => status.value === "success" && Boolean(user.value));

  async function restoreSession() {
    const run = pollingRun;
    try {
      const result = await invoke<LoginStatus>("get_login_status");
      if (run !== pollingRun) return;
      status.value = result.status;
      message.value = result.message;
      user.value = result.status === "success" ? result.user : undefined;
    } catch {
      if (run !== pollingRun) return;
      status.value = "loggedOut";
    }
  }

  async function openLogin() {
    if (loading.value) return;
    dialogVisible.value = true;
    if (!loggedIn.value) await refreshQrCode();
  }

  function closeLogin() {
    dialogVisible.value = false;
    pollingRun += 1;
    loading.value = false;
  }

  async function refreshQrCode() {
    if (loading.value) return;
    const run = ++pollingRun;
    sessionVersion.value += 1;
    user.value = undefined;
    loading.value = true;
    qrImage.value = "";
    status.value = "waiting";
    message.value = "正在获取登录二维码…";
    try {
      const result = await invoke<QrLoginStart>("start_qr_login");
      if (run !== pollingRun) return;
      qrImage.value = result.qrImage;
      message.value = "请使用手机 QQ 扫描二维码";
      loading.value = false;
      while (run === pollingRun && dialogVisible.value) {
        await delay(1800);
        if (run !== pollingRun || !dialogVisible.value) return;
        const result = await invoke<LoginStatus>("poll_qr_login");
        if (run !== pollingRun) return;
        status.value = result.status;
        message.value = result.message;
        if (result.status === "success") {
          if (!result.user) throw new Error("Rust 后端未返回登录用户信息");
          user.value = result.user;
          await delay(700);
          if (run !== pollingRun) return;
          dialogVisible.value = false;
          pollingRun += 1;
          return;
        }
        if (result.status === "expired" || result.status === "error") return;
      }
    } catch (error) {
      if (run !== pollingRun) return;
      status.value = "error";
      message.value = typeof error === "string"
        ? error
        : error instanceof Error
          ? error.message
          : "登录服务暂时不可用";
    } finally {
      if (run === pollingRun) loading.value = false;
    }
  }

  async function logout() {
    pollingRun += 1;
    loading.value = true;
    dialogVisible.value = false;
    user.value = undefined;
    status.value = "loggedOut";
    sessionVersion.value += 1;
    try {
      await invoke("logout_qzone");
    } finally {
      user.value = undefined;
      qrImage.value = "";
      status.value = "loggedOut";
      message.value = "尚未登录";
      loading.value = false;
    }
  }

  return { dialogVisible, loading, qrImage, status, message, user, loggedIn, sessionVersion, restoreSession, openLogin, closeLogin, refreshQrCode, logout };
});
