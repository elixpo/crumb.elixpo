import Link from 'next/link'
import { SiteFooter } from '@/app/components/SiteFooter'
import { SiteHeader } from '@/app/components/SiteHeader'
import { pageMetadata } from '@/lib/seo'

const REPO = 'https://github.com/elixpo/crumb.elixpo'

export const metadata = pageMetadata({
  title: 'Plugins',
  description: 'Add useful tools to Crumb for files, repositories, databases, cloud services, and everyday developer work.',
  path: '/plugins',
  keywords: ['AI terminal plugins', 'MCP terminal', 'developer tools', 'terminal integrations'],
})

const plugins = [
  { mark: 'FS', name: 'Workspace tools', copy: 'Read files, search folders, and prepare edits inside the project you opened.', status: 'Built in' },
  { mark: 'GH', name: 'GitHub', copy: 'Work with issues, pull requests, checks, and releases from the terminal.', status: 'Planned' },
  { mark: 'DB', name: 'Databases', copy: 'Inspect schemas and run approved queries without handing the model an open connection.', status: 'Planned' },
  { mark: 'MCP', name: 'MCP servers', copy: 'Bring compatible tools into Crumb through one typed, permission-aware interface.', status: 'Foundation' },
]

export default function PluginsPage() {
  return <><SiteHeader /><main className="product-page plugins-page">
    <header className="product-hero"><p className="kicker">Plugins</p><h1>Add tools without<br />giving up control.</h1><p>Plugins let Crumb do more than talk. They can read a file, check a pull request, or query a service—but only through clear inputs, limited output, and permissions you control.</p><div><a className="button" href={`${REPO}/issues/new?template=plugin.yml`}>Request a plugin <span>↗</span></a><Link className="button button-secondary" href="/skills">Explore skills</Link></div></header>

    <section className="plugin-grid">{plugins.map(plugin => <article key={plugin.name}><div><i>{plugin.mark}</i><span>{plugin.status}</span></div><h2>{plugin.name}</h2><p>{plugin.copy}</p></article>)}</section>

    <section className="plugin-explainer"><div><p className="kicker">The easy distinction</p><h2>Skills know how.<br />Plugins can do.</h2><p>A skill explains the best way to approach a task. A plugin supplies a tool for that task. A connector signs you into an outside service when the plugin needs one.</p></div><ol><li><b>Skill</b><span>“Review this Rust change carefully.”</span></li><li><b>Plugin</b><span>Read the diff and leave a review.</span></li><li><b>Connector</b><span>Use your approved GitHub account.</span></li></ol></section>

    <section className="plugin-safety"><p className="kicker">Safe by default</p><h2>The model cannot give itself more access.</h2><div><article><b>See the request</b><p>Crumb shows what a tool wants to do before an important action.</p></article><article><b>Stop the chain</b><p>Ctrl+C stops the agent, the plugin, and its active command together.</p></article><article><b>Remove it cleanly</b><p>Disable a plugin or revoke a connector without breaking the native shell.</p></article></div></section>

    <section className="request-banner"><div><p className="kicker">Need another tool?</p><h2>Request the capability.<br />We will keep it bounded.</h2><p>Tell us what the plugin should do, what access it needs, and how failure should behave.</p></div><div><a className="button button-light" href={`${REPO}/issues/new?template=plugin.yml`}>Propose a plugin</a></div></section>
  </main><SiteFooter /></>
}
