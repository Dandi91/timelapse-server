// The wall: up to four cameras side by side, all following one wall clock.
//
// The clock advances on its own; every camera seeks to the clock's time in its own footage and
// plays at whatever rate keeps it there (cameras may record at different speedups). A camera
// without footage at the clock's time pauses behind an overlay; while any camera with footage is
// still loading, the clock waits for it, so the picture stays in step.

const MAX_CAMS = 4;
/** Video seconds a camera may drift from the clock before it is re-seeked rather than nudged. */
const MAX_DRIFT = 1.0;
/** Smaller drift is corrected by playing up to this much faster or slower. */
const MAX_NUDGE = 0.25;
/** Restart gaps shorter than this don't hide a camera. */
const GAP_SLACK = 5000;

let streams = [];
let tiles = [];
let liveClock = null;
/** Bumped by every reload, so a reload overtaken by a newer one stops quietly. */
let generation = 0;

/** The shared clock, in wall-clock ms. */
const clock = {
  wall: null,
  playing: false,
  rate: 1,
  from: null,
  to: null,
  live: false,
  last: null,
};

function message(text) {
  $('#message').textContent = text || '';
}

// --- clock -------------------------------------------------------------------------------------

/** Wall-clock ms per real second: the first camera's speedup at 1×, times the speed setting. */
function wallPerSecond() {
  const first = tiles.find((t) => t.cam.loaded);
  return (first ? first.cam.wallPerVideoSecond : 6000) * clock.rate;
}

/** Tiles with footage at the clock's time. */
function activeTiles() {
  return tiles.filter((t) => t.cam.loaded && t.cam.hasFootageAt(clock.wall, GAP_SLACK));
}

/** A camera that should be showing a picture but can't yet. */
function anyLoading() {
  return activeTiles().some((t) => t.video.seeking || t.video.readyState < 3);
}

function tick(now) {
  const elapsed = clock.last == null ? 0 : Math.min(now - clock.last, 250);
  clock.last = now;
  if (clock.wall != null) {
    const loading = anyLoading();
    $('#waiting').hidden = !loading || !clock.playing;
    if (clock.playing && !loading) clock.wall += (elapsed / 1000) * wallPerSecond();
    const end = clock.live ? newestFootage() : clock.to;
    if (end != null && clock.wall >= end) {
      clock.wall = end;
      if (!clock.live) setPlaying(false);
    }
    sync();
    render();
  }
  requestAnimationFrame(tick);
}

function newestFootage() {
  const ends = tiles.filter((t) => t.cam.loaded).map((t) => t.cam.newest);
  return ends.length ? Math.max(...ends) : null;
}

/**
 * Bring every camera to the clock: seek when it is far off, otherwise steer its rate so small
 * drift dies out within a couple of seconds; play or pause with the clock. While the clock waits
 * for a camera that is loading, the others pause too, so nobody runs ahead.
 */
function sync() {
  const holding = clock.playing && anyLoading();
  for (const tile of tiles) {
    const { cam, video } = tile;
    if (!cam.loaded) {
      showOverlay(tile, 'No footage in this range');
      continue;
    }
    if (!cam.hasFootageAt(clock.wall, GAP_SLACK)) {
      if (!video.paused) video.pause();
      const next = cam.nextFootageAfter(clock.wall);
      showOverlay(tile, next ? `No footage until ${formatTime(next)}` : clock.live ? 'Recording…' : 'No footage after this');
      continue;
    }
    showOverlay(tile, null);
    const target = cam.timeAt(clock.wall);
    const drift = video.currentTime - target;
    const playing = clock.playing && !holding;
    if (!video.seeking && (Math.abs(drift) > MAX_DRIFT || (!playing && Math.abs(drift) > 0.05))) {
      video.currentTime = target;
    }
    const nominal = wallPerSecond() / cam.wallPerVideoSecond;
    const nudge = Math.min(Math.max(-drift / 2, -MAX_NUDGE), MAX_NUDGE);
    const rate = Math.min(Math.max(nominal * (1 + nudge), 0.0625), 16);
    if (Math.abs(video.playbackRate - rate) > 0.005) video.playbackRate = rate;
    if (playing && video.paused) video.play().catch(() => {});
    if (!playing && !video.paused) video.pause();
  }
}

function showOverlay(tile, text) {
  tile.overlay.hidden = !text;
  if (text) tile.overlay.textContent = text;
}

function setPlaying(playing) {
  clock.playing = playing && clock.wall != null;
  controls.refresh();
}

function seekClock(ms) {
  if (clock.wall == null) return;
  const end = clock.live ? newestFootage() : clock.to;
  clock.wall = Math.min(Math.max(ms, clock.from), end ?? ms);
  sync();
  render();
}

function render() {
  $('#clock').textContent = clock.wall == null ? '–' : formatTime(clock.wall);
  timeline.setCursor(clock.wall);
  $('#live').hidden = !clock.live;
  if (clock.live && clock.wall != null) {
    const behind = Math.max(Date.now() - clock.wall, 0) / 1000;
    $('#live-lag').textContent = behind < 90 ? `${Math.round(behind)} s behind` : `${formatDuration(behind)} behind`;
  }
  for (const tile of tiles) {
    tile.open.href = clock.wall == null ? `./#stream=${tile.stream.id}` : `./#stream=${tile.stream.id}&t=${Math.round(clock.wall)}`;
  }
}

// --- timeline and controls -----------------------------------------------------------------------

let selection = null;

const timeline = new Timeline($('#timeline'), {
  thumbUrl,
  onSeek: seekClock,
  onSelect(from, to) {
    selection = [from, to];
    timeline.setSelection(from, to);
    $('#export-range').textContent = `${formatTime(from)} – ${formatTime(to)}`;
  },
});

function drawTimeline() {
  const loaded = tiles.filter((t) => t.cam.loaded);
  if (!loaded.length) return;
  clock.from = Math.min(...loaded.map((t) => t.cam.from));
  clock.to = clock.live ? Date.now() : Math.max(...loaded.map((t) => t.cam.to));
  timeline.setData({
    lanes: tiles.map((t) => ({
      label: t.stream.label,
      segments: t.cam.segments,
      recordingFrom: clock.live && t.cam.loaded ? t.cam.newest : null,
    })),
    from: clock.from,
    to: clock.to,
  });
  if (selection) timeline.setSelection(...selection);
}

$('#zoom-in').addEventListener('click', () => timeline.zoom(1 / 2));
$('#zoom-out').addEventListener('click', () => timeline.zoom(2));
$('#zoom-fit').addEventListener('click', () => timeline.fit());

const controls = setUpControls({
  root: $('#wall'),
  playButton: $('#play'),
  fullscreenButton: $('#fullscreen'),
  clickTargets: [$('#grid')],
  actions: {
    isPlaying: () => clock.playing,
    toggle: () => setPlaying(!clock.playing),
    // Video seconds of the first camera, as the player does.
    jump: (seconds) => seekClock(clock.wall + seconds * (wallPerSecond() / clock.rate)),
    step: (frames) => {
      setPlaying(false);
      const first = tiles.find((t) => t.cam.loaded);
      const fps = first ? first.stream.settings.out_fps : 30;
      seekClock(clock.wall + (frames / fps) * (wallPerSecond() / clock.rate));
    },
  },
});

$('#rate').addEventListener('change', (event) => { clock.rate = Number(event.target.value); });

// --- loading -------------------------------------------------------------------------------------

function selectedIds() {
  return [...document.querySelectorAll('#pickers input:checked')].map((input) => Number(input.value));
}

function buildTiles() {
  for (const tile of tiles) tile.cam.unload();
  const grid = $('#grid');
  grid.replaceChildren();
  tiles = selectedIds().map((id) => {
    const stream = streams.find((s) => s.id === id);
    const el = document.createElement('div');
    el.className = 'tile';
    const video = document.createElement('video');
    video.muted = true;
    video.playsInline = true;
    const label = document.createElement('div');
    label.className = 'tile-label';
    label.textContent = stream.label;
    const open = document.createElement('a');
    open.className = 'tile-open';
    open.textContent = 'Open';
    open.title = 'Open in the player at this time';
    open.addEventListener('click', (event) => event.stopPropagation());
    const overlay = document.createElement('div');
    overlay.className = 'tile-overlay';
    overlay.hidden = true;
    el.append(video, label, open, overlay);
    grid.append(el);
    const tile = { stream, el, video, overlay, open };
    tile.cam = new CamPlayer(video, { onError: (text) => showOverlay(tile, text), onUpdate: drawTimeline });
    return tile;
  });
  grid.dataset.count = String(tiles.length);
}

async function showRange(button) {
  const mine = ++generation;
  document.querySelectorAll('.ranges button').forEach((b) => b.classList.toggle('active', b === button));
  clearInterval(liveClock);
  setPlaying(false);
  buildTiles();
  if (!tiles.length) {
    clock.wall = null;
    message('Pick at least one camera.');
    return;
  }
  message('');
  const range = button.dataset.range;
  const now = Date.now();
  const limits = range === 'all' ? {} : range === 'live'
    ? { from: now - 60 * MINUTE_MS, live: true }
    : { from: now - Number(range), to: now };
  clock.live = range === 'live';
  try {
    await Promise.all(tiles.map((t) => t.cam.load(t.stream, limits)));
  } catch (error) {
    if (mine === generation) message(String(error));
  }
  if (mine !== generation) return;
  const loaded = tiles.filter((t) => t.cam.loaded);
  if (!loaded.length) {
    clock.wall = null;
    timeline.setData({ lanes: [], from: limits.from ?? now - 60 * MINUTE_MS, to: limits.to ?? now });
    render();
    message('None of these cameras has footage in this range.');
    return;
  }
  drawTimeline();
  // Live starts a little before the newest footage every camera has; anything else at the start.
  clock.wall = clock.live
    ? Math.max(Math.min(...loaded.map((t) => t.cam.newest)) - 30_000, clock.from)
    : clock.from;
  if (clock.live) {
    liveClock = setInterval(drawTimeline, 5000);
  }
  sync();
  setPlaying(true);
}

function saveSelection() {
  history.replaceState(null, '', `#streams=${selectedIds().join(',')}`);
}

function limitPickers() {
  const checked = selectedIds().length;
  document.querySelectorAll('#pickers input').forEach((input) => {
    input.disabled = !input.checked && checked >= MAX_CAMS;
  });
}

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
  const wanted = new URLSearchParams(location.hash.slice(1)).get('streams');
  const initial = wanted
    ? wanted.split(',').map(Number)
    : streams.filter((s) => s.segments).slice(0, MAX_CAMS).map((s) => s.id);
  for (const stream of streams) {
    const label = document.createElement('label');
    const input = document.createElement('input');
    input.type = 'checkbox';
    input.value = stream.id;
    input.checked = initial.includes(stream.id);
    input.addEventListener('change', () => {
      limitPickers();
      saveSelection();
      showRange(document.querySelector('.ranges button.active') ?? document.querySelector('.ranges button'));
    });
    label.append(input, ` ${stream.label}`);
    $('#pickers').append(label);
  }
  limitPickers();
  await showRange(document.querySelector('.ranges button[data-range="all"]'));
}

document.querySelectorAll('.ranges button[data-range]').forEach((button) => {
  button.addEventListener('click', () => showRange(button));
});

// --- export --------------------------------------------------------------------------------------

$('#export').addEventListener('click', async () => {
  const note = $('#export-message');
  if (!selection) {
    note.textContent = 'Shift-drag on the timeline to pick a range first.';
    return;
  }
  const mode = document.querySelector('input[name="export-mode"]:checked').value;
  const [from, to] = selection;
  $('#export').disabled = true;
  const results = await Promise.all(tiles.map((t) =>
    api('api/exports', { method: 'POST', body: { stream_id: t.stream.id, from, to, mode } })
      .then(() => null)
      .catch((error) => `${t.stream.label}: ${error.message}`)));
  $('#export').disabled = false;
  const failed = results.filter(Boolean);
  const queued = results.length - failed.length;
  note.innerHTML = '';
  if (queued) {
    note.append(`Queued ${queued} export${queued === 1 ? '' : 's'}. `);
    const link = document.createElement('a');
    link.href = 'exports.html';
    link.textContent = 'Follow them on the Exports page';
    note.append(link);
  }
  if (failed.length) note.append(` ${queued ? 'Skipped' : 'Nothing queued'}: ${failed.join('; ')}`);
});

setUpNav();
init();
requestAnimationFrame(tick);
