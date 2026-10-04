// The player page: one camera, a range picker, the timeline, live mode and the export form.
// A link like `#stream=3&t=1791024466000` opens a stream at a wall-clock time; `&sync=1` turns on
// sync, which plays it in step with the other tabs that have sync on (see sync.js).

const video = $('#video');
const cam = new CamPlayer(video, { onError: message, onUpdate: drawTimeline });

// While live: extends the timeline to now, so the stretch being recorded grows.
let liveClock = null;
let streams = [];
/** Set while a tab opened with sync on is loading, before it joins: it mustn't start on its own. */
let syncPending = false;

const sync = new SyncClock({ probe: (wall) => probeCam(cam, wall), onChange: syncChanged });

function message(text) {
  $('#message').textContent = text || '';
}

// --- timeline ----------------------------------------------------------------------------------

const timeline = new Timeline($('#timeline'), {
  thumbUrl,
  onSeek(ms) {
    if (sync.enabled) {
      sync.seek(ms, true);
      return;
    }
    if (!cam.loaded) return;
    video.currentTime = cam.timeAt(ms);
    video.play().catch(() => {});
  },
  onSelect(from, to) {
    $('#export-from').value = toLocalInput(from, true);
    $('#export-to').value = toLocalInput(to, true);
    drawSelection();
  },
});

function drawTimeline() {
  if (!cam.loaded) return;
  timeline.setData({
    lanes: [{ label: cam.stream.label, segments: cam.segments, recordingFrom: cam.live ? cam.newest : null }],
    from: cam.from,
    to: cam.to,
  });
  drawSelection();
}

/** The export range from the form, shown on the timeline. */
function drawSelection() {
  const from = Date.parse($('#export-from').value);
  const to = Date.parse($('#export-to').value);
  timeline.setSelection(Number.isNaN(from) ? null : from, Number.isNaN(to) ? null : to);
}

$('#zoom-in').addEventListener('click', () => timeline.zoom(1 / 2));
$('#zoom-out').addEventListener('click', () => timeline.zoom(2));
$('#zoom-fit').addEventListener('click', () => timeline.fit());

/** The wall-clock time showing: the shared clock's when in sync, else the video's. */
function nowShowing() {
  return sync.enabled ? sync.wall() : cam.loaded ? cam.wall : null;
}

/** The clock shows the time playing ('time'), or how long it plays until the end ('left'). */
let clockMode = 'time';
try { if (localStorage.getItem('clockMode') === 'left') clockMode = 'left'; } catch { /* unavailable */ }

/** Real seconds until the end of the loaded range, at the current speed; gaps don't count. */
function timeLeft() {
  if (!cam.loaded) return null;
  if (sync.enabled) {
    const wall = sync.wall();
    return wall == null ? null : cam.footageAfter(wall) / sync.wps;
  }
  return Math.max(cam.duration - video.currentTime, 0) / (video.playbackRate || 1);
}

/** "1:02:03 left", "02:03 left". */
function formatLeft(seconds) {
  const s = Math.ceil(seconds);
  const pad = (n) => String(n).padStart(2, '0');
  const h = Math.floor(s / 3600);
  const rest = `${pad(Math.floor((s % 3600) / 60))}:${pad(s % 60)}`;
  return `${h ? `${h}:${rest}` : rest} left`;
}

function titleClock() {
  $('#clock').title = clockMode === 'left' ? 'Click to show the time playing' : 'Click to show how long it plays until the end';
}
titleClock();

$('#clock').addEventListener('click', (event) => {
  clockMode = clockMode === 'left' ? 'time' : 'left';
  try { localStorage.setItem('clockMode', clockMode); } catch { /* unavailable */ }
  titleClock();
  updateClock();
  // Leave space to play and pause, rather than pressing the clock again.
  if (event.detail) event.currentTarget.blur();
});

function updateClock() {
  const wall = nowShowing();
  if (wall == null) return;
  const left = clockMode === 'left' ? timeLeft() : null;
  $('#clock').textContent = left == null ? formatTime(wall) : formatLeft(left);
  timeline.setCursor(wall);
  updateLive(wall);
}

/** In live mode: how far behind real time the picture is, and a way back to the newest footage. */
function updateLive(wall) {
  $('#live').hidden = !cam.live || !cam.loaded;
  if (!cam.live || !cam.loaded) {
    $('#jump-live').hidden = true;
    return;
  }
  const behind = Math.max(Date.now() - wall, 0) / 1000;
  $('#live-lag').textContent = behind < 90 ? `${Math.round(behind)} s behind` : `${formatDuration(behind)} behind`;
  // The newest footage itself trails by up to a segment; only offer a jump when well behind that.
  $('#jump-live').hidden = cam.newest - wall < 2 * MINUTE_MS;
}

$('#jump-live').addEventListener('click', () => {
  if (sync.enabled) {
    sync.seek(cam.wallAt(Math.max(cam.duration - 3, 0)), true);
    return;
  }
  video.currentTime = Math.max(cam.duration - 3, 0);
  video.play().catch(() => {});
});

// --- controls ----------------------------------------------------------------------------------

const controls = setUpControls({
  root: $('#player'),
  playButton: $('#play'),
  fullscreenButton: $('#fullscreen'),
  clickTargets: [video],
  actions: {
    isPlaying: () => (sync.enabled ? sync.playing : !video.paused),
    toggle: () => {
      if (sync.enabled) sync.playing ? sync.pause() : sync.play();
      else video.paused ? video.play().catch(() => {}) : video.pause();
    },
    jump: (seconds) => {
      if (sync.enabled) sync.seek(sync.wall() + seconds * clockSpeed());
      else if (cam.loaded) video.currentTime = Math.min(Math.max(video.currentTime + seconds, 0), cam.duration);
    },
    step: (frames) => {
      if (!cam.loaded) return;
      const seconds = frames / cam.stream.settings.out_fps;
      if (sync.enabled) {
        sync.seek(sync.wall() + seconds * clockSpeed(), false);
        return;
      }
      video.pause();
      video.currentTime = Math.max(video.currentTime + seconds, 0);
    },
  },
});

/** Wall-clock ms per second of this camera's video at the clock, so jumps match the player's. */
function clockSpeed() {
  return cam.loaded ? cam.speedAt(sync.wall()) : sync.wps / sync.rate;
}
video.addEventListener('play', controls.refresh);
video.addEventListener('pause', controls.refresh);

$('#rate').addEventListener('change', (event) => {
  const rate = Number(event.target.value);
  if (sync.enabled) sync.setRate(rate);
  else video.playbackRate = rate;
});
// In sync the video's rate is steered to the clock; the menu shows the clock's.
video.addEventListener('ratechange', () => {
  if (sync.enabled) return;
  $('#rate').value = String(video.playbackRate);
  updateClock();
});
video.addEventListener('timeupdate', () => { if (!sync.enabled) updateClock(); });
video.addEventListener('seeked', () => { if (!sync.enabled) updateClock(); });

// --- sync --------------------------------------------------------------------------------------

function showOverlay(text) {
  $('#overlay').hidden = !text;
  if (text) $('#overlay').textContent = text;
}

/** Each frame in sync: keep the video on the clock and show where the clock is. */
function followFrame() {
  if (sync.enabled) {
    showOverlay(followClock(cam, sync));
    updateClock();
    const others = sync.tabs - 1;
    $('#sync-info').textContent = sync.waiting ? 'waiting for footage…'
      : others ? `in step with ${others} other tab${others === 1 ? '' : 's'}` : 'no other tabs yet';
  }
  requestAnimationFrame(followFrame);
}

function syncChanged() {
  controls.refresh();
  $('#rate').value = String(sync.rate);
}

function setSync(on) {
  if (on === sync.enabled) return;
  syncPending = false;
  if (on) {
    sync.enable({ wall: cam.loaded ? cam.wall : null, playing: cam.loaded && !video.paused, rate: Number($('#rate').value) });
  } else {
    // Carry on from here at the same speed, on its own.
    const { playing, rate } = sync;
    sync.disable();
    showOverlay(null);
    video.playbackRate = rate;
    if (playing && cam.loaded) video.play().catch(() => {});
    else video.pause();
  }
  $('#sync').classList.toggle('active', on);
  $('#sync').setAttribute('aria-pressed', String(on));
  $('#sync-info').hidden = !on;
  saveHash();
  controls.refresh();
}

/**
 * After the user picks a range in sync: live moves everyone to the newest footage; any other range
 * moves the clock to its start, unless the clock is already within it.
 */
function placeClock(live) {
  const wall = sync.wall();
  if (wall == null || !cam.loaded) return;
  if (live) sync.seek(cam.wallAt(Math.max(cam.duration - 5, 0)), true);
  else if (wall < cam.from || wall > cam.newest) sync.seek(cam.from);
}

$('#sync').addEventListener('click', () => setSync(!sync.enabled));

$('#new-tab').addEventListener('click', () => {
  // Open the next camera along that has footage, and put this tab in sync with it.
  const i = streams.indexOf(currentStream());
  const after = [...streams.slice(i + 1), ...streams.slice(0, i + 1)];
  const next = after.find((stream) => stream.segments) ?? after[0];
  window.open(`./#stream=${next?.id ?? ''}&sync=1`, '_blank');
  setSync(true);
});

if (!SyncClock.supported) {
  $('#sync').hidden = true;
  $('#new-tab').hidden = true;
}

// --- loading -----------------------------------------------------------------------------------

/**
 * Load a stream for a window; `startAt` is a wall-clock time to begin at. `chosen`: the user picked
 * this range, so in sync it may move the clock.
 */
async function loadStream(stream, { from = null, to = null, live = false, startAt = null } = {}, chosen = false) {
  clearInterval(liveClock);
  message('');
  const found = await cam.load(stream, { from, to, live, startAt });
  if (!found) {
    $('#clock').textContent = '–';
    timeline.setData({ lanes: [], from: from ?? Date.now() - 60 * MINUTE_MS, to: to ?? Date.now() });
    updateLive(0);
    message(live ? 'Nothing recorded yet; a segment appears once it is finished.' : 'No footage in this range.');
    return;
  }
  drawTimeline();
  if (sync.enabled) {
    if (chosen) placeClock(live);
  } else if (!syncPending) {
    video.play().catch(() => {});
  }
  if (live) {
    liveClock = setInterval(() => {
      cam.to = Date.now();
      drawTimeline();
      updateClock();
    }, 5000);
  }
}

function currentStream() {
  return streams.find((stream) => String(stream.id) === $('#stream').value);
}

function describe(stream) {
  const parts = [statusOf(stream)];
  if (stream.segments) {
    parts.push(`${stream.segments} segments, ${formatBytes(stream.bytes)}`);
    parts.push(`${formatTime(stream.first_wall)} – ${formatTime(stream.last_wall)}`);
  }
  const speedup = stream.settings.out_fps / stream.settings.sample_fps;
  parts.push(`${+speedup.toFixed(2)}× speed`);
  return parts.join(' · ');
}

function setActive(button) {
  document.querySelectorAll('.ranges button').forEach((b) => b.classList.toggle('active', b === button));
}

/** Value for a datetime-local input, to the minute or (`seconds`) to the second. */
function toLocalInput(ms, seconds = false) {
  const date = new Date(ms - new Date(ms).getTimezoneOffset() * 60_000);
  return date.toISOString().slice(0, seconds ? 19 : 16);
}

async function showRange(button, { startAt = null, chosen = true } = {}) {
  const stream = currentStream();
  if (!stream) return;
  setActive(button);
  const range = button.dataset.range;
  try {
    if (range === 'all') await loadStream(stream, { startAt }, chosen);
    else if (range === 'live') await loadStream(stream, { from: Date.now() - 60 * MINUTE_MS, live: true }, chosen);
    else await loadStream(stream, { from: Date.now() - Number(range), to: Date.now(), startAt }, chosen);
    if (cam.loaded) {
      $('#from').value = toLocalInput(cam.from);
      $('#to').value = toLocalInput(cam.to);
    }
  } catch (error) {
    message(String(error));
  }
}

document.querySelectorAll('.ranges button[data-range]').forEach((button) => {
  button.addEventListener('click', () => showRange(button));
});

$('#load-custom').addEventListener('click', async () => {
  const from = Date.parse($('#from').value);
  const to = Date.parse($('#to').value);
  if (Number.isNaN(from) || Number.isNaN(to) || to <= from) {
    message('Pick a start before the end.');
    return;
  }
  setActive($('#load-custom'));
  try {
    await loadStream(currentStream(), { from, to }, true);
  } catch (error) {
    message(String(error));
  }
});

/** The link to this tab: its stream, and whether it's in sync. */
function saveHash() {
  history.replaceState(null, '', `#stream=${$('#stream').value}${sync.enabled ? '&sync=1' : ''}`);
}

$('#stream').addEventListener('change', () => {
  saveHash();
  $('#stream-info').textContent = describe(currentStream());
  showRange(document.querySelector('.ranges button[data-range="all"]'));
});

async function init() {
  try {
    streams = await api('api/streams');
  } catch (error) {
    message(String(error));
    return;
  }
  if (!streams.length) {
    message('No streams yet. Add one on the Streams page.');
    return;
  }
  const select = $('#stream');
  for (const stream of streams) select.append(new Option(stream.label, stream.id));
  // The stream (and time) named in the link, else the first stream with something to show.
  const params = new URLSearchParams(location.hash.slice(1));
  const initial = streams.find((stream) => String(stream.id) === params.get('stream'))
    ?? streams.find((stream) => stream.segments) ?? streams[0];
  select.value = String(initial.id);
  $('#stream-info').textContent = describe(initial);
  const startAt = Number(params.get('t')) || null;
  syncPending = params.get('sync') === '1' && SyncClock.supported;
  await showRange(document.querySelector('.ranges button[data-range="all"]'), { startAt, chosen: false });
  if (syncPending) setSync(true);
}

/** Keep the status line current; stream list changes are picked up on the next page load. */
function onEvent(event) {
  if (event.type !== 'status') return;
  const stream = streams.find((s) => s.id === event.stream_id);
  if (!stream) return;
  Object.assign(stream, { status: event.status, status_detail: event.detail });
  if (stream === currentStream()) $('#stream-info').textContent = describe(stream);
}

// --- export ------------------------------------------------------------------------------------

function markExport(input) {
  const wall = nowShowing();
  if (wall == null) return;
  input.value = toLocalInput(wall, true);
  drawSelection();
}

$('#mark-from').addEventListener('click', () => markExport($('#export-from')));
$('#mark-to').addEventListener('click', () => markExport($('#export-to')));
$('#export-from').addEventListener('input', drawSelection);
$('#export-to').addEventListener('input', drawSelection);

$('#export').addEventListener('click', async () => {
  const note = $('#export-message');
  const stream = currentStream();
  const from = Date.parse($('#export-from').value);
  const to = Date.parse($('#export-to').value);
  if (!stream || Number.isNaN(from) || Number.isNaN(to)) {
    note.textContent = 'Pick a start and an end first: shift-drag on the timeline, or use the buttons.';
    return;
  }
  const mode = document.querySelector('input[name="export-mode"]:checked').value;
  $('#export').disabled = true;
  try {
    const job = await api('api/exports', { method: 'POST', body: { stream_id: stream.id, from, to, mode } });
    note.innerHTML = '';
    note.append('Queued. ');
    const link = document.createElement('a');
    link.href = `exports.html#export=${job.id}`;
    link.textContent = 'Follow it on the Exports page';
    note.append(link);
  } catch (error) {
    note.textContent = error.message;
  } finally {
    $('#export').disabled = false;
  }
});

setUpNav();
init();
subscribe(onEvent);
requestAnimationFrame(followFrame);
