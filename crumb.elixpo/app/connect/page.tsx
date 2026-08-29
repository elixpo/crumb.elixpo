import { redirect } from 'next/navigation'
import { currentUser } from '@/lib/auth'
import { bindings } from '@/lib/cloudflare'
import { MODEL_POLICY } from '@/lib/model-policy'

export default async function ConnectPage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const params = await searchParams
  const user = await currentUser()
  if (!user) redirect('/api/auth/login?return_to=%2Fconnect')
  const connection = await bindings().DB.prepare(`
    SELECT token_expires_at FROM pollinations_connections
    WHERE user_id = ? AND token_expires_at > unixepoch()
  `).bind(user.id).first<{ token_expires_at: number }>()
  const result = typeof params.pollinations === 'string' ? params.pollinations : ''
  return <main className="center"><section className="panel connect-panel">
    <div className="mark">c</div><p className="eyebrow">Signed in as {user.email}</p>
    <h1>{connection ? 'Pollinations is connected.' : 'Connect Pollinations to Crumb.'}</h1>
    {result === 'connected' && <p>Your connector is ready for Crumb CLI device sign-in.</p>}
    <p>The scoped connector supports {MODEL_POLICY.text.join(' + ')} for the harness, {MODEL_POLICY.image.join(' + ')} for images, and one default per other modality.</p>
    <div className="permissions"><span>✓ Profile and usage</span><span>✓ Scoped model allowlist</span><span>✓ Revoke from Pollinations anytime</span></div>
    <a className="primary" href="/api/integrations/pollinations/connect">{connection ? 'Reconnect Pollinations' : 'Continue to Pollinations'}</a>
    <p className="fine">After connecting, run <code>crumb auth login</code> to authorize this terminal.</p>
  </section></main>
}
