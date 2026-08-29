import { NextResponse } from 'next/server'
import { currentUser } from '@/lib/auth'
import { bindings } from '@/lib/cloudflare'
import { digest, randomToken } from '@/lib/encoding'
import type { HandoffRequest } from '@/lib/handoff'
import { exchangeCode } from '@/lib/pollinations'
import { encryptToken } from '@/lib/secrets'

interface Context { userId: string; handoff: HandoffRequest; verifier: string }

function finish(handoff: HandoffRequest, values: Record<string, string>) {
  const destination = new URL(handoff.redirectUri)
  destination.searchParams.set('state', handoff.state)
  for (const [key, value] of Object.entries(values)) destination.searchParams.set(key, value)
  return NextResponse.redirect(destination)
}

export async function GET(request: Request) {
  const url = new URL(request.url)
  const state = url.searchParams.get('state') || ''
  const key = `pollinations:${state}`
  const stored = await bindings().KV.get(key)
  if (!stored) return NextResponse.redirect(new URL('/?connect=invalid_state', request.url))
  await bindings().KV.delete(key)
  const context = JSON.parse(stored) as Context
  const code = url.searchParams.get('code')
  if (!code || url.searchParams.has('error')) return finish(context.handoff, { error: 'access_denied' })
  const user = await currentUser()
  if (!user || user.id !== context.userId) return finish(context.handoff, { error: 'invalid_session' })
  try {
    const tokens = await exchangeCode(code, context.verifier, request.url)
    const now = Math.floor(Date.now() / 1000)
    await bindings().DB.prepare(`
      INSERT INTO pollinations_connections
        (user_id, access_token_encrypted, token_expires_at, oauth_scope, updated_at)
      VALUES (?, ?, ?, ?, ?)
      ON CONFLICT(user_id) DO UPDATE SET
        access_token_encrypted = excluded.access_token_encrypted,
        token_expires_at = excluded.token_expires_at,
        oauth_scope = excluded.oauth_scope,
        updated_at = excluded.updated_at
    `).bind(
      user.id,
      await encryptToken(tokens.access_token, user.id),
      now + (Number(tokens.expires_in) || 7 * 86400),
      tokens.scope || '',
      now,
    ).run()
    const grant = randomToken()
    await bindings().DB.prepare(`
      INSERT INTO terminal_grants (code_hash, user_id, code_challenge, expires_at)
      VALUES (?, ?, ?, ?)
    `).bind(await digest(grant), user.id, context.handoff.challenge, now + 120).run()
    return finish(context.handoff, { code: grant })
  } catch (error) {
    const reference = crypto.randomUUID().slice(0, 8)
    console.error(`[crumb/pollinations] callback failed ref=${reference}`, error instanceof Error ? error.message : 'unknown error')
    return finish(context.handoff, { error: 'connection_failed', error_ref: reference })
  }
}
