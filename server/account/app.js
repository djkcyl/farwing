const byId = (id) => document.getElementById(id);
const apiRoot = '/v1/account';
const statusLine = byId('status');
const deviceCode = byId('device-code');
const codeInstructions = byId('code-instructions');
let busy = false;
let scanId = new URLSearchParams(location.search).get('qr');
const scanPanel = byId('qr-login');
const scanRequests = (action) => api('qr', { id: scanId, action });

async function api(path, body) {
  const options = body === undefined ? {} : {
    method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body),
  };
  const response = await fetch(`${apiRoot}/${path}`, options);
  const payload = await response.json();
  if (!response.ok) throw new Error(payload.error || '请求失败');
  return payload;
}

async function load() {
  const { username, devices, passkey, deviceLogin } = await api('status');
  byId('signed-in').hidden = !username;
  byId('signed-out').hidden = Boolean(username);
  if (!username) {
    deviceCode.hidden = true;
    byId('code').value = '';
  }
  byId('account-name').textContent = username ?? '';
  byId('password').value = '';
  byId('no-devices').hidden = devices.length > 0;
  byId('devices').replaceChildren(...devices.map((device) => {
    const item = document.createElement('li');
    item.textContent = `${device.name} · ${device.platform === 'ios' ? 'iOS' : device.platform === 'windows' ? 'Windows' : 'Web'} · ${device.device_id}`;
    if (device.platform === 'ios') {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'secondary';
      button.textContent = '生成迁移码';
      button.addEventListener('click', () => run(() => showCode('device-transfer-code', { device_id: device.device_id },
        '请在 5 分钟内将迁移码粘贴到重签版 App 的「自建账号 → 迁移原设备」。')));
      item.append(' ', button);
    }
    return item;
  }));
  const supported = passkey && globalThis.SimpleWebAuthnBrowser?.browserSupportsWebAuthn();
  for (const button of document.querySelectorAll('[data-passkey]')) button.hidden = !supported;
  if (deviceLogin) {
    for (const id of ['register', 'passkey-register', 'new-code']) byId(id).hidden = true;
    for (const button of byId('devices').querySelectorAll('button')) button.hidden = true;
  }
  if (username && scanId && deviceLogin) {
    const entry = await scanRequests('scan');
    byId('qr-device').textContent = `${entry.name} · ${entry.device_id}`;
    scanPanel.hidden = false;
  } else {
    scanPanel.hidden = true;
  }
  byId('passkey-info').textContent = passkey ? '当前浏览器不支持 Passkey。' : 'Passkey 需要受信任的 HTTPS 域名。';
  byId('passkey-info').hidden = Boolean(supported);
}

async function run(action) {
  if (busy) return;
  busy = true;
  statusLine.textContent = '';
  const buttons = [...document.querySelectorAll('button')];
  for (const button of buttons) button.disabled = true;
  try {
    await action();
  } catch (error) {
    statusLine.textContent = error.name === 'NotAllowedError' ? 'Passkey 操作已取消或超时。' : error.message;
  } finally {
    busy = false;
    for (const button of document.querySelectorAll('button')) button.disabled = false;
  }
}

const username = () => byId('username').value;
const password = () => byId('password').value;
const bind = (id, action) => byId(id).addEventListener('click', () => run(action));

async function passkey(path, registering, data = {}) {
  const { options, requestId } = await api(`passkey/${path}/options`, data);
  const browser = globalThis.SimpleWebAuthnBrowser;
  const credential = await (registering ? browser.startRegistration({ optionsJSON: options })
    : browser.startAuthentication({ optionsJSON: options }));
  await api(`passkey/${path}/verify`, { requestId, credential });
  await load();
}

byId('identity').addEventListener('submit', (event) => {
  event.preventDefault();
  run(async () => {
    await api('login', { username: username(), password: password() });
    await load();
  });
});
bind('register', async () => {
  await api('register', { username: username(), password: password() });
  await load();
});
bind('passkey-register', () => passkey('register', true, { username: username() }));
bind('passkey-login', () => passkey('login', false));
bind('passkey-add', async () => {
  await passkey('registration', true);
  statusLine.textContent = 'Passkey 已添加。';
});
bind('logout', async () => {
  await api('logout', {});
  await load();
});
async function showCode(endpoint, body, instructions) {
  const { code } = await api(endpoint, body);
  codeInstructions.textContent = instructions;
  byId('code').value = code;
  deviceCode.hidden = false;
}
for (const action of ['confirm', 'cancel']) {
  bind('qr-' + action, async () => {
    const result = await scanRequests(action);
    scanPanel.hidden = true;
    history.replaceState(null, '', location.pathname);
    scanId = null;
    statusLine.textContent = action === 'cancel' ? '已取消登录。'
      : result.login_status === 4 ? '已确认，请返回设备。' : '已确认，请在部署机后台批准设备。';
  });
}

bind('new-code', () => showCode('device-code', {}, '请在 5 分钟内将关联码粘贴到 App 的自建账号设置。'));
bind('copy-code', async () => {
  await navigator.clipboard.writeText(byId('code').value);
  statusLine.textContent = '已复制。';
});

run(load);
window.addEventListener('pageshow', (event) => { if (event.persisted) run(load); });
