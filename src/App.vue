<script setup lang="ts">
import { ref, reactive, computed, watch, onMounted, onBeforeUnmount, nextTick } from "vue";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";
import { open, ask, message } from "@tauri-apps/plugin-dialog";
import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

const baseUrl = ref("https://app.emooai.com/open-api/v1");
const apiKey = ref("");
const emooUserId = ref("");

// 开机自启（状态由系统 launchd/注册表持久化，启动时读真实状态）
const autostartOn = ref(false);

// 鉴权门禁
const authed = ref(false);
const saving = ref(false);
const connStatus = ref("");
const connOk = ref<boolean | null>(null);

// ---------- 类型 ----------
interface FolderNode {
  item_type: string;
  id: number;
  name: string;
  parent_id: number | null;
  has_children: boolean | null;
}
interface Task {
  id: number;
  name: string;
  localPath: string;
  isDir: boolean;
  targetFolderId: number | null;
  scheduleEnabled: boolean;
  scheduleIntervalSecs: number;
  status: string;
  statusMessage: string;
  lastSyncedAt: number | null;
  createdAt: number;
  updatedAt: number;
  permission?: PermissionSetting | null;
}
interface LogEntry {
  taskId: number;
  ts: number;
  level: string;
  message: string;
  detail: string;
}
interface FileRecord {
  taskId: number;
  relativePath: string;
  docKey: string;
  contentHash: string;
  size: number;
  mtime: number;
  syncedAt: number;
  status: string;
}
interface PermissionAudience {
  type: "all" | "none" | "specified";
  user_open_ids?: string[];
  group_ids?: number[];
}
interface PermissionSetting {
  visible: PermissionAudience;
  deletable: PermissionAudience;
}
interface WsUser {
  open_id: string;
  user_id: number;
  ws_username: string;
  email?: string;
  ws_group_list?: { id: number; group_name: string }[];
}
interface TaskPatch {
  name?: string;
  scheduleEnabled?: boolean;
  scheduleIntervalSecs?: number;
}

// ---------- 文件夹树（新建任务表单内复用） ----------
const rootChildren = ref<FolderNode[]>([]);
const childMap = reactive<Record<number, FolderNode[]>>({});
const expanded = reactive<Record<number, boolean>>({});
const loadingFolder = ref<number | null>(null);
const selectedFolderId = ref<number | null>(null);
const nameOf = reactive<Record<number, string>>({});
const parentOf = reactive<Record<number, number | null>>({});

const maskedKey = computed(() => {
  const k = apiKey.value;
  if (!k) return "";
  if (k.length <= 9) return "••••";
  return k.slice(0, 5) + "••••" + k.slice(-4);
});

const breadcrumb = computed(() => {
  const path: string[] = [];
  let cur: number | null = selectedFolderId.value;
  while (cur != null) {
    path.unshift(nameOf[cur] ?? `#${cur}`);
    cur = parentOf[cur] ?? null;
  }
  return ["根目录", ...path];
});

type Row =
  | { kind: "folder"; node: FolderNode; depth: number }
  | { kind: "note"; text: string; depth: number };

const flatRows = computed<Row[]>(() => {
  const out: Row[] = [];
  const walk = (nodes: FolderNode[], depth: number) => {
    for (const n of nodes) {
      if (n.item_type !== "folder") continue;
      out.push({ kind: "folder", node: n, depth });
      if (expanded[n.id] && childMap[n.id]) {
        const hasSub = childMap[n.id].some((c) => c.item_type === "folder");
        if (hasSub) walk(childMap[n.id], depth + 1);
        else out.push({ kind: "note", text: "（无子文件夹）", depth: depth + 1 });
      }
    }
  };
  walk(rootChildren.value, 1);
  return out;
});

function record(nodes: FolderNode[]) {
  for (const n of nodes) {
    if (n.item_type === "folder") {
      nameOf[n.id] = n.name;
      parentOf[n.id] = n.parent_id ?? null;
    }
  }
}

function resetTree() {
  for (const k of Object.keys(childMap)) delete childMap[Number(k)];
  for (const k of Object.keys(expanded)) delete expanded[Number(k)];
  for (const k of Object.keys(nameOf)) delete nameOf[Number(k)];
  for (const k of Object.keys(parentOf)) delete parentOf[Number(k)];
}

async function loadRootFolders() {
  const items = await invoke<FolderNode[]>("test_connection", {
    baseUrl: baseUrl.value,
    apiKey: apiKey.value,
  });
  rootChildren.value = items || [];
  resetTree();
  record(rootChildren.value);
}

// 重新拉取根文件夹（反映 Emoo 端测试连接后的结构变更）。失败保留旧树。
const treeLoading = ref(false);
async function refreshFolders() {
  if (!baseUrl.value || !apiKey.value) return;
  treeLoading.value = true;
  try {
    await loadRootFolders();
  } catch (e) {
    connStatus.value = "刷新文件夹失败：" + errMsg(e);
    connOk.value = false;
  } finally {
    treeLoading.value = false;
  }
}

async function toggle(node: FolderNode) {
  if (expanded[node.id]) {
    expanded[node.id] = false;
    return;
  }
  if (!childMap[node.id]) {
    loadingFolder.value = node.id;
    try {
      const items = await invoke<FolderNode[]>("list_folder_items", {
        baseUrl: baseUrl.value,
        apiKey: apiKey.value,
        folderId: node.id,
      });
      childMap[node.id] = items || [];
      record(childMap[node.id]);
    } finally {
      loadingFolder.value = null;
    }
  }
  expanded[node.id] = true;
}

function selectFolder(id: number | null) {
  selectedFolderId.value = id;
}

// ---------- 任务列表 ----------
const tasks = ref<Task[]>([]);
const refreshing = ref(false);

async function refreshTasks() {
  refreshing.value = true;
  try {
    tasks.value = await invoke<Task[]>("list_tasks");
  } finally {
    refreshing.value = false;
  }
}

// 实时态：进度 / 日志（事件驱动）
const taskProgress = reactive<Record<number, { cur: number; total: number; msg: string } | null>>({});
const taskLogs = reactive<Record<number, LogEntry[]>>({});
const logOpen = reactive<Record<number, boolean>>({});
const filesOpen = reactive<Record<number, boolean>>({});
const taskFiles = reactive<Record<number, FileRecord[]>>({});
const globalLogs = ref<LogEntry[]>([]);
const drawerOpen = ref(false);

function pushLog(e: LogEntry) {
  const arr = taskLogs[e.taskId] ?? (taskLogs[e.taskId] = []);
  arr.unshift(e);
  if (arr.length > 100) arr.length = 100;
  globalLogs.value.unshift(e);
  if (globalLogs.value.length > 300) globalLogs.value.length = 300;
}

const unlistenFns: UnlistenFn[] = [];
async function subscribe() {
  unlistenFns.push(
    await listen<{
      taskId: number;
      status: string;
      statusMessage: string;
      lastSyncedAt: number | null;
    }>("task://status", (ev) => {
      const p = ev.payload;
      const t = tasks.value.find((x) => x.id === p.taskId);
      if (t) {
        t.status = p.status;
        t.statusMessage = p.statusMessage;
        t.lastSyncedAt = p.lastSyncedAt;
      }
      if (p.status !== "syncing") taskProgress[p.taskId] = null;
    }),
  );
  unlistenFns.push(
    await listen<{ taskId: number; current: number; total: number; message: string }>(
      "task://progress",
      (ev) => {
        const p = ev.payload;
        taskProgress[p.taskId] = { cur: p.current, total: p.total, msg: p.message };
      },
    ),
  );
  unlistenFns.push(
    await listen<LogEntry>("task://log", (ev) => pushLog(ev.payload)),
  );
  unlistenFns.push(
    await listen<{ taskId: number; reason: string }>("task://paused", (ev) => {
      const p = ev.payload;
      const t = tasks.value.find((x) => x.id === p.taskId);
      if (t) {
        t.status = "paused";
        t.statusMessage = p.reason;
      }
      taskProgress[p.taskId] = null;
    }),
  );
}

// ---------- 操作 ----------
function errMsg(e: unknown) {
  return e instanceof Error ? e.message : String(e);
}

async function syncOne(t: Task) {
  try {
    await invoke("sync_task_now", { id: t.id });
  } catch (e) {
    connStatus.value = "同步失败：" + errMsg(e);
    connOk.value = false;
  } finally {
    await refreshTasks();
    reloadFilesIfOpen(t);
  }
}

async function patchTask(t: Task, patch: TaskPatch) {
  try {
    const updated = await invoke<Task>("update_task", { id: t.id, patch });
    Object.assign(t, updated);
  } catch (e) {
    connStatus.value = "更新失败：" + errMsg(e);
    connOk.value = false;
  }
}

// 定时同步：先勾选「定时同步」并设定好间隔，再点「开启同步」启动。
// 与「立即同步」解耦——勾选只是展开配置，不触发同步；由调度器按间隔驱动。
const schedChecked = reactive<Record<number, boolean>>({});
const schedDraftInterval = reactive<Record<number, number>>({});

function schedShown(t: Task) {
  return t.scheduleEnabled || !!schedChecked[t.id];
}
function intervalDisplay(t: Task) {
  return t.scheduleEnabled
    ? t.scheduleIntervalSecs
    : schedDraftInterval[t.id] ?? t.scheduleIntervalSecs ?? 600;
}
function onSchedCheck(t: Task, e: Event) {
  const checked = (e.target as HTMLInputElement).checked;
  schedChecked[t.id] = checked;
  if (checked && schedDraftInterval[t.id] == null) {
    schedDraftInterval[t.id] = Math.max(60, t.scheduleIntervalSecs || 600);
  }
  // 已在运行时取消勾选 → 停止定时
  if (!checked && t.scheduleEnabled) {
    void patchTask(t, { scheduleEnabled: false });
  }
}
function onIntervalChange(t: Task, e: Event) {
  const v = Math.max(60, Number((e.target as HTMLInputElement).value) || 60);
  if (t.scheduleEnabled) {
    void patchTask(t, { scheduleIntervalSecs: v });
  } else {
    schedDraftInterval[t.id] = v;
  }
}
async function startSchedule(t: Task) {
  const interval = schedDraftInterval[t.id] ?? t.scheduleIntervalSecs ?? 600;
  await patchTask(t, { scheduleEnabled: true, scheduleIntervalSecs: interval });
  schedChecked[t.id] = false; // 已开启，回到「运行中」展示态
}
async function stopSchedule(t: Task) {
  await patchTask(t, { scheduleEnabled: false });
}

async function removeTask(t: Task) {
  // 注意：webview 里原生 confirm() 不弹窗，必须走 Tauri dialog 插件的系统弹窗。
  const ok = await ask(
    `确定删除任务「${t.name}」？\n已同步到 Emoo 的文档不会被删除，可在 Emoo 手动清理。`,
    { title: "删除任务", kind: "warning", okLabel: "删除", cancelLabel: "取消" },
  );
  if (!ok) return;
  try {
    await invoke("delete_task", { id: t.id });
    await refreshTasks();
  } catch (e) {
    await message("删除失败：" + errMsg(e), { title: "删除失败", kind: "error" });
  }
}

async function toggleLog(t: Task) {
  logOpen[t.id] = !logOpen[t.id];
  if (logOpen[t.id] && !taskLogs[t.id]) {
    try {
      const ls = await invoke<LogEntry[]>("list_log", { taskId: t.id, limit: 50 });
      taskLogs[t.id] = ls ?? [];
    } catch {
      taskLogs[t.id] = [];
    }
  }
}

async function toggleFiles(t: Task) {
  filesOpen[t.id] = !filesOpen[t.id];
  if (filesOpen[t.id] && !taskFiles[t.id]) {
    try {
      taskFiles[t.id] =
        (await invoke<FileRecord[]>("list_file_records", { taskId: t.id })) ?? [];
    } catch {
      taskFiles[t.id] = [];
    }
  }
}
function reloadFilesIfOpen(t: Task) {
  if (filesOpen[t.id]) {
    invoke<FileRecord[]>("list_file_records", { taskId: t.id })
      .then((rs) => (taskFiles[t.id] = rs ?? []))
      .catch(() => {});
  }
}
function fmtSize(n: number) {
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
  return (n / 1024 / 1024).toFixed(1) + " MB";
}
function fileStatusMeta(s: string) {
  if (s === "source_deleted") return { text: "本地已删除，远端文档保留", cls: "fs-del" };
  return { text: "已同步", cls: "fs-ok" };
}

// ---------- 新建任务表单 ----------
const showNew = ref(false);
const newName = ref("");
const newPath = ref("");
const newIsDir = ref(false);
const newSched = ref(false);
const newInterval = ref(600);
const folderCount = ref<number | null>(null);
const folderError = ref("");
const formError = ref("");
const creating = ref(false);

function openNew() {
  showNew.value = !showNew.value;
  if (showNew.value) {
    newName.value = "";
    newPath.value = "";
    newIsDir.value = false;
    newSched.value = false;
    newInterval.value = 600;
    folderCount.value = null;
    folderError.value = "";
    formError.value = "";
    selectedFolderId.value = null;
    // 每次打开新建表单重新拉取根，反映 Emoo 端测试连接后的结构变更
    void refreshFolders();
  }
}

async function checkFolderCap() {
  if (!newIsDir.value || !newPath.value) {
    folderCount.value = null;
    folderError.value = "";
    return;
  }
  try {
    const n = await invoke<number | null>("count_folder_files", { path: newPath.value });
    folderCount.value = n;
    folderError.value =
      n != null && n > 20
        ? `文件夹内可同步文件 ${n} 个，超过 20 个上限，请精简或改选`
        : "";
  } catch {
    folderCount.value = null;
    folderError.value = "";
  }
}

async function pickFile() {
  const sel = await open({ multiple: false });
  if (typeof sel === "string") {
    newPath.value = sel;
    newIsDir.value = false;
    folderError.value = "";
    folderCount.value = null;
  }
}

async function pickFolder() {
  const sel = await open({ directory: true, multiple: false });
  if (typeof sel === "string") {
    newPath.value = sel;
    newIsDir.value = true;
    await checkFolderCap();
  }
}

async function createTask() {
  formError.value = "";
  if (!newName.value.trim()) {
    formError.value = "请填写任务名称";
    return;
  }
  if (!newPath.value) {
    formError.value = "请选择本地文件或文件夹";
    return;
  }
  if (folderError.value) {
    formError.value = folderError.value;
    return;
  }
  creating.value = true;
  try {
    await invoke("create_task", {
      new: {
        name: newName.value.trim(),
        localPath: newPath.value,
        isDir: newIsDir.value,
        targetFolderId: selectedFolderId.value,
        scheduleEnabled: newSched.value,
        scheduleIntervalSecs: newSched.value ? Number(newInterval.value) || 600 : undefined,
      },
    });
    showNew.value = false;
    await refreshTasks();
  } catch (e) {
    formError.value = "创建失败：" + errMsg(e);
  } finally {
    creating.value = false;
  }
}

// ---------- 展示辅助 ----------
function statusMeta(s: string) {
  switch (s) {
    case "syncing":
      return { text: "同步中", cls: "st-sync" };
    case "paused":
      return { text: "已暂停", cls: "st-pause" };
    case "error":
      return { text: "错误", cls: "st-err" };
    default:
      return { text: "就绪", cls: "st-idle" };
  }
}
function fmtTime(ts: number | null) {
  if (!ts) return "从未";
  try {
    return new Date(ts * 1000).toLocaleString();
  } catch {
    return "—";
  }
}
function targetLabel(t: Task) {
  if (t.targetFolderId == null) return "根目录";
  return nameOf[t.targetFolderId] ?? `文件夹 #${t.targetFolderId}`;
}
function progressPct(p: { cur: number; total: number; msg: string } | null | undefined) {
  if (!p || !p.total) return 0;
  return Math.round((p.cur / p.total) * 100);
}

// ---------- 窗口自适应 ----------
// 只在「结构性变化」（挂载 / 鉴权切换 / 新建表单开合 / 任务增删）时拟合窗口；
// 日志/抽屉/文件列表的开合不触发拟合，超出部分由页面自身滚动，避免窗口反复抖动。
// 用户手动缩放过窗口后停止自动拟合，尊重其设置（含最大化/全屏）。
const userResized = ref(false);
let selfSizing = false;

function onWinResize() {
  // 我们的 setSize 也会触发 resize；用 selfSizing 标记区分，避免误判。
  if (!selfSizing) userResized.value = true;
}

async function fitWindow(center = true) {
  if (userResized.value) return; // 用户已手动调整窗口，不再自动拟合
  try {
    await nextTick();
    await new Promise((r) => requestAnimationFrame(() => r(null)));
    const el = document.querySelector(".wrap") as HTMLElement | null;
    if (!el) return;
    const win = getCurrentWindow();
    const factor = await win.scaleFactor();
    const outer = await win.outerSize();
    const chrome = Math.max(0, outer.height / factor - window.innerHeight);
    const w = authed.value ? 760 : 520;
    const wrapH = Math.ceil(el.offsetHeight);
    const maxH = (window.screen?.availHeight || 900) - 60;
    // 与 tauri.conf.json 的 minWidth/minHeight 对齐：自动拟合不低于此下限。
    const MIN_W = 560;
    const MIN_H = 480;
    const h = Math.min(Math.max(wrapH + chrome, MIN_H), maxH);
    const finalW = Math.max(w, MIN_W);
    selfSizing = true;
    await win.setSize(new LogicalSize(finalW, h));
    if (center) await win.center();
    // 等 webview 派发完本次 setSize 引起的 resize 事件再放开
    setTimeout(() => {
      selfSizing = false;
    }, 250);
  } catch {
    /* 非 Tauri 环境忽略 */
  }
}

let fitTimer: ReturnType<typeof setTimeout> | undefined;
function scheduleFit(center: boolean) {
  if (fitTimer) clearTimeout(fitTimer);
  fitTimer = setTimeout(() => {
    fitTimer = undefined;
    fitWindow(center);
  }, 40);
}

// 开机自启：读取系统真实状态；切换时调用插件写入 launchd( macOS ) / 注册表( Windows )。
async function loadAutostart() {
  try {
    autostartOn.value = await isEnabled();
  } catch {
    /* 非 Tauri 环境忽略 */
  }
}
async function toggleAutostart(e: Event) {
  const checked = (e.target as HTMLInputElement).checked;
  try {
    if (checked) await enable();
    else await disable();
    autostartOn.value = checked;
  } catch (err) {
    autostartOn.value = !checked; // 复选框回滚
    await message("设置开机自启失败：" + errMsg(err), { title: "失败", kind: "error" });
  }
}

onMounted(async () => {
  window.addEventListener("resize", onWinResize);
  void loadAutostart();
  try {
    const cfg = await invoke<{ base_url: string; api_key: string; emoo_user_id: string | null }>("load_config");
    if (cfg.base_url) baseUrl.value = cfg.base_url;
    if (cfg.emoo_user_id) emooUserId.value = cfg.emoo_user_id;
    if (cfg.api_key) {
      apiKey.value = cfg.api_key;
      try {
        await loadRootFolders();
        authed.value = true;
        await subscribe();
        await refreshTasks();
      } catch (e: unknown) {
        authed.value = false;
        connOk.value = false;
        connStatus.value =
          "上次保存的连接已失效，请重新填写并保存：" + errMsg(e);
      }
    }
  } catch (e) {
    console.warn("load_config 失败", e);
  }
  try {
    globalLogs.value = await invoke<LogEntry[]>("list_log", { limit: 100 });
  } catch {
    /* ignore */
  }
  await fitWindow(true);
});

onBeforeUnmount(() => {
  window.removeEventListener("resize", onWinResize);
  if (fitTimer) clearTimeout(fitTimer);
  for (const fn of unlistenFns) fn();
});

// 鉴权切换需要重新居中；新建表单开合 / 任务增删只调高度、保持窗口位置
watch(authed, () => scheduleFit(true));
watch(showNew, () => scheduleFit(false));
watch(() => tasks.value.length, () => scheduleFit(false));

async function saveAndTest() {
  if (!baseUrl.value || !apiKey.value) {
    connOk.value = false;
    connStatus.value = "请填写 API 地址和 API Key。";
    return;
  }
  saving.value = true;
  connOk.value = null;
  connStatus.value = "保存并测试中…";
  try {
    await invoke("save_config", { baseUrl: baseUrl.value, apiKey: apiKey.value, emooUserId: emooUserId.value });
    await loadRootFolders();
    authed.value = true;
    connOk.value = true;
    connStatus.value = "连接成功，可开始同步。";
    await subscribe();
    await refreshTasks();
  } catch (e: unknown) {
    connOk.value = false;
    connStatus.value = "失败：" + errMsg(e);
  } finally {
    saving.value = false;
  }
}

function logout() {
  authed.value = false;
  userResized.value = false; // 退出后允许下次进入时重新自适应
  connStatus.value = "";
  connOk.value = null;
}

// ---------- 通讯录 / 文档权限 ----------
const wsUsers = ref<WsUser[]>([]);
const wsLoading = ref(false);
const wsKeyword = ref("");
const pickerOpen = ref(false);
const pickerMulti = ref(true);
const pickerSelected = ref<Set<string>>(new Set());
let pickerResolve: ((val: string[] | null) => void) | null = null;
const userName = computed(() => {
  const m: Record<string, string> = {};
  for (const u of wsUsers.value) m[u.open_id] = u.ws_username;
  return m;
});

async function loadWsUsers(kw: string) {
  wsLoading.value = true;
  try {
    wsUsers.value = await invoke<WsUser[]>("list_ws_users", {
      keyword: kw || null,
      pageSize: 100,
      currentPage: 1,
    });
  } catch (e) {
    wsUsers.value = [];
    connStatus.value = "通讯录加载失败：" + errMsg(e);
    connOk.value = false;
  } finally {
    wsLoading.value = false;
  }
}

function openPicker(multi: boolean, initial: string[]): Promise<string[] | null> {
  pickerMulti.value = multi;
  pickerSelected.value = new Set(initial);
  pickerOpen.value = true;
  void loadWsUsers(wsKeyword.value);
  return new Promise((resolve) => {
    pickerResolve = resolve;
  });
}
function togglePick(openId: string) {
  const s = new Set(pickerSelected.value);
  if (s.has(openId)) s.delete(openId);
  else {
    if (!pickerMulti.value) s.clear();
    s.add(openId);
  }
  pickerSelected.value = s;
}
function confirmPicker() {
  const arr = [...pickerSelected.value];
  pickerOpen.value = false;
  pickerResolve?.(arr);
  pickerResolve = null;
}
function cancelPicker() {
  pickerOpen.value = false;
  pickerResolve?.(null);
  pickerResolve = null;
}
async function pickCaller() {
  const sel = await openPicker(false, emooUserId.value ? [emooUserId.value] : []);
  if (sel && sel.length) emooUserId.value = sel[0];
}

// 任务权限编辑
const permOpen = ref(false);
const permTask = ref<Task | null>(null);
const permEnabled = ref(false);
const permVisible = ref<PermissionAudience>({ type: "all" });
const permDeletable = ref<PermissionAudience>({ type: "none" });
const permSaving = ref(false);

function openPermission(t: Task) {
  permTask.value = t;
  if (t.permission) {
    permEnabled.value = true;
    permVisible.value = JSON.parse(JSON.stringify(t.permission.visible));
    permDeletable.value = JSON.parse(JSON.stringify(t.permission.deletable));
  } else {
    permEnabled.value = false;
    permVisible.value = { type: "all" };
    permDeletable.value = { type: "none" };
  }
  void loadWsUsers(""); // 预载通讯录便于解析姓名
  permOpen.value = true;
}
async function pickAudience(target: "visible" | "deletable") {
  const aud = target === "visible" ? permVisible : permDeletable;
  const sel = await openPicker(true, aud.value.user_open_ids ?? []);
  if (sel) aud.value = { ...aud.value, user_open_ids: sel };
}
async function savePermission() {
  const t = permTask.value;
  if (!t) return;
  permSaving.value = true;
  try {
    const perm: PermissionSetting | null = permEnabled.value
      ? { visible: permVisible.value, deletable: permDeletable.value }
      : null;
    const updated = await invoke<Task>("set_task_permission", { id: t.id, permission: perm });
    Object.assign(t, updated);
    permOpen.value = false;
  } catch (e) {
    await message("权限保存失败：" + errMsg(e), { title: "失败", kind: "error" });
  } finally {
    permSaving.value = false;
  }
}
function audienceLabel(a: PermissionAudience | undefined | null): string {
  if (!a) return "未设置";
  if (a.type === "all") return "所有人";
  if (a.type === "none") return "仅创建者/超管";
  return `指定 ${a.user_open_ids?.length ?? 0} 人`;
}
</script>

<template>
  <main class="wrap">
    <header class="hd">
      <img src="/logo.png" class="brand" alt="" />
      <h1>Emoo 数据同步</h1>
      <span class="spacer"></span>
      <label v-if="authed" class="sw top-sw" title="开机/登录后自动启动">
        <input type="checkbox" :checked="autostartOn" @change="toggleAutostart" />
        <span>开机自启</span>
      </label>
      <button v-if="authed" class="link" @click="logout">退出</button>
    </header>

    <!-- 鉴权门禁 -->
    <section v-if="!authed" class="card">
      <h2>鉴权配置</h2>
      <p class="muted">首次使用请填写并保存，连接成功后进入同步。</p>
      <label>API 地址
        <input v-model="baseUrl" placeholder="https://app.emooai.com/open-api/v1" />
      </label>
      <label>API Key（emoo_ 开头，绑用户）
        <input v-model="apiKey" type="password" placeholder="emoo_xxxxxxxx" />
      </label>
      <label>调用者 open_id（设置文档权限用，可留空）
        <div class="row-input">
          <input v-model="emooUserId" placeholder="ou_xxxxxxxx（你的 open_id）" />
          <button class="sm" type="button" @click="pickCaller">从通讯录选择</button>
        </div>
        <span class="muted small">仅在「设置文档权限」时作为 Emoo-User-Id 请求头；该接口需工作区超管权限。</span>
      </label>
      <div class="actions">
        <button class="primary" :disabled="saving" @click="saveAndTest">
          <span v-if="saving" class="spinner"></span>
          {{ saving ? "保存并测试中…" : "保存并测试连接" }}
        </button>
      </div>
      <p v-if="connStatus" :class="['msg', connOk ? 'ok' : connOk === false ? 'err' : '']">{{ connStatus }}</p>
    </section>

    <!-- 同步页 -->
    <template v-else>
      <div class="bar">
        <span class="live"></span>已连接 {{ baseUrl
        }}<span class="muted"> · {{ maskedKey }}</span>
      </div>

      <div class="toolbar">
        <button class="primary sm" @click="openNew">{{ showNew ? "收起新建" : "+ 新建任务" }}</button>
        <button class="sm" :disabled="refreshing" @click="refreshTasks">
          {{ refreshing ? "刷新中…" : "刷新" }}
        </button>
        <span class="spacer"></span>
        <span class="muted count">共 {{ tasks.length }} 个任务</span>
      </div>

      <!-- 新建任务表单 -->
      <section v-if="showNew" class="card">
        <h2>新建任务</h2>
        <label>任务名称
          <input v-model="newName" placeholder="例如：周报同步" />
        </label>

        <div class="field">
          <span class="lab">目标知识库文件夹</span>
          <div class="crumb">
            <span>当前：{{ breadcrumb.join(" / ") }}</span>
            <span class="spacer"></span>
            <button class="sm link" :disabled="treeLoading" @click="refreshFolders">
              <span v-if="treeLoading" class="spinner sm"></span>
              {{ treeLoading ? "刷新中…" : "刷新文件夹" }}
            </button>
          </div>
          <div class="tree">
            <div class="row" :class="{ sel: selectedFolderId === null }" @click="selectFolder(null)">
              <span class="tog"></span>
              <svg class="ic-folder" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" /></svg>
              <span class="nm">根目录</span>
              <svg v-if="selectedFolderId === null" class="ic-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5" /></svg>
            </div>
            <template v-for="(r, i) in flatRows" :key="r.kind === 'folder' ? 'f' + r.node.id : 'n' + i">
              <div
                v-if="r.kind === 'folder'"
                class="row"
                :class="{ sel: selectedFolderId === r.node.id }"
                :style="{ paddingLeft: 10 + r.depth * 16 + 'px' }"
                @click="selectFolder(r.node.id)"
              >
                <span class="tog" @click.stop="toggle(r.node)">
                  <span v-if="loadingFolder === r.node.id" class="spinner sm"></span>
                  <svg v-else-if="r.node.has_children" class="chev" :class="{ open: expanded[r.node.id] }" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6" /></svg>
                </span>
                <svg class="ic-folder" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" /></svg>
                <span class="nm">{{ r.node.name }}</span>
                <svg v-if="selectedFolderId === r.node.id" class="ic-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5" /></svg>
              </div>
              <div v-else class="row note" :style="{ paddingLeft: 10 + r.depth * 16 + 'px' }">
                <span class="tog"></span><span class="nm muted">{{ r.text }}</span>
              </div>
            </template>
          </div>
        </div>

        <div class="field">
          <span class="lab">本地路径</span>
          <div class="row-input">
            <button class="sm" @click="pickFile">选文件</button>
            <button class="sm" @click="pickFolder">选文件夹</button>
            <span class="path" :class="{ empty: !newPath }">{{ newPath || "未选择" }}</span>
            <span v-if="newIsDir && folderCount != null" class="cap">可同步 {{ folderCount }} 个文件</span>
          </div>
        </div>

        <div class="switches">
          <label class="sw">
            <input type="checkbox" v-model="newSched" />
            <span>启用定时同步</span>
            <input v-if="newSched" class="intv" type="number" min="60" step="60" v-model.number="newInterval" />
            <span v-if="newSched" class="muted">秒（最少 60）</span>
          </label>
        </div>

        <p v-if="folderError" class="msg err">{{ folderError }}</p>
        <p v-if="formError" class="msg err">{{ formError }}</p>

        <div class="actions">
          <button class="primary" :disabled="creating || !!folderError" @click="createTask">
            <span v-if="creating" class="spinner"></span>
            {{ creating ? "创建中…" : "创建任务" }}
          </button>
          <button class="sm" @click="showNew = false">取消</button>
        </div>
      </section>

      <!-- 任务列表 -->
      <div class="tlist">
        <p v-if="!tasks.length" class="empty muted">
          还没有任务。点「+ 新建任务」开始同步本地文件或文件夹到 Emoo。
        </p>
        <section v-for="t in tasks" :key="t.id" class="tcard">
          <div class="tcard-hd">
            <span class="tname">{{ t.name }}</span>
            <span class="badge type">{{ t.isDir ? "文件夹" : "文件" }}</span>
            <span v-if="t.status !== 'idle'" class="badge" :class="statusMeta(t.status).cls">
              <span v-if="t.status === 'syncing'" class="spinner xs"></span>
              {{ statusMeta(t.status).text }}
            </span>
            <span class="spacer"></span>
            <span class="muted ls">上次同步：{{ fmtTime(t.lastSyncedAt) }}</span>
          </div>

          <div class="paths">
            <span class="muted">本地：</span><span class="mono">{{ t.localPath }}</span>
            <span class="muted arr">→</span>
            <span class="muted">Emoo：</span><span class="mono">{{ targetLabel(t) }}</span>
          </div>

          <p v-if="t.statusMessage" class="msg" :class="t.status === 'error' ? 'err' : t.status === 'paused' ? 'warn' : 'muted'">
            {{ t.statusMessage }}
          </p>

          <div v-if="taskProgress[t.id]" class="prog">
            <div class="prog-bar"><div class="prog-fill" :style="{ width: progressPct(taskProgress[t.id]) + '%' }"></div></div>
            <span class="muted prog-txt">{{ taskProgress[t.id]?.cur }}/{{ taskProgress[t.id]?.total }} · {{ taskProgress[t.id]?.msg }}</span>
          </div>

          <div class="controls">
            <button class="sm" :disabled="t.status === 'syncing'" @click="syncOne(t)">
              <span v-if="t.status === 'syncing'" class="spinner sm"></span>
              立即同步
            </button>

            <label class="sw">
              <input
                type="checkbox"
                :checked="t.scheduleEnabled || !!schedChecked[t.id]"
                @change="onSchedCheck(t, $event)"
              />
              <span>定时同步</span>
            </label>
            <span v-if="schedShown(t)" class="sched-on">
              <input
                class="intv"
                type="number"
                min="60"
                step="60"
                :value="intervalDisplay(t)"
                @change="onIntervalChange(t, $event)"
              />
              <span class="muted small">秒</span>
              <button v-if="!t.scheduleEnabled" class="sm" @click="startSchedule(t)">开启同步</button>
              <button v-else class="sm" @click="stopSchedule(t)">停止同步</button>
            </span>

            <button class="sm link" @click="toggleLog(t)">{{ logOpen[t.id] ? "收起日志" : "日志" }}</button>
            <button class="sm link" @click="toggleFiles(t)">
              {{ filesOpen[t.id] ? "收起文件" : "文件" }}
              <span v-if="(taskFiles[t.id]?.length ?? 0)" class="muted small">({{ taskFiles[t.id].length }})</span>
            </button>
            <button class="sm link" @click="openPermission(t)">
              权限
              <span v-if="t.permission" class="muted small">·可见:{{ audienceLabel(t.permission.visible) }}</span>
              <span v-else class="muted small">·未设置</span>
            </button>
            <span class="spacer"></span>
            <button class="sm danger" @click="removeTask(t)">删除</button>
          </div>

          <div v-if="logOpen[t.id]" class="logbox">
            <p v-if="!taskLogs[t.id] || !taskLogs[t.id].length" class="muted small">暂无日志</p>
            <div v-for="(l, i) in taskLogs[t.id]" :key="i" class="logline" :class="'lv-' + l.level">
              <span class="lt">{{ fmtTime(l.ts) }}</span>
              <span class="lm">{{ l.message }}</span>
              <span v-if="l.detail" class="ld muted">{{ l.detail }}</span>
            </div>
          </div>

          <div v-if="filesOpen[t.id]" class="logbox">
            <p v-if="!taskFiles[t.id] || !taskFiles[t.id].length" class="muted small">暂无已同步文件</p>
            <div v-for="(f, i) in taskFiles[t.id]" :key="i" class="fline">
              <span class="fstatus" :class="fileStatusMeta(f.status).cls" :title="fileStatusMeta(f.status).text">●</span>
              <span class="fname">{{ f.relativePath }}</span>
              <span class="muted small fsz">{{ fmtSize(f.size) }}</span>
              <span class="muted small fkey" :title="f.docKey">{{ f.docKey.slice(0, 14) }}…</span>
            </div>
          </div>
        </section>
      </div>

      <!-- 全局日志抽屉 -->
      <section class="drawer">
        <div class="drawer-hd" @click="drawerOpen = !drawerOpen">
          <span>最近日志（{{ globalLogs.length }}）</span>
          <span class="muted">{{ drawerOpen ? "收起 ▲" : "展开 ▼" }}</span>
        </div>
        <div v-if="drawerOpen" class="logbox global">
          <p v-if="!globalLogs.length" class="muted small">暂无日志</p>
          <div v-for="(l, i) in globalLogs" :key="i" class="logline" :class="'lv-' + l.level">
            <span class="lt">{{ fmtTime(l.ts) }}</span>
            <span class="lwho muted">#{{ l.taskId }}</span>
            <span class="lm">{{ l.message }}</span>
            <span v-if="l.detail" class="ld muted">{{ l.detail }}</span>
          </div>
        </div>
      </section>
    </template>

    <!-- 权限编辑弹窗 -->
    <div v-if="permOpen" class="overlay" @click.self="permOpen = false">
      <div class="modal">
        <div class="modal-hd">
          <span>文档权限{{ permTask ? " — " + permTask.name : "" }}</span>
          <button class="link sm" @click="permOpen = false">✕</button>
        </div>
        <div class="modal-bd">
          <div class="perm-warn">⚠️ 最终以本工具设置为准：在 Emoo 网页上对文档权限的修改，会在下次同步时被覆盖。</div>
          <label class="sw">
            <input type="checkbox" v-model="permEnabled" />
            <span>启用权限控制</span>
            <span class="muted small">（关闭 = 不做任何权限设置，保留 Emoo 默认）</span>
          </label>
          <div v-if="permEnabled" class="perm-fields">
            <div class="perm-row">
              <span class="perm-lab">可见</span>
              <div class="perm-opt">
                <label class="sw"><input type="radio" value="all" v-model="permVisible.type" /> 所有人</label>
                <label class="sw"><input type="radio" value="none" v-model="permVisible.type" /> 仅创建者/超管</label>
                <label class="sw"><input type="radio" value="specified" v-model="permVisible.type" /> 指定成员</label>
              </div>
              <div v-if="permVisible.type === 'specified'" class="perm-aud">
                <button class="sm" @click="pickAudience('visible')">选择成员</button>
                <span class="muted small">已选 {{ permVisible.user_open_ids?.length ?? 0 }} 人</span>
                <span v-if="(permVisible.user_open_ids?.length ?? 0)" class="chips">
                  <span v-for="oid in permVisible.user_open_ids" :key="oid" class="chip">{{ userName[oid] ?? oid.slice(0, 10) }}</span>
                </span>
              </div>
            </div>
            <div class="perm-row">
              <span class="perm-lab">可删除</span>
              <div class="perm-opt">
                <label class="sw"><input type="radio" value="all" v-model="permDeletable.type" /> 所有人</label>
                <label class="sw"><input type="radio" value="none" v-model="permDeletable.type" /> 仅创建者/超管</label>
                <label class="sw"><input type="radio" value="specified" v-model="permDeletable.type" /> 指定成员</label>
              </div>
              <div v-if="permDeletable.type === 'specified'" class="perm-aud">
                <button class="sm" @click="pickAudience('deletable')">选择成员</button>
                <span class="muted small">已选 {{ permDeletable.user_open_ids?.length ?? 0 }} 人</span>
                <span v-if="(permDeletable.user_open_ids?.length ?? 0)" class="chips">
                  <span v-for="oid in permDeletable.user_open_ids" :key="oid" class="chip">{{ userName[oid] ?? oid.slice(0, 10) }}</span>
                </span>
              </div>
            </div>
            <p v-if="!emooUserId" class="msg warn small">未设置「调用者 open_id」，权限接口可能被拒（请在鉴权配置填写你的 open_id，且账号需为工作区超管）。</p>
          </div>
        </div>
        <div class="modal-ft">
          <button class="sm" @click="permOpen = false">取消</button>
          <button class="primary sm" :disabled="permSaving" @click="savePermission">
            <span v-if="permSaving" class="spinner sm"></span>
            保存
          </button>
        </div>
      </div>
    </div>

    <!-- 通讯录选择弹窗 -->
    <div v-if="pickerOpen" class="overlay" @click.self="cancelPicker">
      <div class="modal">
        <div class="modal-hd">
          <span>{{ pickerMulti ? "选择成员" : "选择调用者（自己）" }}</span>
          <button class="link sm" @click="cancelPicker">✕</button>
        </div>
        <div class="modal-bd">
          <div class="row-input">
            <input v-model="wsKeyword" placeholder="搜索姓名 / 邮箱" @keydown.enter="loadWsUsers(wsKeyword)" />
            <button class="sm" @click="loadWsUsers(wsKeyword)">搜索</button>
            <span class="muted small">{{ pickerMulti ? "已选 " + pickerSelected.size : "单选" }}</span>
          </div>
          <div class="userlist">
            <p v-if="wsLoading" class="muted small">加载中…</p>
            <p v-else-if="!wsUsers.length" class="muted small">无结果</p>
            <label
              v-for="u in wsUsers"
              :key="u.open_id"
              class="userrow"
              :class="{ sel: pickerSelected.has(u.open_id) }"
            >
              <input type="checkbox" :checked="pickerSelected.has(u.open_id)" @change="togglePick(u.open_id)" />
              <span class="un">{{ u.ws_username }}</span>
              <span class="muted small">{{ u.email ?? "" }}</span>
            </label>
          </div>
        </div>
        <div class="modal-ft">
          <button class="sm" @click="cancelPicker">取消</button>
          <button class="primary sm" :disabled="pickerMulti && pickerSelected.size === 0" @click="confirmPicker">确定</button>
        </div>
      </div>
    </div>
  </main>
</template>

<style scoped>
.wrap {
  max-width: 720px;
  margin: 0 auto;
  padding: 16px 18px 22px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.hd {
  display: flex;
  align-items: center;
  gap: 9px;
}
.brand {
  width: 22px;
  height: 22px;
  border-radius: 6px;
  flex: none;
}
.spacer {
  flex: 1;
}
h1 {
  font-size: 16px;
  margin: 0;
  letter-spacing: 0.2px;
}
.top-sw {
  font-size: 12px;
  color: var(--muted);
  cursor: pointer;
  user-select: none;
}
.card {
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 14px 16px;
  background: var(--card-bg);
  display: flex;
  flex-direction: column;
  gap: 11px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.04);
}
h2 {
  font-size: 13px;
  margin: 0;
  color: var(--muted);
  font-weight: 600;
}
.muted {
  color: var(--muted);
}
.small {
  font-size: 11px;
}
label {
  display: flex;
  flex-direction: column;
  gap: 5px;
  font-size: 12px;
  color: var(--muted);
}
input,
select {
  padding: 8px 10px;
  font-size: 13px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--input-bg);
  color: inherit;
  transition: border-color 0.15s;
}
input:focus {
  outline: none;
  border-color: #9373ee;
}
.field {
  display: flex;
  flex-direction: column;
  gap: 5px;
  font-size: 12px;
  color: var(--muted);
}
.lab {
  font-size: 12px;
}
.row-input {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.row-input button {
  flex: none;
}
.path {
  flex: 1;
  min-width: 120px;
  font-size: 13px;
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  padding-left: 4px;
}
.path.empty {
  color: var(--muted);
  opacity: 0.7;
}
.cap {
  font-size: 11px;
  color: var(--muted);
  flex: none;
}
.actions {
  display: flex;
  gap: 8px;
  align-items: center;
}
button {
  padding: 8px 15px;
  font-size: 13px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--btn-bg);
  color: inherit;
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 7px;
  transition: border-color 0.15s, background 0.15s;
}
button:hover:not(:disabled) {
  border-color: #9373ee;
}
button:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}
button.primary {
  background: #9373ee;
  color: #fff;
  border-color: #9373ee;
}
button.primary:hover:not(:disabled) {
  background: #8466e6;
}
button.primary .spinner {
  border-color: rgba(255, 255, 255, 0.35);
  border-top-color: #fff;
}
button.sm {
  padding: 5px 11px;
  font-size: 12px;
}
button.link {
  border: none;
  background: transparent;
  color: var(--muted);
  padding: 5px 8px;
}
button.link:hover {
  color: #9373ee;
}
button.danger {
  color: #d73a49;
  border-color: transparent;
}
button.danger:hover:not(:disabled) {
  border-color: #d73a49;
}
.bar {
  font-size: 12px;
  color: var(--muted);
  padding: 0 2px;
  display: flex;
  align-items: center;
  gap: 6px;
}
.live {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: #2ea043;
  flex: none;
}
.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 0 2px;
}
.toolbar .count {
  font-size: 11px;
}
.msg {
  margin: 0;
  font-size: 12px;
}
.msg.ok {
  color: #2ea043;
}
.msg.err {
  color: #d73a49;
  word-break: break-word;
}
.msg.warn {
  color: #b08400;
}
.crumb {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 12px;
  color: var(--muted);
  margin-bottom: 2px;
}
.tree {
  border: 1px solid var(--border);
  border-radius: 8px;
  min-height: 132px;
  max-height: 200px;
  overflow: auto;
  background: var(--input-bg);
  padding: 3px;
}
.tree .row {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 5px 8px;
  font-size: 13px;
  border-radius: 6px;
  cursor: pointer;
  user-select: none;
  color: var(--text);
}
.tree .row:hover {
  background: rgba(147, 115, 238, 0.09);
}
.tree .row.sel {
  background: rgba(147, 115, 238, 0.16);
}
.tree .row.sel .nm {
  color: #9373ee;
  font-weight: 500;
}
.tree .row.note {
  cursor: default;
}
.tree .row.note:hover {
  background: transparent;
}
.tog {
  width: 14px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
}
.chev {
  width: 11px;
  height: 11px;
  color: var(--muted);
  transition: transform 0.15s ease;
}
.chev.open {
  transform: rotate(90deg);
}
.ic-folder {
  width: 15px;
  height: 15px;
  color: var(--muted);
  flex: none;
}
.tree .row.sel .ic-folder {
  color: #9373ee;
}
.ic-check {
  width: 14px;
  height: 14px;
  color: #9373ee;
  flex: none;
}
.nm {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.switches {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.sw {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  flex-direction: row;
  font-size: 12px;
  color: var(--text);
}
.sw input[type="checkbox"] {
  width: 15px;
  height: 15px;
  accent-color: #9373ee;
}
.intv {
  width: 78px;
  padding: 4px 8px;
}
/* 任务列表 */
.tlist {
  display: flex;
  flex-direction: column;
  gap: 10px;
  max-height: 380px;
  overflow-y: auto;
  padding-right: 2px;
}
.empty {
  text-align: center;
  padding: 26px 0;
  font-size: 13px;
}
.tcard {
  border: 1px solid var(--border);
  border-radius: 12px;
  padding: 12px 14px;
  background: var(--card-bg);
  display: flex;
  flex-direction: column;
  gap: 9px;
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.04);
}
.tcard-hd {
  display: flex;
  align-items: center;
  gap: 8px;
}
.tname {
  font-size: 14px;
  font-weight: 600;
}
.ls {
  font-size: 11px;
}
.badge {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 999px;
  border: 1px solid var(--border);
  display: inline-flex;
  align-items: center;
  gap: 5px;
  white-space: nowrap;
}
.badge.type {
  color: var(--muted);
}
.st-idle {
  color: var(--muted);
}
.st-sync {
  color: #9373ee;
  border-color: rgba(147, 115, 238, 0.4);
}
.st-pause {
  color: #b08400;
  border-color: rgba(176, 132, 0, 0.4);
}
.st-err {
  color: #d73a49;
  border-color: rgba(215, 58, 73, 0.4);
}
.paths {
  font-size: 12px;
  display: flex;
  align-items: center;
  gap: 5px;
  flex-wrap: wrap;
}
.mono {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}
.arr {
  margin: 0 2px;
}
.prog {
  display: flex;
  align-items: center;
  gap: 8px;
}
.prog-bar {
  flex: 1;
  height: 6px;
  background: var(--border);
  border-radius: 999px;
  overflow: hidden;
}
.prog-fill {
  height: 100%;
  background: #9373ee;
  transition: width 0.2s ease;
}
.prog-txt {
  font-size: 11px;
  flex: none;
}
.controls {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.sched-on {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}
.sched-on .intv {
  width: 72px;
}
/* 文件记录列表 */
.fline {
  font-size: 11px;
  display: flex;
  align-items: center;
  gap: 8px;
  line-height: 1.6;
}
.fstatus {
  flex: none;
}
.fstatus.fs-ok {
  color: #2ea043;
}
.fstatus.fs-del {
  color: #b08400;
}
.fname {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.fsz {
  flex: none;
}
.fkey {
  flex: none;
  font-family: ui-monospace, monospace;
  opacity: 0.7;
}
.logbox {
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--input-bg);
  padding: 8px 10px;
  max-height: 180px;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.logbox.global {
  max-height: 220px;
}
.logline {
  font-size: 11px;
  display: flex;
  gap: 8px;
  align-items: baseline;
  line-height: 1.5;
}
.lt {
  flex: none;
  color: var(--muted);
  font-family: ui-monospace, monospace;
}
.lwho {
  flex: none;
  font-family: ui-monospace, monospace;
}
.lm {
  flex: none;
}
.ld {
  flex: 1;
  word-break: break-all;
}
.lv-warn .lm {
  color: #b08400;
}
.lv-error .lm {
  color: #d73a49;
}
.lv-info .lm {
  color: var(--text);
}
.drawer {
  border: 1px solid var(--border);
  border-radius: 10px;
  background: var(--card-bg);
  overflow: hidden;
}
.drawer-hd {
  display: flex;
  justify-content: space-between;
  padding: 9px 12px;
  font-size: 12px;
  cursor: pointer;
  user-select: none;
}
.drawer-hd:hover {
  background: rgba(147, 115, 238, 0.06);
}
.spinner {
  width: 13px;
  height: 13px;
  border: 2px solid rgba(147, 115, 238, 0.25);
  border-top-color: #9373ee;
  border-radius: 50%;
  animation: spin 0.7s linear infinite;
  display: inline-block;
  flex: none;
}
.spinner.sm {
  width: 12px;
  height: 12px;
  border-width: 2px;
}
.spinner.xs {
  width: 10px;
  height: 10px;
  border-width: 1.5px;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
/* 弹窗（权限编辑 / 通讯录选择） */
.overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.42);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
  padding: 16px;
  box-sizing: border-box;
}
.modal {
  width: 100%;
  max-width: 460px;
  max-height: 88vh;
  overflow: hidden;
  background: var(--card-bg);
  border-radius: 12px;
  border: 1px solid var(--border);
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.22);
  display: flex;
  flex-direction: column;
}
.modal-hd {
  flex: 0 0 auto;
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 12px 14px;
  font-size: 13px;
  font-weight: 600;
  border-bottom: 1px solid var(--border);
}
.modal-bd {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.modal-ft {
  flex: 0 0 auto;
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  padding: 10px 14px;
  border-top: 1px solid var(--border);
}
.perm-warn {
  font-size: 12px;
  color: #b08400;
  background: rgba(176, 132, 0, 0.1);
  border: 1px solid rgba(176, 132, 0, 0.3);
  border-radius: 8px;
  padding: 8px 10px;
  line-height: 1.5;
}
.perm-fields {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.perm-row {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.perm-lab {
  font-size: 12px;
  color: var(--muted);
}
.perm-opt {
  display: flex;
  gap: 12px;
  flex-wrap: wrap;
}
.perm-opt input[type="radio"] {
  accent-color: #9373ee;
  width: 14px;
  height: 14px;
}
.perm-aud {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.chips {
  display: inline-flex;
  gap: 4px;
  flex-wrap: wrap;
}
.chip {
  font-size: 11px;
  background: rgba(147, 115, 238, 0.12);
  color: #9373ee;
  padding: 1px 8px;
  border-radius: 999px;
}
.userlist {
  max-height: 280px;
  overflow-y: auto;
  border: 1px solid var(--border);
  border-radius: 8px;
  margin-top: 4px;
}
.userrow {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 10px;
  font-size: 13px;
  cursor: pointer;
  flex-direction: row;
  border-bottom: 1px solid var(--border);
}
.userrow:last-child {
  border-bottom: none;
}
.userrow:hover {
  background: rgba(147, 115, 238, 0.06);
}
.userrow.sel {
  background: rgba(147, 115, 238, 0.14);
}
.userrow input[type="checkbox"] {
  width: 15px;
  height: 15px;
  accent-color: #9373ee;
}
.un {
  font-weight: 500;
}
</style>

<style>
body {
  margin: 0;
}
:root {
  font-family: Inter, -apple-system, "PingFang SC", "Microsoft YaHei", Avenir, Helvetica, Arial,
    sans-serif;
  font-size: 14px;
  line-height: 1.5;
  color: #1f2328;
  background-color: #f4f4f6;
  --text: #1f2328;
  --border: #e2e2e7;
  --card-bg: #ffffff;
  --input-bg: #ffffff;
  --btn-bg: #ffffff;
  --muted: #6b7280;
}
@media (prefers-color-scheme: dark) {
  :root {
    color: #e6e6e6;
    background-color: #16151d;
    --text: #e6e6e6;
    --border: #34343e;
    --card-bg: #21202a;
    --input-bg: #18171f;
    --btn-bg: #2c2b36;
    --muted: #9aa0aa;
  }
}
</style>
