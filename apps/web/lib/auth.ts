import { cookies } from 'next/headers'
import { bindings, originFor } from '@/lib/cloudflare'
import { randomToken } from '@/lib/encoding'

export interface User {
  id: string
  email: string
  displayName: string
}

interface AccountsUser {
  id: string
  email: string
  displayName?: string
}

export async function currentUser(): Promise<User | null> {
  const session = (await cookies()).get('crumb_session')?.value
  if (!session) return null
  const row = await bindings().DB.prepare(`
    SELECT users.id, users.email, users.display_name
    FROM sessions JOIN users ON users.id = sessions.user_id
    WHERE sessions.id = ? AND sessions.expires_at > unixepoch()
  `).bind(session).first<{ id: string; email: string; display_name: string }>()
  return row ? { id: row.id, email: row.email, displayName: row.display_name } : null
}

export function accountsAuthorizeUrl(state: string, requestUrl: string): string {
  const env = bindings()
  const url = new URL('/oauth/authorize', env.ACCOUNTS_ORIGIN || 'https://accounts.elixpo.com')
  url.search = new URLSearchParams({
    response_type: 'code',
    client_id: env.NEXT_PUBLIC_ELIXPO_CLIENT_ID,
    redirect_uri: `${originFor(requestUrl)}/api/auth/callback`,
    state,
    scope: 'openid profile email',
  }).toString()
  return url.toString()
}

export async function finishAccountsLogin(code: string, requestUrl: string): Promise<User> {
  const env = bindings()
  const accounts = env.ACCOUNTS_ORIGIN || 'https://accounts.elixpo.com'
  const tokenResponse = await fetch(new URL('/api/auth/token', accounts), {
    method: 'POST',
    headers: { 'content-type': 'application/json', accept: 'application/json' },
    body: JSON.stringify({
      grant_type: 'authorization_code',
      code,
      client_id: env.NEXT_PUBLIC_ELIXPO_CLIENT_ID,
      client_secret: env.ELIXPO_CLIENT_SECRET,
      redirect_uri: `${originFor(requestUrl)}/api/auth/callback`,
    }),
  })
  const tokens = await tokenResponse.json() as { access_token?: string }
  if (!tokenResponse.ok || !tokens.access_token) throw new Error('Accounts token exchange failed')
  const userResponse = await fetch(new URL('/api/auth/me', accounts), {
    headers: { authorization: `Bearer ${tokens.access_token}`, accept: 'application/json' },
  })
  const account = await userResponse.json() as AccountsUser
  if (!userResponse.ok || !account.id || !account.email) throw new Error('Accounts profile lookup failed')
  const user = { id: account.id, email: account.email, displayName: account.displayName || account.email }
  const db = bindings().DB
  await db.prepare(`
    INSERT INTO users (id, email, display_name, updated_at)
    VALUES (?, ?, ?, unixepoch())
    ON CONFLICT(id) DO UPDATE SET email = excluded.email, display_name = excluded.display_name,
      updated_at = excluded.updated_at
  `).bind(user.id, user.email, user.displayName).run()
  const session = randomToken()
  await db.prepare('INSERT INTO sessions (id, user_id, expires_at) VALUES (?, ?, ?)')
    .bind(session, user.id, Math.floor(Date.now() / 1000) + 15 * 86400).run()
  ;(await cookies()).set('crumb_session', session, {
    httpOnly: true,
    secure: process.env.NODE_ENV === 'production',
    sameSite: 'lax',
    path: '/',
    maxAge: 15 * 86400,
  })
  return user
}
