-- Long-lived bearer credentials for paired devices (the Apple TV app), which
-- can't do bastion's browser redirect. Issued through the device-code flow in
-- src/server/devices.rs: a signed-in browser approves the code the device
-- shows, and the device is handed a token tied to that user.
--
-- Only the SHA-256 of the token is stored. Identity and perms are frozen at
-- link time, like a cookie session's are at sign-in; re-link to pick up perms
-- granted since.
CREATE TABLE device_tokens (
    id           TEXT PRIMARY KEY,
    token_hash   TEXT NOT NULL UNIQUE,
    user_sub     TEXT NOT NULL,
    login        TEXT NOT NULL,
    perms_json   TEXT NOT NULL,
    name         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL
);

CREATE INDEX device_tokens_user ON device_tokens(user_sub);
