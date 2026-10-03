// The player page: one camera, a range picker, the timeline, live mode and the export form.
// A link like `#stream=3&t=1791024466000` opens a stream at a wall-clock time.

const video = $('#video');
const cam = new CamPlayer(video, { onError: message, onUpdate: drawTimeline });

// While live: extends the timeline to now, so the stretch being recorded grows.
let liveClock = null;
let streams = [];

function message(text) {
  $('#message').textContent = text || '';
}

// --- timeline ----------------------------------------------------------------------------------

const timeline = new Timeline($('#timeline'), {
  thumbUrl,
  onSeek(ms) {
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

function updateClock() {
  if (!cam.loaded) return;
  const wall = cam.wall;
  $('#clock').textContent = formatTime(wall);
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
    isPlaying: () => !video.paused,
    toggle: () => (video.paused ? video.play().catch(() => {}) : video.pause()),
    jump: (seconds) => {
      if (cam.loaded) video.currentTime = Math.min(Math.max(video.currentTime + seconds, 0), cam.duration);
    },
    step: (frames) => {
      if (!cam.loaded) return;
      video.pause();
      video.currentTime = Math.max(video.currentTime + frames / cam.stream.settings.out_fps, 0);
    },
  },
});
video.addEventListener('play', controls.refresh);
video.addEventListener('pause', controls.refresh);

$('#rate').addEventListener('change', (event) => { video.playbackRate = Number(event.target.value); });
video.addEventListener('ratechange', () => { $('#rate').value = String(video.playbackRate); });
video.addEventListener('timeupdate', updateClock);
video.addEventListener('seeked', updateClock);

// --- loading -----------------------------------------------------------------------------------

/** Load a stream for a window; `startAt` is a wall-clock time to begin at. */
async function open(stream, { from = null, to = null, live = false, startAt = null } = {}) {
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
  video.play().catch(() => {});
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

async function showRange(button, startAt = null) {
  const stream = currentStream();
  if (!stream) return;
  setActive(button);
  const range = button.dataset.range;
  try {
    if (range === 'all') await open(stream, { startAt });
    else if (range === 'live') await open(stream, { from: Date.now() - 60 * MINUTE_MS, live: true });
    else await open(stream, { from: Date.now() - Number(range), to: Date.now(), startAt });
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
    await open(currentStream(), { from, to });
  } catch (error) {
    message(String(error));
  }
});

$('#stream').addEventListener('change', () => {
  history.replaceState(null, '', `#stream=${$('#stream').value}`);
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
  await showRange(document.querySelector('.ranges button[data-range="all"]'), startAt);
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
  if (!cam.loaded) return;
  input.value = toLocalInput(cam.wall, true);
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
