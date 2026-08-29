import type { Metadata } from 'next'
import { redirect } from 'next/navigation'
import { SiteFooter } from '@/app/components/SiteFooter'
import { SiteHeader } from '@/app/components/SiteHeader'
import { currentUser } from '@/lib/auth'
import { bindings } from '@/lib/cloudflare'
import { MODEL_POLICY } from '@/lib/model-policy'

export const metadata: Metadata = { title: 'Connections', description: 'Securely connect model providers to Crumb LNT.' }

export default async function ConnectPage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const params = await searchParams
  const user = await currentUser()
  if (!user) redirect('/login')
  const connection = await bindings().DB.prepare(`
    SELECT token_expires_at, oauth_scope, updated_at FROM pollinations_connections
    WHERE user_id = ? AND token_expires_at > unixepoch()
  `).bind(user.id).first<{ token_expires_at: number; oauth_scope: string; updated_at: number }>()
  const result = typeof params.pollinations === 'string' ? params.pollinations : ''
  return <><SiteHeader /><main className="account-page"><div className="account-heading"><p className="kicker">Account connections</p><h1>Bring your models.<br />Keep your keys safe.</h1><p>Connect providers once in the browser, then authorize Crumb on each device.</p></div>
    {result === 'connected' && <div className="notice success-notice">Pollinations is connected. You can now authorize the CLI.</div>}
    {result === 'error' && <div className="notice error-notice">The connection could not be completed. Please try again.</div>}
    <section className="connector-layout"><article className="provider-card"><div className="provider-head"><div className="pollinations-logo">p</div><div><p>Model provider</p><h2>Pollinations</h2></div><span className={connection ? 'state connected' : 'state'}>{connection ? 'Connected' : 'Not connected'}</span></div>
      <p className="provider-copy">Crumb uses a scoped connector so your Pollinations key never appears in terminal history or local configuration.</p>
      <div className="model-groups"><div><span>Text harness</span><strong>{MODEL_POLICY.text.join(' · ')}</strong></div><div><span>Images</span><strong>{MODEL_POLICY.image.join(' · ')}</strong></div><div><span>Other modalities</span><strong>One safe default per modality</strong></div></div>
      {connection && <div className="connection-meta"><span>Scope</span><code>{connection.oauth_scope}</code><span>Expires</span><strong>{new Date(connection.token_expires_at * 1000).toLocaleDateString('en-US', { dateStyle: 'medium' })}</strong></div>}
      <a className="button provider-button" href="/api/integrations/pollinations/connect">{connection ? 'Reconnect Pollinations' : 'Connect Pollinations'} <span>→</span></a>
    </article><aside className="device-card"><p className="kicker">Next step</p><h2>Authorize this terminal</h2><p>Once the provider is connected, open Crumb and start its secure device flow.</p><pre><span>$</span> crumb auth login</pre><ol><li>Crumb displays a verification link.</li><li>Confirm the device in your browser.</li><li>The CLI receives a short-lived connector.</li></ol><a href="/docs#authentication">Device flow documentation →</a></aside></section>
  </main><SiteFooter /></>
}
