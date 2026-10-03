// A wall-clock timeline: footage and gaps (one lane per camera), playback cursor, export
// selection and time ticks. Scroll to zoom around the pointer, drag to pan, click to seek,
// shift-drag to select a range, hover for the time and a thumbnail of the lane under the pointer.

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const TICK_STEPS = [MINUTE, 5 * MINUTE, 10 * MINUTE, 15 * MINUTE, 30 * MINUTE, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY];
const MIN_SPAN = 2 * MINUTE;
const SINGLE_LANE = 26;
const LANE = 18;
const LANE_GAP = 2;
const TICKS = 18;
const DRAG_THRESHOLD = 4;

class Timeline {
  /**
   * `thumbUrl(segment)` gives a segment's sprite URL; `onSeek(ms)` and `onSelect(from, to)` report
   * clicks and shift-drags.
   */
  constructor(root, { thumbUrl, onSeek, onSelect }) {
    this.root = root;
    this.thumbUrl = thumbUrl;
    this.onSeek = onSeek;
    this.onSelect = onSelect;
    this.lanes = [];
    this.domain = [0, 1];
    this.viewport = [0, 1];
    this.cursor = null;
    this.selection = null;
    this.drag = null;

    this.canvas = document.createElement('canvas');
    this.hover = document.createElement('div');
    this.hover.className = 'tl-hover';
    this.hover.hidden = true;
    this.thumb = document.createElement('div');
    this.thumb.className = 'tl-thumb';
    this.label = document.createElement('div');
    this.label.className = 'tl-time';
    this.hover.append(this.thumb, this.label);
    root.append(this.canvas, this.hover);

    new ResizeObserver(() => this.draw()).observe(root);
    // Follow light/dark switches.
    matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => this.draw());
    root.addEventListener('wheel', (e) => this.onWheel(e), { passive: false });
    root.addEventListener('pointerdown', (e) => this.onDown(e));
    root.addEventListener('pointermove', (e) => this.onMove(e));
    root.addEventListener('pointerup', (e) => this.onUp(e));
    root.addEventListener('pointerleave', () => { if (!this.drag) this.hover.hidden = true; });
    root.addEventListener('dblclick', () => this.fit());
  }

  /**
   * New footage: `lanes` is `[{label, segments, recordingFrom}]`, one per camera; `recordingFrom`
   * (live only) is where footage still being recorded starts. The viewport stays where it was if
   * it still makes sense, else shows it all.
   */
  setData({ lanes, from, to }) {
    const sameDomain = this.domain[0] === from;
    const [oldFrom, oldTo] = this.domain;
    const zoomed = this.viewport[0] > oldFrom || this.viewport[1] < oldTo;
    this.lanes = lanes;
    this.domain = [from, Math.max(to, from + MIN_SPAN)];
    if (!sameDomain || !zoomed) this.viewport = [...this.domain];
    this.clampViewport();
    this.root.style.height = `${this.trackHeight() + TICKS}px`;
    this.draw();
  }

  trackHeight() {
    return this.lanes.length <= 1 ? SINGLE_LANE : this.lanes.length * (LANE + LANE_GAP) - LANE_GAP;
  }

  laneTop(i) {
    return this.lanes.length <= 1 ? 0 : i * (LANE + LANE_GAP);
  }

  laneHeight() {
    return this.lanes.length <= 1 ? SINGLE_LANE : LANE;
  }

  setCursor(ms) {
    this.cursor = ms;
    // While playing, keep the cursor in view when zoomed in.
    const [a, b] = this.viewport;
    if (ms != null && !this.drag && (ms < a || ms > b) && this.isZoomed()) {
      const span = b - a;
      this.viewport = [ms - span * 0.1, ms + span * 0.9];
      this.clampViewport();
    }
    this.draw();
  }

  setSelection(from, to) {
    this.selection = from != null && to != null && to > from ? [from, to] : null;
    this.draw();
  }

  isZoomed() {
    return this.viewport[0] > this.domain[0] + 1 || this.viewport[1] < this.domain[1] - 1;
  }

  fit() {
    this.viewport = [...this.domain];
    this.draw();
  }

  /** Zoom by `factor` (< 1 zooms in) keeping `anchor` (ms) under the same pixel. */
  zoom(factor, anchor = (this.viewport[0] + this.viewport[1]) / 2) {
    const [a, b] = this.viewport;
    const span = Math.min(Math.max((b - a) * factor, MIN_SPAN), this.domain[1] - this.domain[0]);
    const share = (anchor - a) / (b - a);
    this.viewport = [anchor - span * share, anchor - span * share + span];
    this.clampViewport();
    this.draw();
  }

  clampViewport() {
    const [d0, d1] = this.domain;
    let [a, b] = this.viewport;
    const span = Math.min(b - a, d1 - d0);
    if (a < d0) [a, b] = [d0, d0 + span];
    if (b > d1) [a, b] = [d1 - span, d1];
    this.viewport = [a, b];
  }

  width() {
    return this.root.clientWidth || 1;
  }

  msAt(x) {
    const [a, b] = this.viewport;
    return a + (x / this.width()) * (b - a);
  }

  xAt(ms) {
    const [a, b] = this.viewport;
    return ((ms - a) / (b - a)) * this.width();
  }

  local(event) {
    const box = this.root.getBoundingClientRect();
    return [event.clientX - box.left, event.clientY - box.top];
  }

  laneAt(y) {
    if (this.lanes.length <= 1) return 0;
    return Math.min(Math.max(Math.floor(y / (LANE + LANE_GAP)), 0), this.lanes.length - 1);
  }

  // --- interaction ---------------------------------------------------------------------------

  onWheel(event) {
    event.preventDefault();
    const [x, y] = this.local(event);
    this.zoom(Math.exp(Math.sign(event.deltaY) * 0.2), this.msAt(x));
    this.showHover(x, y);
  }

  onDown(event) {
    if (event.button !== 0) return;
    // Keeps drags going outside the timeline; not available for every pointer.
    try { this.root.setPointerCapture(event.pointerId); } catch { /* fine without */ }
    const [x] = this.local(event);
    this.drag = { x, viewport: [...this.viewport], select: event.shiftKey, moved: false, from: this.msAt(x) };
  }

  onMove(event) {
    const [x, y] = this.local(event);
    const drag = this.drag;
    if (!drag) {
      this.showHover(x, y);
      return;
    }
    if (Math.abs(x - drag.x) > DRAG_THRESHOLD) drag.moved = true;
    if (!drag.moved) return;
    if (drag.select) {
      const to = this.msAt(x);
      this.selection = [Math.min(drag.from, to), Math.max(drag.from, to)];
      this.showHover(x, y);
    } else {
      const [a, b] = drag.viewport;
      const shift = ((x - drag.x) / this.width()) * (b - a);
      this.viewport = [a - shift, b - shift];
      this.clampViewport();
      this.hover.hidden = true;
    }
    this.draw();
  }

  onUp(event) {
    const drag = this.drag;
    this.drag = null;
    if (!drag) return;
    if (!drag.moved) {
      this.onSeek(this.msAt(this.local(event)[0]));
    } else if (drag.select && this.selection) {
      this.onSelect(...this.selection.map(Math.round));
    }
  }

  // --- hover -----------------------------------------------------------------------------------

  /** The segment of lane `lane` at `ms` and where in its video that is, or null in a gap. */
  footageAt(lane, ms) {
    const seg = this.lanes[lane]?.segments.find((s) => s.wall_start <= ms && ms < s.wall_end);
    if (!seg) return null;
    const fraction = (ms - seg.wall_start) / Math.max(seg.wall_end - seg.wall_start, 1);
    return { seg, videoOffset: fraction * seg.media_dur };
  }

  showHover(x, y) {
    const ms = this.msAt(x);
    const lane = this.laneAt(y);
    const at = this.footageAt(lane, ms);
    const name = this.lanes.length > 1 ? `${this.lanes[lane].label} · ` : '';
    const hasAny = this.lanes.some((l) => l.segments.length);
    this.label.textContent = name + formatTime(ms) + (at || !hasAny ? '' : ' · no footage');
    const thumbs = at?.seg.thumbs;
    if (thumbs) {
      const tile = Math.min(Math.floor(at.videoOffset / at.seg.thumb_interval), thumbs - 1);
      this.thumb.hidden = false;
      this.thumb.style.backgroundImage = `url("${this.thumbUrl(at.seg)}")`;
      this.thumb.style.backgroundPosition = `${-tile * 160}px 0`;
    } else {
      this.thumb.hidden = true;
    }
    this.hover.hidden = false;
    const box = this.hover.getBoundingClientRect();
    const left = Math.min(Math.max(x - box.width / 2, 0), this.width() - box.width);
    this.hover.style.left = `${left}px`;
  }

  // --- drawing ---------------------------------------------------------------------------------

  draw() {
    const W = this.width();
    const track = this.trackHeight();
    const height = track + TICKS;
    const ratio = window.devicePixelRatio || 1;
    if (this.canvas.width !== Math.round(W * ratio) || this.canvas.height !== Math.round(height * ratio)) {
      this.canvas.width = Math.round(W * ratio);
      this.canvas.height = Math.round(height * ratio);
      this.canvas.style.width = `${W}px`;
      this.canvas.style.height = `${height}px`;
    }
    const g = this.canvas.getContext('2d');
    g.setTransform(ratio, 0, 0, ratio, 0, 0);
    g.clearRect(0, 0, W, height);
    const css = getComputedStyle(this.root);
    const color = (name) => css.getPropertyValue(name).trim();
    const laneH = this.laneHeight();

    this.lanes.forEach((lane, i) => this.drawLane(g, W, lane, this.laneTop(i), laneH, color));
    if (!this.lanes.length) {
      g.fillStyle = color('--line');
      g.fillRect(0, 0, W, track);
    }

    if (this.selection) {
      const x1 = this.xAt(this.selection[0]);
      const x2 = this.xAt(this.selection[1]);
      g.fillStyle = 'rgb(0 0 0 / 22%)';
      g.fillRect(x1, 0, x2 - x1, track);
      g.fillStyle = color('--accent');
      g.fillRect(x1 - 1, 0, 2, track);
      g.fillRect(x2 - 1, 0, 2, track);
    }

    if (this.cursor != null) {
      g.fillStyle = color('--accent');
      g.fillRect(Math.round(this.xAt(this.cursor)) - 1, 0, 2, track);
    }

    this.drawTicks(g, W, track, color('--muted'));
  }

  drawLane(g, W, lane, top, h, color) {
    // Gaps are the bare track; footage is drawn per session, continuous within one.
    g.fillStyle = color('--line');
    g.fillRect(0, top, W, h);
    g.fillStyle = color('--footage');
    let start = null;
    lane.segments.forEach((seg, i) => {
      start ??= seg.wall_start;
      const next = lane.segments[i + 1];
      if (!next || next.session_id !== seg.session_id || next.wall_start - seg.wall_end > 1000) {
        const x1 = this.xAt(start);
        const x2 = this.xAt(seg.wall_end);
        if (x2 >= 0 && x1 <= W) g.fillRect(x1, top, Math.max(x2 - x1, 1), h);
        start = null;
      }
    });

    // Live: being recorded, not watchable until its segment finishes.
    if (lane.recordingFrom != null) {
      const x1 = Math.max(this.xAt(lane.recordingFrom), 0);
      const x2 = Math.min(this.xAt(this.domain[1]), W);
      if (x2 > x1) {
        g.save();
        g.beginPath();
        g.rect(x1, top, x2 - x1, h);
        g.clip();
        g.strokeStyle = color('--footage');
        g.lineWidth = 2;
        for (let x = x1 - h; x < x2; x += 7) {
          g.beginPath();
          g.moveTo(x, top + h);
          g.lineTo(x + h, top);
          g.stroke();
        }
        g.restore();
      }
    }

    if (this.lanes.length > 1) {
      g.font = '600 11px system-ui, sans-serif';
      g.textBaseline = 'middle';
      g.fillStyle = color('--fg');
      g.fillText(lane.label, 6, top + h / 2 + 1);
    }
  }

  drawTicks(g, W, track, muted) {
    const [a, b] = this.viewport;
    const perPixel = (b - a) / W;
    const step = TICK_STEPS.find((s) => s / perPixel >= 80) ?? DAY;
    // Ticks fall on local clock boundaries.
    const offset = new Date(a).getTimezoneOffset() * MINUTE;
    let t = Math.ceil((a - offset) / step) * step + offset;
    g.fillStyle = muted;
    g.font = '11px system-ui, sans-serif';
    g.textBaseline = 'top';
    for (; t <= b; t += step) {
      const x = Math.round(this.xAt(t));
      g.fillRect(x, track, 1, 4);
      const date = new Date(t);
      const midnight = date.getHours() === 0 && date.getMinutes() === 0;
      const text = midnight || step >= DAY
        ? date.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' })
        : date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
      const width = g.measureText(text).width;
      g.fillText(text, Math.min(Math.max(x - width / 2, 0), W - width), track + 6);
    }
  }
}
