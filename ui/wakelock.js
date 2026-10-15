// Keeps the screen on while the player plays. Browsers don't count muted video as playback when
// deciding whether to let the screen sleep, and timelapses have no sound. The Screen Wake Lock API
// is only offered on secure pages, so over plain HTTP this says so instead.

/**
 * `isPlaying()` says whether the screen should stay on; call the returned `update()` whenever that
 * may have changed. `notice` shows why the screen may sleep anyway.
 */
function keepAwake({ isPlaying, notice }) {
  let lock = null;
  let requesting = false;

  const explain = (text) => {
    notice.hidden = !text;
    notice.textContent = text ? 'Screen may sleep' : '';
    notice.title = text || '';
  };

  const update = async () => {
    const wanted = isPlaying() && document.visibilityState === 'visible';
    if (!('wakeLock' in navigator)) {
      if (wanted) {
        explain(window.isSecureContext
          ? 'This browser can’t keep the screen on.'
          : 'Keeping the screen on needs the page over HTTPS.');
      }
      return;
    }
    if (!wanted) {
      // A hidden page loses its lock anyway; release it here only on pause.
      if (lock && !isPlaying()) {
        const held = lock;
        lock = null;
        await held.release().catch(() => {});
      }
      return;
    }
    if (lock || requesting) return;
    requesting = true;
    try {
      lock = await navigator.wakeLock.request('screen');
      explain(null);
      // The browser drops the lock when the page is hidden; visibilitychange asks again.
      lock.addEventListener('release', () => { lock = null; });
      // Paused while the request was pending.
      if (!isPlaying()) update();
    } catch (error) {
      // E.g. refused by the browser's battery saver.
      explain(`The browser refused to keep the screen on: ${error.message}`);
    } finally {
      requesting = false;
    }
  };

  document.addEventListener('visibilitychange', update);
  return update;
}
