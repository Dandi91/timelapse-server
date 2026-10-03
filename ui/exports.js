// The exports page: jobs with live progress, download and delete.

let jobs = [];

function message(text) {
  $('#message').textContent = text || '';
}

async function load() {
  try {
    jobs = await api('api/exports');
    render();
    message('');
  } catch (error) {
    message(error.message);
  }
}

function render() {
  $('#exports tbody').replaceChildren(...jobs.map(row));
  $('#empty').hidden = jobs.length > 0;
  // Coming from the player's link: point at the new job.
  const wanted = new URLSearchParams(location.hash.slice(1)).get('export');
  $(`#exports tr[data-id="${wanted}"]`)?.classList.add('highlight');
}

function td(content, className) {
  const cell = document.createElement('td');
  if (className) cell.className = className;
  if (content instanceof Node) cell.append(content);
  else cell.textContent = content ?? '';
  return cell;
}

function rangeText(from, to) {
  const sameDay = new Date(from).toDateString() === new Date(to).toDateString();
  const end = sameDay
    ? new Date(to).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' })
    : formatTime(to);
  return `${formatTime(from)} – ${end}`;
}

function row(job) {
  const tr = document.createElement('tr');
  tr.dataset.id = job.id;
  tr.append(td(job.stream_label));

  const range = document.createElement('div');
  range.textContent = rangeText(job.actual_from_ms ?? job.from_ms, job.actual_to_ms ?? job.to_ms);
  if (job.actual_from_ms != null && job.actual_from_ms < job.from_ms) {
    const note = document.createElement('div');
    note.className = 'muted small';
    note.textContent = `starts ${Math.round((job.from_ms - job.actual_from_ms) / 1000)} s early, at a keyframe`;
    range.append(note);
  }
  tr.append(td(range, 'small'));

  const mode = job.used_mode && job.used_mode !== job.mode ? `${job.used_mode} (asked ${job.mode})` : job.mode;
  tr.append(td(mode, 'small'));
  tr.append(stateCell(job));

  const clip = job.state === 'done' ? `${clipLength(job.duration)} · ${formatBytes(job.bytes)}` : '';
  tr.append(td(clip, 'small'));

  const actions = document.createElement('div');
  actions.className = 'row-actions';
  if (job.state === 'done') {
    const link = document.createElement('a');
    link.href = `api/exports/${job.id}/file`;
    link.className = 'button';
    link.textContent = 'Download';
    actions.append(link);
  }
  const busy = job.state === 'queued' || job.state === 'running';
  const remove = document.createElement('button');
  remove.type = 'button';
  remove.className = 'danger';
  remove.textContent = busy ? 'Cancel' : 'Delete';
  remove.addEventListener('click', () => removeJob(job, busy));
  actions.append(remove);
  tr.append(td(actions));
  return tr;
}

/** "54.1 s", "4m 05s", "1h 20m". */
function clipLength(seconds) {
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${String(Math.round(seconds % 60)).padStart(2, '0')}s`;
  return formatDuration(seconds);
}

function stateCell(job) {
  const cell = document.createElement('td');
  cell.className = 'state-cell';
  const badge = document.createElement('span');
  badge.className = `badge ${job.state}`;
  badge.textContent = job.state;
  cell.append(badge);
  if (job.state === 'running') {
    const bar = document.createElement('progress');
    bar.max = 1;
    bar.value = job.progress;
    bar.title = `${Math.round(job.progress * 100)} %`;
    cell.append(bar);
  }
  if (job.state === 'failed' && job.error) {
    const error = document.createElement('div');
    error.className = 'error small';
    error.textContent = job.error;
    cell.append(error);
  }
  return cell;
}

async function removeJob(job, busy) {
  const what = busy ? 'Cancel this export?' : `Delete this export${job.bytes ? ` (${formatBytes(job.bytes)})` : ''}?`;
  if (!confirm(what)) return;
  try {
    await api(`api/exports/${job.id}`, { method: 'DELETE' });
    jobs = jobs.filter((j) => j.id !== job.id);
    render();
  } catch (error) {
    message(error.message);
  }
}

const refresh = debounce(load, 200);

function onEvent(event) {
  if (event.type === 'export_updated') {
    const job = jobs.find((j) => j.id === event.id);
    if (!job) return refresh();
    Object.assign(job, { state: event.state, progress: event.progress });
    $(`#exports tr[data-id="${job.id}"] .state-cell`)?.replaceWith(stateCell(job));
  } else if (event.type === 'exports_changed' || event.type === 'resync') {
    refresh();
  }
}

setUpNav();
load();
subscribe(onEvent);
