# IQ Tables

A database portal for [IQ Labs](https://iqlabs.dev) on-chain tables — browse every table on IQ, draft new databases as **ghost data**, and inscribe them to Solana when they're ready. Built to be hosted on IQ's own on-chain web (IQ Pages).

- **Explore** every IQ database and table through the IQ gateway, with sorting, filtering, CSV/JSON export, and an **official / unofficial** filter (official = written by the database's own wallet).
- **Workspace**: build tables and rows locally (type them in, paste CSV/JSON, or upload a file). Nothing costs anything until you inscribe.
- **Per-database wallets**: each database gets its own wallet, derived from a signature by your main wallet. It is the database's on-chain creator (its official identity) and a public donation address with a QR code.
- **Packing + compression**: hundreds of records per inscription, so a 0.001 SOL write carries a whole page of data.
- **Draft changes to live tables**: open an inscribed table in the workspace, edit or delete records, and inscribe only the changes.

Everything is **dependency-free Rust** compiled to WebAssembly: SHA-256/512, Keccak-256, Ed25519, Base58, Solana transaction encoding (legacy and v1), the IQ program's instructions, JSON, a context-mixing compressor, a QR encoder and the UI. The only JavaScript is `web/host.js`, a ~220-line bridge to the DOM, `fetch`, `localStorage` and the wallet, because browsers can't run WebAssembly without it.

## Status

Prototype. The Rust core is checked byte-for-byte against the official SDK (`@iqlabs-official/solana-sdk` 0.2.0), and the whole app passes an end-to-end test against a mock chain that validates every instruction with the program's IDL. **It has not yet written to mainnet.** See [What still needs checking on mainnet](#what-still-needs-checking-on-mainnet).

## Deploy to IQ Pages

`site/` holds the built app: one self-contained `index.html` (about 790 KB, WebAssembly embedded) and `iqpages.json`. With IQ's git CLI (`npm install -g @iqlabs-official/iq-git-cli`):

```bash
cd site
iqgit init
iqgit create iq-tables --public
iqgit add .
iqgit commit -m "IQ Tables v0.1.0"
iqgit push
iqgit pages deploy
```

`iqgit pages deploy` prints the live link (`browser.iqlabs.dev/<commit-table-address>`). Uploading the 790 KB page costs roughly 0.006 SOL. To give it a stable name, point a `.sol` domain at it (the CLI prints the steps). A dedicated domain also gives the portal its own browser origin, which matters because every IQ Pages site otherwise shares `browser.iqlabs.dev` — including its browser storage.

## Run it locally (PyCharm or any terminal)

`run.py` (Python standard library only) serves the app at `http://localhost:8000` and opens your browser. If you've changed the Rust or web sources and Rust is installed, it rebuilds first. In PyCharm: right-click `run.py` → **Run 'run'**. Stop it with the red square.

```bash
python run.py              # rebuild if needed, then serve
python run.py --no-build   # serve the committed build as is
```

Use `http://localhost`, not a `file://` path: wallet extensions like Phantom don't run on `file://` pages.

## Build

```bash
rustup target add wasm32-unknown-unknown
./build.sh            # writes site/index.html, site/iqpages.json, dist/multi/
cargo test --release  # 13 unit tests, incl. byte-for-byte SDK comparisons
```

No crates are used. If the wasm target can't be installed but `rust-src` can, `BUILD_STD=1 ./build.sh` builds the standard library from source.

## How it works

### Packs
A packed table has two on-chain columns, `id` and `p`. Each on-chain row is a *pack* of many records:

```
{"id": "<pack id>", "p": "IQT1z<compressed text>"}
```

- Records are laid out column by column (all values of column 1, then column 2, …) so similar values sit together.
- The layout is compressed by `src/codec.rs`: a bitwise context-mixing compressor (order 0–5 contexts, a match model, a logistic mixer and two SSE stages). On a 120-record parts catalog it beats `gzip -9` by 33% and Brotli-11 by 18%.
- The bytes are written with a 92-character alphabet (printable ASCII minus `"` and `\`), 13 bits per 2 characters, so the text never needs escaping inside the JSON-in-JSON that IQ stores.
- A pack fills one v1 transaction: the metadata JSON must be ≤ 3,400 bytes (700 on the legacy format). The planner packs as many records as fit.
- Every pack carries its own column list, so a table can gain columns later and old packs still decode.
- Records are keyed by the table's id column. Newer packs replace older records with the same id; deletions are tombstone records. Pack ids are content hashes, so an accidental double inscription changes nothing.
- `IQT1j<json>` is the uncompressed variant (readable and searchable by the gateway, about 5× more writes).

In the end-to-end test, 600 randomized parts records packed into 4 inscriptions (about 172 per pack).

### Wallets
- Your wallet (Phantom, Solflare, Backpack — anything implementing the Wallet Standard) is your account. It signs one funding transfer and the unlock messages; it never signs the bulk writes.
- **Database wallet** = Ed25519 keypair from `SHA-256("iq-tables/db-wallet/v1" ‖ signature)`, where `signature` is your wallet's signature of the message in `state::derivation_message(name)`. Ed25519 signatures are deterministic, so the same wallet always recovers the same database wallet on any device, and nothing is stored. On first use the app asks for two signatures and checks they match before relying on this.
- **The message and domain string are frozen.** Changing either would change every database wallet's address.
- The database wallet creates the DbRoot (so it is the creator and the *official* signer), optionally locks table creation to itself, creates tables (each **locked** = only it may write, or **open** = anyone, shown as unofficial), and signs every pack with the v1 transaction format.
- Anyone can send SOL to a database wallet to fund future inscriptions: that's the donation address. **Withdraw** sweeps it back; **Reveal secret key** exports it for Phantom.
- Contributors use the same flow: their own main wallet derives their own wallet for that database, and their rows show as unofficial.

### Inscribing
Each step (create DbRoot, create table, one-time account setup, each pack) is **simulated first** through the RPC: program errors and the exact cost appear before any SOL moves. The step is then sent and polled until confirmed. Progress is saved after every step, so closing the tab and pressing Resume continues where it left off. If the RPC rejects v1 transactions, Auto mode falls back to legacy transactions and re-plans packs.

### Costs (from IQ Labs' own cost model)
| Item | Cost |
|---|---|
| Each pack (direct write) | 0.001 SOL program fee + 0.000005 SOL network fee |
| First write from a new wallet | ~0.062 SOL one-time rent (IQ user accounts) |
| Create a database | ~0.002–0.003 SOL rent |
| Create a table | rent + IQ's table-creation fee (not published; the simulation shows the exact amount before sending) |

## Source map
| File | What |
|---|---|
| `src/crypto/` | SHA-256/512, Keccak-256, Base58, Ed25519 (TweetNaCl port) |
| `src/solana.rs` | PDAs, message compilation, legacy + v1 wire formats |
| `src/iq.rs` | IQ program: seeds, PDAs, instruction encoders, account decoders |
| `src/codec.rs` | Compressor + text-safe encoding |
| `src/pack.rs` | Pack layout, planner, merge (latest wins, tombstones) |
| `src/app.rs` | State, events, explorer/wallet/funding flows |
| `src/inscribe.rs` | Simulate → send → confirm pipeline |
| `src/views.rs` | HTML rendering |
| `src/qr.rs` | QR encoder for donation addresses |
| `web/host.js` | The browser bridge |
| `tools/` | Reference fixtures from the official SDK and the end-to-end test (dev only) |

## Tests
- `cargo test --release`: hashes, Base58, Ed25519 signatures and 150 random PDAs against the SDK and noble; every instruction (`initialize_db_root`, `manage_table_creators`, `create_table` open/locked, `user_initialize`, `db_code_in`, `realloc_account`) byte-for-byte against the SDK's builder; full v1 transactions byte-identical to the SDK's `buildV1Transaction`; JSON escaping identical to `JSON.stringify`; codec and pack round-trips; merge rules.
- `cd tools && npm install && CHROME_PATH=/path/to/chrome npm run e2e`: 32 checks. The built page runs in headless Chromium against a mock chain that verifies every signature (both wire formats), decodes every instruction with the program's IDL, and compares each instruction's accounts and data with the SDK's builder. Covered: explore, search, HTML escaping, derivation (checked independently), funding, CSV import, packing, inscription with DbRoot realloc, locked vs open tables, read-back, editing a live record, unofficial contributions, rejecting writes to a locked table at simulation, and mobile layout.
- `npm run qr`: decodes the generated QR with jsQR.

## What still needs checking on mainnet
The mock follows the SDK and IDL, but these are assumptions until a real run:
1. **Writer locks**: whether the program itself enforces a table's `writers` list (the SDK does client-side; the mock assumes the program does too).
2. **Table-creation fee**: amount unknown; the simulation reveals it.
3. **Schema checks**: whether the program checks row keys against the columns on-chain (packs always use `id` and `p`, so either way works).
4. **v1 through browser RPCs**: whether `simulateTransaction` accepts v1 transactions on your RPC. If not, turn off simulation in Settings, or let Auto fall back to legacy.
5. **RPC**: the public endpoint is heavily rate-limited; use your own RPC URL (Helius, QuickNode, Triton) in Settings.

A first real run with ~0.1 SOL (one small database, one table, one pack) answers all five.

## License
MIT — see LICENSE.
