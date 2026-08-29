import { NextResponse } from 'next/server'
import { currentUser } from '@/lib/auth'
import { bindings } from '@/lib/cloudflare'
import { randomToken } from '@/lib/encoding'
import { readHandoff } from '@/lib/handoff'
import { authorizeUrl, challenge, verifier } from '@/lib/pollinations'

export async function GET(request: Request) {
  const url = new URL(request.url)
  const handoff = readHandoff(url)
  if (!handoff) return NextResponse.json({ error: 'Invalid terminal handoff' }, { status: 400 })
  const user = await currentUser()
  if (!user) {
    const returnTo = `${url.pathname}?${url.searchParams}`
    return NextResponse.redirect(new URL(`/api/auth/login?return_to=${encodeURIComponent(returnTo)}`, request.url))
  }
  const oauthState = randomToken()
  const pkceVerifier = verifier()
  await bindings().KV.put(`pollinations:${oauthState}`, JSON.stringify({
    userId: user.id,
    handoff,
    verifier: pkceVerifier,
  }), { expirationTtl: 600 })
  return NextResponse.redirect(authorizeUrl(oauthState, await challenge(pkceVerifier), request.url))
}
