import { NextResponse } from 'next/server'
import { finishAccountsLogin } from '@/lib/auth'
import { bindings } from '@/lib/cloudflare'

export const runtime = 'edge'

export async function GET(request: Request) {
  const url = new URL(request.url)
  const code = url.searchParams.get('code')
  const state = url.searchParams.get('state')
  if (!code || !state || url.searchParams.has('error')) return NextResponse.redirect(new URL('/?auth=denied', request.url))
  const key = `accounts:${state}`
  const returnTo = await bindings().KV.get(key)
  if (!returnTo) return NextResponse.redirect(new URL('/?auth=invalid_state', request.url))
  await bindings().KV.delete(key)
  try {
    await finishAccountsLogin(code, request.url)
    return NextResponse.redirect(new URL(returnTo, request.url))
  } catch (error) {
    console.error('[crumb/accounts] callback failed', error instanceof Error ? error.message : 'unknown error')
    return NextResponse.redirect(new URL('/?auth=failed', request.url))
  }
}
