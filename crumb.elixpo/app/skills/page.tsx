import Link from 'next/link'
import { SiteFooter } from '@/app/components/SiteFooter'
import { SiteHeader } from '@/app/components/SiteHeader'
import { pageMetadata } from '@/lib/seo'

const REPO = 'https://github.com/elixpo/crumb.elixpo'

export const metadata = pageMetadata({
  title: 'Skills and Plugins',
  description: 'Discover how Crumb skills provide task instructions while plugins expose typed, permissioned terminal capabilities.',
  path: '/skills',
  keywords: ['AI terminal skills', 'terminal plugins', 'agent skills', 'MCP terminal tools'],
})

const skillExamples = [
  { icon: 'Rs', title: 'Rust maintainer', copy: 'Reads workspace conventions, narrows failures, prepares minimal patches, and asks you to run expensive checks.', state: 'Foundation' },
  { icon: 'DB', title: 'Database debugger', copy: 'Inspects schema and migration context, proposes reversible changes, and keeps writes behind approval.', state: 'Planned' },
  { icon: 'GH', title: 'Repository operator', copy: 'Works with issues, pull requests, releases, and CI through a narrowly scoped GitHub connector.', state: 'Planned' },
  { icon: 'Ops', title: 'Incident investigator', copy: 'Collects bounded diagnostics, builds a timeline, and separates observation from mutating recovery steps.', state: 'Planned' },
]

export default function SkillsPage() {
  return <><SiteHeader /><main className="product-page skills-page">
    <header className="product-hero"><p className="kicker">Skills & plugins</p><h1>Teach the terminal<br />how your work works.</h1><p>Skills are inspectable instructions for a kind of task. Plugins are typed capabilities. Together they let Crumb adapt without giving a model unrestricted access.</p><div><a className="button" href={`${REPO}/issues/new?template=skill.yml`}>Request a skill <span>↗</span></a><a className="button button-secondary" href={`${REPO}/issues/new?template=plugin.yml`}>Request a plugin <span>↗</span></a></div></header>

    <section className="skill-definition"><article><span>01</span><h2>Skill</h2><p>A versioned playbook: when to activate, what context to collect, which model route to prefer, and what must be approved.</p><code>@skill:rust-maintainer</code></article><article><span>02</span><h2>Plugin</h2><p>A replaceable capability exposed through a typed boundary, with schemas, risk classes, cancellation, and bounded output.</p><code>@plugin:github</code></article><article><span>03</span><h2>Connector</h2><p>A user-authorized service identity. Connectors grant narrow access; they never silently expand what a skill or plugin may do.</p><code>@connector:pollinations</code></article></section>

    <section className="skill-gallery"><div className="section-heading"><div><p className="kicker">Designed for discovery</p><h2>Start from intent,<br />not tool syntax.</h2></div><p>Type <code>@skill:</code> and press Tab to inspect enabled skills. Crumb loads only what the request needs.</p></div><div className="skill-grid">{skillExamples.map(skill => <article key={skill.title}><div><i>{skill.icon}</i><span>{skill.state}</span></div><h3>{skill.title}</h3><p>{skill.copy}</p><small>Explicit activation · bounded context</small></article>)}</div></section>

    <section className="skill-lifecycle"><div><p className="kicker">A reviewable lifecycle</p><h2>Nothing installs itself.</h2><p>Discovery, installation, activation, permissions, and removal remain user-controlled stages.</p></div><ol><li><b>Discover</b><span>Read metadata before loading instructions.</span></li><li><b>Inspect</b><span>Review models, tools, scopes, and source.</span></li><li><b>Enable</b><span>Add it to live configuration explicitly.</span></li><li><b>Run</b><span>Approve consequential actions at execution time.</span></li></ol></section>

    <section className="request-banner"><div><p className="kicker">Missing your workflow?</p><h2>Define the outcome.<br />We will define the boundary.</h2><p>Open a structured request with activation examples, expected tools, permissions, and acceptance criteria.</p></div><div><a className="button button-light" href={`${REPO}/issues/new?template=skill.yml`}>Propose a skill</a><a className="button button-outline-light" href={`${REPO}/issues/new?template=plugin.yml`}>Propose a plugin</a></div></section>
  </main><SiteFooter /></>
}
