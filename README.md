# sft

`sft` is a small Rust TUI for choosing an SSH config host and moving files between your local machine and that host.

It is intentionally focused: SSH config host selection, basic host editing, split-pane browsing, multi-select, confirmation, and transfer over OpenSSH `scp`. It uses your existing SSH setup, including `~/.ssh/config`, agent keys, `ProxyJump`, and host aliases.

## Install

```sh
cargo install --path .
```

## Usage

```sh
sft
sft my-server
sft user@example.com --local ~/Downloads --remote /var/tmp
sft -F ~/.ssh/config workbox
```

Run `sft` without a host to open the host picker from `~/.ssh/config`.

The host picker shows a status marker on each row: a green circle means the SSH TCP endpoint is reachable, a yellow triangle means it is slow, and a red `x` means it is unreachable. This check does not attempt SSH authentication or ask for a password. The right sidebar shows the selected host's basic SSH fields, local usage stats, and a small login activity bar plot.

## Keys

Host picker:

| Key | Action |
| --- | --- |
| `Enter` | Show password popup, then open selected host |
| `n` | Add a Host block |
| `e` | Edit selected single-alias Host block |
| `p` | Set or clear the selected host's session password |
| `d` | Delete selected single-alias Host block |
| `r` | Reload ssh config |
| `q`, `Esc` | Quit |

Host form:

| Key | Action |
| --- | --- |
| `Tab`, arrows | Change field |
| `Enter` | Save |
| `Esc` | Cancel |

File transfer:

| Key | Action |
| --- | --- |
| `Tab`, `Left`, `Right` | Switch pane |
| `Enter` | Open directory or copy file/selection |
| `Backspace` | Go to parent directory |
| `j/k`, arrows | Move selection |
| `Ctrl-Space` | Toggle selected item |
| `Ctrl-A` | Select or clear all visible entries |
| `.` | Toggle dotfiles |
| `s` | Cycle sort mode |
| `r` | Refresh both panes |
| `g` | Jump to a local or remote directory; `Tab` completes directories |
| `f` | Search/filter the active pane as you type |
| `p` | Set or replace the session password for this host |
| `q`, `Esc` | Quit |

## Notes

- Requires `ssh` and `scp` in `PATH`.
- Works with key-based auth, `ssh-agent`, or a session password entered in the popup shown before opening a host.
- Leave the password popup blank and press `Enter` to use key/agent auth.
- Passwords are not written to `~/.ssh/config`; they are kept for the running `sft` process only and passed to OpenSSH through `SSH_ASKPASS`.
- Host editing preserves unknown directives in a single-alias `Host` block, such as `ProxyJump` and `ForwardAgent`.
- Wildcard and multi-alias `Host` blocks can be opened when concrete aliases are present, but are treated as read-only for safety.
- Directory copies use `scp -r`.
- Transfers may overwrite existing destination files, so `sft` always asks for confirmation.

## Development

```sh
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
```
