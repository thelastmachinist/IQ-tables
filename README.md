# IQ Tables

A database portal for [IQ Labs](https://iqlabs.dev) on-chain tables — browse every table on IQ, draft new databases as **ghost data**, and inscribe them to Solana when they're ready. Built to be hosted on IQ's own on-chain web (IQ Pages).

- **Explore** every IQ database and table through IQ's gateway, with sorting, filtering, CSV/JSON export, and an **official / unofficial** filter (official = written by the database's own wallet).
- **My tables**: everything your account has made in one place — databases your wallets own (with their tables and balances), drafts in this browser, and files your wallets inscribed.
- **Your wallet is your account**: drop in its key file (a Solana key file like `id.json`) or paste the secret key, and you're signed in. The key stays in the tab's memory — nothing is stored, no name or email, no wallet extension — and closing the tab signs out. No wallet yet? *Make a new wallet* downloads its key as a Solana key file.
- **Money in plain words**: *Add funds* shows the address and a QR code to send SOL to from an exchange; *Send* takes an address or a `name.sol`. Databases belong to the wallet you sign in with, which pays for saves.
- **Editor, laid out like phpMyAdmin**: databases and tables (and views) in a tree; a database has *Structure* (its tables with Browse / Structure / Search / Insert / Empty / Drop, create a table, import a spreadsheet as a table, views), *SQL*, *Search* (every table at once), *Export*, *Import*, *Operations* and *Save*; a table has *Browse* (a spreadsheet: type in cells, Tab/Enter/arrows, paste from Excel or Google Sheets, undo/redo, sort, filter), *Structure* (columns with types, "can be empty", defaults, auto-numbering, comments; Change / Drop / Primary / Unique / Index; keys, relations and rules; `SHOW CREATE TABLE`), *SQL*, *Search* (query by example, find and replace), *Insert* (a typed form), *Export* (CSV, JSON, SQL), *Import* (CSV, JSON, SQL), *Operations* (rename, comment, who can add rows, copy, compression, empty, delete) and *Save*. Every screen builds a SQL statement, runs it through the same engine as the SQL tab and shows it afterwards. Changes are held as ghost data until you press **Save to blockchain**, which shows the cost first.
- **A MySQL-style SQL server in the browser**: joins, subqueries, `UNION`/`INTERSECT`/`EXCEPT`, CTEs (incl. recursive), `GROUP BY … WITH ROLLUP`, window functions, ~100 functions, typed columns, `PRIMARY KEY`/`UNIQUE`/`FOREIGN KEY` (with `CASCADE`/`SET NULL`)/`CHECK`/`NOT NULL`/`DEFAULT`/`AUTO_INCREMENT`, `ALTER TABLE` (add, drop, modify, change, rename and reorder columns; add and drop keys, relations and rules), `RENAME TABLE`, `TRUNCATE`, `DROP TABLE`, views, `GRANT`/`REVOKE`, `SHOW …`, `EXPLAIN`, and loading phpMyAdmin / mysqldump exports. Structure changes to saved tables cost one small write — rows already on chain aren't rewritten.
- **Power users** can add more keys for a session, make extra wallets derived from their key (the same key always brings them back), move SOL between wallets, and download a passphrase-protected copy of their keys.
- **Links and files in cells**: `https://…`, `name.sol` (opens in IQ's browser), `iq://table/<table>/<record>`, `iq://db/<database>` and `iq://tx/<signature>` are clickable. Attach a file (up to 4 MB) to a row and it's inscribed with IQ's own file instruction — in parts with IQ's chunked upload when it's bigger than one transaction — then linked from the cell.
- **Live links to IQ git projects**: paste a repository's IQ browser link (`https://browser.iqlabs.dev/<repository>`) into a cell and it shows the project's newest commit, with its history and files one click away. A table listing software stays current while the projects change — the table itself is never rewritten. "Copy link to this version" gives a link that never changes.
- **Rows of any size**: a row too big for one transaction (a whole program in a cell, say) is written in parts with IQ's chunked upload and read back as one row. The parts go out in parallel with IQ's own upload speed profiles.
- **Crowdfunded uploads**: publish a big file's fingerprints (one SHA-256 per piece) in an open table; anyone can pay to upload pieces from their own balance — from their copy of the file or straight from a web server — and everyone who downloads it gets exactly the organizer's file, whoever uploaded which piece.
- **Packing + compression**: hundreds of records per inscription, so a 0.001 SOL write carries a whole page of data. A save that would take four or more writes goes out as one write sent in parts instead, which is cheaper and lands all at once.
- **Checkpoints** (`OPTIMIZE TABLE`, or Operations → Checkpoint): rewrites a table's current rows in one write so readers start there instead of replaying its whole history.
- **Edit live tables**: open a saved table in the Editor, change or delete records, and only the changes are written.

Everything is **dependency-free Rust** compiled to WebAssembly: SHA-256/512, Keccak-256, Ed25519, PBKDF2, AES-256-GCM, Base58, Solana transaction encoding (legacy and v1), the IQ program's instructions, JSON, a context-mixing compressor, a QR encoder and the UI. The only JavaScript is `web/host.js`, a ~360-line bridge to the DOM, `fetch`, `localStorage` (drafts and settings only) and files, because browsers can't run WebAssembly without it.

## Status

Prototype. The Rust core is checked byte-for-byte against the official SDK (`@iqlabs-official/solana-sdk` 0.2.0), the whole app passes an end-to-end test against a mock chain that validates every instruction with the program's IDL, and its transactions have been dry-run against IQ's deployed program on devnet (see [Checked against the real program](#checked-against-the-real-program)). **It has not yet written to mainnet.** See [Before using it on mainnet](#before-using-it-on-mainnet).

## Deploy to IQ Pages

`site/` holds the built app: one self-contained `index.html` (about 2 MB, WebAssembly embedded) and `iqpages.json`. With IQ's git CLI (`npm install -g @iqlabs-official/iq-git-cli`; on first run it asks for your wallet's key file and an RPC URL — a free Helius one works):

```bash
cd site
iqgit init
iqgit create iq-tables --public
iqgit add .
iqgit commit -m "IQ Tables v0.1.0"
iqgit push
iqgit pages deploy
```

`iqgit push` uploads the files (the 2 MB page goes up in parallel parts — `--speed medium` or `heavy` if your RPC allows; the default, `light`, is one part at a time). `iqgit pages deploy` is done once: it adds the repository to IQ Pages' gallery and charges IQ's one-time **0.2 SOL** deploy fee. The site at `browser.iqlabs.dev/<commit-table-address>` is served from the repository's **latest commit**, so each later `iqgit push` updates it with no re-deploy; unchanged files aren't uploaded again. Every commit also stays readable at its own address, so a version can be pinned.

Rough cost of the first deploy, from a wallet IQ has never seen: ~0.28 SOL — the 0.2 SOL deploy fee, ~0.05 SOL one-time IQ account setup (a deposit), ~0.017 SOL for the repository's table, ~0.01 SOL for the page itself and a few 0.001 SOL writes. Each later update is ~0.012 SOL.

All IQ Pages sites share the origin `browser.iqlabs.dev` (the same browser storage). That's why IQ Tables keeps nothing about accounts in the browser: keys live in the tab's memory only, and storage holds just drafts and settings. Because another site could change those settings, `.sol` names in *Send* are always resolved through IQ's own gateway, and a banner shows whenever tables are read from a gateway other than IQ's.

## Run it locally (PyCharm or any terminal)

`run.py` (Python standard library only) serves the app at `http://localhost:8000` and opens your browser. If you've changed the Rust or web sources and Rust is installed, it rebuilds first. In PyCharm: right-click `run.py` → **Run 'run'**. Stop it with the red square.

```bash
python run.py              # rebuild if needed, then serve
python run.py --no-build   # serve the committed build as is
```

## Build

```bash
rustup target add wasm32-unknown-unknown
./build.sh            # writes site/index.html, site/iqpages.json, dist/multi/
cargo test --release  # 38 unit tests, incl. byte-for-byte SDK comparisons
```

No crates are used. If the wasm target can't be installed but `rust-src` can, `BUILD_STD=1 ./build.sh` builds the standard library from source.

## How it works

### Accounts and wallets
- **Signing in** is dropping in a wallet's key anywhere on the page (or choosing the file, or pasting the secret key): a Solana CLI keypair file (`[12,34,…]`, 64 bytes), a list of them, or text with base58 secret keys (one per line, optionally `label: key`). The first key is the main wallet. Keys live in the tab's memory only; nothing is written to the browser's storage, and closing the tab signs out.
- **Make a new wallet** generates a key, downloads it as `wallet-<address>.json` in the Solana CLI format (usable with the Solana CLI and IQ's `iqgit`), and signs in with it. That file is the only way into the wallet.
- **Add funds** shows the main wallet's address and QR code; **Send** accepts an address or a `.sol` name (resolved through IQ's gateway `/sns/<name>`), and shows a review step before anything moves. Buying SOL with a card needs a licensed on-ramp, which the portal doesn't embed.
- **Databases belong to the wallet you sign in with**: it creates the DbRoot (so it's the creator and the *official* signer), optionally locks table creation to itself, creates tables (each **locked** = only it may write, or **open** = anyone, shown as unofficial), and signs every pack. A database can use another of your wallets instead (Save → Advanced); if that wallet runs short, saving moves what's needed from the main balance first.
- **Extra wallets** are derived from the main key: `master = SHA-256("iq-tables/key-master/v1" ‖ key seed)`, `seed(i) = SHA-256("iq-tables/account/v1/wallet" ‖ master ‖ u32le(i))`. Signing in with the same key rescans for derived wallets with on-chain activity and brings them back. **The domain strings are frozen**; changing them would change every derived address.
- A **passphrase-protected copy** of every wallet (`<name>.iqaccount.json`) is optional. It's encrypted with the IQ SDK's own `passwordEncrypt` scheme — PBKDF2-SHA256 × 250,000 → AES-256-GCM — so the SDK's `passwordDecrypt` opens it too (the tests check both ways). Dropping it in asks for the passphrase.
- Contributors use the same flow with their own wallet; their rows show as unofficial.

### Editor
- The sheet shows the saved rows (read from the chain, filtered to the database's own wallet and yours) with your unsaved changes on top: new rows are green, edited cells amber, deleted rows struck through, and cells a row still needs before it can be saved are outlined red. Headers show each column's type. The Save tab and the bar above the sheet count the changes and show the cost.
- Excel keys: type to replace, Enter/F2 to edit in place, Tab/Enter to move, arrows and Shift+arrows, Ctrl+C / Ctrl+V (tab-separated, so pasting from Excel or Sheets adds rows and columns as needed), Delete, Ctrl+Z / Ctrl+Y (a paste undoes in one step). Column menus sort, rename, insert, move, set the ID column, open the column's type and rules, and delete columns.
- New tables start the way most people want them: an automatic `id` number, then the named columns (untyped until you give them a type). Typing, pasting, the Insert form and SQL all go through one set of rules (`src/constraints.rs`): types are converted or rejected, keys must be unique, relations must point at existing rows (or cascade), rules must hold. The sheet lets you leave required cells for later and lists what's missing on the Save tab.
- **SQL** (`src/sql/`, `src/sql_exec.rs`, `src/ddl.rs`) reads the saved + unsaved rows; writes become unsaved changes like any edit, so `ROLLBACK`, Undo and the Save tab all work on them. Comparisons follow MySQL's defaults (case-insensitive text, `'10' = 10`). It runs entirely in the page over rows read from the chain — there are no indexes, and none are needed at these sizes.
- Views, and the rows of IQ Tables' own settings table (`_iqt`, hidden in the editor), are stored in the database like any table, so everyone who opens it sees them. Saved queries (bookmarks) stay in this browser.

### Structure on chain
A table's on-chain columns stay `id` and `p` forever; its *SQL* structure lives in **structure records** written into the table like packs (`{"id":"~s…","p":"IQT1s<json>"}`, compressed as `IQT1S`). Only records from the database's own wallet count, and the newest wins. A record holds the columns (display name, type, "can be empty", default, auto-numbering, comment), the primary key, unique keys and indexes, relations, rules and the table comment.

- Records in packs are stored under **storage keys**, not display names, so renaming a column, reordering columns or changing a type is one structure record — the rows already on chain are read through the new structure, converted by type on the way out. Dropped columns' keys are retired, never reused. A column added to a saved table records the value older rows should show (its default).
- Changing the primary key re-keys rows when they are read; `TRUNCATE` writes a record that hides everything saved before it.
- `RENAME TABLE` and who-can-add-rows (`GRANT`/`REVOKE INSERT`, or Operations) use IQ's `update_table` instruction; the table's address never changes. `DROP TABLE` hides the rows and takes the table off the database's list with `update_db_root_table_list`. Both are allowed only for the database's creator — checked against the deployed program on devnet (see below).
- Nothing on chain is ever erased: dropped tables, deleted rows and old structures stay in the chain's history; IQ Tables stops showing them.

### Packs
A packed table has two on-chain columns, `id` and `p`. Each on-chain row is a *pack* of many records:

```
{"id": "<pack id>", "p": "IQT1z<compressed text>"}
```

- Records are laid out column by column (all values of column 1, then column 2, …) so similar values sit together.
- The layout is compressed by `src/codec.rs`: a bitwise context-mixing compressor (order 0–5 contexts, a match model, a logistic mixer and two SSE stages). On a 120-record parts catalog it beats `gzip -9` by 33% and Brotli-11 by 18%.
- The bytes are written with a 92-character alphabet (printable ASCII minus `"` and `\`), 13 bits per 2 characters, so the text never needs escaping inside the JSON-in-JSON that IQ stores.
- A pack fills one transaction: the metadata JSON must be ≤ 3,400 bytes with v1 transactions (700 with legacy ones). The planner packs as many records as fit.
- Every pack carries its own column list, so a table can gain columns later and old packs still decode.
- Records are keyed by the table's id column. Newer packs replace older records with the same id; deletions are tombstone records. Pack ids are content hashes, so an accidental double inscription changes nothing.
- `IQT1j<json>` is the uncompressed variant (readable and searchable by the gateway, about 5× more writes).

In the end-to-end test, 600 randomized parts records packed into 4 inscriptions (about 150 per pack) — and since four direct writes cost more than one chunked write, the save went out as a single write sent in 3 parts.

### Bigger than one transaction
IQ already has a way to store data bigger than a transaction — the SDK's chunked `codeIn` — and IQ Tables uses it as is:

- Under 10 parts, the data goes out as a **linked list**: each part is a `send_code` transaction naming the one before it (the first names `Genesis`), and a final `db_code_in` (or `user_inventory_code_in` for a file) points at the last one.
- From 10 parts up, it uses an **upload session**: `create_session` (numbered by the wallet's `total_session_files`), one `post_chunk` per part, then the final write points at the session.
- Parts are 3,600 bytes with v1 transactions (850 with legacy ones), split on UTF-8 boundaries like the SDK. The final write carries `{filetype, method, filename, total_chunks}` without the data.
- IQ's gateway reassembles chunked rows and files; reading straight from Solana, IQ Tables follows the linked list or collects the session's `post_chunk` transactions itself.
- Rows larger than a pack are sent this way automatically, and so is a whole save when one chunked write is cheaper than several direct ones. If a save fails or the tab closes midway, the unfinished write's parts are sent again (they cost only the network fee, and an open upload session is reused).

**Parallel parts.** A session's parts carry their own numbers, so — like IQ's SDK (`uploadSession`) — IQ Tables sends them without waiting for each other, using the SDK's speed profiles (Settings → *Upload speed*): light (1 at a time, 2/s, the SDK's default), medium (5, 50/s), heavy (50, 100/s — IQ Tables' default) or extreme (100, 250/s). Parts are signed locally and sent without preflight; landed parts are found with `getSignatureStatuses`, 256 at a time; a part not seen after 20 s is sent again with a fresh blockhash (if both copies land, readers take either, so a duplicate only costs its network fee); when the RPC answers "too many requests", the number in flight halves and then climbs back. Only after every part has landed does the finalizing write go out. Stop keeps the parts already on chain, and Resume sends the rest. The practical limit is how many requests per second your RPC accepts.

### Checkpoints
A table's rows are rebuilt by replaying every pack and structure record ever written to it. After many edits, a **checkpoint** (`OPTIMIZE TABLE t`, `CHECKPOINT t`, or Operations → Checkpoint) makes the next save write the table's current rows as a snapshot (one chunked write when that's cheaper), then a structure record listing the snapshot's packs. Readers walk the history newest-first and stop at the newest official checkpoint whose packs they have, so opening the table reads a few writes instead of all of them. Operations shows how many writes opening the table takes today. Nothing is erased: the older history is still on chain; readers just don't need it.

### Crowdfunded uploads
For big files many people want kept — public-domain software, say — where no single person should have to pay for all of it.

- **The organizer** chooses the file in the Editor (a database's Structure tab → *Crowdfund a big file*). The browser fingerprints it a piece at a time (1 MB pieces up to 16 MB, 4 MB above; files up to 8 GB, read a piece at a time) and makes a table that anyone may add rows to. Saving writes the table and its structure record, which carries the **manifest**: file name, size, type, SHA-256 of the whole file, piece size, the SHA-256 of every piece, and optionally a web address where the file can be read and a description.
- **Anyone** can then open the table's page and upload missing pieces, paid from their own balance: from their own copy of the file, or straight from the manifest's web address (read a byte range at a time; the server has to allow browsers to read it). Each piece is checked against its fingerprint before anything is sent, uploaded as an ordinary IQ file (`user_inventory_code_in`, in parallel parts), then recorded with a small row in the table: piece number, SHA-256, and the file's signature. Pieces are picked at random so different people rarely upload the same one.
- **Readers** read the table's whole history and use the first manifest the organizer published (a later one is shown as a warning and ignored), and accept a piece only if its bytes hash to the manifest's value, trying other copies if one doesn't match. *Download* assembles the pieces, checks each one and then the whole file, and saves it. It doesn't matter which wallet uploaded what.
- A row can claim the right fingerprint for the wrong bytes, which makes a piece look present until someone downloads it. The download skips such copies, and in that browser the piece shows as missing again so it can be uploaded; other visitors only find out when they download.
- The pieces are ordinary IQ files, so any IQ tool can open them one by one; putting them back together follows this manifest, which only IQ Tables reads today. Downloading goes through IQ's gateway.

### Live links to IQ git
IQ Labs' git keeps each repository's commits in an IQ table (`git_commits:<owner>:<repo>` in the database `iq-git-v1`, whose address is what `browser.iqlabs.dev/<address>` shows). A cell holding that link reads the table's meta to confirm it's a repository — the address must be the one IQ git derives for that name — then its commits, newest first, counting only commits signed by the repository's owner. The cell shows the name, newest commit message and age; clicking it opens the history and the files of any commit (each commit's tree is an IQ inscription listing its files, and each file an inscription), with links to open it in IQ's browser. A commit list is re-read when the page shows it again after two minutes, or on Refresh. A live link follows the project; for a fixed version, "Copy link to this version" gives `iq://tx/<commit's tree>#<repo>@<commit>`, which opens that commit's files and never changes. Live links read through IQ's gateway, so in *Solana directly* mode they show as plain links.

### Links and files
- Any cell whose value is a link renders as one: web links open in a new tab; `name.sol` opens `browser.iqlabs.dev/name.sol`; other addresses open in IQ's browser; `iq://table/<table>` / `iq://table/<table>/<record id>` and `iq://db/<database>` open in the explorer (with "database › table › record" as the label); `iq://tx/<signature>` and bare signatures open the inscription viewer.
- Every table, database and record has a **copy link** button, so linking a row to a record in another table is copy → paste.
- **Attaching a file** (📎 on a ghost row) inscribes it with `user_inventory_code_in`, exactly like the SDK's `codeIn`: same metadata, text files stored as text, binary as base64. Files that fit one transaction (~2.4 KB binary / ~3.2 KB text with v1) are one direct write; bigger ones, up to 4 MB, go in parts (see [Bigger than one transaction](#bigger-than-one-transaction)). The cell gets `iq://tx/<signature>#<filename>`. A 4 MB file is roughly 1,550 parts, sent several at a time (see *Parallel parts*).
- **IQ git links**: see [Live links to IQ git](#live-links-to-iq-git).
- **Opening a file** asks IQ's gateway first (`/data/<signature>`) and falls back to reading the transaction from Solana. Images preview inline, text shows as text, packs decode to their records, anything can be downloaded. "My tables" lists every file your wallets inscribed (IQ gateway `/user/<wallet>/assets`), including ones made with other IQ tools.

### Inscribing
Each step (create DbRoot, create table, one-time account setup, each pack) is **simulated first** through the RPC: program errors and the exact cost appear before any SOL moves. The step is then sent and polled until confirmed. Progress is saved after every step, so closing the tab and pressing Resume continues where it left off.

Like the SDK, Auto mode uses v1 transactions only when the cluster's v1 feature gate (`txv1aq4…GLL`) is active, and before the first v1-sized write it grows a wallet's IQ accounts if they were made by the pre-upgrade program (`realloc_account` to 4,213 / 4,215 bytes).

### Reading
By default tables are read through IQ's gateway (fast, cached, with search and files). Settings → *Read tables from → Solana directly* reads live instead: the database list via `getProgramAccounts`, table metadata via `getAccountInfo`, and rows by walking the table's transaction history (`getSignaturesForAddress` + batched `getTransaction`) and decoding each inline `db_code_in`. On devnet, reads go through IQ's devnet gateway (`dev-gateway.iqlabs.dev`).

### Costs (measured against IQ's program on devnet)
| Item | Cost |
|---|---|
| Each pack or file (direct write) | 0.001 SOL program fee + 0.000005 SOL network fee |
| First write from a new wallet | ~0.05 SOL one-time rent (IQ user accounts: 4,213 + 4,215 bytes + user state) |
| Create a database | ~0.0115 SOL rent (2,133-byte account) |
| Create a table | ~0.015 SOL rent + 0.00093 SOL IQ table-creation fee |
| Change a saved table's structure (`ALTER`, `TRUNCATE`) | one write: 0.001 SOL + 0.000005 SOL |
| Rename a table or change who may add rows | 0.000005 SOL (plus rent if the table account has to grow) |
| Drop a saved table | one write + 0.000005 SOL |
| A row or file in 2–9 parts (linked list) | 0.003 SOL program fee + 0.000005 SOL per transaction |
| A row or file in 10+ parts (upload session) | 0.005 SOL program fee + ~0.00073 SOL session rent + 0.000005 SOL per transaction |
| A crowdfunded piece of 1 MB / 4 MB | ~0.0087 / ~0.0145 SOL (a session write, plus a direct write for its row) |

Where the money goes: the program fees (0.001 / 0.003 / 0.005 SOL per write) go to IQ's fee wallet (`EWNSTD8…4wZ1`). The network fee (0.000005 SOL per signature) is Solana's: half is burned, half goes to the validator that included the transaction. Rent is a deposit held in the accounts a write creates (database, table, a wallet's IQ accounts, each upload session) — not spent, but IQ's program has no instruction that closes these accounts, so it can't be withdrawn today. Whatever is left in a database wallet stays yours. The simulation before each step shows the exact amount on the cluster you're using.

## Source map
| File | What |
|---|---|
| `src/crypto/` | SHA-256/512, Keccak-256, Base58, Ed25519 (TweetNaCl port), PBKDF2 + AES-256-GCM |
| `src/solana.rs` | PDAs, message compilation, legacy + v1 wire formats, transaction parsing |
| `src/iq.rs` | IQ program: seeds, PDAs, instruction encoders and decoders, account decoders |
| `src/codec.rs` | Compressor + text-safe encoding |
| `src/pack.rs` | Pack layout, planner, merge (latest wins, tombstones) |
| `src/account.rs` | Accounts: key formats, derived wallets, the passphrase-protected file |
| `src/accounts_flow.rs` | Sign in/out with keys, new wallets, Add funds / Send, transfers, rescans, airdrops |
| `src/sheet.rs` | Spreadsheet model: saved rows + unsaved changes, cell edits, paste, columns |
| `src/editor.rs` | Editor state: selection, keys, undo/redo, saving |
| `src/schema.rs` | Column types (conversion and display), column rules, keys, structure records |
| `src/constraints.rs` | Applying changes under the table's rules (types, keys, relations with cascades, checks) |
| `src/sql/` | SQL lexer, parser, expression evaluator (functions, regular expressions) and query engine |
| `src/sql_exec.rs`, `src/ddl.rs` | Running statements against a database: DML, DDL, SHOW, dumps |
| `src/dates.rs` | Dates and times for SQL (parsing, arithmetic, `DATE_FORMAT`) |
| `src/ws_actions.rs` | What the editor's screens do (each builds and runs SQL) |
| `src/app.rs` | State, routing, events, explorer and workspace flows |
| `src/chain.rs` | Reading databases and rows straight from Solana |
| `src/attach.rs` | Inscribing files into cells (direct or in parts); the inscription viewer |
| `src/git.rs` | Live links to IQ git repositories: commits, files, pinned versions |
| `src/upload.rs` | Sending a session's parts in parallel (IQ's speed profiles, bulk confirmation, resends, rate-limit back-off) |
| `src/crowd.rs` | Crowdfunded uploads: manifests, fingerprinting, contributing pieces, checked downloads |
| `src/inscribe.rs` | Simulate → send → confirm pipeline |
| `src/views.rs`, `src/views_account.rs`, `src/views_ws.rs` | HTML rendering (explorer, account, editor) |
| `src/qr.rs` | QR encoder for donation addresses |
| `web/host.js` | The browser bridge (DOM, fetch, storage, keys and paste, files, drag and drop) |
| `tools/` | Reference fixtures from the official SDK and the end-to-end test (dev only) |

## Tests
- `cargo test --release` (38 tests): hashes, Base58, Ed25519 signatures and 150 random PDAs against the SDK and noble; every instruction (`initialize_db_root`, `manage_table_creators`, `create_table` open/locked, `user_initialize`, `db_code_in`, `user_inventory_code_in`, `realloc_account`, `update_table` open/locked, `update_db_root_table_list`, and the chunked path: `send_code`, `create_session`, `post_chunk`, linked-list and session `db_code_in` / `user_inventory_code_in`, session PDAs and chunk splitting) byte-for-byte against the SDK's builder; IQ git addresses against the ones on mainnet; full v1 transactions byte-identical to the SDK's `buildV1Transaction`; PBKDF2 and AES-GCM against Node; opening the SDK's `passwordEncrypt` output; parsing our own transactions back; passphrase-file round-trips, key formats and signing in with a key (derived wallets reproducible from the key); JSON escaping identical to `JSON.stringify`; codec and pack round-trips; merge rules; the spreadsheet model (edits, paste, saved-column rules); the SQL engine (joins, subqueries, grouping, window functions, functions), DDL and table rules, structure records, dumps and importing a phpMyAdmin export; checkpoints (merge, early stop) and choosing between direct and chunked writes; crowdfunded manifests (first one wins, wrong-hash records ignored) and piece costs.
- `cd tools && npm install && CHROME_PATH=/path/to/chrome npm run e2e` (155 checks): the built page in headless Chromium against a mock chain that verifies every signature (both wire formats), decodes every instruction with the program's IDL and compares its accounts and data with the SDK's builder, plus a mock IQ gateway. Covers: explore, search, HTML escaping; signing in with a key (a new wallet's Solana key file, dropping it in, pasting the secret key, nothing stored in the browser, a derived wallet found again, the passphrase-protected copy opened with the SDK's `passwordDecrypt`, wrong passphrase), Add funds, sending to a `.sol` name; the spreadsheet (typing, Tab/Enter, paste from Excel, undo/redo, CSV import of 600 rows); SQL (DROP/CREATE/INSERT, WHERE/ORDER BY, GROUP BY, DELETE, SHOW CHANGES, COMMIT); saving with the signed-in wallet as the database's owner; moving SOL between wallets; CSV import and packing; attaching a file (wallet setup + `user_inventory_code_in`); inscription with DbRoot realloc; locked vs open tables; web, record and file links; the file viewer and download; record links; My tables; editing a live record; unofficial contributions; phpMyAdmin-style administration of saved tables (the column form, `ALTER TABLE`, unique keys and rules enforced, the Insert form, query-by-example search with Edit links, find and replace, rename and writers via `update_table`, views, a JOIN over saved and unsaved rows, a MySQL-style dump, `TRUNCATE`, `DROP TABLE` via `update_db_root_table_list`, and a clean browser rebuilding the structure from the chain); a 600-row save as one linked-list write; a 40 KB file and a 70 KB program sent through upload sessions in parallel and read back byte for byte (also straight from Solana), through an RPC that rate-limits and one that drops parts; a crowdfunded 2.3 MB file (fingerprinted, one piece from a contributor's copy, the rest from a web server by byte range, a forged record skipped, the download identical to the original); a checkpoint, and a clean browser opening the table from it; live IQ git links (newest commit, a new commit appearing without touching the table, a look-alike commit from another wallet ignored, files, pinned versions); growing pre-upgrade accounts before a v1 write; rejecting writes to a locked table at simulation; reading everything back straight from Solana with batched RPC; devnet airdrops; mobile layout.
- `npm run qr`: decodes the generated QR with jsQR.

## Checked against the real program
IQ's program is deployed on devnet, and devnet has v1 transactions switched on. The portal's own Rust code built these transactions, which were run through `simulateTransaction` on devnet (signature checks off, so nothing was signed or sent):

- **One v1 transaction doing the whole flow** — `initialize_db_root`, `manage_table_creators`, `create_table` (locked), `user_initialize`, `db_code_in` with a 1,486-byte pack, `user_inventory_code_in` with a text file: **succeeded**. Every instruction encoding matches the deployed program.
- **Writer locks are enforced on chain**: an outsider's `db_code_in` to a locked table fails with the program's `NotAuthorized` (6000); the same write to an open table succeeds.
- `user_initialize` creates full-size (post-upgrade) accounts on devnet, so no resize is needed for new wallets there.
- The fees and sizes in the cost table above come from these runs.
- **`update_table` and `update_db_root_table_list`** (used by rename, who-can-add-rows and `DROP TABLE`), built by the portal's Rust code for an existing devnet database: renaming a table, locking it to one writer and replacing the database's table list **succeeded** when signed by the database's creator, and each failed with `NotAuthorized` (6000) when signed by another funded wallet.
- **Chunked uploads** on devnet: a linked-list write charged a flat 0.003 SOL and an upload-session write 0.005 SOL plus the session's rent, with the part transactions paying only the network fee; only the session path advances the wallet's `total_session_files`. IQ's gateway serves such rows reassembled (rows of up to 100 KB are already on IQ's mainnet tables).
- The deployed app itself was loaded in a real browser on devnet: it read balances and the database list (`getProgramAccounts`) straight from Solana.

Still to do: a funded end-to-end run (the public devnet faucet was dry at the time), and mainnet.

## Before using it on mainnet
1. **An RPC that accepts browser requests.** Solana's own mainnet endpoint answers browsers with `403 Access forbidden`, so the default is PublicNode's free endpoint (`solana-rpc.publicnode.com`), which takes writes, balances and account reads from web pages. It doesn't serve full history or `getProgramAccounts`, so *Read tables from → Solana directly* needs an RPC such as Helius (Settings). Reading through IQ's gateway needs neither.
2. **v1 transactions are live on mainnet** (the feature gate `txv1aq4…GLL` is activated), so saves use 4 KB writes; Auto mode checks it like the SDK does.
3. Know the limits: types, keys, relations and rules are enforced by IQ Tables (and any tool that reads the structure records), not by the chain — other programs can still write anything to an *open* table, and those rows show as unofficial. Auto-numbering on an open table can collide if two people add rows at the same time, since each browser numbers from the rows it has read; readers then keep the newer row under that id. Composite primary keys become an automatic `id` plus a unique key. There are no triggers, stored procedures or SQL users (`GRANT` maps to who may add rows). A checkpoint snapshots what the database's wallet saw: unofficial rows written before it aren't carried into it. A live git link shows whatever the project's owner commits next — use a version link where that matters. Crowdfunded files are checked against the organizer's fingerprints, not vetted: only share what you have the right to share, since nothing on the blockchain can be taken down.
4. A first small real run (one database, one table, one pack, ~0.1 SOL) — the simulation step shows any program error and the exact cost before anything is sent.

## License
MIT — see LICENSE.
