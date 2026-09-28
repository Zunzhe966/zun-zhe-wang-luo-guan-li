<template>
  <div class="page">
    <header class="page-head">
      <div><h1>设置</h1><p>应用版本与软件更新</p></div>
    </header>

    <section class="panel">
      <div class="stat-grid">
        <div class="stat-card">
          <div class="stat-label">当前版本</div>
          <div class="stat-value">{{ version || '—' }}</div>
          <div class="stat-sub">com.magic.agent</div>
        </div>
        <div class="stat-card">
          <div class="stat-label">更新状态</div>
          <div class="stat-value" :class="{ good: updaterState.state === 'idle' || updaterState.state === 'latest', warn: updaterState.state === 'available' }">{{ statusText }}</div>
          <div class="stat-sub">{{ updaterState.state === 'available' ? `新版本 ${updaterState.latestVersion}` : updaterState.channelLabel }}</div>
          <button class="btn primary" :disabled="updaterState.busy" @click="checkUpdate({ manual: true })">{{ updaterState.busy ? '处理中…' : '检查更新' }}</button>
        </div>
      </div>
      <div v-if="updaterState.state === 'downloading' || updaterState.state === 'installing'" class="progress-bar">
        <div class="progress-fill" :style="{ width: updaterState.progress + '%' }"></div>
        <span class="progress-text">{{ updaterState.state === 'downloading' ? `下载中 ${updaterState.progress}%` : '安装中…' }}</span>
      </div>
    </section>

    <section class="panel">
      <h2>更新通道</h2>
      <p class="muted">选择从哪个源检查更新。两个通道使用同一把签名公钥校验，切换只改变下载来源。</p>
      <div class="channel-list">
        <label
          v-for="ch in channels"
          :key="ch.id"
          class="channel-item"
          :class="{ active: updaterState.channel === ch.id, disabled: updaterState.busy }"
        >
          <input
            type="radio"
            name="update-channel"
            :value="ch.id"
            v-model="channelModel"
            :disabled="updaterState.busy"
          />
          <div class="channel-body">
            <div class="channel-name">{{ ch.name }}</div>
            <div class="channel-desc">{{ ch.desc }}</div>
            <code class="channel-endpoint">{{ ch.endpoint }}</code>
          </div>
        </label>
      </div>
      <p v-if="channelHint" class="muted channel-hint">{{ channelHint }}</p>
    </section>

    <section class="panel">
      <h2>接管方式</h2>
      <p class="muted">控制点击「启动代理」时如何接管系统网络。接管动作始终会记账，失败时自动还原系统代理设置。</p>
      <label class="channel-item" :class="{ active: confirmModel }">
        <input type="checkbox" v-model="confirmModel" />
        <div class="channel-body">
          <div class="channel-name">接管前确认（确认模式）</div>
          <div class="channel-desc">启动前先做只读网络体检，列出将被关闭的第三方代理等混乱源，经你确认后才执行接管。关闭则为快速模式：直接体检→记账→清理→启动。</div>
        </div>
      </label>
    </section>

    <section class="panel">
      <h2>关于</h2>
      <p class="muted">尊者网络管理 / Network Manager</p>
      <p class="muted">macOS Tauri 2 桌面代理软件 · Rust + Vue3</p>
      <p class="muted">当前更新通道：{{ updaterState.channelLabel }}</p>
    </section>
  </div>
</template>

<script setup>
import { ref, computed, onMounted } from 'vue';
import { getVersion } from '@tauri-apps/api/app';
import { updaterState, checkUpdate, refreshChannel, selectChannel } from '../updater.js';

const version = ref('');

// ── 接管方式（P1-3 确认模式） ─────────────────────────────
// 开关持久化在 config.confirmTakeover，保存走 App.vue 的 saveConfig 通道；
// 保存失败时父组件回退 config，本组件的 get 自动反映回退态（同更新通道模式）。
const props = defineProps({ config: Object });
const emit = defineEmits(['update']);

const confirmModel = computed({
  get: () => !!props.config?.confirmTakeover,
  set: (v) => emit('update', { confirmTakeover: v }),
});

// ── 更新通道 ──────────────────────────────────────────────
// local  = 本地开发测试（自己用：本机起 http.server 7878）
// github = GitHub Releases（真实用户用：公开分发）
// 当前选中通道与更新状态都在全局唯一的 updaterState 里，与启动静默检查共享。
const channels = [
  {
    id: 'local',
    name: '本地开发测试',
    desc: '从本机 127.0.0.1:7878 拉取更新包，供开发者调试发版流程。',
    endpoint: 'http://127.0.0.1:7878/latest.json',
  },
  {
    id: 'github',
    name: 'GitHub 公开发布',
    desc: '从 GitHub Releases 拉取更新包，面向所有终端用户，无需本机服务。',
    endpoint: 'https://github.com/Zunzhe966/zun-zhe-wang-luo-guan-li/releases/latest/download/latest.json',
  },
];

// 用可写 computed 绑定单选：切换失败或通道未变更时，选中态由 Vue 自动回退，
// 不会出现「点了没反应、界面却已经选中」的状态不同步（旧实现用
// :checked + @change 会有这个问题）。
const channelModel = computed({
  get: () => updaterState.channel,
  set: (id) => selectChannel(id),
});

const channelHint = computed(() =>
  updaterState.channel === 'local'
    ? '提示：本地通道需要先在终端运行 cd scripts/updater-feed && python3 -m http.server 7878 --bind 127.0.0.1'
    : '',
);

const statusText = computed(() => ({
  idle: '未检查',
  checking: '检查中',
  available: '有新版本',
  downloading: '下载中',
  installing: '安装中',
  latest: '已是最新',
  error: '检查失败',
}[updaterState.state] || '—'));

onMounted(async () => {
  try {
    version.value = await getVersion();
  } catch (e) {
    console.error('getVersion failed', e);
  }
  await refreshChannel();
});
</script>

<style scoped>
.channel-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
  margin-top: 12px;
}
.channel-item {
  display: flex;
  gap: 12px;
  align-items: flex-start;
  padding: 12px 14px;
  border: 1px solid var(--line);
  border-radius: 10px;
  cursor: pointer;
  transition: border-color 0.15s, background 0.15s;
  background: var(--panel2);
}
.channel-item:hover {
  border-color: var(--accent);
}
.channel-item.active {
  border-color: var(--accent);
  background: rgba(77, 124, 254, 0.14);
}
.channel-item.disabled {
  opacity: 0.55;
  cursor: not-allowed;
}
.channel-item input[type='radio'] {
  margin-top: 3px;
  accent-color: var(--accent);
}
.channel-body {
  flex: 1;
  min-width: 0;
}
.channel-name {
  font-weight: 600;
  margin-bottom: 3px;
}
.channel-desc {
  font-size: 12.5px;
  color: var(--muted);
  line-height: 1.5;
}
.channel-endpoint {
  display: block;
  margin-top: 6px;
  font-size: 11.5px;
  color: var(--muted);
  word-break: break-all;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.channel-hint {
  margin-top: 10px;
  font-size: 12px;
}
</style>
