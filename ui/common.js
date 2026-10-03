// Shared by the pages: API calls, formatting, the nav bar, and the live event stream.

const $ = (selector, root = document) => root.querySelector(selector);
const MINUTE_MS = 60_000;

class ApiError extends Error {}

/** Call the API. JSON bodies in and out; a lost session sends the browser to the login page. */
async function api(path, { method = 'GET', body } = {}) {
  const response = await fetch(path, {
    method,
    headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (response.status === 401 && !path.startsWith('api/login')) {
    location.href = 'login.html';
    throw new ApiError('Not logged in');
  }
  const type = response.headers.get('content-type') || '';
  const data = type.includes('json') ? await response.json() : await response.text();
  if (!response.ok) throw new ApiError(data?.error || `${response.status} ${response.statusText}`);
  return data;
}

function formatTime(ms) {
  return new Date(ms).toLocaleString(undefined, {
    weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit', second: '2-digit',
  });
}

function formatBytes(bytes) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (bytes >= 1024 && i < units.length - 1) { bytes /= 1024; i++; }
  return `${bytes.toFixed(i ? 1 : 0)} ${units[i]}`;
}

function formatDuration(seconds) {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  return `${m}m`;
}

/** "5G", "500M", "1.5T" -> bytes; empty -> null. */
function parseSize(text) {
  text = text.trim();
  if (!text) return null;
  const match = /^(\d+(?:\.\d+)?)\s*([kmgt]?)i?b?$/i.exec(text);
  if (!match) throw new ApiError(`Can't read size "${text}"; try 500M or 50G`);
  const power = ' kmgt'.indexOf(match[2].toLowerCase() || ' ');
  return Math.round(Number(match[1]) * 1024 ** power);
}

/** "36h", "7d", "90m" -> seconds; empty -> null. */
function parseDuration(text) {
  text = text.trim();
  if (!text) return null;
  const match = /^(\d+(?:\.\d+)?)\s*([smhdw]?)$/i.exec(text);
  if (!match) throw new ApiError(`Can't read duration "${text}"; try 36h or 7d`);
  const factor = { '': 1, s: 1, m: 60, h: 3600, d: 86400, w: 604800 }[match[2].toLowerCase()];
  return Math.round(Number(match[1]) * factor);
}

/** Inverse of parseSize, for filling in a form: whole units where possible. */
function sizeInput(bytes) {
  if (bytes == null) return '';
  for (const [unit, size] of [['T', 1024 ** 4], ['G', 1024 ** 3], ['M', 1024 ** 2], ['K', 1024]]) {
    if (bytes >= size) return `${+(bytes / size).toFixed(2)}${unit}`;
  }
  return String(bytes);
}

function durationInput(seconds) {
  if (seconds == null) return '';
  for (const [unit, size] of [['d', 86400], ['h', 3600], ['m', 60]]) {
    if (seconds % size === 0) return `${seconds / size}${unit}`;
  }
  return `${seconds}s`;
}

/** Status word to show for a stream. */
function statusOf(stream) {
  return stream.enabled ? stream.status : 'disabled';
}

/** Show the logout button only when a password is set. */
async function setUpNav() {
  const state = await api('api/auth').catch(() => null);
  const logout = $('#logout');
  if (!logout) return;
  logout.hidden = !state?.required;
  logout.addEventListener('click', async () => {
    await api('api/logout', { method: 'POST' }).catch(() => {});
    location.href = 'login.html';
  });
}

/**
 * Listen to server events. `onEvent` gets each one; after a reconnect or `resync` it gets
 * `{type: 'resync'}`, meaning "refetch everything".
 */
function subscribe(onEvent) {
  let source;
  let wasOpen = false;
  const connect = () => {
    source = new EventSource('api/events');
    source.onopen = () => {
      if (wasOpen) onEvent({ type: 'resync' });
      wasOpen = true;
    };
    source.onmessage = (message) => {
      try { onEvent(JSON.parse(message.data)); } catch { /* ignore malformed */ }
    };
    source.onerror = () => {
      // EventSource retries by itself while the server is reachable; if it gave up, retry later.
      if (source.readyState === EventSource.CLOSED) setTimeout(connect, 5000);
    };
  };
  connect();
}

/** Run `fn` once, `ms` after the last call, however many calls arrive meanwhile. */
function debounce(fn, ms) {
  let timer;
  return (...args) => { clearTimeout(timer); timer = setTimeout(() => fn(...args), ms); };
}
