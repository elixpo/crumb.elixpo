import Link from 'next/link'
import { SiteFooter } from './components/SiteFooter'
import { SiteHeader } from './components/SiteHeader'
import { DynamicCta } from './components/DynamicCta'

export default function Home() {
  return <><SiteHeader /><main>
    <section className="hero">
      <div className="hero-glow" />
      <p className="status-pill"><span /> Native shell · optional intelligence</p>
      <h1>The terminal that<br /><em>speaks your language.</em></h1>
      <p className="hero-copy">Crumb is the Natural Language Terminal. Commands run natively; models, skills, and tools appear only when you ask for them.</p>
      <div className="hero-actions"><a className="button" href="https://github.com/elixpo/crumb.elixpo">Get Crumb <span>↗</span></a><Link className="button button-secondary" href="/profile/connectors">Check Connectors</Link></div>
      <div className="trust-strip"><span>No shell replacement</span><i /><span>No keys in history</span><i /><span>Open source</span></div>
      <div className="terminal-window" aria-label="Crumb terminal preview">
        <div className="terminal-title"><div><i /><i /><i /></div><span>crumb — ~/projects/NLT</span><b>NLT</b></div>
        <pre><span className="dim">╭─[ crumb ]─[ ~/projects/NLT ]─[ linux ]─[ git:main ]</span>{'\n'}<span className="caret">╰─❯</span> <span className="ask">find why the API tests fail and fix the smallest issue</span>{'\n'}<span className="agent">◆ harness</span> qwen-coder · effort: high · skill: rust-maintainer{'\n'}<span className="tool">  ↳ read</span> Cargo.toml, crates/api/tests{'\n'}<span className="tool">  ↳ run </span><span className="command">cargo test -p crumb-api</span>{'\n'}<span className="red">    error[E0308]</span> mismatched status type at src/routes.rs:84{'\n'}<span className="tool">  ↳ patch</span> crates/api/src/routes.rs <span className="yes">approved</span>{'\n'}<span className="tool">  ↳ run </span><span className="command">cargo test -p crumb-api</span>{'\n'}<span className="green">    12 passed</span>; 0 failed{'\n\n'}<span className="agent">◆ done</span> Normalized the handler&apos;s status conversion; no API behavior changed.{'\n'}<span className="dim">  1 file changed · session saved · Ctrl+C cancels at any step</span></pre>
      </div>
    </section>

    <section className="section" id="features"><p className="kicker">Built around the shell</p><div className="section-heading"><h2>Intelligence without taking over.</h2><p>Your shell remains the source of truth. Crumb adds a deliberate AI layer around it—not inside it. <Link className="inline-arrow" href="/features">Explore every feature →</Link></p></div>
      <div className="feature-grid">
        <article><span>01</span><div className="feature-icon">$_</div><h3>Native by default</h3><p>Bash, Zsh, and PowerShell retain their real semantics, processes, state, and shortcuts.</p></article>
        <article><span>02</span><div className="feature-icon">◆</div><h3>Skills over guesses</h3><p>Skills define the model, tools, context, and permissions needed for a task.</p></article>
        <article><span>03</span><div className="feature-icon">⌁</div><h3>Safe connections</h3><p>Bind providers in your browser and authorize devices without pasting secrets into a terminal.</p></article>
      </div>
    </section>

    <section className="boundary-section"><div><p className="kicker">A clear boundary</p><h2>You decide when AI enters the session.</h2><p>Every ordinary command stays ordinary. Natural language routes into an isolated agent environment with explicit permissions and provider-neutral interfaces.</p><Link className="text-link" href="/docs">Read how Crumb works <span>→</span></Link></div><ol><li><b>1</b><span><strong>Type a command</strong>It runs in your persistent native shell.</span></li><li><b>2</b><span><strong>Ask in plain English</strong>Crumb selects an applicable skill and model.</span></li><li><b>3</b><span><strong>Approve real actions</strong>Models cannot grant themselves permissions.</span></li></ol></section>

    <section className="flow-section">
      <div className="flow-heading"><p className="kicker">One line, three layers</p><h2>Fast enough to stay in flow.<br />Strict enough to trust.</h2></div>
      <div className="flow-track"><article><span>01</span><b>Deterministic router</b><p>Known commands stay native. Plain-language work enters the agent without a prompt prefix.</p></article><i>→</i><article><span>02</span><b>Skill + Harness</b><p>Crumb selects configured instructions, model effort, tools, and context for the task.</p></article><i>→</i><article><span>03</span><b>Native execution</b><p>The isolated agent calls Bash, Zsh, or PowerShell through typed, cancellable permissions.</p></article></div>
    </section>

    <section className="namespace-section">
      <div><p className="kicker">Designed for recall</p><h2>A small grammar for serious work.</h2><p>Write normally to ask. Use `/` for Crumb actions and `@` to bring precise context into the request.</p><Link className="text-link" href="/docs/cli">Learn the CLI grammar <span>→</span></Link></div>
      <div className="namespace-cards"><article><code>/</code><span><b>Control the terminal</b><small>/skills · /mode · /connectors · /doctor</small></span></article><article><code>@</code><span><b>Reference context</b><small>@file · @folder · @skill · @connector</small></span></article><article><code>⌃C</code><span><b>Stop the whole chain</b><small>Cancel the model, Harness, and child process.</small></span></article></div>
    </section>

    <section className="ecosystem-section"><div><p className="kicker">Composable by design</p><h2>Bring the workflow.<br />Keep the boundaries.</h2><p>Skills teach Crumb how to approach a job. Plugins expose replaceable capabilities. Connectors grant narrow access to services you choose.</p><div><Link className="button" href="/skills">Explore skills</Link><Link className="button button-secondary" href="/profile/connectors">Manage connectors</Link></div></div><aside><div><span>Skills</span><strong>Task instructions</strong><small>Discoverable · inspectable · versioned</small></div><div><span>Plugins</span><strong>Local capabilities</strong><small>Typed tools · explicit risk · replaceable</small></div><div><span>Connectors</span><strong>External services</strong><small>Scoped OAuth · revocable · user-owned</small></div></aside></section>

    <DynamicCta />
  </main><SiteFooter /></>
}
