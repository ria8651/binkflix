-- Per-service keypair used to identify binkflix to bastion. Generated on
-- first boot, persisted across restarts. Public JWK is shipped to bastion in
-- POST /api/services/register; private JWK signs short-lived JWTs for
-- authenticated calls back to bastion (e.g. permission-catalog re-sync).
--
-- Singleton (`id = 1`). If the row is lost, binkflix generates a fresh
-- keypair and bastion sees a key mismatch — admin must approve a new
-- registration.

CREATE TABLE service_keypair (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  private_jwk  TEXT    NOT NULL,
  public_jwk   TEXT    NOT NULL,
  kid          TEXT    NOT NULL,
  created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
  approved_at  INTEGER NULL
);