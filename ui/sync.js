// Playback in step across tabs and windows: player tabs with sync on share one clock over a
// BroadcastChannel, so several cameras can be watched side by side, each in its own window.
//
// The clock is a wall-clock time advancing at some wall-clock ms per real second. One tab, the
// leader, owns it: whichever tab was last used to play, pause, seek or change speed. The leader
// broadcasts the clock when it changes and twice a second besides; the others extrapolate between
// messages, since every tab reads the same computer clock. Each tab reports what its camera can
// show at the clock's time, and the leader acts on those reports: it waits while a camera is still
// loading, skips stretches where no camera has footage, and stops at the newest footage (or, live,
// waits there for more). If the leader closes or goes quiet, another tab takes over.
//
// At 1× the clock runs at the speed of the leader's footage (or, where it has none, of the first
// camera that has), so 1× means "this camera's own pace" in the tab being used.

const SYNC_CHANNEL = 'timelapse-sync';
const SYNC_HEARTBEAT_MS = 500;
/** A tab not heard from for this long has gone. Hidden tabs report only about once a second. */
const SYNC_PEER_TIMEOUT_MS = 2500;
/** A leader not heard from for this long is replaced (plus up to a second, so tabs don't collide). */
const SYNC_LEADER_TIMEOUT_MS = 3000;
/** How long a tab turning sync on waits to hear the clock before starting its own. */
const SYNC_JOIN_WAIT_MS = 300;
/** A camera loading for longer than this stops holding the others up. */
const SYNC_HOLD_LIMIT_MS = 10_000;
/** Restart gaps shorter than this don't count as missing footage. */
const GAP_SLACK = 5000;
/** Video seconds a camera may drift from the clock before it is re-seeked rather than nudged. */
const MAX_DRIFT = 1.0;
/** Smaller drift is corrected by playing up to this much faster or slower. */
const MAX_NUDGE = 0.25;

class SyncClock {
  /**
   * `probe(wall)` describes this tab's camera at wall-clock `wall`:
   * {footage, loading, speed, next, end, live}. `onChange()` runs when play state or speed change.
   */
  constructor({ probe, onChange = () => {} }) {
    this.id = `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
    this.probe = probe;
    this.onChange = onChange;
    this.enabled = false;
    this.clock = null;
    this.initial = null;
    this.peers = new Map();
    this.heard = 0;
    this.sent = 0;
    this.reportedLoading = false;
    this.loadingSince = null;
    this.patience = SYNC_LEADER_TIMEOUT_MS + Math.random() * 1000;
    this.channel = null;
    this.timer = null;
    this.joinTimer = null;
    this.leave = () => this.post({ type: 'bye' });
  }

  static get supported() {
    return 'BroadcastChannel' in window;
  }

  get isLeader() {
    return this.clock?.leader === this.id;
  }

  get playing() {
    return !!this.clock?.playing;
  }

  /** Playing, but held: waiting for a camera to load, or for new footage. */
  get waiting() {
    return !!this.clock?.playing && !this.clock.advancing;
  }

  get rate() {
    return this.clock?.rate ?? this.initial?.rate ?? 1;
  }

  /** Wall-clock ms per real second. */
  get wps() {
    return this.clock?.wps ?? 6000 * this.rate;
  }

  /** Tabs in sync, this one included. */
  get tabs() {
    this.prune();
    return this.peers.size + 1;
  }

  /** The clock's wall-clock time now, or null before the clock is known. */
  wall(now = Date.now()) {
    const c = this.clock;
    if (!c) return null;
    return c.wall + (c.advancing ? ((now - c.at) * c.wps) / 1000 : 0);
  }

  /** Join the tabs in sync; if there are none, start a clock from `initial` = {wall, playing, rate}. */
  enable(initial) {
    if (this.enabled) return;
    this.enabled = true;
    this.initial = initial;
    this.clock = null;
    this.peers.clear();
    this.heard = Date.now();
    this.channel = new BroadcastChannel(SYNC_CHANNEL);
    this.channel.onmessage = (event) => this.receive(event.data);
    this.post({ type: 'hello' });
    this.joinTimer = setTimeout(() => { if (!this.clock) this.lead(); }, SYNC_JOIN_WAIT_MS);
    this.timer = setInterval(() => this.tick(), 100);
    window.addEventListener('pagehide', this.leave);
  }

  disable() {
    if (!this.enabled) return;
    this.leave();
    window.removeEventListener('pagehide', this.leave);
    clearInterval(this.timer);
    clearTimeout(this.joinTimer);
    this.channel.close();
    this.channel = null;
    this.enabled = false;
    this.clock = null;
  }

  play() { this.lead({ playing: true }); }

  pause() { this.lead({ playing: false }); }

  /** Move the clock to `wall`; `playing`, if given, also plays or pauses. */
  seek(wall, playing) {
    if (!Number.isFinite(wall)) return;
    this.lead(playing === undefined ? { wall } : { wall, playing });
  }

  setRate(rate) { this.lead({ rate }); }

  /** Become the leader, with the clock as it is now changed by `changes`. */
  lead(changes = {}) {
    if (!this.enabled) return;
    clearTimeout(this.joinTimer);
    const now = Date.now();
    const c = this.clock;
    const base = c
      ? { wall: this.wall(now), playing: c.playing, rate: c.rate, wps: c.wps }
      : { ...this.initial, wall: this.initial?.wall ?? now };
    const next = { ...base, ...changes };
    this.setClock({
      leader: this.id,
      since: now,
      at: now,
      wall: next.wall,
      playing: !!next.playing,
      advancing: false,
      rate: next.rate ?? 1,
      wps: next.wps ?? 6000 * (next.rate ?? 1),
    });
    this.decide(true);
  }

  /** What this tab's camera can show at `wall`. A hidden tab never holds the others up. */
  report(wall) {
    const report = this.probe(wall);
    const loading = !!report.loading && this.playing && !document.hidden;
    if (!loading) this.loadingSince = null;
    else this.loadingSince ??= Date.now();
    return { ...report, loading: loading && Date.now() - this.loadingSince < SYNC_HOLD_LIMIT_MS };
  }

  /** The leader's job: set the clock's pace and hold it, skip it or stop it as the cameras need. */
  decide(force = false) {
    const c = this.clock;
    const now = Date.now();
    const current = this.wall(now);
    let wall = current;
    this.prune();
    const reports = [this.report(wall), ...[...this.peers.values()].map((p) => p.report).filter(Boolean)];
    const reference = reports.find((r) => r.footage && r.speed) ?? reports.find((r) => r.speed);
    const wps = reference ? reference.speed * c.rate : c.wps;
    let { playing } = c;
    // Nobody has footage here: go on to the first footage after it.
    if (playing && !reports.some((r) => r.footage)) {
      const next = reports.map((r) => r.next).filter((n) => n != null && n > wall);
      if (next.length) wall = Math.min(...next);
    }
    const ends = reports.map((r) => r.end).filter((e) => e != null);
    const end = ends.length ? Math.max(...ends) : null;
    const live = reports.some((r) => r.live);
    let atEnd = false;
    if (end != null && wall >= end) {
      wall = end;
      atEnd = true;
      if (!live) playing = false;
    }
    const advancing = playing && !atEnd && !reports.some((r) => r.loading);
    const changed = force || wall !== current || playing !== c.playing || advancing !== c.advancing
      || Math.abs(wps - c.wps) > wps * 0.001;
    if (changed) this.setClock({ ...c, wall, at: now, playing, advancing, wps });
    if (changed || now - this.sent >= SYNC_HEARTBEAT_MS) this.post({ type: 'clock', clock: this.clock });
  }

  tick() {
    if (!this.clock) return;
    if (this.isLeader) {
      this.decide();
      return;
    }
    const now = Date.now();
    const report = this.report(this.wall(now));
    if (now - this.sent >= SYNC_HEARTBEAT_MS || report.loading !== this.reportedLoading) {
      this.reportedLoading = report.loading;
      this.post({ type: 'status', report });
    }
    if (now - this.heard > this.patience) this.lead();
  }

  receive(msg) {
    const now = Date.now();
    if (msg.type === 'bye') {
      this.peers.delete(msg.from);
      // Its successor is whoever runs out of patience first.
      if (msg.from === this.clock?.leader) this.heard = now - SYNC_LEADER_TIMEOUT_MS;
      return;
    }
    const peer = this.peers.get(msg.from) ?? {};
    peer.seen = now;
    if (msg.type === 'status') peer.report = msg.report;
    this.peers.set(msg.from, peer);
    if (msg.type === 'hello' && this.isLeader) {
      this.post({ type: 'clock', clock: this.clock });
    } else if (msg.type === 'clock') {
      const theirs = msg.clock;
      const mine = this.clock;
      // The most recent leader wins; a tab still leading from before is told so.
      const current = !mine || theirs.since > mine.since
        || (theirs.since === mine.since && theirs.leader >= mine.leader);
      if (current) {
        clearTimeout(this.joinTimer);
        this.setClock(theirs);
        this.heard = now;
      } else if (this.isLeader) {
        this.post({ type: 'clock', clock: mine });
      }
    }
  }

  setClock(clock) {
    const before = this.clock;
    this.clock = clock;
    if (!before || before.playing !== clock.playing || before.rate !== clock.rate) this.onChange();
  }

  prune() {
    const now = Date.now();
    for (const [id, peer] of this.peers) {
      if (now - peer.seen > SYNC_PEER_TIMEOUT_MS) this.peers.delete(id);
    }
  }

  post(msg) {
    if (!this.channel) return;
    this.channel.postMessage({ ...msg, from: this.id });
    this.sent = Date.now();
  }
}

/** What `cam` can show at `wall`, for `SyncClock`'s probe. */
function probeCam(cam, wall) {
  if (!cam.loaded || wall == null) {
    return { footage: false, loading: false, speed: null, next: null, end: null, live: cam.live };
  }
  const footage = cam.hasFootageAt(wall, GAP_SLACK);
  return {
    footage,
    loading: footage && (cam.video.seeking || cam.video.readyState < 3),
    speed: cam.speedAt(wall),
    next: cam.nextFootageAfter(wall),
    end: cam.newest,
    live: cam.live,
  };
}

/**
 * Bring `cam` to the shared clock: seek when it is far off, otherwise steer its rate so small
 * drift dies out within a couple of seconds; play or pause with the clock. Returns a notice to
 * show over the picture, or null.
 */
function followClock(cam, sync) {
  const wall = sync.wall();
  const { video } = cam;
  if (wall == null || !cam.loaded) return null;
  if (!cam.hasFootageAt(wall, GAP_SLACK)) {
    if (!video.paused) video.pause();
    const next = cam.nextFootageAfter(wall);
    if (next) return `No footage until ${formatTime(next)}`;
    return cam.live ? 'Waiting for new footage…' : 'No footage after this';
  }
  const target = cam.timeAt(wall);
  const drift = video.currentTime - target;
  const playing = sync.clock.advancing;
  if (!video.seeking && (Math.abs(drift) > MAX_DRIFT || (!playing && Math.abs(drift) > 0.05))) {
    video.currentTime = target;
  }
  // The footage under the clock sets this camera's pace, not its current settings.
  const nominal = sync.wps / cam.speedAt(wall);
  const nudge = Math.min(Math.max(-drift / 2, -MAX_NUDGE), MAX_NUDGE);
  const rate = Math.min(Math.max(nominal * (1 + nudge), 0.0625), 16);
  if (Math.abs(video.playbackRate - rate) > 0.005) video.playbackRate = rate;
  if (playing && video.paused) video.play().catch(() => {});
  if (!playing && !video.paused) video.pause();
  return null;
}
