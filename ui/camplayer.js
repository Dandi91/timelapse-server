// One camera's playback: an HLS playlist built server-side for a wall-clock range, and the
// mapping between the video's position and wall-clock time, from the same segment list the
// playlist was built from. The player page has one; the wall has one per camera.

// Only for Safari's native HLS, which doesn't say when it reloads the playlist.
const NATIVE_LIVE_REFRESH_MS = 10_000;

class CamPlayer {
  /** `onError(text)` reports fatal playback errors; `onUpdate()` runs when segments change. */
  constructor(video, { onError = () => {}, onUpdate = () => {} } = {}) {
    this.video = video;
    this.onError = onError;
    this.onUpdate = onUpdate;
    this.stream = null;
    this.segments = [];
    this.offsets = [];
    this.from = null;
    this.to = null;
    this.live = false;
    this.hls = null;
    this.timer = null;
    this.refreshing = false;
  }

  get loaded() {
    return this.segments.length > 0;
  }

  /** Seconds of video in the loaded range. */
  get duration() {
    if (!this.loaded) return 0;
    return this.offsets[this.offsets.length - 1] + this.segments[this.segments.length - 1].media_dur;
  }

  /** End of the newest footage loaded, wall-clock ms. */
  get newest() {
    return this.loaded ? this.segments[this.segments.length - 1].wall_end : null;
  }

  /**
   * Load `stream` for a wall-clock window; `live` follows new segments as they finish.
   * Returns false when there is no footage in the window. `startAt` is a wall-clock time to
   * start from; by default the start, or the newest footage when live.
   */
  async load(stream, { from = null, to = null, live = false, startAt = null } = {}) {
    this.unload();
    this.stream = stream;
    this.live = live;
    const query = new URLSearchParams();
    if (from != null) query.set('from', Math.round(from));
    if (to != null) query.set('to', Math.round(to));
    const segments = await api(`api/streams/${stream.id}/segments?${query}`);
    if (this.stream !== stream) return false; // superseded meanwhile
    this.setSegments(segments);
    if (!this.loaded) return false;
    // Live shows from the footage on, not an empty stretch before a stream that just started.
    this.from = live ? Math.max(from, segments[0].wall_start) : (from ?? segments[0].wall_start);
    this.to = live ? Date.now() : (to ?? this.newest);
    this.queryFrom = from;
    if (live) query.set('live', '1');
    let start = -1;
    if (startAt != null) start = this.timeAt(startAt);
    else if (live) start = Math.max(this.duration - 5, 0);
    this.attach(`streams/${stream.id}/playlist.m3u8?${query}`, start);
    return true;
  }

  unload() {
    clearInterval(this.timer);
    if (this.hls) this.hls.destroy();
    this.hls = null;
    this.video.removeAttribute('src');
    this.video.load();
    this.segments = [];
    this.offsets = [];
  }

  setSegments(segments) {
    this.segments = segments;
    let total = 0;
    this.offsets = segments.map((segment) => { const start = total; total += segment.media_dur; return start; });
  }

  attach(url, startPosition) {
    // The segment list must cover everything the player can reach, or the wall clock would stall
    // on footage it doesn't know; so it is refreshed whenever a live playlist grows. (Entries are
    // keyframe parts, not segments, so only a change in their number says anything.)
    let fragments = null;
    const onPlaylist = (count) => {
      if (!this.live) return;
      if (count === null || (fragments !== null && count !== fragments)) this.refresh();
      fragments = count;
    };
    if (window.Hls && Hls.isSupported()) {
      // Segments are large (a 10-minute segment at 1080p is ~50 MB). Appending one in a single
      // piece stalls playback for a moment; progressive mode appends while it downloads.
      this.hls = new Hls({ startPosition, progressive: true });
      this.hls.on(Hls.Events.ERROR, (_, data) => {
        if (data.fatal) this.onError(`Playback error: ${data.details}`);
      });
      this.hls.on(Hls.Events.LEVEL_UPDATED, (_, data) => onPlaylist(data.details.fragments.length));
      this.hls.loadSource(url);
      this.hls.attachMedia(this.video);
    } else if (this.video.canPlayType('application/vnd.apple.mpegurl')) {
      // Safari plays HLS natively.
      this.video.src = url;
      this.video.addEventListener('loadedmetadata', () => {
        this.video.currentTime = Math.max(startPosition, 0);
      }, { once: true });
      if (this.live) this.timer = setInterval(() => onPlaylist(null), NATIVE_LIVE_REFRESH_MS);
    } else {
      this.onError('This browser cannot play HLS.');
    }
  }

  /** Pick up segments finished since the last look. */
  async refresh() {
    if (this.refreshing || !this.stream) return;
    this.refreshing = true;
    const stream = this.stream;
    try {
      const query = new URLSearchParams();
      if (this.queryFrom != null) query.set('from', Math.round(this.queryFrom));
      const segments = await api(`api/streams/${stream.id}/segments?${query}`).catch(() => null);
      if (!segments?.length || this.stream !== stream) return;
      this.setSegments(segments);
      this.to = Date.now();
      this.onUpdate();
    } finally {
      this.refreshing = false;
    }
  }

  /** Wall-clock ms at video time `t`, interpolating within the segment. */
  wallAt(t) {
    let i = this.offsets.length - 1;
    while (i > 0 && this.offsets[i] > t) i--;
    const segment = this.segments[i];
    const fraction = Math.min(Math.max((t - this.offsets[i]) / segment.media_dur, 0), 1);
    return segment.wall_start + fraction * (segment.wall_end - segment.wall_start);
  }

  /** Video time for wall-clock `ms`; a time in a gap goes to the footage right after it. */
  timeAt(ms) {
    const i = this.segments.findIndex((segment) => segment.wall_end > ms);
    if (i < 0) return Math.max(this.duration - 0.1, 0);
    const segment = this.segments[i];
    const span = segment.wall_end - segment.wall_start;
    const fraction = span > 0 ? Math.min(Math.max((ms - segment.wall_start) / span, 0), 1) : 0;
    return this.offsets[i] + fraction * segment.media_dur;
  }

  /**
   * Whether there is footage at wall-clock `ms`. A gap between footage shorter than `slack` ms
   * (a pipeline restart) counts as footage; time before the first or after the last doesn't.
   */
  hasFootageAt(ms, slack = 0) {
    const i = this.segments.findIndex((s) => s.wall_end > ms);
    if (i < 0) return false;
    const next = this.segments[i];
    if (next.wall_start <= ms) return true;
    return i > 0 && next.wall_start - this.segments[i - 1].wall_end <= slack;
  }

  /** Start of the first footage after wall-clock `ms`, or null. */
  nextFootageAfter(ms) {
    return this.segments.find((s) => s.wall_start > ms)?.wall_start ?? null;
  }

  /** Wall-clock ms per second of video with the stream's current settings. */
  get wallPerVideoSecond() {
    const s = this.stream?.settings;
    return s ? (1000 * s.out_fps) / s.sample_fps : 6000;
  }

  /**
   * Wall-clock ms per second of video in the footage at wall-clock `ms` (or the next footage).
   * Taken from the segment itself, not the stream's settings: footage recorded before a change
   * of speed keeps its own.
   */
  speedAt(ms) {
    const seg = this.segments.find((s) => s.wall_end > ms) ?? this.segments[this.segments.length - 1];
    const ratio = seg && seg.media_dur > 0 ? (seg.wall_end - seg.wall_start) / seg.media_dur : 0;
    return ratio > 0 ? ratio : this.wallPerVideoSecond;
  }

  /** The wall-clock time now showing. */
  get wall() {
    return this.loaded ? this.wallAt(this.video.currentTime) : null;
  }
}

/** A segment's thumbnail sprite, next to it under the stream's playlist directory. */
function thumbUrl(segment) {
  const relative = segment.path.replace(/^streams\/\d+\//, '').replace(/\.ts$/, '.jpg');
  return `streams/${segment.stream_id}/${relative}`;
}
