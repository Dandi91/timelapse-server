// Plays a stream's timelapse through an HLS playlist built server-side, and maps the video's
// position back to wall-clock time using the same segment list the playlist was built from.

const video = $('#video');

// Only for Safari's native HLS, which doesn't say when it reloads the playlist.
const NATIVE_LIVE_REFRESH_MS = 10_000;

// What is loaded now: the stream, its segments in playback order, and where each one starts in
// the video (seconds). `from`/`to` are the wall-clock window shown on the timeline.
let view = null;
let hls = null;
let liveTimer = null;
// While live: extends the timeline to now, so the stretch being recorded grows.
let liveClock = null;

function message(text) {
  $('#message').textContent = text || '';
}

// --- wall clock <-> video time ---------------------------------------------------------------

function offsetsOf(segments) {
  let total = 0;
  return segments.map((segment) => { const start = total; total += segment.media_dur; return start; });
}

/** Wall-clock ms at video time `t`, interpolating within the segment. */
function wallAt(t) {
  const { segments, offsets } = view;
  let i = offsets.length - 1;
  while (i > 0 && offsets[i] > t) i--;
  const segment = segments[i];
  const fraction = Math.min(Math.max((t - offsets[i]) / segment.media_dur, 0), 1);
  return segment.wall_start + fraction * (segment.wall_end - segment.wall_start);
}

/** Video time for wall-clock `ms`; a time in a gap goes to the footage right after it. */
function timeAt(ms) {
  const { segments, offsets } = view;
  const i = segments.findIndex((segment) => segment.wall_end > ms);
  if (i < 0) {
    const last = segments.length - 1;
    return offsets[last] + segments[last].media_dur - 0.1;
  }
  const segment = segments[i];
  const span = segment.wall_end - segment.wall_start;
  const fraction = span > 0 ? Math.min(Math.max((ms - segment.wall_start) / span, 0), 1) : 0;
  return offsets[i] + fraction * segment.media_dur;
}

// --- timeline ----------------------------------------------------------------------------------

/** A segment's thumbnail sprite, next to it under the stream's playlist directory. */
function thumbUrl(segment) {
  const relative = segment.path.replace(/^streams\/\d+\//, '').replace(/\.ts$/, '.jpg');
  return `streams/${segment.stream_id}/${relative}`;
}

const timeline = new Timeline($('#timeline'), {
  thumbUrl,
  onSeek(ms) {
    if (!view) return;
    video.currentTime = timeAt(ms);
    video.play().catch(() => {});
  },
  onSelect(from, to) {
    $('#export-from').value = toLocalInput(from, true);
    $('#export-to').value = toLocalInput(to, true);
    drawSelection();
  },
});

function drawTimeline() {
  if (!view) return;
  const newest = view.segments[view.segments.length - 1].wall_end;
  timeline.setData({
    segments: view.segments,
    from: view.from,
    to: view.to,
    recordingFrom: view.live ? newest : null,
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
  if (!view) return;
  const wall = wallAt(video.currentTime);
  $('#clock').textContent = formatTime(wall);
  timeline.setCursor(wall);
  updateLive(wall);
}

/** In live mode: how far behind real time the picture is, and a way back to the newest footage. */
function updateLive(wall) {
  const badge = $('#live');
  badge.hidden = !view?.live;
  if (!view?.live) {
    $('#jump-live').hidden = true;
    return;
  }
  const newest = view.segments[view.segments.length - 1].wall_end;
  const behind = Math.max(Date.now() - wall, 0) / 1000;
  $('#live-lag').textContent = behind < 90 ? `${Math.round(behind)} s behind` : `${formatDuration(behind)} behind`;
  // The newest footage itself trails by up to a segment; only offer a jump when well behind that.
  $('#jump-live').hidden = newest - wall < 2 * MINUTE_MS;
}

$('#jump-live').addEventListener('click', () => {
  if (!view) return;
  const total = view.offsets[view.offsets.length - 1] + view.segments[view.segments.length - 1].media_dur;
  video.currentTime = Math.max(total - 3, 0);
  video.play().catch(() => {});
});

// --- loading -----------------------------------------------------------------------------------

/** `onPlaylist(segmentCount)` runs whenever the player (re)loads the playlist. */
function attach(url, startPosition, onPlaylist) {
  if (hls) hls.destroy();
  hls = null;
  if (window.Hls && Hls.isSupported()) {
    // Segments are large (a 10-minute segment at 1080p is ~50 MB). Appending one in a single
    // piece stalls playback for a moment; progressive mode appends while it downloads.
    hls = new Hls({ startPosition, progressive: true });
    hls.on(Hls.Events.ERROR, (_, data) => {
      if (data.fatal) message(`Playback error: ${data.details}`);
    });
    if (onPlaylist) hls.on(Hls.Events.LEVEL_UPDATED, (_, data) => onPlaylist(data.details.fragments.length));
    hls.loadSource(url);
    hls.attachMedia(video);
  } else if (video.canPlayType('application/vnd.apple.mpegurl')) {
    // Safari plays HLS natively.
    video.src = url;
    video.addEventListener('loadedmetadata', () => { video.currentTime = Math.max(startPosition, 0); }, { once: true });
    if (onPlaylist) liveTimer = setInterval(() => onPlaylist(null), NATIVE_LIVE_REFRESH_MS);
  } else {
    message('This browser cannot play HLS.');
    return;
  }
  video.play().catch(() => {});
}

/** Load `stream` for a wall-clock window; `live` follows new segments as they finish. */
async function open(stream, { from = null, to = null, live = false } = {}) {
  clearInterval(liveTimer);
  clearInterval(liveClock);
  message('');
  const query = new URLSearchParams();
  if (from != null) query.set('from', Math.round(from));
  if (to != null) query.set('to', Math.round(to));
  const segments = await api(`api/streams/${stream.id}/segments?${query}`);
  if (!segments.length) {
    view = null;
    if (hls) hls.destroy();
    hls = null;
    video.removeAttribute('src');
    video.load();
    $('#clock').textContent = '–';
    timeline.setData({ segments: [], from: from ?? Date.now() - 60 * MINUTE_MS, to: to ?? Date.now() });
    updateLive(0);
    message(live ? 'Nothing recorded yet; a segment appears once it is finished.' : 'No footage in this range.');
    return;
  }
  if (live) query.set('live', '1');
  view = {
    stream,
    segments,
    offsets: offsetsOf(segments),
    // Live shows from the footage on, not an empty stretch before a stream that just started.
    from: live ? Math.max(from, segments[0].wall_start) : (from ?? segments[0].wall_start),
    to: live ? Date.now() : (to ?? segments[segments.length - 1].wall_end),
    live,
  };
  drawTimeline();
  if (live) {
    liveClock = setInterval(() => {
      view.to = Date.now();
      drawTimeline();
      updateClock();
    }, 5000);
  }
  const total = view.offsets[segments.length - 1] + segments[segments.length - 1].media_dur;
  // Live starts a few seconds before the newest footage; anything else from the start.
  // The page's segment list must cover everything the player can reach, or the wall clock would
  // stall on footage it doesn't know. So it is refreshed whenever the player's playlist grows.
  const onPlaylist = live
    ? (count) => { if (count === null || count > view.segments.length) refreshLive(stream, from); }
    : null;
  attach(`streams/${stream.id}/playlist.m3u8?${query}`, live ? Math.max(total - 5, 0) : -1, onPlaylist);
}

let refreshing = false;

/** Pick up segments finished since the last look. */
async function refreshLive(stream, from) {
  if (!view || view.stream.id !== stream.id || !view.live || refreshing) return;
  refreshing = true;
  try {
    await refreshSegments(stream, from);
  } finally {
    refreshing = false;
  }
}

async function refreshSegments(stream, from) {
  const query = new URLSearchParams();
  if (from != null) query.set('from', Math.round(from));
  const segments = await api(`api/streams/${stream.id}/segments?${query}`).catch(() => null);
  if (!segments || !segments.length || !view || view.stream.id !== stream.id) return;
  view.segments = segments;
  view.offsets = offsetsOf(segments);
  view.to = Date.now();
  drawTimeline();
}

// --- controls ----------------------------------------------------------------------------------

let streams = [];

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

async function showRange(button) {
  const stream = currentStream();
  if (!stream) return;
  setActive(button);
  const range = button.dataset.range;
  try {
    if (range === 'all') await open(stream);
    else if (range === 'live') await open(stream, { from: Date.now() - 3_600_000, live: true });
    else await open(stream, { from: Date.now() - Number(range), to: Date.now() });
    if (view) {
      $('#from').value = toLocalInput(view.from);
      $('#to').value = toLocalInput(view.to);
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

$('#rate').addEventListener('change', (event) => { video.playbackRate = Number(event.target.value); });
video.addEventListener('ratechange', () => { $('#rate').value = String(video.playbackRate); });
video.addEventListener('timeupdate', updateClock);
video.addEventListener('seeked', updateClock);

async function init() {
  try {
    streams = await api('api/streams');
  } catch (error) {
    message(String(error));
    return;
  }
  if (!streams.length) {
    message('No streams yet. Add one with `timelapse-server add`.');
    return;
  }
  const select = $('#stream');
  for (const stream of streams) select.append(new Option(stream.label, stream.id));
  // The stream named in the link (#stream=3), else the first with something to show.
  const wanted = new URLSearchParams(location.hash.slice(1)).get('stream');
  const initial = streams.find((stream) => String(stream.id) === wanted)
    ?? streams.find((stream) => stream.segments) ?? streams[0];
  select.value = String(initial.id);
  select.dispatchEvent(new Event('change'));
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
  if (!view) return;
  input.value = toLocalInput(wallAt(video.currentTime), true);
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
