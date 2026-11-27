const byId = (id) => document.getElementById(id);
const message = byId('message');
const confirmation = byId('confirmation');
const date = new Intl.DateTimeFormat('zh-CN', { dateStyle: 'medium', timeStyle: 'short' });
const platformNames = { windows: 'Windows', ios: 'iOS / iPadOS', web: '网页' };
let busy = false;
let deviceToRevoke;
let displayedState;

async function request(path, id) {
  const options = id === undefined ? {} : {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ id }),
  };
  const response = await fetch(path, options);
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || '请求失败。');
  return result;
}

function button(label, className, action) {
  const element = document.createElement('button');
  element.type = 'button';
  element.className = className;
  element.textContent = label;
  element.addEventListener('click', action);
  return element;
}

function list(target, entries, empty, render) {
  const element = document.createElement(entries.length ? 'ul' : 'p');
  element.className = entries.length ? 'device-list' : 'muted';
  if (entries.length) element.append(...entries.map(render));
  else element.textContent = empty;
  byId(target).replaceChildren(element);
}

function row(entry, status, buttons) {
  const item = document.createElement('li');
  const summary = document.createElement('div');
  const title = document.createElement(entry.name ? 'strong' : 'code');
  title.textContent = entry.name ?? entry.id;
  const details = document.createElement('p');
  details.textContent = [entry.name && entry.id, platformNames[entry.platform], status].filter(Boolean).join(' · ');
  summary.append(title, details);
  const actions = document.createElement('div');
  actions.className = 'actions';
  actions.append(...buttons);
  item.append(summary, actions);
  return item;
}

function mutate(action, id, success) {
  return run(async () => {
    await request('/api/devices/' + action, id);
    await load();
    message.textContent = success;
  });
}

async function load() {
  const state = await request('/api/devices');
  const snapshot = JSON.stringify(state);
  if (snapshot === displayedState) return;
  const { requests, grants, devices } = state;
  const remove = (id) => button('删除', 'secondary', () => mutate('delete', id, '已删除准入记录。'));
  list('requests', requests, '暂无设备申请。', (entry) => row(entry,
    [entry.approvedAt === null ? '待批准' : '已批准，等待设备重连', date.format(entry.requestedAt)].join(' · '),
    [...(entry.approvedAt === null
      ? [button('批准', '', () => mutate('approve', entry.id, '已批准，设备重连后即可加入。'))] : []), remove(entry.id)]));
  list('grants', grants, '暂无预准入设备。', (entry) =>
    row(entry, '有效期至 ' + date.format(entry.expiresAt), [remove(entry.id)]));
  list('devices', devices, '暂无已准入设备。', (entry) => row(entry, null,
    [button('撤销准入', 'secondary', () => {
      deviceToRevoke = entry.id;
      byId('confirm-message').textContent = '撤销 ' + entry.name + '（' + entry.id + '）的准入，并结束其远控会话。';
      confirmation.showModal();
    })]));
  displayedState = snapshot;
}

async function run(action) {
  if (busy) return;
  busy = true;
  delete message.dataset.error;
  message.textContent = '';
  for (const element of document.querySelectorAll('main button')) element.disabled = true;
  try {
    await action();
  } catch (error) {
    message.dataset.error = '';
    message.textContent = error.message;
  } finally {
    busy = false;
    for (const element of document.querySelectorAll('main button')) element.disabled = false;
  }
}

byId('refresh').addEventListener('click', () => run(load));
byId('add').addEventListener('submit', (event) => {
  event.preventDefault();
  const field = byId('device-id');
  run(async () => {
    await request('/api/devices/preapprove', field.value.trim().toLowerCase());
    field.value = '';
    await load();
    message.textContent = '已新增准入。';
  });
});
byId('cancel').addEventListener('click', () => confirmation.close());
byId('confirm').addEventListener('click', () => {
  confirmation.close();
  mutate('revoke', deviceToRevoke, '已撤销设备准入。');
});
run(load);

setInterval(async () => {
  if (busy || document.hidden || confirmation.open) return;
  busy = true;
  try {
    await load();
  } catch (error) {
    message.dataset.error = '';
    message.textContent = error.message;
  } finally {
    busy = false;
  }
}, 5_000);
