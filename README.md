# keetui

A KeePass-compatible TUI password manager. Opens KDBX 3.1/4.x databases, with
full read/write support, TOTP codes, and a Wayland clipboard with auto-clear.

Built with [ratatui](https://ratatui.rs) and
[keepass-rs](https://github.com/sseemayer/keepass-rs); interoperable with
KeePassXC.

## Usage

```sh
keetui                                       # browse for a database
keetui /path/to/vault.kdbx
keetui /path/to/vault.kdbx --keyfile /path/to/key
keetui /path/to/vault.kdbx --lock-after 10   # lock after 10 idle minutes (0 = never)
```

### Opening a database

Started without a path, keetui opens a small file browser in the current
folder showing subfolders and `.kdbx` files. Type to filter, `↑`/`↓` to move,
`Enter` to open a folder or database, and `Backspace`/`←` to go up. `~` jumps
home, `/` jumps to the root, `Ctrl-a` also shows hidden and non-`.kdbx` files,
and `Ctrl-n` creates a new database in the current folder. You can also click,
double-click and scroll. From the unlock screen, `Ctrl-o` opens the browser to
switch to a different database.

### New database

Point keetui at a file that doesn't exist yet (`keetui ~/new.kdbx`), or press
`Ctrl-n` on the unlock screen, to create an empty database. Choose the file
name, enter the master password twice, and optionally add an existing key file.
New databases are KDBX4 with Argon2d (64 MiB), and KeePassXC can open them.

### Browsing

Unlock with the master password (and optional key file). The screen has three
panes — groups, entries, and the selected entry — with a key bar at the bottom
that shows what you can do right now (every item in it is clickable).

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

`Tab`/`↑`/`↓` move between fields, `Ctrl-r` shows the password, `Ctrl-g`
generates one, `Ctrl-u` clears the field, `Ctrl-s` saves the entry, `Esc`
cancels. Pasting inserts into the focused field (line breaks are kept only
in Notes); outside a text field or the search box a paste is ignored, so
pasted text never runs as commands. The OTP field accepts an `otpauth://` URL or a bare base32 secret
(stored KeePassXC-compatibly). Custom fields, tags and attachments are shown
read-only and kept intact when editing.

URLs without a scheme open as `https://`. `cmd://`, `file://` and other
non-web schemes are refused.

## Saving

Saves are atomic: the database is serialized, verified by re-parsing, the old
file is backed up to `<name>.kdbx.bak`, and the new file replaces the original
via rename (each written to a fresh file first, so a symlink in the way is
replaced, never written through). A vault opened through a symlink is saved
to the link's target. If another program changed the file since keetui
opened or last saved it, keetui asks before overwriting those changes. A database opened from a KDBX3 file is written back as KDBX4
(KeePassXC-compatible) with the same Argon2d settings as new databases, after
a one-time confirmation.

Entry edits record the previous version in KeePass history, like KeePassXC.

## Clipboard

Copies go through `wl-copy` (Wayland required) and are marked sensitive
(`--sensitive`, wl-clipboard 2.3+), so clipboard history managers that honor
the hint don't record them. They auto-clear after 15 seconds, or as soon as
keetui exits — on quit, on a crash, or when the
terminal is closed — since `wl-copy` would otherwise keep serving the secret.
Caveat: if you copy something else in another application during those 15
seconds, the auto-clear will clear that too.

## Security notes

Proportionate to a personal tool: the master password and form buffers are
zeroized after use, protected fields stay encrypted in memory via the keepass
crate, secrets are piped (never passed as arguments), and nothing is logged.
Core dumps are disabled and the process is marked non-dumpable, so a crash
can't write decrypted secrets to disk and other processes of the same user
can't read keetui's memory. No mlock/swap hardening — use full-disk
encryption and encrypted swap.

## Development

```sh
cargo test                                  # unit + KDBX round-trip tests
cargo run --example make_test_db -- /tmp/test.kdbx   # create a demo vault (password: test)
cargo run -- /tmp/test.kdbx

# interop check against KeePassXC
keepassxc-cli ls -R /tmp/test.kdbx
keepassxc-cli show -t /tmp/test.kdbx "Work/GitHub"   # TOTP
```
