export interface HandoffRequest {
  redirectUri: string
  state: string
  challenge: string
}

const CALLBACKS = new Set([
  'http://localhost:3000/auth/callback',
  'http://127.0.0.1:3000/auth/callback',
])

export function readHandoff(url: URL): HandoffRequest | null {
  const redirectUri = url.searchParams.get('redirect_uri') || ''
  const state = url.searchParams.get('state') || ''
  const challenge = url.searchParams.get('code_challenge') || ''
  if (!CALLBACKS.has(redirectUri) || !/^[A-Za-z0-9_-]{32,128}$/.test(state) || !/^[A-Za-z0-9_-]{43}$/.test(challenge)) return null
  return { redirectUri, state, challenge }
}

export function handoffQuery(request: HandoffRequest): string {
  return new URLSearchParams({
    redirect_uri: request.redirectUri,
    state: request.state,
    code_challenge: request.challenge,
  }).toString()
}
