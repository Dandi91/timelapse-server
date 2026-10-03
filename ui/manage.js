// The streams page: list with live status, add/edit/delete, logs, and the system panel.

const DEFAULTS = {
  sample_fps: 5, source_fps: 30, out_fps: 30, height: 1080, crf: 21, preset: 'veryfast', segment_minutes: 10,
};
const SETTING_FIELDS = Object.keys(DEFAULTS);

let streams = [];
let editing = null; // the stream being edited, or null when adding

function message(text) {
  $('#message').textContent = text || '';
}

// --- list --------------------------------------------------------------------------------------

async function loadStreams() {
  try {
    streams = await api('api/streams');
    render();
    message('');
  } catch (error) {
    message(error.message);
  }
}

function render() {
  const body = $('#streams tbody');
  body.replaceChildren(...streams.map(row));
  $('#empty').hidden = streams.length > 0;
}

function cell(content, className) {
  const td = document.createElement('td');
  if (className) td.className = className;
  if (content instanceof Node) td.append(content);
  else td.textContent = content ?? '';
  return td;
}

function button(text, onClick, className) {
  const b = document.createElement('button');
  b.type = 'button';
  b.textContent = text;
  if (className) b.className = className;
  b.addEventListener('click', onClick);
  return b;
}

function row(stream) {
  const tr = document.createElement('tr');
  tr.dataset.id = stream.id;

  const toggle = document.createElement('input');
  toggle.type = 'checkbox';
  toggle.checked = stream.enabled;
  toggle.title = stream.enabled ? 'Recording enabled' : 'Recording disabled';
  toggle.addEventListener('change', () => patch(stream, { enabled: toggle.checked }));
  tr.append(cell(toggle));

  const name = document.createElement('div');
  const link = document.createElement('a');
  link.href = `./#stream=${stream.id}`;
  link.textContent = stream.label;
  const url = document.createElement('div');
  url.className = 'muted small ellipsis';
  url.textContent = stream.url;
  url.title = stream.url;
  name.append(link, url);
  tr.append(cell(name));

  tr.append(statusCell(stream));

  const s = stream.settings;
  const speedup = s.out_fps / s.sample_fps;
  tr.append(cell(`${s.height}p · ${+speedup.toFixed(2)}× · crf ${s.crf}`, 'small'));

  const limit = (value, format) => (value == null ? '' : ` / ${format(value)}`);
  tr.append(cell(`${formatBytes(stream.bytes)}${limit(stream.max_bytes, formatBytes)}`, 'small'));
  const recorded = stream.recorded_ms / 1000;
  tr.append(cell(`${formatDuration(recorded)}${limit(stream.max_duration_secs, formatDuration)}`, 'small'));

  const actions = document.createElement('div');
  actions.className = 'row-actions';
  actions.append(
    button('Edit', () => openEditor(stream)),
    button('Log', () => openLog(stream)),
    button('Restart', () => restart(stream)),
    button('Delete', () => remove(stream), 'danger'),
  );
  tr.append(cell(actions));
  return tr;
}

function statusCell(stream) {
  const td = document.createElement('td');
  td.className = 'status-cell';
  const status = statusOf(stream);
  const badge = document.createElement('span');
  badge.className = `badge ${status}`;
  badge.textContent = status;
  td.append(badge);
  if (stream.enabled && stream.status_detail) {
    const detail = document.createElement('div');
    detail.className = 'muted small ellipsis';
    detail.textContent = stream.status_detail;
    detail.title = stream.status_detail;
    td.append(detail);
  }
  return td;
}

/** Status events update one row in place; anything else refetches the list. */
const refresh = debounce(loadStreams, 300);
const refreshSystemSoon = debounce(() => refreshSystem(), 2000);

function onEvent(event) {
  if (event.type === 'status') {
    const stream = streams.find((s) => s.id === event.stream_id);
    if (!stream) return refresh();
    Object.assign(stream, { status: event.status, status_detail: event.detail, status_at: event.at });
    const tr = $(`#streams tr[data-id="${stream.id}"]`);
    tr?.querySelector('.status-cell')?.replaceWith(statusCell(stream));
  } else {
    refresh();
    refreshSystemSoon();
  }
}

// --- actions -----------------------------------------------------------------------------------

async function patch(stream, changes) {
  try {
    const updated = await api(`api/streams/${stream.id}`, { method: 'PATCH', body: changes });
    Object.assign(stream, updated);
    render();
  } catch (error) {
    message(error.message);
    loadStreams();
  }
}

async function restart(stream) {
  try {
    await api(`api/streams/${stream.id}/restart`, { method: 'POST' });
  } catch (error) {
    message(error.message);
  }
}

async function remove(stream) {
  const size = stream.bytes ? ` and its ${formatBytes(stream.bytes)} of recordings` : '';
  if (!confirm(`Delete "${stream.label}"${size}? This can't be undone.`)) return;
  try {
    await api(`api/streams/${stream.id}`, { method: 'DELETE' });
    streams = streams.filter((s) => s.id !== stream.id);
    render();
  } catch (error) {
    message(error.message);
  }
}

// --- editor ------------------------------------------------------------------------------------

const form = $('#stream-form');

function openEditor(stream) {
  editing = stream;
  $('#editor-title').textContent = stream ? `Edit ${stream.label}` : 'Add stream';
  $('#form-error').textContent = '';
  const settings = stream ? stream.settings : DEFAULTS;
  form.url.value = stream?.url ?? '';
  form.label.value = stream?.label ?? '';
  for (const field of SETTING_FIELDS) form[field].value = settings[field];
  form.max_size.value = sizeInput(stream?.max_bytes);
  form.max_duration.value = durationInput(stream?.max_duration_secs);
  form.live_only.checked = stream?.live_only ?? true;
  form.enabled.checked = stream?.enabled ?? true;
  updateSpeedup();
  $('#editor').showModal();
}

function updateSpeedup() {
  const sample = Number(form.sample_fps.value);
  const out = Number(form.out_fps.value);
  const minutes = Number(form.segment_minutes.value);
  if (!(sample > 0 && out > 0)) {
    $('#speedup').textContent = '';
    return;
  }
  const speedup = out / sample;
  const hour = 3600 / speedup;
  const plays = hour < 60 ? `${Math.round(hour)}s` : formatDuration(hour);
  $('#speedup').textContent = `${+speedup.toFixed(2)}× speed: an hour of stream plays in ${plays}` +
    (minutes > 0 ? `; each segment holds ${minutes} min of stream.` : '.');
}

form.addEventListener('input', updateSpeedup);
$('#cancel').addEventListener('click', () => $('#editor').close());

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  $('#form-error').textContent = '';
  let body;
  try {
    const settings = {};
    for (const field of SETTING_FIELDS) {
      settings[field] = field === 'preset' ? form[field].value : Number(form[field].value);
    }
    body = {
      url: form.url.value.trim(),
      label: form.label.value.trim(),
      settings,
      max_bytes: parseSize(form.max_size.value),
      max_duration_secs: parseDuration(form.max_duration.value),
      live_only: form.live_only.checked,
      enabled: form.enabled.checked,
    };
  } catch (error) {
    $('#form-error').textContent = error.message;
    return;
  }
  $('#save').disabled = true;
  try {
    if (editing) {
      await api(`api/streams/${editing.id}`, { method: 'PATCH', body });
    } else {
      await api('api/streams', { method: 'POST', body });
    }
    $('#editor').close();
    loadStreams();
  } catch (error) {
    $('#form-error').textContent = error.message;
  } finally {
    $('#save').disabled = false;
  }
});

// --- log -----------------------------------------------------------------------------------------

let logStream = null;

async function openLog(stream) {
  logStream = stream;
  $('#log-title').textContent = `${stream.label}: capture log`;
  $('#log').textContent = 'Loading…';
  $('#log-dialog').showModal();
  await loadLog();
}

async function loadLog() {
  try {
    const text = await api(`api/streams/${logStream.id}/log?lines=300`);
    $('#log').textContent = text || '(empty)';
    $('#log').scrollTop = $('#log').scrollHeight;
  } catch (error) {
    $('#log').textContent = error.message;
  }
}

$('#log-refresh').addEventListener('click', loadLog);
$('#log-close').addEventListener('click', () => $('#log-dialog').close());

// --- system ------------------------------------------------------------------------------------

async function refreshSystem() {
  let system;
  try {
    system = await api('api/system');
  } catch {
    return;
  }
  const total = system.disk_total_bytes || 1;
  const used = total - system.disk_free_bytes;
  const recordings = Math.min(system.recordings_bytes, used);
  $('#disk-bar .recordings').style.width = `${(recordings / total) * 100}%`;
  $('#disk-bar .other').style.width = `${((used - recordings) / total) * 100}%`;
  const low = system.disk_free_bytes < system.min_free_bytes;
  $('#disk-text').textContent =
    `Recordings ${formatBytes(system.recordings_bytes)} · exports ${formatBytes(system.exports_bytes)} · ` +
    `${formatBytes(system.disk_free_bytes)} free of ${formatBytes(total)}` +
    (low ? ` · below the ${formatBytes(system.min_free_bytes)} minimum: oldest footage is being pruned` : '');
  $('#disk-text').classList.toggle('error', low);
  const v = system.versions;
  $('#versions').textContent = `yt-dlp ${v.yt_dlp ?? '?'} · ${(v.ffmpeg ?? 'ffmpeg ?').replace(/ Copyright.*/, '')}`;
}

$('#update-yt-dlp').addEventListener('click', async (event) => {
  const output = $('#update-output');
  event.target.disabled = true;
  output.hidden = false;
  output.textContent = 'Updating yt-dlp…';
  try {
    const result = await api('api/system/update-yt-dlp', { method: 'POST' });
    output.textContent = result.output +
      (result.ok ? '\n\nRunning recorders keep the old version until restarted.' : '');
  } catch (error) {
    output.textContent = error.message;
  } finally {
    event.target.disabled = false;
    refreshSystem();
  }
});

// --- start ---------------------------------------------------------------------------------------

$('#add').addEventListener('click', () => openEditor(null));
setUpNav();
loadStreams();
refreshSystem();
setInterval(refreshSystem, 60_000);
subscribe((event) => (event.type === 'resync' ? (loadStreams(), refreshSystem()) : onEvent(event)));
