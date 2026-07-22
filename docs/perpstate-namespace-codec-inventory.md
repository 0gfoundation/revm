# Off-trie PerpDEX namespace → codec inventory (Stage B ground truth)

**Generated:** 2026-07-22, branch `perpdex` @ 4049bca8 (agent-audited against
`storage/mod.rs` 2505 lines, `storage/keys.rs`, `types/*`, rmp-serde 1.3.0 + alloy-primitives
sources). Line numbers refer to that commit.

**Purpose:** the typed store's `take_delta` must reproduce each namespace's canonical bytes AND
delta key-set semantics EXACTLY (both feed the block commitment). This table is the ground truth
for the Stage B cutover.

## Codec ground rules (verified against rmp-serde 1.3.0)

- `encode<T>()` (mod.rs:38) = positional msgpack: structs = fixarray of field values in
  declaration order, no field names (`#[serde(rename)]` inert).
- Integers: minimal-width msgpack int — positive fixint ≤0x7f, else 0xcc/0xcd/0xce/0xcf;
  negative fixint ≥−32, else 0xd0..0xd3. Signed fields holding non-negative values use UINT markers.
- **i128 = `serialize_bytes(to_be_bytes())` → `0xC4 0x10 ++ 16 BE bytes`** (PerpPosition.last_funding_index,
  FundingState.cumulative_funding_index, PremiumIndexAccumulator.weighted_sum).
- bool = 0xc2/0xc3. serde_bytes fixed arrays = bin8 (`0xC4 len ++ raw`). `Address` = bin8(20)
  (`0xC4 0x14 ++ 20B`). `[u64;30]` = array16 (`0xDC 0x00 0x1E ++ 30 uints`). serde_repr enums = u8 fixint.
- `store_blob(key, &[])` = delete/absent. `save_cached` defers (struct overlay + `ser_blob` at block
  end — byte-identical to immediate `encode`).

## Namespace table

| ns | key fn | value type | codec | writers | readers |
|---|---|---|---|---|---|
| admn | `admin_key()` :158 | `Address` | msgpack bin8(20) | save_admin :460 (save_cached) | load_admin :456 (cached; absent→ZERO) |
| acct | `account_key(user)` :171 | `UserAccount` | fixarray(5): [bin(usdc decimal-string), int pwb(i64), uint mf, uint tf, uint nonce] | save_account :487, mutate_account :502, save_user_fee_rates :534, save_user_nonce :911 | load_account :469, load_account_ref :479, load_user_fee_rates :523, load_user_nonce :904 |
| mfee | `market_fee_total_key(mid)` :177 | `u64` | msgpack uint (VARIABLE width 1–9B, not 8B BE) | add_market_fee_total :552 (**encode+store_blob direct :564; NO-OP when amount==0**) | load_market_fee_total :545 (cached) |
| tcnt | `trade_count_key(mid)` :165 | `u64` | msgpack uint (save_cached deferred) | next_trade_id :893 (ALWAYS writes; returns pre-increment) | (internal) |
| pos | `position_key(user,mid)` :183 | `PerpPosition` | fixarray(12), last = bin8(16) i128 BE | save_position :637 (**side effect: registry_add/remove on amount ZERO-CROSSING :648-653**) | load_position :614 (absent→default leverage=1), load_position_ref :626 |
| bord | `user_buy_orders_key(u,mid)` :188 | `Vec<OrderEntry>` price DESC | msgpack array of fixarray(4): [bin8(32) id, uint price, uint amount, uint mf_bps]. **Empty vec = `0x90` (1B, NOT delete)** | save_buy_orders :755, mutate_buy_orders :812 | load_buy_orders :730, _ref :744 |
| sord | `user_sell_orders_key` :193 | `Vec<OrderEntry>` price ASC | identical to bord | save_sell_orders :794, mutate_sell_orders :831 | load_sell_orders :770, _ref :783 |
| (order) | `order_key(&[u8;32])` :210 = RAW order id | `Order` | fixarray(9): [bin8(20) owner, uint mid, side, price, qty, filled, otype, tif, status] | save_order :867; **delete_order :881 = store_blob(&[])** | load_order :851 → Option, load_order_ref :860 |
| mkt | `market_key(mid)` :216 | `Market` | fixarray(15), field 10 = bool active | save_market :937, mutate_market :949, save_mark_price :1013 | load_market :921 → Option, _ref :930, load_mark_price :1004 |
| mhot | `market_hot_key(mid)` :222 | `MarketHot` | fixarray(4): [bb, ba, last, oi] | mutate_market_hot :982 via save_best_bid/ask :1584/:1598, save_last_traded :1801, refresh_* :1609/:1621 | load_market_hot :972 (absent→zeros) + field accessors |
| preg | `position_registry_key(mid)` :229 | `Vec<Address>` insertion order | **RAW: concatenated 20B addresses; empty = delete** | save_position_registry :689 via registry_add :701 / registry_remove :714 (**write ONLY on membership change**; driven only by save_position hook) | load_position_registry :681 (**raw load_blob + unpack every read, no cache tier**) |
| bidp | `bid_prices_key(mid)` :236 | `Vec<u64>` ASC (best = .last()) | msgpack array of uints | save_bid_prices :1090, mutate :1133 (always writes), **insert_bid_price :1433 (writes ONLY on actual insert)**, remove_bid_price :1476 (always writes) | load_bid_prices :1072, _ref :1082 |
| askp | `ask_prices_key(mid)` :241 | `Vec<u64>` ASC (best = .first()) | identical | save :1116, mutate :1150, insert :1456, remove :1487 | load :1099, _ref :1108 |
| bidl | `bid_level_key(mid,price)` :246 | `LevelBlob{count,ids}` :1221 | **RAW pack_level :1227: count==0 → EMPTY (delete); else count(8B BE) ++ 32B ids.** Deferred via perp_store_struct w/ custom ser_level :1253 | save_level :1347 (via save_bid_level :1375), mutate_level :1170 (push_bid_order :1499, decr_level_count :1551 — count−=n sat, ids.clear() at 0) | load_level :1281, load_level_arc :1312, load_bid_level :1357, _arc :1366, load_bid_count :1532 |
| askl | `ask_level_key` :251 | `LevelBlob` | identical | save_ask_level :1412, push_ask_order :1512, decr_level_count :1551 | load_ask_level :1394, _arc :1403, load_ask_count :1540 |
| apik | `api_key_key(user,key_id)` :264 | `ApiKey` | fixarray(2): [bin8(32) pubkey, uint expiry]. **Direct encode+store_blob :1651** | save_api_key :1644 (also RMWs akid), delete_api_key :1662 (&[] + RMWs akid) | load_api_key :1632 (raw+decode every read) → Option |
| akid | `api_key_ids_key(user)` :269 | `Vec<u8>` key_ids sorted ASC | **msgpack ARRAY of uints (NOT bin — no serde_bytes)**. **Empty after last delete = `0x90` (1B) — key NOT deleted** | save_api_key_ids :1684 (from save_api_key :1657 only when id newly added; from delete_api_key :1670 always) | load_api_key_ids :1673 |
| orcl | `oracle_key()` :280 | `Address` | msgpack bin8(20), direct store_blob :1708 | save_oracle :1703 | load_oracle :1695 (raw; absent→ZERO) |
| mkgr | `market_manager_key()` :289 | `Address` | same, direct :1724 | save_market_manager :1719 | load_market_manager :1711 |
| idxp | `index_price_state_key(mid)` :294 | `IndexPriceState` | fixarray(2): [uint price, uint ts], direct :1746 | save :1740 | load :1729 (raw; absent→default) |
| idxh | `index_price_history_key(mid)` :299 | `IndexPriceHistory` | **fixarray(1) WRAPPING** [array of fixarray(2)] (single-field struct), direct :1768 | save :1762 | load :1751 |
| bswn | `price_basis_window_key(mid)` :304 | `PriceBasisWindow` | fixarray(6): [array16(30), array16(30), uint u8, uint u8, uint, uint], direct :1788 | save :1782 | load :1771 |
| fund | `funding_state_key(mid)` :309 | `FundingState` | fixarray(3): [int rate(i64), uint next_ts, bin8(16) i128 BE], direct :1828 | save :1822 | load :1811 |
| pacc | `premium_accumulator_key(mid)` :314 | `PremiumIndexAccumulator` | fixarray(5): [bin8(16) i128 BE, uint, uint, int i64, uint], direct :1949 | save :1943 | load :1932 |
| infd | `insurance_fund_key()` :325 | **`u64`** (unsigned) | msgpack uint, direct :1846 | save :1841, absorb :1853 (load→min→save) | load :1833 (raw; absent→0) |
| seen | `seen_sig_key(&h)` :338 = "seen"++keccak(sig)[..28] | presence marker | **RAW single byte `[0x01]`**; delete = &[] | mark_signature_seen :1891; **gc_seen_buckets :1911 deletes via `store_blob(B256::new(*id), &[])` :1925 — raw key bytes from bucket, bypassing key derivation** | is_signature_seen :1881 (!is_empty) |
| snbk | `seen_bucket_key(bucket)` :348, bucket = ts/15, retention 6 | `Vec<[u8;32]>` of FULL seen-ns KEYS | **RAW pack_order_ids :1191: concatenated 32B, no count prefix**; empty/GC = &[] | mark_signature_seen :1900 (append), gc :1927 (delete) | (internal) |

Excluded: `cmit` (on-trie sstore, finalize_block_commitment :259), erc20 (on-trie sload/sstore).

## Traps the typed store MUST honor

1. **Delta KEY-SET semantics** — the commitment hashes every WRITTEN key (changed or not).
   Dirty-marking must fire under EXACTLY today's write conditions:
   - `insert_bid/ask_price`: dirty only on actual insert;
   - `registry_add/remove`: dirty only on membership change;
   - `add_market_fee_total`: no write when amount==0;
   - `remove_*_price`, `mutate_*` slow paths, `next_trade_id`, `delete_api_key`: write unconditionally.
2. **Empty ≠ delete for lists**: bord/sord empty vec = `0x90` key present; akid empty = `0x90` key
   present. A delete-on-empty typed store forks the commitment. (Contrast: LevelBlob count==0 →
   EMPTY bytes = delete; preg empty = delete.)
3. **Raw codecs** (never msgpack): preg (20B concat), bidl/askl (pack_level), seen ([0x01]),
   snbk (32B concat). bidl/askl already flow through `perp_store_struct` with the custom `ser_level`
   fn-ptr — the typed store arm reuses `pack_level`.
4. **Bare-value msgpack**: mfee/tcnt/infd = variable-width uint; admn/orcl/mkgr = bin8(20) Address;
   bidp/askp = bare `Vec<u64>`; akid = array-of-uints (adding serde_bytes would fork).
5. **save_position side effect**: registry_add/remove on amount zero-crossing — the typed
   `set_position` path must keep this hook (single choke point for preg).
6. **gc_seen_buckets deletes by raw key bytes** (`B256::new(*id)`) — the snbk bucket stores derived
   keys, so the typed store's seen-ns deletion API must accept a raw B256.
7. **Misc**: save/load_open_interest currently has no production caller; UserFeeRates is a view
   (never stored); load_position_registry re-unpacks every read (no cache — typed store improves
   this legitimately, value-identical); order tombstone = &[] used by 4 modules (delete-on-terminal).
