CREATE TABLE identities (platform BLOB NOT NULL, id_node BLOB NOT NULL,
    id BLOB NOT NULL UNIQUE, wallet BLOB NOT NULL, tx TEXT NOT NULL, net BLOB NOT NULL,
    l1 BLOB NOT NULL, deadline INTEGER NOT NULL, status TEXT NOT NULL,
    at INTEGER NOT NULL DEFAULT (unixepoch()), nonce INTEGER, send BLOB, cancel BLOB,
    hashes BLOB, signed_at INTEGER, worst BLOB, loss BLOB, block INTEGER,
    block_hash BLOB, reverted INTEGER, failure TEXT, detail BLOB,
    PRIMARY KEY (platform, id_node));
CREATE INDEX identities_status ON identities (status);
CREATE TABLE nonce (next INTEGER NOT NULL);
