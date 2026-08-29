# Authentication

Crumb connects an Elixpo account to Pollinations through:

```bash
crumb auth login
crumb auth status
crumb auth logout
```

The same actions are available inside Crumb as `:auth login`, `:auth status`,
and `:auth logout`. Login opens the Crumb account site, completes Elixpo
Accounts and Pollinations authorization, and returns a one-time code to
`http://localhost:3000/auth/connector/callback`.

The callback URL never contains the Pollinations credential. Crumb exchanges
the short-lived code using PKCE and saves the resulting credential in the OS
keyring. Set `CRUMB_ACCOUNT_URL=http://localhost:3000` when testing against the
local web app.

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

For headless or ephemeral sessions, `POLLINATIONS_API_KEY` remains an explicit
process-only override. Crumb does not persist that value.

## Web configuration

See [account connector development](../development/account-connector.md) for
the Accounts registration, Pollinations App Key, Cloudflare bindings, and
local port layout.
