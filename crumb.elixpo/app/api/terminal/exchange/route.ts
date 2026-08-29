import { NextResponse } from 'next/server'
import { bindings } from '@/lib/cloudflare'
import { digest } from '@/lib/encoding'
import { decryptToken } from '@/lib/secrets'

export async function POST(request: Request) {
  const body = await request.json().catch(() => null) as { code?: string; code_verifier?: string } | null
  if (!body?.code || !body.code_verifier) return NextResponse.json({ error: 'invalid_request' }, { status: 400 })
  const db = bindings().DB
  const codeHash = await digest(body.code)
  const row = await db.prepare(`
    SELECT grants.user_id, grants.code_challenge, connections.access_token_encrypted,
           connections.token_expires_at
    FROM terminal_grants grants
    JOIN pollinations_connections connections ON connections.user_id = grants.user_id
    WHERE grants.code_hash = ? AND grants.used_at IS NULL AND grants.expires_at > unixepoch()
  `).bind(codeHash).first<{
    user_id: string
    code_challenge: string
    access_token_encrypted: string
    token_expires_at: number
  }>()
  if (!row || row.code_challenge !== await digest(body.code_verifier) || row.token_expires_at <= Date.now() / 1000) {
    return NextResponse.json({ error: 'invalid_grant' }, { status: 400 })
  }
  const consumed = await db.prepare(`
    UPDATE terminal_grants SET used_at = unixepoch()
    WHERE code_hash = ? AND used_at IS NULL
  `).bind(codeHash).run()
  if (!consumed.meta.changes) return NextResponse.json({ error: 'invalid_grant' }, { status: 400 })
  const accessToken = await decryptToken(row.access_token_encrypted, row.user_id)
  return NextResponse.json(
    { access_token: accessToken, expires_at: row.token_expires_at },
    { headers: { 'cache-control': 'no-store' } },
  )
}
