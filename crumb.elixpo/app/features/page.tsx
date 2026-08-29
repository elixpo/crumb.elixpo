import Link from 'next/link'
import { SiteFooter } from '@/app/components/SiteFooter'
import { SiteHeader } from '@/app/components/SiteHeader'
import { pageMetadata } from '@/lib/seo'

export const metadata = pageMetadata({
  title: 'Features',
  description: 'Explore Crumb’s native shell, deterministic natural-language routing, agent Harness, skills, permissions, sessions, and secure connectors.',
  path: '/features',
  keywords: ['AI terminal features', 'natural language shell', 'agentic terminal', 'native shell AI'],
})

const features = [
  { icon: '$_', title: 'A real native shell', copy: 'Bash, Zsh, and PowerShell keep ownership of expansion, pipes, processes, environment state, and full-screen applications.', tag: 'Native' },
  { icon: '↳', title: 'Deterministic routing', copy: 'Executable discovery and shell syntax decide locally whether input is a command or a natural-language task. Routing never spends a token.', tag: 'Zero AI' },
  { icon: '◆', title: 'Replaceable Harness', copy: 'Agent sessions run through an optional Harness boundary, with exact model and effort selection kept outside the shell core.', tag: 'Agent' },
  { icon: '⌃C', title: 'Hard cancellation', copy: 'Ctrl+C cancels the turn, Harness process group, MCP descendants, and active child command instead of merely hiding output.', tag: 'Control' },
  { icon: '◇', title: 'Skills, not guesses', copy: 'Discoverable skills define activation, task instructions, preferred models, context, tools, and required approvals.', tag: 'Extensible' },
  { icon: '⌁', title: 'Scoped connectors', copy: 'Link a provider in the browser, authorize a device, and keep credentials out of prompts, memories, logs, and terminal history.', tag: 'Secure' },
  { icon: '◫', title: 'Persistent sessions', copy: 'Resume useful context with bounded journals that store event metadata and digests rather than raw secrets or unrestricted output.', tag: 'Stateful' },
  { icon: '⚑', title: 'Explicit permissions', copy: 'Every tool carries a risk class. Models can request capabilities, but they cannot grant permissions to themselves.', tag: 'Policy' },
  { icon: '⊘', title: 'Graceful failure', copy: 'Crumb starts quickly and stays useful when the provider, network, connector, skill, optimizer, or entire AI layer is unavailable.', tag: 'Reliable' },
]

export default function FeaturesPage() {
  return <><SiteHeader /><main className="product-page">
    <header className="product-hero"><p className="kicker">Product features</p><h1>AI that understands<br />where the shell ends.</h1><p>Crumb combines native terminal semantics with a separate, permissioned agent runtime. The result feels fast for everyday commands and deliberate for consequential work.</p><div><Link className="button" href="/docs/getting-started">Get started</Link><Link className="button button-secondary" href="/docs/security">Read the security model</Link></div></header>

    <section className="product-feature-grid">{features.map((feature, index) => <article key={feature.title}><div><span>{String(index + 1).padStart(2, '0')}</span><em>{feature.tag}</em></div><i>{feature.icon}</i><h2>{feature.title}</h2><p>{feature.copy}</p></article>)}</section>

    <section className="feature-comparison"><div><p className="kicker">The operating contract</p><h2>Native first.<br />Agent when useful.</h2></div><div className="comparison-table"><div><span>Input</span><span>Owner</span><span>Network</span></div><div><strong>cargo test --workspace</strong><span>Native shell</span><b>Never required</b></div><div><strong>fix the failing tests</strong><span>Agent Harness</span><b>Provider route</b></div><div><strong>/skills</strong><span>Crumb locally</span><b>Never required</b></div><div><strong>@file:Cargo.toml</strong><span>Context resolver</span><b>Only with request</b></div></div></section>

    <section className="product-banner"><p className="kicker">Build your layer</p><h2>Skills shape the work.<br />Plugins provide the tools.</h2><p>Keep domain instructions reviewable and capabilities replaceable without hard-coding a profession into the terminal.</p><Link className="button button-light" href="/skills">Explore the ecosystem</Link></section>
  </main><SiteFooter /></>
}
