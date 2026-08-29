# Account connector development

The account site lives in `crumbs.elixpo` and runs as a Next.js Cloudflare Pages
application with D1 and KV bindings.

## OAuth registrations

Register Crumb as a confidential web client in `accounts.elixpo` with scopes
`openid profile email` and these callbacks:

- `http://localhost:3001/api/auth/callback` for local development.
- `https://crumb.elixpo.com/api/auth/callback` for production.

No Accounts webhook or custom scope is required for the initial connector.
An account-deletion webhook can be added later for proactive cleanup.

Register the Pollinations application callback as:

- `http://localhost:3001/api/integrations/pollinations/callback` locally.
- `https://crumb.elixpo.com/api/integrations/pollinations/callback` in production.

Pollinations requests `profile usage` and uses PKCE.

## Local configuration

Copy `crumbs.elixpo/.env.local.example` to `crumbs.elixpo/.env.local` and set:

- `NEXT_PUBLIC_ELIXPO_CLIENT_ID`
- `ELIXPO_CLIENT_SECRET`
- `POLLINATIONS_APP_KEY`
- `CONNECTOR_ENCRYPTION_KEY` to a high-entropy server secret

Never commit `.env.local`. The web app runs on port `3001`; the terminal owns
`127.0.0.1:3000` while `crumb auth login` is active.

Apply `crumbs.elixpo/migrations/0001_initial.sql` to the bound D1 database before
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

`./deploy.sh all` runs the same sequence. The Next-on-Pages build produces the
Pages Functions worker, so this application does not need a second standalone
Worker service.

## Handoff security

1. Crumb creates a random state and PKCE verifier, listens only on loopback,
   and opens `/connect` in the browser.
2. The site authenticates with Accounts and connects Pollinations.
3. The site stores the provider token encrypted and sends only a two-minute,
   single-use code to the loopback callback.
4. Crumb exchanges the code with its verifier and stores the returned token in
   the OS keyring.

Only `http://localhost:3000/auth/callback` and its `127.0.0.1` equivalent are
accepted as terminal callbacks.
