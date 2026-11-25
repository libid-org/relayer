CREATE TABLE requests (id BLOB PRIMARY KEY, wallet BLOB NOT NULL,
    tx TEXT NOT NULL, net BLOB NOT NULL, l1 BLOB NOT NULL, deadline INTEGER NOT NULL,
    status TEXT NOT NULL, block INTEGER, reverted INTEGER, failure TEXT, detail BLOB,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()), gas_used INTEGER, gas_price BLOB, l1_fee BLOB,
    included_at INTEGER, loss BLOB, block_hash BLOB);
CREATE TABLE attempts (hash BLOB PRIMARY KEY, id BLOB NOT NULL,
    nonce INTEGER NOT NULL, cancel INTEGER NOT NULL, raw BLOB NOT NULL,
    block INTEGER NOT NULL, at INTEGER NOT NULL, worst BLOB NOT NULL,
    sent_at INTEGER NOT NULL DEFAULT (unixepoch()));
CREATE INDEX attempts_nonce ON attempts (nonce);
CREATE TABLE nonce (next INTEGER NOT NULL);
