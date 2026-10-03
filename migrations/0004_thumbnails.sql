-- A sprite of keyframe thumbnails per segment, stored next to it as <seq>.jpg. `thumbs` is the
-- number of tiles (NULL: not made yet, 0: making it failed); tile k shows the segment at
-- k * thumb_interval seconds of video.
ALTER TABLE segments ADD COLUMN thumbs INTEGER;
ALTER TABLE segments ADD COLUMN thumb_interval REAL;
