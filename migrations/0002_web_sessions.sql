-- Browser logins. Only a hash of each token is stored, so a copy of the database can't be used
-- to log in.
CREATE TABLE web_sessions (
    token_hash TEXT    PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
