# Account connector development

The account site lives in `crumb.elixpo` and runs as a Next.js application on
Cloudflare Workers through OpenNext, with D1 and KV bindings.

## OAuth registrations

Register Crumb as a confidential web client in `accounts.elixpo` with scopes
`openid profile email` and these callbacks:

- `http://localhost:3000/auth/callback` for local development.
- `https://crumb.elixpo.com/auth/callback` for production.

No custom Accounts scope is required. Register an Accounts lifecycle webhook:

- `POST https://localhost:3000/api/webhook/account_delete` locally.
- `POST https://crumb.elixpo.com/api/webhook/account_delete` in production.

Subscribe it to `user.deleted`. Accounts must sign the exact request bytes as
`HMAC-SHA256(secret, "<unix timestamp>.<body>")` and send
`X-Elixpo-Timestamp`, `X-Elixpo-Signature: sha256=<hex>`, and a stable
`X-Elixpo-Event-Id`. The shared secret is
The destination URL is configured in Accounts as
`ELIXPO_ACCOUNTS_DELETION_WEBHOOK`. Store the separate `whk_…` secret returned
by Accounts in Crumb as `ELIXPO_ACCOUNTS_WEBHOOK_SECRET`.

Register the Pollinations application callback as:

- `http://localhost:3000/api/integrations/pollinations/callback` locally.
- `https://crumb.elixpo.com/api/integrations/pollinations/callback` in production.

Pollinations requests `profile usage` and uses PKCE.

## Local configuration

Copy `crumb.elixpo/.env.local.example` to `crumb.elixpo/.env.local` and set:

- `NEXT_PUBLIC_ELIXPO_CLIENT_ID`
- `NEXT_PUBLIC_ELIXPO_CLIENT_ID_CLI`
- `ELIXPO_CLIENT_SECRET`
- `ELIXPO_ACCOUNTS_WEBHOOK_SECRET`
- `POLLINATIONS_APP_KEY`
- `CONNECTOR_ENCRYPTION_KEY` to a high-entropy server secret

Never commit `.env.local`. The local web app runs on port `3000`.

`./deploy.sh secrets` uses application values from `crumb.elixpo/.env.local`
when present and falls back per missing key to the SOPS-encrypted root `.env`.

Apply `crumb.elixpo/migrations/0001_initial.sql` to the bound D1 database before
testing. Replace the placeholder D1 and KV identifiers in `wrangler.toml` only
with the resources intended for this project.

The root deployment script provisions and binds the Cloudflare resources with
Wrangler:

```bash
./deploy.sh provision
./deploy.sh secrets
./deploy.sh migrate
./deploy.sh build
./deploy.sh deploy
```

`./deploy.sh all` runs the complete sequence. OpenNext produces the Worker and
static-assets bundle; the first deployment creates the Worker service.

## Handoff security

1. Crumb creates a random state and PKCE verifier, listens only on loopback,
   and opens `/connect` in the browser.
2. The site authenticates with Accounts and connects Pollinations.
3. The site stores the provider token encrypted and sends only a two-minute,
   single-use code to the loopback callback.
4. Crumb exchanges the code with its verifier and stores the returned token in
   the OS keyring.

Only `http://localhost:3000/auth/connector/callback` and its `127.0.0.1`
equivalent are accepted as terminal callbacks.

## Backend route contract

- `GET /api/auth/login` and `GET /auth/callback` — Accounts handshake.
- `POST /api/webhook/account_delete` — signed Accounts deletion hook.
- `GET /api/integrations/pollinations/connect` — start Pollinations PKCE.
- `GET /api/integrations/pollinations/callback` — mint and encrypt the scoped connector.
- `POST /api/terminal/exchange` — consume the terminal's one-time PKCE grant.

Frontend work is deferred until the harness, token optimizers, and CLI UI land.
