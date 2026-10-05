# keetui

A KeePass-compatible TUI password manager. Opens KDBX 3.1/4.x databases, with
full read/write support, TOTP codes, and a Wayland clipboard with auto-clear.

Built with [ratatui](https://ratatui.rs) and
[keepass-rs](https://github.com/sseemayer/keepass-rs); interoperable with
KeePassXC.

## Install

Needs Rust 1.89 or newer ([rustup](https://rustup.rs)), and `wl-copy` from
wl-clipboard for copying to the clipboard.

```sh
git clone https://github.com/sircraig/keetui
cd keetui
cargo install --path . --locked
```

This builds an optimized binary and puts it in `~/.cargo/bin`, which rustup
adds to your `PATH`. Run the same command again to update after pulling, and
`cargo uninstall keetui` to remove it. `--locked` builds with the exact
dependency versions in `Cargo.lock`.

## Usage

```sh
keetui                                       # pick a recent database, or browse
keetui /path/to/vault.kdbx
keetui /path/to/vault.kdbx --keyfile /path/to/key
keetui /path/to/vault.kdbx --lock-after 10   # lock after 10 idle minutes (0 = never)
```

### Opening a database

Started without a path, keetui lists the databases you opened recently, newest
first. `↑`/`↓` (or `j`/`k`) move, `Enter` or a double-click opens the selected
database, `Ctrl-o` browses for another one, `Ctrl-n` creates a new one, and `d`
takes the selected database off the list (the file itself is kept). A database
that isn't where it was, deleted or on a drive that isn't mounted, is marked
"not found". The list holds the last 10 databases that were unlocked or
created, in `$XDG_STATE_HOME/keetui/recent` (by default
`~/.local/state/keetui/recent`): one path per line, readable by you only.
Delete the file to clear it.

The file browser starts in the current folder, showing subfolders and `.kdbx`
files. Type to filter, `↑`/`↓` to move, `Enter` to open a folder or database,
and `Backspace`/`←` to go up. `~` jumps home, `/` jumps to the root, `Ctrl-a`
also shows hidden and non-`.kdbx` files, and `Ctrl-n` creates a new database in
the current folder. You can also click, double-click and scroll. From the
unlock screen, `Ctrl-o` opens the browser to switch to a different database.

`Esc` goes back to the screen you came from, and quits from the list of recent
databases. Started with a path, `Esc` on the unlock screen quits.

### New database

Point keetui at a file that doesn't exist yet (`keetui ~/new.kdbx`), or press
`Ctrl-n` on the recent databases, the file browser or the unlock screen, to
create an empty database. Choose the file name, enter the master password
twice, optionally add an existing key file, and press `Ctrl-s` (or `Enter` on
the last fields) to create it.
New databases are KDBX4 with Argon2d (64 MiB), and KeePassXC can open them.

### Browsing

Unlock with the master password (and optional key file); `Esc` cancels an
unlock that is taking too long. The screen has three panes — groups, entries,
and the selected entry — with a key bar at the bottom that shows what you can
do right now (every item in it is clickable).

| Key | Action |
|---|---|
| `j`/`k`, `↑`/`↓` | move selection (`PgUp`/`PgDn`, `g`/`G` page, top, bottom) |
| `h`/`l`, `←`/`→` | collapse/expand groups, move between panes |
| `Tab` | switch pane (groups ↔ entries) |
| `Enter` | groups: show entries · entries: **copy password** |
| `Space` | expand/collapse group |
| `/` | search every group — title, username, URL, notes, tags (`Esc` ends it) |
| `c` / `y` / `t` / `u` | copy password / username / TOTP code / URL |
| `o` | open the URL in your browser (`xdg-open`) |
| `r` | show/hide the password |
| `a` / `A` | new entry / new group |
| `e` | edit entry (in the groups pane: rename group) |
| `d` | delete (to recycle bin when the database has one) |
| `Ctrl-g` | password generator |
| `Ctrl-s` | save |
| `Ctrl-l` | lock |
| `q` | quit (prompts when there are unsaved changes) |
| `?` | help |

Search covers all groups (recycle bin excluded); space-separated words must all
match. Going back to the groups pane ends the search.

### Locking

keetui locks itself after 5 minutes without a key press, click or scroll
(`--lock-after MINUTES`; `0` turns this off), and `Ctrl-l` locks it right
away. Locking forgets the decrypted database: unlocking reads it from disk
again and returns to where you were. If there are unsaved changes, they are
kept in memory behind the master password instead, so nothing is lost, and
quitting from the lock screen offers to save them. A revealed password hides
itself again after 30 seconds.

### Mouse

Click to select groups and entries, click `▸`/`▾` (or double-click) to expand
a group, and use the wheel to scroll. Double-click an entry to copy its
password. In the entry pane, click the username, password or TOTP code to copy
it, click the URL to open it, or use the `[copy]`/`[show]`/`[open]`/`[edit]`
buttons; dialogs have clickable buttons too. Hold `Shift` to select text with
the terminal instead, or start with `--no-mouse`.

### Entry editor

`Tab`/`↑`/`↓` move between fields, `Ctrl-r` shows the password and TOTP secret
(both are masked otherwise), `Ctrl-g` generates a password, `Ctrl-u` clears
the field, `Ctrl-s` saves the entry, `Esc` cancels. Pasting inserts into the
focused field (line breaks are kept only in Notes); outside a text field or
the search box a paste is ignored, so pasted text never runs as commands. The
OTP field accepts an `otpauth://` URL or a bare base32 secret (stored
KeePassXC-compatibly). Custom fields, tags and attachments are shown read-only
and kept intact when editing.

URLs without a scheme open as `https://`. `cmd://`, `file://` and other
non-web schemes are refused, and `mailto:` links open without their query
part (some mail clients would attach local files named in it).

## Saving

Saves are atomic: the database is serialized, verified by re-parsing, the old
file is backed up to `<name>.kdbx.bak`, and the new file replaces the original
via rename (each written to a fresh file first, so a symlink in the way is
replaced, never written through). A vault opened through a symlink is saved to
the link's target. If another program changed the file since keetui opened or
last saved it, keetui asks before overwriting those changes.

keetui can only write KDBX 4.1, which KeePassXC 2.7+ and KeePass 2.48+ open.
A database in another format is converted when you first save it, after a
confirmation: a KDBX 4.0 file keeps its own encryption settings, while KDBX 3.1
and KeePass 1.x (.kdb) files get the same Argon2d settings as new databases.

Entry edits record the previous version in KeePass history, like KeePassXC.

## Clipboard

Copies go through `wl-copy` (Wayland required) and are marked sensitive
(`--sensitive`, wl-clipboard 2.3+), so clipboard history managers that honor
the hint don't record them. They auto-clear after 15 seconds, or as soon as
keetui exits — on quit, on a crash, or when the terminal is closed — since
`wl-copy` would otherwise keep serving the secret. Caveat: if you copy
something else in another application during those 15 seconds, the auto-clear
will clear that too.

## Security notes

Proportionate to a personal tool. While a vault is open, its contents are in
memory as plain text: the keepass crate keeps protected fields (passwords,
TOTP secrets) in buffers that are wiped when freed, but does not encrypt
them. keetui wipes the master password, the key file and form buffers when
it is done with them, and sizes those buffers so typing doesn't leave copies
behind; text shown on screen also passes through the terminal library's
buffers, which are not wiped. Core dumps are disabled and the process is
marked non-dumpable, so a crash can't write decrypted secrets to disk and
other processes of the same user can't read keetui's memory. Secrets are
piped to `wl-copy` (never passed as arguments), and nothing is logged. No
mlock/swap hardening — use full-disk encryption and encrypted swap.

## Development

```sh
cargo test                                  # unit + KDBX round-trip tests
cargo run --example make_test_db -- /tmp/test.kdbx   # create a demo vault (password: test)
cargo run -- /tmp/test.kdbx

# interop check against KeePassXC
keepassxc-cli ls -R /tmp/test.kdbx
keepassxc-cli show -t /tmp/test.kdbx "Work/GitHub"   # TOTP
```
