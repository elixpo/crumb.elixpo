import { redirect } from 'next/navigation'
import { SiteFooter } from '@/app/components/SiteFooter'
import { SiteHeader } from '@/app/components/SiteHeader'
import { currentUser } from '@/lib/auth'
import { bindings, config } from '@/lib/cloudflare'
import { MODEL_POLICY } from '@/lib/model-policy'
import { pageMetadata } from '@/lib/seo'

export const metadata = pageMetadata({ title: 'Connectors', description: 'Manage the services securely connected to your Crumb account.', path: '/profile/connectors', noIndex: true })

export default async function ConnectorsPage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const params = await searchParams
  const user = await currentUser()
  if (!user) redirect('/login?return_to=%2Fprofile%2Fconnectors')
  const connection = await bindings().DB.prepare(`
    SELECT token_expires_at, oauth_scope, updated_at FROM pollinations_connections
    WHERE user_id = ? AND token_expires_at > unixepoch()
  `).bind(user.id).first<{ token_expires_at: number; oauth_scope: string; updated_at: number }>()
  const result = typeof params.pollinations === 'string' ? params.pollinations : ''
  return <><SiteHeader /><main className="account-page"><div className="profile-intro"><div className="account-heading"><p className="kicker">Profile connectors</p><h1>Your secure service layer.</h1><p>Connect providers once in the browser, then authorize each Crumb terminal without pasting private keys.</p></div><aside className="profile-pill"><img src={`${config().accountsOrigin}/api/avatar/${encodeURIComponent(user.id)}`} alt="" /><span><small>Signed in as</small><strong>{user.displayName}</strong><em>{user.email}</em></span></aside></div>
    {result === 'connected' && <div className="notice success-notice">Pollinations is connected. You can now authorize the CLI.</div>}
    {['failed', 'denied', 'invalid_session'].includes(result) && <div className="notice error-notice">The Pollinations connection could not be completed. Please try again.</div>}
    <section className="connector-layout"><article className="provider-card"><div className="provider-head"><div className="pollinations-logo">p</div><div><p>Model provider</p><h2>Pollinations</h2></div><span className={connection ? 'state connected' : 'state'}>{connection ? 'Connected' : 'Not connected'}</span></div>
      <p className="provider-copy">Crumb uses a scoped connector so your Pollinations key never appears in terminal history or local configuration.</p>
      <div className="model-groups"><div><span>Text harness</span><strong>{MODEL_POLICY.text.join(' · ')}</strong></div><div><span>Images</span><strong>{MODEL_POLICY.image.join(' · ')}</strong></div><div><span>Other modalities</span><strong>One safe default per modality</strong></div></div>
      {connection && <div className="connection-meta"><span>Scope</span><code>{connection.oauth_scope}</code><span>Expires</span><strong>{new Date(connection.token_expires_at * 1000).toLocaleDateString('en-US', { dateStyle: 'medium' })}</strong></div>}
      <a className="button provider-button" href="/api/integrations/pollinations/connect">{connection ? 'Reconnect Pollinations' : 'Connect Pollinations'} <span>→</span></a>
    </article><aside className="device-card"><p className="kicker">Next step</p><h2>Authorize this terminal</h2><p>Once Pollinations is connected, open Crumb and start its secure device flow.</p><pre><span>$</span> crumb auth login</pre><ol><li>Crumb displays a verification link.</li><li>Confirm the device in your browser.</li><li>The CLI receives a scoped connector.</li></ol><a href="/docs/authentication">Device flow documentation →</a></aside></section>
  </main><SiteFooter /></>
}
