import { redirect } from 'next/navigation'
import { currentUser } from '@/lib/auth'
import { handoffQuery, readHandoff } from '@/lib/handoff'

export default async function ConnectPage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const params = await searchParams
  const url = new URL('https://crumb.invalid/connect')
  for (const [key, value] of Object.entries(params)) {
    if (typeof value === 'string') url.searchParams.set(key, value)
  }
  const handoff = readHandoff(url)
  if (!handoff) return <main className="center"><section className="panel"><p className="eyebrow">Crumb account</p><h1>Invalid connection request.</h1><p>Return to the terminal and run <code>crumb auth login</code> again.</p></section></main>
  const query = handoffQuery(handoff)
  const user = await currentUser()
  if (!user) redirect(`/api/auth/login?return_to=${encodeURIComponent(`/connect?${query}`)}`)
  return <main className="center"><section className="panel connect-panel">
    <div className="mark">c</div><p className="eyebrow">Signed in as {user.email}</p>
    <h1>Connect Pollinations to Crumb.</h1>
    <p>Crumb will receive a short-lived provider credential through a one-time exchange and save it in your operating system keychain.</p>
    <div className="permissions"><span>✓ Profile and usage</span><span>✓ No key in the callback URL</span><span>✓ Revoke from Pollinations anytime</span></div>
    <a className="primary" href={`/api/integrations/pollinations/connect?${query}`}>Continue to Pollinations</a>
    <p className="fine">Only continue if you started this request from your terminal.</p>
  </section></main>
}
