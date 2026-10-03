-- Keyframe index of a segment as JSON [{offset, time}, ...], for byte-range playlists. NULL: not
-- indexed yet; []: indexing failed, so the segment is served whole.
ALTER TABLE segments ADD COLUMN parts TEXT;
