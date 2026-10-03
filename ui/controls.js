// Playback controls shared by the player and the wall: a play/pause button, fullscreen of the
// whole player (video, clock and timeline together), and keyboard shortcuts.
//
//   space / k     play or pause
//   ← / →         back / forward 5 s of video (shift: 30 s)
//   , / .         one frame back / forward (pauses)
//   f             fullscreen

/**
 * `actions` = {toggle(), isPlaying(), jump(videoSeconds), step(frames)}; `root` is what goes
 * fullscreen; `clickTargets` toggle playback when clicked.
 */
function setUpControls({ root, playButton, fullscreenButton, clickTargets = [], actions }) {
  const refresh = () => {
    const playing = actions.isPlaying();
    const glyph = document.createElement('span');
    glyph.className = 'glyph';
    glyph.textContent = playing ? '❚❚' : '▶';
    playButton.replaceChildren(glyph);
    playButton.title = playing ? 'Pause (space)' : 'Play (space)';
  };

  playButton.addEventListener('click', () => { actions.toggle(); refresh(); });
  for (const target of clickTargets) target.addEventListener('click', () => { actions.toggle(); refresh(); });

  const toggleFullscreen = () => {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    else root.requestFullscreen?.().catch(() => {});
  };
  fullscreenButton.addEventListener('click', toggleFullscreen);
  document.addEventListener('fullscreenchange', () => {
    fullscreenButton.textContent = document.fullscreenElement ? 'Exit fullscreen' : 'Fullscreen';
  });

  document.addEventListener('keydown', (event) => {
    // Typing in a form field is not a shortcut.
    if (event.target.closest('input, select, textarea, [contenteditable]') || event.ctrlKey || event.metaKey || event.altKey) return;
    // Space on a focused button presses that button; doing both would toggle twice.
    if (event.key === ' ' && event.target.closest('button')) return;
    const handled = {
      ' ': () => actions.toggle(),
      k: () => actions.toggle(),
      ArrowLeft: () => actions.jump(event.shiftKey ? -30 : -5),
      ArrowRight: () => actions.jump(event.shiftKey ? 30 : 5),
      ',': () => actions.step(-1),
      '.': () => actions.step(1),
      f: toggleFullscreen,
    }[event.key];
    if (!handled) return;
    event.preventDefault();
    handled();
    refresh();
  });

  return { refresh };
}
