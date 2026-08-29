import { redirect } from 'next/navigation'
import { SiteHeader } from '@/app/components/SiteHeader'
import { currentUser } from '@/lib/auth'
import { pageMetadata } from '@/lib/seo'

export const metadata = pageMetadata({ title: 'Sign in', description: 'Sign in to manage your Crumb account and provider connections.', path: '/login', noIndex: true })

export default async function LoginPage() {
  if (await currentUser()) redirect('/profiles')
  return <><SiteHeader /><main className="auth-page">
    <section className="auth-card">
      <div className="auth-logos"><img src="/favicon.ico" alt="Crumb" /><span>↔</span><div className="elixpo-logo">e</div></div>
      <p className="kicker">Secure account connection</p>
      <h1>Sign in to Crumb</h1>
      <p>Use your Elixpo account to connect Pollinations once, then authorize terminals through the device flow.</p>
      <a className="button auth-button" href="/api/auth/login?return_to=%2Fconnect">Continue with Elixpo</a>
      <div className="trust-row"><span>Encrypted connector</span><span>Device authorization</span><span>No pasted keys</span></div>
      <p className="fine">Crumb requests only your identity, profile, and email.</p>
    </section>
  </main></>
}
