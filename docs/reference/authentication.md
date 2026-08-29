# Authentication

Crumb supports Pollinations bring-your-own-key credentials through:

```bash
crumb auth login
crumb auth status
crumb auth logout
```

The same actions are available inside Crumb as `:auth login`, `:auth status`,
and `:auth logout`. Login reads the key with terminal echo disabled and never
accepts it as a command-line argument.

## Linux

Crumb uses the desktop Secret Service over a pure-Rust Zbus client. A logged-in
GNOME or KDE desktop normally already provides an unlocked store, so no system
development package is required.

On Debian or Ubuntu, install GNOME Keyring only when no Secret Service provider
is present:

```bash
sudo apt install gnome-keyring
```

KWallet and KeePassXC's Secret Service integration are also compatible. Crumb
returns a redacted error when the store is absent or locked; it never silently
writes the key to a plaintext file.

For headless or ephemeral sessions, set `POLLINATIONS_API_KEY` in the process
environment. This override is used without being persisted by Crumb.
