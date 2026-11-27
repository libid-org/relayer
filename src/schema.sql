CREATE TABLE identities (platform BLOB NOT NULL, id_node BLOB NOT NULL,
    id BLOB NOT NULL UNIQUE, wallet BLOB NOT NULL, net BLOB NOT NULL,
    l1 BLOB NOT NULL, deadline INTEGER NOT NULL, status TEXT NOT NULL,
    at INTEGER NOT NULL DEFAULT (unixepoch()), nonce INTEGER, hashes BLOB,
    signed_at INTEGER, worst BLOB, loss BLOB, block INTEGER, block_hash BLOB,
    reverted INTEGER, holds INTEGER, failure TEXT, tx TEXT NOT NULL, send BLOB,
    cancel BLOB, detail BLOB, PRIMARY KEY (platform, id_node),
    CHECK (loss IS NULL OR holds IS NOT NULL));
CREATE INDEX identities_status ON identities (status);
CREATE TABLE nonce (next INTEGER NOT NULL);
