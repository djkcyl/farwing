(() => {
  'use strict';

  const TOKEN_KEY = 'openuuyc.self-host.device-token.v1';
  const MAX_CONTROL_BUFFER = 512 * 1024;
  const MAX_TEXT_BYTES = 64 * 1024;
  const ALLOWED_TOKEN = /^[A-Za-z0-9_-]{32,128}$/u;
  const ALLOWED_DEVICE_ID = /^[a-f0-9]{16}$/u;
  const $ = (id) => document.getElementById(id);

  const ui = {
    welcome: $('welcome-panel'),
    pairForm: $('pair-form'),
    pairName: $('controller-name'),
    pairCode: $('pair-code'),
    pairError: $('pair-error'),
    devices: $('devices-panel'),
    devicesList: $('devices-list'),
    devicesError: $('devices-error'),
    refresh: $('refresh-button'),
    forget: $('forget-button'),
    session: $('session-panel'),
    deviceName: $('session-device-name'),
    back: $('back-button'),
    disconnect: $('disconnect-button'),
    status: $('connection-status'),
    statusText: document.querySelector('#connection-status .status-text'),
    video: $('remote-video'),
    videoStage: $('video-stage'),
    placeholder: $('stage-placeholder'),
    resolution: $('video-resolution'),
    stageTitle: $('stage-state-title'),
    stageDetail: $('stage-state-detail'),
    sessionError: $('session-error'),
    audio: $('audio-button'),
    audioHint: $('audio-hint'),
    keyboard: $('keyboard-input'),
    sendText: $('send-text-button'),
    toast: $('toast'),
  };

  const state = {
    token: null,
    devices: [],
    currentDevice: null,
    epoch: 0,
    abort: null,
    socket: null,
    pendingSession: null,
    authSent: false,
    peer: null,
    control: null,
    text: null,
    stream: null,
    currentScreenId: 0,
    localCandidates: [],
    remoteCandidates: [],
    offerSent: false,
    remoteDescriptionSet: false,
    signalingReady: false,
    dataReady: false,
    pressedKeys: new Map(),
    pendingPointer: false,
    lastPointerAt: 0,
    frameTimer: 0,
    frameCallback: 0,
    authTimer: 0,
    answerTimer: 0,
    frameDeadline: 0,
    toastTimer: 0,
    keyboardFocused: false,
    stopped: true,
  };

  function setVisible(element, visible) {
    element.hidden = !visible;
  }

  function setError(element, message) {
    element.textContent = message || '';
    setVisible(element, Boolean(message));
  }

  function setAudioButton(enabled) {
    const icon = document.createElement('span');
    icon.className = 'audio-symbol';
    icon.setAttribute('aria-hidden', 'true');
    icon.textContent = enabled ? '◗' : '◖';
    const label = document.createElement('span');
    label.textContent = enabled ? '关闭声音' : '开启声音';
    ui.audio.replaceChildren(icon, label);
  }

  function showToast(message) {
    ui.toast.textContent = message;
    setVisible(ui.toast, true);
    window.clearTimeout(state.toastTimer);
    state.toastTimer = window.setTimeout(() => setVisible(ui.toast, false), 3200);
  }

  function setScreen(screen) {
    setVisible(ui.welcome, screen === 'welcome');
    setVisible(ui.devices, screen === 'devices');
    setVisible(ui.session, screen === 'session');
  }

  function showStep(name, status) {
    const order = ['signaling', 'media', 'frame'];
    const currentIndex = order.indexOf(name);
    document.querySelectorAll('.state-step').forEach((node) => {
      const index = order.indexOf(node.dataset.step);
      node.classList.toggle('is-active', index === currentIndex && status === 'active');
      node.classList.toggle('is-done', index < currentIndex || (index === currentIndex && status === 'done'));
    });
  }

  function setStatus(key, label) {
    ui.status.dataset.state = key;
    ui.statusText.textContent = label;
  }

  function sessionDetail(title, detail) {
    ui.stageTitle.textContent = title;
    ui.stageDetail.textContent = detail;
  }

  function apiErrorMessage(body, fallback) {
    if (body && typeof body.error === 'string' && body.error.length <= 180) return body.error;
    return fallback;
  }

  async function request(path, options = {}) {
    const headers = new Headers(options.headers || {});
    if (options.body !== undefined) headers.set('Content-Type', 'application/json');
    if (options.auth !== false && state.token) headers.set('Authorization', `Bearer ${state.token}`);
    const response = await fetch(path, {
      method: options.method || 'GET',
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: options.signal,
      cache: 'no-store',
      credentials: 'same-origin',
      redirect: 'error',
      referrerPolicy: 'no-referrer',
    });
    let body = {};
    try { body = await response.json(); } catch { /* Use a generic error below. */ }
    if (!response.ok) {
      const error = new Error(apiErrorMessage(body, `请求失败（${response.status}）`));
      error.status = response.status;
      throw error;
    }
    return body;
  }

  function loadSavedToken() {
    try {
      const token = localStorage.getItem(TOKEN_KEY);
      return token && ALLOWED_TOKEN.test(token) ? token : null;
    } catch {
      return null;
    }
  }

  function persistToken(token) {
    state.token = token;
    try {
      localStorage.setItem(TOKEN_KEY, token);
      return true;
    } catch {
      return false;
    }
  }

  function forgetToken() {
    try { localStorage.removeItem(TOKEN_KEY); } catch { /* The memory token is still cleared. */ }
    state.token = null;
  }

  function getSignalUrl(value) {
    if (typeof value !== 'string' || value.length > 2048) throw new Error('服务器未提供有效的信令地址。');
    let announced;
    try { announced = new URL(value, location.href); } catch { throw new Error('服务器返回的信令地址无效。'); }
    const expectedProtocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const httpProtocol = announced.protocol === 'wss:' ? 'https:' : announced.protocol === 'ws:' ? 'http:' : '';
    const expectedOrigin = location.origin;
    const normalizedOrigin = httpProtocol ? `${httpProtocol}//${announced.host}` : '';
    if (announced.protocol !== expectedProtocol || normalizedOrigin !== expectedOrigin
      || announced.pathname !== '/v1/session' || announced.search || announced.hash
      || announced.username || announced.password) {
      throw new Error('信令地址与当前站点来源不一致。请通过服务器配置的地址访问。');
    }
    return `${expectedProtocol}//${location.host}/v1/session`;
  }

  function safeSocketClose(socket, code = 1000, reason = 'closed') {
    if (!socket) return;
    try {
      if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
        socket.close(code, reason);
      }
    } catch { /* Socket may already be closed. */ }
  }

  function cleanupDataChannel(channel) {
    if (!channel) return;
    channel.onopen = null;
    channel.onclose = null;
    channel.onerror = null;
    channel.onmessage = null;
    try { channel.close(); } catch { /* Already closed. */ }
  }

  function current(epoch) {
    return state.epoch === epoch && !state.stopped;
  }

  function clearSessionTimers() {
    window.clearTimeout(state.authTimer);
    window.clearTimeout(state.answerTimer);
    window.clearTimeout(state.frameDeadline);
    window.clearInterval(state.frameTimer);
    state.authTimer = 0;
    state.answerTimer = 0;
    state.frameDeadline = 0;
    state.frameTimer = 0;
    if (state.frameCallback && typeof ui.video.cancelVideoFrameCallback === 'function') {
      ui.video.cancelVideoFrameCallback(state.frameCallback);
    }
    state.frameCallback = 0;
  }

  function clearPeer() {
    clearSessionTimers();
    cleanupDataChannel(state.control);
    cleanupDataChannel(state.text);
    state.control = null;
    state.text = null;
    state.dataReady = false;
    if (state.peer) {
      state.peer.onicecandidate = null;
      state.peer.onconnectionstatechange = null;
      state.peer.oniceconnectionstatechange = null;
      state.peer.ondatachannel = null;
      state.peer.ontrack = null;
      try { state.peer.close(); } catch { /* Already closed. */ }
    }
    state.peer = null;
    if (state.stream) {
      for (const track of state.stream.getTracks()) {
        try { track.stop(); } catch { /* Track is already ended. */ }
      }
    }
    state.stream = null;
    ui.video.pause();
    ui.video.srcObject = null;
    ui.video.dataset.state = 'idle';
    ui.video.dataset.frameReady = 'false';
    ui.video.muted = true;
    ui.video.volume = 1;
    ui.videoStage.dataset.frameReady = 'false';
    setVisible(ui.placeholder, true);
    ui.resolution.textContent = '等待画面';
    ui.audio.disabled = true;
    ui.audio.setAttribute('aria-pressed', 'false');
    setAudioButton(false);
    ui.audioHint.hidden = true;
    ui.keyboard.disabled = true;
    ui.sendText.disabled = true;
    releasePressedKeys();
  }

  function releasePressedKeys() {
    const pressed = [...state.pressedKeys.values()];
    state.pressedKeys.clear();
    for (const key of pressed) sendInput({ action: 'kbd_release', key });
  }

  function stopSession(options = {}) {
    const wasStopped = state.stopped;
    state.epoch += 1;
    state.stopped = true;
    if (state.abort) {
      try { state.abort.abort(); } catch { /* Request already finished. */ }
    }
    state.abort = null;
    releasePressedKeys();
    if (state.socket && state.signalingReady && options.sendLeave !== false && state.socket.readyState === WebSocket.OPEN) {
      try { state.socket.send(JSON.stringify({ type: 'leave' })); } catch { /* Closing the socket also ends the session. */ }
    }
    if (!state.signalingReady && options.sendLeave !== false && state.pendingSession) {
      orphanSession(state.pendingSession, state.socket, state.authSent);
    } else {
      safeSocketClose(state.socket, 1000, 'controller left');
    }
    state.socket = null;
    state.pendingSession = null;
    state.authSent = false;
    clearPeer();
    state.localCandidates = [];
    state.remoteCandidates = [];
    state.offerSent = false;
    state.remoteDescriptionSet = false;
    state.signalingReady = false;
    state.pendingPointer = false;
    ui.disconnect.disabled = true;
    ui.disconnect.textContent = '断开连接';
    ui.keyboard.blur();
    ui.status.dataset.state = 'idle';
    ui.statusText.textContent = wasStopped ? '未连接' : '已断开';
    if (options.keepSession) {
      setStatus(options.error ? 'error' : 'idle', options.error ? '连接已结束' : '已断开');
      if (options.error) sessionDetail('连接未完成', options.error);
    }
  }

  function setDevicesView() {
    state.currentDevice = null;
    setScreen('devices');
  }

  function renderDevices(devices) {
    state.devices = devices;
    ui.devicesList.replaceChildren();
    if (!devices.length) {
      const empty = document.createElement('div');
      empty.className = 'empty-state';
      empty.textContent = '此控制器尚未授权远程主机。请使用绑定到目标主机的控制器配对码。';
      ui.devicesList.append(empty);
      return;
    }

    devices.forEach((device, index) => {
      const row = document.createElement('article');
      row.className = 'device-row';
      const main = document.createElement('div');
      main.className = 'device-main';
      const avatar = document.createElement('span');
      avatar.className = 'device-avatar';
      avatar.setAttribute('aria-hidden', 'true');
      avatar.textContent = '▣';
      const copy = document.createElement('div');
      copy.className = 'device-copy';
      const title = document.createElement('div');
      title.className = 'device-title';
      title.textContent = typeof device.name === 'string' ? device.name : 'Windows 主机';
      const meta = document.createElement('div');
      meta.className = 'device-meta';
      const onlineDot = document.createElement('span');
      onlineDot.className = `online-dot${device.online ? ' online' : ''}`;
      onlineDot.setAttribute('aria-hidden', 'true');
      const status = document.createElement('span');
      status.textContent = device.online ? '在线' : '离线';
      const separator = document.createElement('span');
      separator.className = 'dot-separator';
      separator.textContent = '·';
      const control = document.createElement('span');
      control.textContent = device.controllable ? '接受控制' : '暂不接受控制';
      meta.append(onlineDot, status, separator, control);
      copy.append(title, meta);
      main.append(avatar, copy);

      const connect = document.createElement('button');
      connect.className = 'button button-primary button-small';
      connect.type = 'button';
      if (index === 0) connect.id = 'connect-button';
      connect.dataset.connectId = device.id;
      connect.textContent = device.online && device.controllable ? '连接设备 →' : '暂不可连接';
      connect.disabled = !ALLOWED_DEVICE_ID.test(device.id || '') || !device.online || !device.controllable;
      connect.addEventListener('click', () => startSession(device));
      row.append(main, connect);
      ui.devicesList.append(row);
    });
  }

  async function refreshDevices() {
    if (!state.token) {
      setScreen('welcome');
      return;
    }
    const abort = new AbortController();
    state.abort = abort;
    setError(ui.devicesError, '');
    ui.refresh.disabled = true;
    try {
      const result = await request('/v1/devices', { signal: abort.signal });
      if (state.abort !== abort || !state.token) return;
      const devices = Array.isArray(result.devices) ? result.devices.filter((device) => device && typeof device === 'object') : [];
      renderDevices(devices);
      setScreen('devices');
      ui.forget.hidden = false;
    } catch (error) {
      if (error.name === 'AbortError') return;
      if (error.status === 401 || error.status === 403) {
        forgetToken();
        setScreen('welcome');
        ui.forget.hidden = true;
        setError(ui.pairError, '此浏览器的授权已失效，请重新绑定。');
      } else {
        setScreen('devices');
        setError(ui.devicesError, `无法读取设备列表：${error.message}`);
      }
    } finally {
      if (state.abort === abort) state.abort = null;
      ui.refresh.disabled = false;
    }
  }

  let enrollmentToken;

  async function pair(event) {
    event.preventDefault();
    setError(ui.pairError, '');
    const name = ui.pairName.value.trim();
    const code = ui.pairCode.value.trim();
    if (!name) {
      setError(ui.pairError, '请填写浏览器名称。');
      return;
    }
    ui.pairForm.querySelector('button[type="submit"]').disabled = true;
    try {
      let token;
      if (code) {
        const result = await request('/v1/pair', {
          method: 'POST',
          auth: false,
          body: { role: 'controller', platform: 'web', name, code },
        });
        if (!ALLOWED_TOKEN.test(result.token || '') || !ALLOWED_DEVICE_ID.test(result.deviceId || '')) {
          throw new Error('配对响应无效。');
        }
        token = result.token;
      } else {
        enrollmentToken ??= Array.from(crypto.getRandomValues(new Uint8Array(32)),
          (byte) => byte.toString(16).padStart(2, '0')).join('');
        const result = await request('/v1/desktop/register', {
          method: 'POST',
          auth: false,
          headers: { Authorization: `Bearer ${enrollmentToken}` },
          body: { platform: 'web', name },
        });
        if (!ALLOWED_DEVICE_ID.test(result.device_id || '')) throw new Error('设备注册响应无效。');
        if (result.status === 'pending') {
          setError(ui.pairError, `设备 ID ${result.device_id} 正在等待管理员批准。批准后再次点击连接。`);
          return;
        }
        token = enrollmentToken;
      }
      ui.pairCode.value = '';
      const saved = persistToken(token);
      enrollmentToken = undefined;
      ui.forget.hidden = false;
      await refreshDevices();
      if (!saved) showToast('授权仅保存在本次页面内，浏览器未允许保存本地令牌。');
    } catch (error) {
      setError(ui.pairError, `连接服务器失败：${error.message}`);
    } finally {
      ui.pairForm.querySelector('button[type="submit"]').disabled = false;
    }
  }

  function encodeVarint(input) {
    let value = typeof input === 'bigint' ? input : BigInt(input);
    if (value < 0n) value = BigInt.asUintN(64, value);
    const result = [];
    while (value > 0x7fn) {
      result.push(Number((value & 0x7fn) | 0x80n));
      value >>= 7n;
    }
    result.push(Number(value));
    return result;
  }

  function encodeVarintField(tag, value) {
    return [...encodeVarint(BigInt(tag << 3)), ...encodeVarint(value)];
  }

  function encodeBytesField(tag, value) {
    const bytes = value instanceof Uint8Array ? value : new TextEncoder().encode(value);
    return [...encodeVarint(BigInt((tag << 3) | 2)), ...encodeVarint(bytes.length), ...bytes];
  }

  function concatBytes(...chunks) {
    const size = chunks.reduce((total, chunk) => total + chunk.length, 0);
    const output = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      output.set(chunk, offset);
      offset += chunk.length;
    }
    return output;
  }

  function encodeEcho(action, sequence, timestamp) {
    const featureFlags = Uint8Array.from(encodeVarintField(1, 6));
    const args = new TextEncoder().encode(JSON.stringify({ seq: sequence }));
    const simpleAction = concatBytes(
      Uint8Array.from(encodeVarintField(1, action)),
      Uint8Array.from(encodeBytesField(2, args)),
      Uint8Array.from(encodeBytesField(4, featureFlags)),
    );
    return concatBytes(
      Uint8Array.from(encodeVarintField(1, sequence)),
      Uint8Array.from(encodeVarintField(2, timestamp)),
      Uint8Array.from(encodeBytesField(3, simpleAction)),
    );
  }

  function readVarint(bytes, cursor) {
    let value = 0n;
    let shift = 0n;
    for (let count = 0; count < 10; count += 1) {
      if (cursor.offset >= bytes.length) return null;
      const byte = bytes[cursor.offset];
      cursor.offset += 1;
      if (shift === 63n && byte > 1) return null;
      value |= BigInt(byte & 0x7f) << shift;
      if ((byte & 0x80) === 0) return value;
      shift += 7n;
    }
    return null;
  }

  function parseProto(bytes) {
    if (!(bytes instanceof Uint8Array) || bytes.length > MAX_CONTROL_BUFFER) return null;
    const fields = [];
    const cursor = { offset: 0 };
    while (cursor.offset < bytes.length) {
      const tag = readVarint(bytes, cursor);
      if (tag === null || tag === 0n) return null;
      const number = Number(tag >> 3n);
      const wire = Number(tag & 7n);
      if (!number) return null;
      if (wire === 0) {
        const value = readVarint(bytes, cursor);
        if (value === null) return null;
        fields.push({ number, wire, value });
      } else if (wire === 1) {
        if (cursor.offset + 8 > bytes.length) return null;
        cursor.offset += 8;
      } else if (wire === 2) {
        const size = readVarint(bytes, cursor);
        if (size === null || size > BigInt(bytes.length - cursor.offset)) return null;
        const end = cursor.offset + Number(size);
        fields.push({ number, wire, value: bytes.subarray(cursor.offset, end) });
        cursor.offset = end;
      } else if (wire === 5) {
        if (cursor.offset + 4 > bytes.length) return null;
        cursor.offset += 4;
      } else {
        return null;
      }
    }
    return {
      fields,
      varint(number, fallback = null) {
        return fields.find((field) => field.number === number && field.wire === 0)?.value ?? fallback;
      },
      bytes(number) {
        return fields.find((field) => field.number === number && field.wire === 2)?.value ?? null;
      },
      repeatedBytes(number) {
        return fields.filter((field) => field.number === number && field.wire === 2).map((field) => field.value);
      },
    };
  }

  function int32(value) {
    return Number(BigInt.asIntN(32, value));
  }

  async function toBytes(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof Blob) return new Uint8Array(await data.arrayBuffer());
    return null;
  }

  function handleHostControl(bytes, epoch) {
    const envelope = parseProto(bytes);
    if (!envelope) return;
    const actionBytes = envelope.bytes(3);
    if (actionBytes) {
      const action = parseProto(actionBytes);
      if (!action) return;
      const type = action.varint(1, 0n);
      if (type === 0n && current(epoch)) {
        const sequence = Number(envelope.varint(1, 0n));
        const timestamp = Number(envelope.varint(2, 0n));
        sendControl(encodeEcho(1, sequence, timestamp));
      }
      return;
    }

    const screensBytes = envelope.bytes(7);
    if (screensBytes) {
      const screens = parseProto(screensBytes);
      if (screens) state.currentScreenId = int32(screens.varint(2, 0n));
      return;
    }
    const captureChange = envelope.bytes(8);
    if (captureChange) {
      const change = parseProto(captureChange);
      if (change && int32(change.varint(1, 0n)) === 0) state.currentScreenId = int32(change.varint(2, 0n));
    }
  }

  function sendControl(bytes) {
    const channel = state.control;
    if (!channel || channel.readyState !== 'open') return false;
    if (channel.bufferedAmount > MAX_CONTROL_BUFFER) return false;
    try {
      channel.send(bytes);
      return true;
    } catch {
      return false;
    }
  }

  function sendInput(event) {
    const channel = state.control;
    if (!channel || channel.readyState !== 'open' || channel.bufferedAmount > MAX_CONTROL_BUFFER) return false;
    let data;
    try { data = new TextEncoder().encode(JSON.stringify(event)); } catch { return false; }
    if (data.length > MAX_CONTROL_BUFFER) return false;
    try { channel.send(data); return true; } catch { return false; }
  }

  function sendText() {
    const value = ui.keyboard.value;
    if (!value || !state.dataReady) return;
    const byteLength = new TextEncoder().encode(value).length;
    if (byteLength > MAX_TEXT_BYTES) {
      showToast('文字内容过长。');
      return;
    }
    if (sendInput({ action: 'text_input', content: value })) {
      ui.keyboard.value = '';
      ui.keyboard.focus();
    } else {
      showToast('控制通道尚未就绪。');
    }
  }

  function handleControlOpen(epoch) {
    if (!current(epoch) || !state.control) return;
    const sequence = 1;
    const timestamp = Date.now();
    if (!sendControl(encodeEcho(0, sequence, timestamp))) {
      showToast('无法初始化远端控制通道。');
    }
  }

  function markChannelsReady(epoch) {
    if (!current(epoch)) return;
    const ready = state.control?.readyState === 'open' && state.text?.readyState === 'open';
    state.dataReady = Boolean(ready);
    ui.keyboard.disabled = !ready;
    ui.sendText.disabled = !ready;
  }

  function chooseCodec(transceiver, kind, mime, fmtp) {
    if (typeof RTCRtpReceiver.getCapabilities !== 'function' || typeof transceiver.setCodecPreferences !== 'function') return;
    const capabilities = RTCRtpReceiver.getCapabilities(kind)?.codecs;
    if (!capabilities) return;
    const matches = capabilities.filter((codec) => codec.mimeType?.toLowerCase() === mime
      && (!fmtp || fmtp(codec.sdpFmtpLine || '')));
    if (matches.length) transceiver.setCodecPreferences(matches);
  }

  function codecPreferences(peer) {
    const video = peer.addTransceiver('video', { direction: 'recvonly' });
    chooseCodec(video, 'video', 'video/h264', (line) => {
      const parts = Object.fromEntries(line.toLowerCase().split(';').map((part) => part.trim().split('=').map((value) => value.trim())));
      return parts['packetization-mode'] === '1' && (parts['profile-level-id'] || '').toLowerCase() === '42e01f';
    });
    const audio = peer.addTransceiver('audio', { direction: 'recvonly' });
    chooseCodec(audio, 'audio', 'audio/opus');
  }

  function transmitCandidate(candidate, epoch) {
    if (!current(epoch) || !state.socket || state.socket.readyState !== WebSocket.OPEN) return;
    const payload = candidate.toJSON ? candidate.toJSON() : candidate;
    const sanitized = { candidate: payload.candidate };
    if (payload.sdpMid !== undefined) sanitized.sdpMid = payload.sdpMid;
    if (payload.sdpMLineIndex !== undefined) sanitized.sdpMLineIndex = payload.sdpMLineIndex;
    if (payload.usernameFragment !== undefined) sanitized.usernameFragment = payload.usernameFragment;
    try { state.socket.send(JSON.stringify({ type: 'candidate', candidate: sanitized })); } catch { /* The session cleanup handles a closed socket. */ }
  }

  function handleLocalCandidate(candidate, epoch) {
    if (!current(epoch) || !candidate) return;
    if (!state.offerSent) state.localCandidates.push(candidate);
    else transmitCandidate(candidate, epoch);
  }

  async function flushRemoteCandidates(epoch) {
    if (!current(epoch) || !state.remoteDescriptionSet || !state.peer) return;
    const pending = state.remoteCandidates.splice(0);
    for (const candidate of pending) {
      try { await state.peer.addIceCandidate(candidate); } catch { /* Candidate can be stale during a restart. */ }
      if (!current(epoch)) return;
    }
  }

  async function setRemoteCandidate(candidate, epoch) {
    if (!current(epoch) || !candidate || typeof candidate.candidate !== 'string') return;
    const safe = {
      candidate: candidate.candidate,
      sdpMid: candidate.sdpMid ?? null,
      sdpMLineIndex: candidate.sdpMLineIndex ?? null,
    };
    if (typeof candidate.usernameFragment === 'string') safe.usernameFragment = candidate.usernameFragment;
    if (!state.remoteDescriptionSet) {
      state.remoteCandidates.push(safe);
      return;
    }
    try { await state.peer.addIceCandidate(safe); } catch { /* Ignore stale trickle arriving during teardown. */ }
  }

  function setFrameReady(epoch) {
    if (!current(epoch) || state.stopped || ui.video.videoWidth < 1 || ui.video.videoHeight < 1) return false;
    if (ui.video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA) return false;
    window.clearTimeout(state.frameDeadline);
    state.frameDeadline = 0;
    ui.video.dataset.state = 'playing';
    ui.video.dataset.frameReady = 'true';
    ui.videoStage.dataset.frameReady = 'true';
    setVisible(ui.placeholder, false);
    ui.resolution.textContent = `${ui.video.videoWidth} × ${ui.video.videoHeight}`;
    ui.audio.disabled = false;
    setStatus('streaming', '正在播放');
    sessionDetail('远端画面已就绪', '首帧已经解码。点击画面可定位并单击，点击后可使用键盘。');
    showStep('frame', 'done');
    return true;
  }

  function watchVideoFrames(epoch) {
    if (!current(epoch)) return;
    if (typeof ui.video.requestVideoFrameCallback === 'function') {
      const watch = () => {
        if (!current(epoch) || ui.video.dataset.frameReady === 'true') return;
        state.frameCallback = ui.video.requestVideoFrameCallback(() => {
          state.frameCallback = 0;
          if (!setFrameReady(epoch)) watch();
        });
      };
      watch();
    } else {
      state.frameTimer = window.setInterval(() => {
        if (setFrameReady(epoch)) {
          window.clearInterval(state.frameTimer);
          state.frameTimer = 0;
        }
      }, 120);
    }
  }

  async function acceptTrack(event, epoch) {
    if (!current(epoch)) return;
    const track = event.track;
    // Hosts advertise audio and video with separate stream IDs.
    if (!state.stream) {
      state.stream = new MediaStream([track]);
      ui.video.srcObject = state.stream;
    } else {
      state.stream.addTrack(track);
    }
    if (track.kind !== 'video') return;
    ui.video.dataset.state = 'decoding';
    ui.resolution.textContent = '正在等待首帧';
    sessionDetail('媒体通道已建立', '正在等待浏览器解码远端画面的第一帧。');
    showStep('frame', 'active');
    watchVideoFrames(epoch);
    try {
      await ui.video.play();
    } catch {
      if (current(epoch)) ui.audioHint.hidden = false;
    }
  }

  function failSession(epoch, message) {
    if (!current(epoch)) return;
    const explanation = message || '连接失败，请确认主机在线并重试。';
    stopSession({ sendLeave: true, keepSession: true, error: explanation });
    setStatus('error', '连接失败');
    ui.disconnect.disabled = false;
    ui.disconnect.textContent = '返回设备';
    setError(ui.sessionError, explanation);
  }

  function handleConnectionState(epoch) {
    if (!current(epoch) || !state.peer) return;
    const value = state.peer.connectionState;
    if (value === 'connected') {
      if (ui.video.dataset.frameReady === 'true') return;
      showStep('media', 'done');
      showStep('frame', 'active');
      setStatus('negotiating', '等待首帧');
      sessionDetail('媒体通道已建立', '音视频连接成功，正在等待首个解码画面。');
    } else if (value === 'connecting' || value === 'new') {
      setStatus('negotiating', '正在协商');
    } else if (value === 'failed') {
      failSession(epoch, '媒体协商失败。请确认浏览器与主机在可互通的网络中。');
    } else if (value === 'disconnected') {
      setStatus('negotiating', '连接中断');
      sessionDetail('媒体连接暂时中断', '浏览器正在等待 WebRTC 连接恢复。');
    } else if (value === 'closed' && !state.stopped) {
      failSession(epoch, '媒体连接已关闭。');
    }
  }

  function makePeer(epoch, iceServers) {
    if (!current(epoch)) return null;
    const peer = new RTCPeerConnection({ iceServers, bundlePolicy: 'max-bundle', rtcpMuxPolicy: 'require' });
    state.peer = peer;
    state.currentScreenId = 0;
    state.localCandidates = [];
    state.remoteCandidates = [];
    state.offerSent = false;
    state.remoteDescriptionSet = false;
    codecPreferences(peer);

    const control = peer.createDataChannel('CONTROL_DATA_CHANNEL', { ordered: true });
    const text = peer.createDataChannel('TEXT_DATA_CHANNEL', { ordered: true });
    control.binaryType = 'arraybuffer';
    state.control = control;
    state.text = text;
    control.onopen = () => {
      if (!current(epoch)) return;
      handleControlOpen(epoch);
      markChannelsReady(epoch);
    };
    control.onmessage = async (event) => {
      if (!current(epoch)) return;
      const bytes = await toBytes(event.data);
      if (bytes && current(epoch)) handleHostControl(bytes, epoch);
    };
    control.onclose = () => markChannelsReady(epoch);
    control.onerror = () => {
      if (current(epoch)) showToast('远端控制通道出现错误。');
    };
    text.onopen = () => markChannelsReady(epoch);
    text.onmessage = async (event) => {
      if (!current(epoch)) return;
      const bytes = await toBytes(event.data);
      if (bytes && current(epoch)) handleHostControl(bytes, epoch);
    };
    text.onclose = () => markChannelsReady(epoch);
    text.onerror = () => {
      if (current(epoch)) showToast('远端文字通道出现错误。');
    };

    peer.onicecandidate = (event) => handleLocalCandidate(event.candidate, epoch);
    peer.ontrack = (event) => { void acceptTrack(event, epoch); };
    peer.onconnectionstatechange = () => handleConnectionState(epoch);
    peer.oniceconnectionstatechange = () => {
      if (!current(epoch)) return;
      if (peer.iceConnectionState === 'failed') failSession(epoch, '网络协商失败。请检查浏览器与主机之间的网络或服务器的中继设置。');
    };
    return peer;
  }

  async function createOffer(epoch, iceServers) {
    try {
      const peer = makePeer(epoch, iceServers);
      if (!peer) return;
      const offer = await peer.createOffer();
      if (!current(epoch)) return;
      await peer.setLocalDescription(offer);
      if (!current(epoch)) return;
      if (!state.socket || state.socket.readyState !== WebSocket.OPEN) throw new Error('信令连接已关闭。');
      state.socket.send(JSON.stringify({ type: 'offer', sdp: peer.localDescription.sdp }));
      state.offerSent = true;
      const queued = state.localCandidates.splice(0);
      queued.forEach((candidate) => transmitCandidate(candidate, epoch));
      state.answerTimer = window.setTimeout(() => failSession(epoch, '主机在规定时间内没有返回媒体应答。'), 22_000);
      sessionDetail('正在协商媒体通道', '正在与远端主机协商 H.264 视频、Opus 音频和可靠控制通道。');
    } catch (error) {
      if (current(epoch)) failSession(epoch, error.message || '无法创建 WebRTC 媒体请求。');
    }
  }

  async function handleSignalMessage(raw, epoch, iceServers) {
    if (!current(epoch)) return;
    let message;
    try { message = JSON.parse(raw); } catch { failSession(epoch, '收到无法识别的信令消息。'); return; }
    if (!message || typeof message.type !== 'string') return;
    if (message.type === 'error') {
      failSession(epoch, typeof message.message === 'string' ? message.message : '信令服务器结束了会话。');
    } else if (message.type === 'ended') {
      const reason = typeof message.reason === 'string' ? message.reason : '';
      const messageText = reason && reason.length <= 120 ? `会话已结束：${reason}` : '远端主机结束了会话。';
      failSession(epoch, messageText);
    } else if (message.type === 'ready') {
      if (state.signalingReady) return;
      state.signalingReady = true;
      state.pendingSession = null;
      state.authSent = false;
      window.clearTimeout(state.authTimer);
      showStep('signaling', 'done');
      showStep('media', 'active');
      setStatus('negotiating', '正在协商');
      sessionDetail('已连接信令服务器', '正在建立 WebRTC 媒体通道。');
      await createOffer(epoch, iceServers);
    } else if (message.type === 'answer') {
      if (!state.peer || typeof message.sdp !== 'string' || message.sdp.length > 1024 * 1024) {
        failSession(epoch, '主机返回的媒体应答无效。');
        return;
      }
      window.clearTimeout(state.answerTimer);
      state.answerTimer = 0;
      try {
        await state.peer.setRemoteDescription({ type: 'answer', sdp: message.sdp });
        if (!current(epoch)) return;
        state.remoteDescriptionSet = true;
        await flushRemoteCandidates(epoch);
        if (!current(epoch)) return;
        state.frameDeadline = window.setTimeout(() => {
          if (ui.video.dataset.frameReady !== 'true') failSession(epoch, '已完成媒体协商，但在规定时间内没有收到远端视频首帧。');
        }, 30_000);
      } catch {
        failSession(epoch, '无法应用主机的媒体应答。');
      }
    } else if (message.type === 'candidate') {
      await setRemoteCandidate(message.candidate, epoch);
    } else if (message.type === 'capability') {
      // Capability notices are informational; the server has already selected the H.264 host path.
    }
  }

  function orphanSession(session, activeSocket = null, authenticationSent = false) {
    if (!session || typeof session.ticket !== 'string') return;
    let url;
    try { url = getSignalUrl(session.signalingUrl); } catch { return; }
    let socket = activeSocket;
    if (!socket || socket.readyState === WebSocket.CLOSING || socket.readyState === WebSocket.CLOSED) {
      try { socket = new WebSocket(url); } catch { return; }
      authenticationSent = false;
    }
    socket.onopen = () => {
      if (authenticationSent) return;
      authenticationSent = true;
      try { socket.send(JSON.stringify({ type: 'authenticate', ticket: session.ticket })); }
      catch { safeSocketClose(socket); }
    };
    const timer = window.setTimeout(() => safeSocketClose(socket, 1000, 'stale session'), 4500);
    socket.onmessage = (event) => {
      try {
        const message = JSON.parse(event.data);
        if (message.type === 'ready') {
          socket.send(JSON.stringify({ type: 'leave' }));
          safeSocketClose(socket, 1000, 'stale session');
          window.clearTimeout(timer);
        }
      } catch { safeSocketClose(socket); }
    };
    socket.onerror = () => { window.clearTimeout(timer); };
    socket.onclose = () => window.clearTimeout(timer);
    if (socket.readyState === WebSocket.OPEN && !authenticationSent) {
      authenticationSent = true;
      try { socket.send(JSON.stringify({ type: 'authenticate', ticket: session.ticket })); }
      catch { safeSocketClose(socket); }
    }
  }

  async function startSession(device) {
    if (!state.token || !device || !ALLOWED_DEVICE_ID.test(device.id || '')) return;
    stopSession({ sendLeave: true });
    const epoch = state.epoch;
    state.stopped = false;
    state.currentDevice = device;
    state.abort = null;
    ['signaling', 'media', 'frame'].forEach((step) => {
      const node = document.querySelector(`.state-step[data-step="${step}"]`);
      node?.classList.remove('is-active', 'is-done');
    });
    ui.deviceName.textContent = device.name || 'Windows 主机';
    ui.disconnect.disabled = true;
    ui.disconnect.textContent = '断开连接';
    ui.audio.disabled = true;
    setError(ui.sessionError, '');
    setError(ui.devicesError, '');
    setStatus('connecting', '正在连接');
    showStep('signaling', 'active');
    sessionDetail('正在申请会话', '正在向自托管服务器申请一次性信令票据。');
    setScreen('session');
    try {
      const session = await request('/v1/sessions', {
        method: 'POST',
        body: { deviceId: device.id },
      });
      if (!current(epoch)) {
        orphanSession(session);
        return;
      }
      state.pendingSession = session;
      if (!ALLOWED_TOKEN.test(session.ticket || '') || !Array.isArray(session.iceServers)) {
        orphanSession(session);
        state.pendingSession = null;
        throw new Error('服务器返回的会话凭据无效。');
      }
      const signalingUrl = getSignalUrl(session.signalingUrl);
      const socket = new WebSocket(signalingUrl);
      state.socket = socket;
      state.authTimer = window.setTimeout(() => failSession(epoch, '信令连接超时。'), 6500);
      socket.onopen = () => {
        if (!current(epoch)) { safeSocketClose(socket); return; }
        try {
          socket.send(JSON.stringify({ type: 'authenticate', ticket: session.ticket }));
          state.authSent = true;
        }
        catch { failSession(epoch, '无法发送信令认证消息。'); }
      };
      socket.onmessage = (event) => { void handleSignalMessage(event.data, epoch, session.iceServers); };
      socket.onerror = () => {
        if (current(epoch)) failSession(epoch, '无法连接同源信令服务。');
      };
      socket.onclose = () => {
        if (current(epoch) && !state.stopped && state.signalingReady) failSession(epoch, '信令连接已关闭。');
      };
      ui.disconnect.disabled = false;
      sessionDetail('正在连接信令服务', '等待服务器确认一次性会话票据。');
    } catch (error) {
      if (!current(epoch) || error.name === 'AbortError') return;
      failSession(epoch, error.message || '无法开始会话。');
    }
  }

  function sendAudioToggle() {
    if (!state.stream) return;
    ui.video.muted = !ui.video.muted;
    if (!ui.video.muted) {
      const result = ui.video.play();
      if (result && typeof result.catch === 'function') {
        result.catch(() => {
          ui.video.muted = true;
          ui.audio.setAttribute('aria-pressed', 'false');
          ui.audioHint.hidden = false;
          showToast('浏览器阻止了音频播放，请再次点击声音按钮。');
        });
      }
    }
    const enabled = !ui.video.muted;
    ui.audio.setAttribute('aria-pressed', String(enabled));
    setAudioButton(enabled);
    ui.audioHint.hidden = enabled;
  }

  function videoPoint(event) {
    const rect = ui.video.getBoundingClientRect();
    const width = ui.video.videoWidth;
    const height = ui.video.videoHeight;
    if (width < 1 || height < 1 || rect.width < 1 || rect.height < 1) return null;
    const scale = Math.min(rect.width / width, rect.height / height);
    const displayWidth = width * scale;
    const displayHeight = height * scale;
    const left = rect.left + (rect.width - displayWidth) / 2;
    const top = rect.top + (rect.height - displayHeight) / 2;
    const x = event.clientX - left;
    const y = event.clientY - top;
    if (x < 0 || y < 0 || x > displayWidth || y > displayHeight) return null;
    return { x: Math.max(0, Math.min(1, x / displayWidth)), y: Math.max(0, Math.min(1, y / displayHeight)) };
  }

  function sendPointer(event) {
    if (!state.dataReady || ui.video.dataset.frameReady !== 'true') return null;
    const point = videoPoint(event);
    if (!point) return null;
    sendInput({
      action: 'mouse_move_absolute',
      screen_id: state.currentScreenId,
      abs_x: point.x,
      abs_y: point.y,
    });
    return point;
  }

  function virtualKey(event) {
    const code = event.code || '';
    if (/^Key[A-Z]$/u.test(code)) return code.charCodeAt(3);
    if (/^Digit[0-9]$/u.test(code)) return code.charCodeAt(5);
    if (/^Numpad[0-9]$/u.test(code)) return code.charCodeAt(6);
    if (/^F(?:[1-9]|1[0-9]|2[0-4])$/u.test(code)) return 0x70 + Number(code.slice(1)) - 1;
    const keys = {
      Enter: 13, NumpadEnter: 13, Escape: 27, Backspace: 8, Tab: 9, Space: 32,
      Delete: 46, Insert: 45, Home: 36, End: 35, PageUp: 33, PageDown: 34,
      ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40,
      ShiftLeft: 16, ShiftRight: 16, ControlLeft: 17, ControlRight: 17,
      AltLeft: 18, AltRight: 18, MetaLeft: 91, MetaRight: 92,
      CapsLock: 20, NumLock: 144, ScrollLock: 145, Pause: 19, PrintScreen: 44,
      ContextMenu: 93, NumpadAdd: 107, NumpadSubtract: 109, NumpadMultiply: 106,
      NumpadDivide: 111, NumpadDecimal: 110, NumpadEqual: 187, Minus: 189,
      Equal: 187, BracketLeft: 219, BracketRight: 221, Backslash: 220,
      Semicolon: 186, Quote: 222, Backquote: 192, Comma: 188, Period: 190, Slash: 191,
    };
    return keys[code] || 0;
  }

  function isRemoteKeyTarget() {
    return state.dataReady && document.activeElement === ui.video;
  }

  function keyDown(event) {
    if (!isRemoteKeyTarget()) return;
    const key = virtualKey(event);
    if (!key) return;
    event.preventDefault();
    if (state.pressedKeys.has(event.code)) return;
    state.pressedKeys.set(event.code, key);
    sendInput({ action: 'kbd_press', key });
  }

  function keyUp(event) {
    const key = state.pressedKeys.get(event.code);
    if (key === undefined) return;
    state.pressedKeys.delete(event.code);
    sendInput({ action: 'kbd_release', key });
    if (isRemoteKeyTarget()) event.preventDefault();
  }

  function stopOnBlur() {
    releasePressedKeys();
    state.keyboardFocused = false;
  }

  function leavePage() {
    releasePressedKeys();
    if (state.socket && state.signalingReady && state.socket.readyState === WebSocket.OPEN) {
      try { state.socket.send(JSON.stringify({ type: 'leave' })); } catch { /* Closing also releases the host. */ }
    }
    safeSocketClose(state.socket, 1000, 'page hidden');
    if (state.peer) {
      try { state.peer.close(); } catch { /* Already ended. */ }
    }
  }

  ui.pairForm.addEventListener('submit', (event) => { void pair(event); });
  ui.refresh.addEventListener('click', () => { void refreshDevices(); });
  ui.forget.addEventListener('click', () => {
    stopSession({ sendLeave: true });
    forgetToken();
    ui.forget.hidden = true;
    setError(ui.devicesError, '');
    setError(ui.pairError, '');
    setScreen('welcome');
    showToast('已清除此浏览器保存的授权令牌。');
  });
  ui.back.addEventListener('click', () => {
    stopSession({ sendLeave: true });
    setDevicesView();
    void refreshDevices();
  });
  ui.disconnect.addEventListener('click', () => {
    stopSession({ sendLeave: true });
    setDevicesView();
    void refreshDevices();
  });
  ui.audio.addEventListener('click', sendAudioToggle);
  ui.sendText.addEventListener('click', sendText);
  ui.keyboard.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      sendText();
    }
  });
  ui.video.addEventListener('pointerdown', (event) => {
    if (!state.dataReady) return;
    const point = sendPointer(event);
    if (point) {
      state.pendingPointer = true;
      ui.video.focus({ preventScroll: true });
      try { ui.video.setPointerCapture(event.pointerId); } catch { /* Pointer capture is optional. */ }
      event.preventDefault();
    }
  });
  ui.video.addEventListener('pointermove', (event) => {
    if (!state.pendingPointer && event.pointerType !== 'mouse') return;
    const now = performance.now();
    if (now - state.lastPointerAt < 16) return;
    state.lastPointerAt = now;
    sendPointer(event);
  });
  ui.video.addEventListener('pointerup', (event) => {
    if (!state.pendingPointer) return;
    const point = sendPointer(event);
    state.pendingPointer = false;
    if (point) sendInput({ action: 'mouse_click', button: 1 });
  });
  ui.video.addEventListener('pointercancel', () => { state.pendingPointer = false; });
  ui.video.addEventListener('blur', stopOnBlur);
  window.addEventListener('keydown', keyDown, { capture: true });
  window.addEventListener('keyup', keyUp, { capture: true });
  window.addEventListener('blur', stopOnBlur);
  window.addEventListener('pagehide', leavePage);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState !== 'visible') stopOnBlur();
  });

  state.token = loadSavedToken();
  if (state.token) {
    ui.forget.hidden = false;
    setScreen('devices');
    void refreshDevices();
  } else {
    setScreen('welcome');
  }
})();
