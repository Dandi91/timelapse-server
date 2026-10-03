// Plays a stream's timelapse through an HLS playlist built server-side, and maps the video's
// position back to wall-clock time using the same segment list the playlist was built from.

const video = $('#video');

// Only for Safari's native HLS, which doesn't say when it reloads the playlist.
const NATIVE_LIVE_REFRESH_MS = 10_000;

// What is loaded now: the stream, its segments in playback order, and where each one starts in
// the video (seconds). `from`/`to` are the wall-clock window shown on the bar.
let view = null;
let hls = null;
let liveTimer = null;

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

// --- coverage bar ------------------------------------------------------------------------------

function drawBar() {
  const bar = $('#bar');
  bar.querySelectorAll('.span').forEach((el) => el.remove());
  const { segments, from, to } = view;
  const width = to - from;
  // One span per session: within a session, footage is continuous.
  let start = null;
  segments.forEach((segment, i) => {
    start ??= segment.wall_start;
    const next = segments[i + 1];
    if (!next || next.session_id !== segment.session_id) {
      const span = document.createElement('div');
      span.className = 'span';
      span.style.left = `${((start - from) / width) * 100}%`;
      span.style.width = `${Math.max(((segment.wall_end - start) / width) * 100, 0.2)}%`;
      bar.append(span);
      start = null;
    }
  });
  $('#bar-start').textContent = formatTime(from);
  $('#bar-end').textContent = formatTime(to);
}

function updateClock() {
  if (!view) return;
  const wall = wallAt(video.currentTime);
  $('#clock').textContent = formatTime(wall);
  const position = (wall - view.from) / (view.to - view.from);
  $('#cursor').style.left = `${Math.min(Math.max(position, 0), 1) * 100}%`;
}

$('#bar').addEventListener('click', (event) => {
  if (!view) return;
  const box = event.currentTarget.getBoundingClientRect();
  const wall = view.from + ((event.clientX - box.left) / box.width) * (view.to - view.from);
  video.currentTime = timeAt(wall);
  video.play().catch(() => {});
});

// --- loading -----------------------------------------------------------------------------------

/** `onPlaylist(segmentCount)` runs whenever the player (re)loads the playlist. */
function attach(url, startPosition, onPlaylist) {
  if (hls) hls.destroy();
  hls = null;
  if (window.Hls && Hls.isSupported()) {
    hls = new Hls({ startPosition });
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
    message(live ? 'Nothing recorded yet; a segment appears once it is finished.' : 'No footage in this range.');
    return;
  }
  if (live) query.set('live', '1');
  view = {
    stream,
    segments,
    offsets: offsetsOf(segments),
    from: from ?? segments[0].wall_start,
    to: live ? Date.now() : (to ?? segments[segments.length - 1].wall_end),
    live,
  };
  drawBar();
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
  drawBar();
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

function toLocalInput(ms) {
  const date = new Date(ms - new Date(ms).getTimezoneOffset() * 60_000);
  return date.toISOString().slice(0, 16);
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

setUpNav();
init();
subscribe(onEvent);
