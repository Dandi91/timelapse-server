// A wall-clock timeline: footage and gaps, playback cursor, export selection and time ticks.
// Scroll to zoom around the pointer, drag to pan, click to seek, shift-drag to select a range,
// hover for the time and a thumbnail.

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const TICK_STEPS = [MINUTE, 5 * MINUTE, 10 * MINUTE, 15 * MINUTE, 30 * MINUTE, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY];
const MIN_SPAN = 2 * MINUTE;
const TRACK = 26;
const HEIGHT = 44;
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
    this.segments = [];
    this.domain = [0, 1];
    this.viewport = [0, 1];
    this.cursor = null;
    this.selection = null;
    this.recordingFrom = null;
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

  /** New footage. The viewport stays where it was if it still makes sense, else shows it all. */
  setData({ segments, from, to, recordingFrom = null }) {
    const sameDomain = this.domain[0] === from;
    this.segments = segments;
    this.recordingFrom = recordingFrom;
    const [oldFrom, oldTo] = this.domain;
    this.domain = [from, Math.max(to, from + MIN_SPAN)];
    const zoomed = this.viewport[0] > oldFrom || this.viewport[1] < oldTo;
    if (!sameDomain || !zoomed) this.viewport = [...this.domain];
    this.clampViewport();
    this.draw();
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

  localX(event) {
    return event.clientX - this.root.getBoundingClientRect().left;
  }

  // --- interaction ---------------------------------------------------------------------------

  onWheel(event) {
    event.preventDefault();
    const factor = Math.exp(Math.sign(event.deltaY) * 0.2);
    this.zoom(factor, this.msAt(this.localX(event)));
    this.showHover(this.localX(event));
  }

  onDown(event) {
    if (event.button !== 0) return;
    // Keeps drags going outside the timeline; not available for every pointer.
    try { this.root.setPointerCapture(event.pointerId); } catch { /* fine without */ }
    const x = this.localX(event);
    this.drag = { x, viewport: [...this.viewport], select: event.shiftKey, moved: false, from: this.msAt(x) };
  }

  onMove(event) {
    const x = this.localX(event);
    const drag = this.drag;
    if (!drag) {
      this.showHover(x);
      return;
    }
    if (Math.abs(x - drag.x) > DRAG_THRESHOLD) drag.moved = true;
    if (!drag.moved) return;
    if (drag.select) {
      const to = this.msAt(x);
      this.selection = [Math.min(drag.from, to), Math.max(drag.from, to)];
      this.showHover(x);
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
      this.onSeek(this.msAt(this.localX(event)));
    } else if (drag.select && this.selection) {
      this.onSelect(...this.selection.map(Math.round));
    }
  }

  // --- hover -----------------------------------------------------------------------------------

  /** The segment at `ms` and where in its video that is, or null in a gap. */
  footageAt(ms) {
    const seg = this.segments.find((s) => s.wall_start <= ms && ms < s.wall_end);
    if (!seg) return null;
    const fraction = (ms - seg.wall_start) / Math.max(seg.wall_end - seg.wall_start, 1);
    return { seg, videoOffset: fraction * seg.media_dur };
  }

  showHover(x) {
    const ms = this.msAt(x);
    const at = this.footageAt(ms);
    this.label.textContent = formatTime(ms) + (at || !this.segments.length ? '' : ' · no footage');
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
    const ratio = window.devicePixelRatio || 1;
    if (this.canvas.width !== Math.round(W * ratio)) {
      this.canvas.width = Math.round(W * ratio);
      this.canvas.height = Math.round(HEIGHT * ratio);
      this.canvas.style.width = `${W}px`;
      this.canvas.style.height = `${HEIGHT}px`;
    }
    const g = this.canvas.getContext('2d');
    g.setTransform(ratio, 0, 0, ratio, 0, 0);
    g.clearRect(0, 0, W, HEIGHT);
    const css = getComputedStyle(this.root);
    const color = (name) => css.getPropertyValue(name).trim();

    // Gaps are the bare track; footage is drawn per session, continuous within one.
    g.fillStyle = color('--line');
    g.fillRect(0, 0, W, TRACK);
    g.fillStyle = color('--footage');
    let start = null;
    this.segments.forEach((seg, i) => {
      start ??= seg.wall_start;
      const next = this.segments[i + 1];
      if (!next || next.session_id !== seg.session_id || next.wall_start - seg.wall_end > 1000) {
        const x1 = this.xAt(start);
        const x2 = this.xAt(seg.wall_end);
        if (x2 >= 0 && x1 <= W) g.fillRect(x1, 0, Math.max(x2 - x1, 1), TRACK);
        start = null;
      }
    });

    // Live: being recorded, not watchable until its segment finishes.
    if (this.recordingFrom != null) {
      const x1 = Math.max(this.xAt(this.recordingFrom), 0);
      const x2 = Math.min(this.xAt(this.domain[1]), W);
      if (x2 > x1) {
        g.save();
        g.beginPath();
        g.rect(x1, 0, x2 - x1, TRACK);
        g.clip();
        g.strokeStyle = color('--footage');
        g.lineWidth = 2;
        for (let x = x1 - TRACK; x < x2; x += 7) {
          g.beginPath();
          g.moveTo(x, TRACK);
          g.lineTo(x + TRACK, 0);
          g.stroke();
        }
        g.restore();
      }
    }

    if (this.selection) {
      const x1 = this.xAt(this.selection[0]);
      const x2 = this.xAt(this.selection[1]);
      g.fillStyle = 'rgb(0 0 0 / 22%)';
      g.fillRect(x1, 0, x2 - x1, TRACK);
      g.fillStyle = color('--accent');
      g.fillRect(x1 - 1, 0, 2, TRACK);
      g.fillRect(x2 - 1, 0, 2, TRACK);
    }

    if (this.cursor != null) {
      g.fillStyle = color('--accent');
      g.fillRect(Math.round(this.xAt(this.cursor)) - 1, 0, 2, TRACK);
    }

    this.drawTicks(g, W, color('--muted'));
  }

  drawTicks(g, W, muted) {
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
      g.fillRect(x, TRACK, 1, 4);
      const date = new Date(t);
      const midnight = date.getHours() === 0 && date.getMinutes() === 0;
      const text = midnight || step >= DAY
        ? date.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' })
        : date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
      const width = g.measureText(text).width;
      g.fillText(text, Math.min(Math.max(x - width / 2, 0), W - width), TRACK + 6);
    }
  }
}
